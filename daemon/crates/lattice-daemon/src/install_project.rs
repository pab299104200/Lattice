//! Safe, idempotent repository-local Lattice installation.

use crate::install::{
    reconcile_hook_config, reconcile_mcp_config, render_config, HookClient, HookMode, InstallPaths,
};
use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::fs::{self};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use toml_edit::{value, Array, DocumentMut, Item, Table};

const START: &str = "<!-- lattice:project-instructions:start -->";
const END: &str = "<!-- lattice:project-instructions:end -->";
const INSTRUCTIONS: &str = r#"<!-- lattice:project-instructions:start -->
## Lattice workflow

For every task, start with `context`; run `prepare_change` before editing. If Lattice is unavailable, continue with repository inspection and report that retrieval was unavailable. Scope `recall` to the current repository, checkout, branch, and task; treat recalled memory as guidance rather than proof. Run `impact` before non-obvious or multi-file changes and `diagnose` against concrete failures. Store only verified, durable outcomes with `remember`.

For long plans, subagent work, or context compaction, preserve the task objective, current authority, accepted decisions, completed validation, and remaining work. Give subagents bounded file ownership and require exact findings and tests in their handoff. After compaction or a material branch/workspace change, refresh with `context` and `prepare_change`, then verify subagent findings before applying them.
<!-- lattice:project-instructions:end -->"#;
const HOOKS: [&str; 7] = [
    "common.sh",
    "session-start.sh",
    "user-prompt-submit.sh",
    "pre-tool-use.sh",
    "post-tool-use.sh",
    "stop.sh",
    "session-end.sh",
];

struct Plan {
    path: PathBuf,
    original: Option<Vec<u8>>,
    content: Vec<u8>,
}

struct StagedFile {
    directory: lattice_core::storage::SecureDir,
    name: String,
    identity: lattice_core::storage::managed_fs::ManagedIdentity,
    published: bool,
}
impl Drop for StagedFile {
    fn drop(&mut self) {
        if !self.published {
            let _ = self.directory.remove_file(&self.name, self.identity);
        }
    }
}

pub(crate) fn install_project(
    workspace: &Path,
    paths: &InstallPaths,
    mode: HookMode,
) -> Result<Vec<PathBuf>> {
    if !workspace.is_absolute() {
        bail!(
            "project workspace must be absolute: {}",
            workspace.display()
        );
    }
    reject_symlink(workspace)?;
    let configured_workspace = workspace.to_path_buf();
    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("resolve workspace {}", workspace.display()))?;
    if !workspace.is_dir() {
        bail!(
            "project workspace is not a directory: {}",
            workspace.display()
        );
    }
    preflight_assets(paths)?;
    let root =
        lattice_core::storage::SecureDir::open(&workspace).context("pin project workspace")?;

    let specs = [
        (workspace.join(".mcp.json"), Kind::Mcp),
        (workspace.join(".codex/config.toml"), Kind::CodexToml),
        (
            workspace.join(".codex/hooks.json"),
            Kind::Hooks(HookClient::Codex),
        ),
        (
            workspace.join(".claude/settings.json"),
            Kind::Hooks(HookClient::ClaudeCode),
        ),
        (workspace.join("AGENTS.md"), Kind::Instructions),
        (workspace.join("CLAUDE.md"), Kind::Instructions),
    ];
    let mut plans = Vec::new();
    for (path, kind) in specs {
        validate_target(&workspace, &path)?;
        let relative_parent = path.parent().unwrap().strip_prefix(&workspace)?;
        let original = (|| -> std::io::Result<Option<Vec<u8>>> {
            let parent = if relative_parent.as_os_str().is_empty() {
                root.try_clone()?
            } else {
                match root.open_dir(relative_parent) {
                    Ok(parent) => parent,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => return Err(error),
                }
            };
            match parent.open_file(path.file_name().unwrap().to_str().unwrap(), false) {
                Ok(mut file) => {
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes)?;
                    Ok(Some(bytes))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            }
        })()
        .with_context(|| format!("read pinned project configuration {}", path.display()))?;
        let content = render(
            kind,
            original.as_deref(),
            &configured_workspace,
            paths,
            mode,
        )
        .with_context(|| format!("preflight {}", path.display()))?;
        plans.push(Plan {
            path,
            original,
            content: content.into_bytes(),
        });
    }

    let lattice = root
        .create_dir(".lattice")
        .context("create project Lattice directory")?;
    let _owner = acquire_install_lock(&lattice, Duration::from_secs(5))?;

    let mut changed = Vec::new();
    let outcome = (|| -> Result<()> {
        for plan in plans {
            if plan.original.as_deref() == Some(plan.content.as_slice()) {
                continue;
            }
            let parent = secure_parent(&root, &workspace, plan.path.parent().unwrap())?;
            let leaf = plan
                .path
                .file_name()
                .and_then(|v| v.to_str())
                .context("non-UTF-8 project filename")?;
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let tmp = format!(".{leaf}.lattice-install-{}-{nonce}.tmp", std::process::id());
            let mut file = parent
                .open_new_file(&tmp)
                .with_context(|| format!("stage {}", plan.path.display()))?;
            let mut staged_guard = StagedFile {
                directory: parent.try_clone()?,
                name: tmp.clone(),
                identity: lattice_core::storage::SecureDir::file_identity(&file)?,
                published: false,
            };
            file.write_all(&plan.content)?;
            file.sync_all()?;
            let (current, expected) = match parent.metadata(leaf)? {
                Some(_) => {
                    let mut bytes = Vec::new();
                    let mut current_file = parent.open_file(leaf, false)?;
                    let identity = lattice_core::storage::SecureDir::file_identity(&current_file)?;
                    current_file.read_to_end(&mut bytes)?;
                    (Some(bytes), Some(identity))
                }
                None => (None, None),
            };
            if current != plan.original {
                bail!("{} changed while project installation was being prepared; no concurrent edit was overwritten", plan.path.display());
            }
            #[cfg(test)]
            BEFORE_PUBLISH.with(|hook| {
                if let Some(hook) = hook.borrow_mut().as_mut() {
                    hook(&plan.path);
                }
            });
            parent
                .replace_from(&tmp, &parent, leaf, staged_guard.identity, expected)
                .with_context(|| {
                    format!(
                        "publish {} (earlier reported paths may already have been updated)",
                        plan.path.display()
                    )
                })?;
            staged_guard.published = true;
            changed.push(plan.path);
        }
        Ok(())
    })();
    outcome.with_context(|| format!("project installation failed; files already updated: [{}]; correct the error and rerun installation", changed.iter().map(|path: &PathBuf| path.display().to_string()).collect::<Vec<_>>().join(", ")))?;
    Ok(changed)
}

