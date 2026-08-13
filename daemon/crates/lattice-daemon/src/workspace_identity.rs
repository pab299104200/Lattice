//! Resolves the distinct identities a Git worktree needs at runtime.
//!
//! `checkout_root` is always the content boundary. `repository_id` and
//! `repository_lattice_dir` are shared only by checkouts of the same Git
//! common directory, so repository memory does not fragment by worktree.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceIdentity {
    /// The requested checkout root. File reads, indexing, graphs, and watcher
    /// state remain rooted here even when repository memory is shared.
    pub(crate) checkout_root: PathBuf,
    /// Stable Git common-directory identity, or checkout root for non-Git
    /// workspaces. This is the only workspace id used for memory scope.
    pub(crate) repository_id: String,
    /// The primary checkout associated with the Git common directory.
    pub(crate) repository_root: PathBuf,
    pub(crate) repository_lattice_dir: PathBuf,
}

impl WorkspaceIdentity {
    pub(crate) fn resolve(checkout_root: &Path) -> Result<Self> {
        let checkout_root = checkout_root.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize checkout root `{}`",
                checkout_root.display()
            )
        })?;

        let Some((common_git_dir, primary_checkout)) = git_identity(&checkout_root)? else {
            return Ok(Self::standalone(checkout_root));
        };
        let repository_id = common_git_dir.to_string_lossy().to_string();
        Ok(Self {
            checkout_root,
            repository_id,
            repository_lattice_dir: primary_checkout.join(".lattice"),
            repository_root: primary_checkout,
        })
    }

    pub(crate) fn standalone(checkout_root: PathBuf) -> Self {
        let repository_id = checkout_root.to_string_lossy().to_string();
        Self {
            repository_lattice_dir: checkout_root.join(".lattice"),
            repository_root: checkout_root.clone(),
            checkout_root,
            repository_id,
        }
    }

    pub(crate) fn memories_path(&self) -> PathBuf {
        self.repository_lattice_dir.join("memories.db")
    }
}

fn git_identity(checkout_root: &Path) -> Result<Option<(PathBuf, PathBuf)>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(checkout_root)
        .args(["rev-parse", "--git-common-dir", "--show-toplevel"])
        .output()
        .with_context(|| format!("failed to execute git for `{}`", checkout_root.display()))?;
    if !output.status.success() {
        return Ok(None);
    }
    let output =
        String::from_utf8(output.stdout).context("git returned a non-UTF-8 repository identity")?;
    let mut lines = output.lines();
    let raw_common_dir = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or_else(|| anyhow!("git did not return a common directory"))?;
    let raw_toplevel = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or_else(|| anyhow!("git did not return a checkout root"))?;
    let common_dir = absolute_from_checkout(checkout_root, raw_common_dir)?;
    let toplevel = absolute_from_checkout(checkout_root, raw_toplevel)?;

    // In a normal linked-worktree repository, the common git directory is
    // `<primary>/.git`; its parent is the only safe canonical home for the
    // repository-owned Lattice directory. Bare or unusual layouts have no
    // `.git` leaf, so retain Git's concrete checkout root instead of guessing.
    let primary_checkout = if common_dir.file_name().is_some_and(|name| name == ".git") {
        common_dir
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("Git common directory has no repository parent"))?
    } else {
        toplevel
    };
    Ok(Some((common_dir, primary_checkout)))
}

fn absolute_from_checkout(checkout_root: &Path, raw: &str) -> Result<PathBuf> {
    let path = PathBuf::from(raw);
    let absolute = if path.is_absolute() {
        path
    } else {
        checkout_root.join(path)
    };
    absolute.canonicalize().with_context(|| {
        format!(
            "Git reported inaccessible repository path `{}` from checkout `{}`",
            raw,
            checkout_root.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::WorkspaceIdentity;
    use lattice_core::memory::{
        Memory, MemoryScope, MemoryStore, MemoryType, MemoryVerificationStatus,
    };
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("lattice-{name}-{nonce}"))
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("git executable must be available for worktree fixture");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn real_worktree_shares_repository_memory_identity_but_keeps_checkout_root() {
        let root = unique_temp_dir("worktree-identity");
        let worktree = root.with_extension("worktree");
        fs::create_dir_all(&root).expect("create fixture repository");
        git(&root, &["init"]);
        git(&root, &["config", "user.email", "lattice@example.test"]);
        git(&root, &["config", "user.name", "Lattice Test"]);
        fs::write(root.join("fixture.rs"), "fn fixture() {}\n").expect("write fixture source");
        git(&root, &["add", "fixture.rs"]);
        git(&root, &["commit", "-m", "fixture"]);
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "fixture-worktree",
                worktree.to_str().expect("utf8 worktree path"),
            ],
        );

        let primary = WorkspaceIdentity::resolve(&root).expect("resolve primary checkout");
        let linked = WorkspaceIdentity::resolve(&worktree).expect("resolve linked worktree");
        assert_eq!(primary.repository_id, linked.repository_id);
        assert_eq!(
            primary.repository_lattice_dir,
            linked.repository_lattice_dir
        );
        assert_ne!(primary.checkout_root, linked.checkout_root);
        assert_eq!(linked.repository_root, primary.checkout_root);

        fs::create_dir_all(&primary.repository_lattice_dir).expect("create shared Lattice store");
        let primary_store =
            MemoryStore::open(&primary.memories_path()).expect("open primary memory store");
        primary_store
            .store(Memory {
                id: String::new(),
                session_id: "fixture".to_string(),
                content: "worktree memory is repository scoped".to_string(),
                memory_type: MemoryType::Observation,
                scope: MemoryScope::Repo,
                confidence: 1.0,
                linked_symbols: vec![],
                linked_files: vec![],
                workspace_id: Some(primary.repository_id.clone()),
                branch: None,
                scope_organization_id: None,
                refresh_key: None,
                source_query: None,
                created_at: 0,
                last_accessed: 0,
                access_count: 0,
                is_stale: false,
                stale_reason: None,
                verification_status: MemoryVerificationStatus::Unverified,
            })
            .expect("store repository memory");
        let linked_store =
            MemoryStore::open(&linked.memories_path()).expect("open linked memory store");
        assert_eq!(
            linked_store.list_all().expect("read shared memories").len(),
            1
        );

        let _ = fs::remove_dir_all(&worktree);
        let _ = fs::remove_dir_all(&root);
    }
}