#[cfg(test)]
thread_local! {
    static BEFORE_PUBLISH: std::cell::RefCell<Option<Box<dyn FnMut(&Path)>>> = std::cell::RefCell::new(None);
}

fn secure_parent(
    root: &lattice_core::storage::SecureDir,
    workspace: &Path,
    parent: &Path,
) -> Result<lattice_core::storage::SecureDir> {
    let relative = parent.strip_prefix(workspace)?;
    let mut current = root.try_clone()?;
    for component in relative.components() {
        let name = component
            .as_os_str()
            .to_str()
            .context("non-UTF-8 project directory")?;
        current = current.create_dir(name)?;
    }
    Ok(current)
}

fn acquire_install_lock(
    directory: &lattice_core::storage::SecureDir,
    deadline: Duration,
) -> Result<std::fs::File> {
    let file = directory.open_or_create_file("install-project.lock")?;
    let started = std::time::Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                bail!("timed out acquiring project installer lock")
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).context("acquire project installer lock")
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Mcp,
    CodexToml,
    Hooks(HookClient),
    Instructions,
}

fn render(
    kind: Kind,
    bytes: Option<&[u8]>,
    workspace: &Path,
    paths: &InstallPaths,
    mode: HookMode,
) -> Result<String> {
    let text = match bytes {
        Some(bytes) => std::str::from_utf8(bytes).context("configuration is not UTF-8")?,
        None => "",
    };
    match kind {
        Kind::Mcp => {
            let mut json: Value = if text.trim().is_empty() {
                Value::Object(Map::new())
            } else {
                serde_json::from_str(text).context("malformed JSON")?
            };
            reconcile_mcp_config(&mut json, &paths.executable, &[workspace.to_path_buf()])?;
            render_config(&json)
        }
        Kind::Hooks(client) => {
            let mut json: Value = if text.trim().is_empty() {
                Value::Object(Map::new())
            } else {
                serde_json::from_str(text).context("malformed JSON")?
            };
            reconcile_hook_config(&mut json, client, paths, mode)?;
            render_config(&json)
        }
        Kind::CodexToml => {
            let mut doc = if text.trim().is_empty() {
                DocumentMut::new()
            } else {
                text.parse::<DocumentMut>().context("malformed TOML")?
            };
            if let Some(item) = doc.as_table().get("mcp_servers") {
                if !item.is_table() {
                    bail!("`mcp_servers` must be a TOML table");
                }
            } else {
                doc["mcp_servers"] = Item::Table(Table::new());
            }
            if let Some(item) = doc["mcp_servers"]
                .as_table()
                .and_then(|table| table.get("lattice"))
            {
                if !item.is_table() {
                    bail!("`mcp_servers.lattice` must be a TOML table");
                }
            }
            let preserved = doc
                .as_table()
                .get("mcp_servers")
                .and_then(Item::as_table)
                .and_then(|table| table.get("lattice"))
                .and_then(Item::as_table)
                .cloned();
            let mut lattice = Table::new();
            if let Some(previous) = preserved {
                for field in [
                    "default_tools_approval_mode",
                    "tools",
                    "enabled_tools",
                    "disabled_tools",
                ] {
                    if let Some(item) = previous.get(field) {
                        lattice.insert(field, item.clone());
                    }
                }
            }
            lattice["command"] = value(paths.executable.to_string_lossy().as_ref());
            let mut args = Array::new();
            args.push("--stdio");
            args.push("--workspace");
            args.push(workspace.to_string_lossy().as_ref());
            lattice["args"] = value(args);
            lattice["enabled"] = value(true);
            doc["mcp_servers"]["lattice"] = Item::Table(lattice);
            Ok(doc.to_string())
        }
        Kind::Instructions => reconcile_instructions(text),
    }
}

fn reconcile_instructions(text: &str) -> Result<String> {
    let starts = text.matches(START).count();
    let ends = text.matches(END).count();
    if starts > 1 || ends > 1 || starts != ends {
        bail!("malformed or duplicate Lattice instruction block markers");
    }
    let base = if starts == 1 {
        let start = text.find(START).unwrap();
        let end_start = text.find(END).unwrap();
        if end_start < start {
            bail!("Lattice instruction block end marker precedes its start marker");
        }
        let end = end_start + END.len();
        format!("{}{}{}", &text[..start], INSTRUCTIONS, &text[end..])
    } else if text.trim().is_empty() {
        format!("{INSTRUCTIONS}\n")
    } else {
        let separator = if text.ends_with('\n') { "\n" } else { "\n\n" };
        format!("{text}{separator}{INSTRUCTIONS}\n")
    };
    Ok(base)
}

fn preflight_assets(paths: &InstallPaths) -> Result<()> {
    for client in ["codex", "claude-code"] {
        for hook in HOOKS {
            let path = paths
                .asset_root
                .join("integrations")
                .join(client)
                .join("hooks")
                .join(hook);
            reject_symlink_if_present(&path)?;
            if !path.is_file() {
                bail!("required hook asset is missing: {}", path.display());
            }
        }
    }
    Ok(())
}
fn reject_symlink(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        bail!("refusing symlink path: {}", path.display());
    }
    Ok(())
}
fn reject_symlink_if_present(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!("refusing symlink path: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
fn validate_target(workspace: &Path, path: &Path) -> Result<()> {
    if !path.starts_with(workspace) {
        bail!(
            "project install target escapes workspace: {}",
            path.display()
        );
    }
    let mut cursor = path.parent();
    while let Some(parent) = cursor {
        if parent == workspace {
            break;
        }
        reject_symlink_if_present(parent)?;
        cursor = parent.parent();
    }
    reject_symlink_if_present(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_target_creation_reports_partial_files_cleans_staging_and_retries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir(&root).unwrap();
        let assets_root = dir.path().join("assets");
        assets(&assets_root);
        let paths = InstallPaths::new(PathBuf::from("/opt/lattice"), assets_root).unwrap();
        BEFORE_PUBLISH.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|path| {
                if path.ends_with("config.toml") {
                    fs::write(path, "# concurrent user edit\n[user]\nkeep = true\n").unwrap();
                }
            }))
        });
        let result = install_project(&root, &paths, HookMode::BestEffort);
        BEFORE_PUBLISH.with(|hook| *hook.borrow_mut() = None);
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("files already updated:"), "{error}");
        assert!(error.contains(".mcp.json"), "{error}");
        assert_eq!(
            fs::read_to_string(root.join(".codex/config.toml")).unwrap(),
            "# concurrent user edit\n[user]\nkeep = true\n"
        );
        assert!(
            !fs::read_dir(root.join(".codex")).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("lattice-install-"))
        );
        install_project(&root, &paths, HookMode::BestEffort).unwrap();
        assert!(fs::read_to_string(root.join(".codex/config.toml"))
            .unwrap()
            .contains("keep = true"));
        assert!(install_project(&root, &paths, HookMode::BestEffort)
            .unwrap()
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn installer_rejects_symlinked_state_and_does_not_take_memory_owner_lock() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let outside = dir.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        let assets_root = dir.path().join("assets");
        assets(&assets_root);
        let paths = InstallPaths::new(PathBuf::from("/opt/lattice"), assets_root).unwrap();
        symlink(&outside, root.join(".lattice")).unwrap();
        assert!(install_project(&root, &paths, HookMode::BestEffort).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        fs::remove_file(root.join(".lattice")).unwrap();
        fs::create_dir(root.join(".lattice")).unwrap();
        let pinned = lattice_core::storage::SecureDir::open(&root.join(".lattice")).unwrap();
        let _memory_owner = lattice_core::memory::RepositoryMemoryOwner::acquire_in(
            &pinned,
            Duration::from_millis(50),
        )
        .unwrap();
        install_project(&root, &paths, HookMode::BestEffort).unwrap();
    }

    fn assets(root: &Path) {
        for c in ["codex", "claude-code"] {
            let d = root.join("integrations").join(c).join("hooks");
            fs::create_dir_all(&d).unwrap();
            for h in HOOKS {
                fs::write(d.join(h), "#!/bin/sh\n").unwrap();
            }
        }
    }
    #[test]
    fn fresh_repeat_and_preservation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir(&root).unwrap();
        let assets_root = dir.path().join("assets");
        assets(&assets_root);
        fs::write(root.join("AGENTS.md"), "custom\n").unwrap();
        fs::create_dir(root.join(".codex")).unwrap();
        fs::write(
            root.join(".codex/config.toml"),
            "# keep\n[custom]\nvalue=1\n",
        )
        .unwrap();
        let paths = InstallPaths::new(PathBuf::from("/opt/lattice"), assets_root).unwrap();
        assert_eq!(
            install_project(&root, &paths, HookMode::BestEffort)
                .unwrap()
                .len(),
            6
        );
        assert!(install_project(&root, &paths, HookMode::BestEffort)
            .unwrap()
            .is_empty());
        assert!(fs::read_to_string(root.join("AGENTS.md"))
            .unwrap()
            .contains("custom"));
        let toml = fs::read_to_string(root.join(".codex/config.toml")).unwrap();
        assert!(toml.contains("# keep"));
        assert!(toml.contains("--stdio"));
    }
    #[test]
    fn preflight_failure_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("AGENTS.md"), format!("{START}\nbroken")).unwrap();
        let assets_root = dir.path().join("assets");
        assets(&assets_root);
        let paths = InstallPaths::new(PathBuf::from("/opt/lattice"), assets_root).unwrap();
        assert!(install_project(&root, &paths, HookMode::BestEffort).is_err());
        assert!(!root.join(".mcp.json").exists());
    }

    #[test]
    fn rejects_reversed_markers_and_toml_shape_before_writing() {
        for (name, content) in [
            ("AGENTS.md", format!("{END}\n{START}\n")),
            (".codex/config.toml", "mcp_servers = false\n".to_string()),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("repo");
            fs::create_dir_all(root.join(".codex")).unwrap();
            fs::write(root.join(name), content).unwrap();
            let assets_root = dir.path().join("assets");
            assets(&assets_root);
            let paths = InstallPaths::new(PathBuf::from("/opt/lattice"), assets_root).unwrap();
            assert!(install_project(&root, &paths, HookMode::BestEffort).is_err());
            assert!(!root.join(".mcp.json").exists());
        }
    }

    #[test]
    fn missing_last_asset_and_partial_existing_install_are_safe() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(root.join(".mcp.json"), "{\"foreign\":true}\n").unwrap();
        let original = fs::read(root.join(".mcp.json")).unwrap();
        let assets_root = dir.path().join("assets");
        assets(&assets_root);
        fs::remove_file(assets_root.join("integrations/claude-code/hooks/session-end.sh")).unwrap();
        let paths = InstallPaths::new(PathBuf::from("/opt/lattice"), assets_root).unwrap();
        assert!(install_project(&root, &paths, HookMode::BestEffort).is_err());
        assert_eq!(fs::read(root.join(".mcp.json")).unwrap(), original);
        assert!(!root.join("AGENTS.md").exists());
    }
    #[cfg(unix)]
    #[test]
    fn refuses_symlink_target() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir(&root).unwrap();
        let assets_root = dir.path().join("assets");
        assets(&assets_root);
        symlink(dir.path().join("outside"), root.join("AGENTS.md")).unwrap();
        let paths = InstallPaths::new(PathBuf::from("/opt/lattice"), assets_root).unwrap();
        assert!(install_project(&root, &paths, HookMode::BestEffort).is_err());
    }
}
