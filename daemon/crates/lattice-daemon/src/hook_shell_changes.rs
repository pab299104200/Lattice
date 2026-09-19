//! Find files a shell command changed, from repository state alone.
//!
//! A shell tool call can rewrite any file, and the hook envelope's command
//! text, output, environment and working directory are all off limits. So the
//! adapter compares `git status` before and after: it keeps one small
//! per-session snapshot of the dirty set under the protected state root and
//! reports paths whose status, size or modification time moved.
//!
//! The comparison is bounded in time, bytes and entries. When a bound is hit
//! it reports `Degraded` rather than a partial answer that looks complete.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};
use tokio::io::AsyncReadExt;

const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
/// A dirty set larger than this is not a working change; it is an unignored
/// build tree. Comparing it on every shell call would be the cost to avoid.
pub(crate) const MAX_DIRTY_ENTRIES: usize = 4_096;
const MAX_STATUS_BYTES: usize = 1024 * 1024;
const MAX_SNAPSHOT_BYTES: u64 = 2 * 1024 * 1024;
const SNAPSHOT_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Fingerprint {
    status: String,
    size: u64,
    modified_ns: i128,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct Snapshot {
    schema_version: u32,
    head: Option<String>,
    entries: BTreeMap<String, Fingerprint>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ShellDetection {
    /// No earlier snapshot, or the checkout moved to another commit. The new
    /// snapshot is the baseline and nothing is attributed to this command.
    Baseline,
    /// Checkout-relative paths that changed since the last snapshot, sorted.
    Changed(Vec<String>),
    /// A bound was hit. The earlier snapshot is kept.
    Degraded,
}

/// Compare the working tree with this session's snapshot, then store the new
/// snapshot. `marker` is a content-free digest naming the session.
pub(crate) async fn detect_shell_changes(
    checkout_root: &Path,
    snapshot_root: &Path,
    marker: &str,
    budget: Duration,
) -> Result<ShellDetection> {
    let Some(current) = read_working_tree(checkout_root, budget).await? else {
        return Ok(ShellDetection::Degraded);
    };
    let path = snapshot_path(snapshot_root, marker)?;
    let previous = load_snapshot(&path);
    store_snapshot(snapshot_root, &path, &current)?;
    Ok(compare(previous.as_ref(), &current))
}

/// Fold a tool-made edit into the snapshot, so the next shell comparison does
/// not attribute it to the shell. A missing snapshot is left missing: the
/// next shell call establishes the baseline.
pub(crate) fn absorb_tool_edit(
    checkout_root: &Path,
    snapshot_root: &Path,
    marker: &str,
    relative_path: &str,
) -> Result<()> {
    let path = snapshot_path(snapshot_root, marker)?;
    let Some(mut snapshot) = load_snapshot(&path) else {
        return Ok(());
    };
    if snapshot.entries.len() >= MAX_DIRTY_ENTRIES && !snapshot.entries.contains_key(relative_path)
    {
        return Ok(());
    }
    let status = snapshot
        .entries
        .get(relative_path)
        .map_or_else(|| "tool".to_string(), |entry| entry.status.clone());
    snapshot.entries.insert(
        relative_path.to_string(),
        fingerprint(checkout_root, relative_path, status),
    );
    store_snapshot(snapshot_root, &path, &snapshot)
}

fn compare(previous: Option<&Snapshot>, current: &Snapshot) -> ShellDetection {
    let Some(previous) = previous else {
        return ShellDetection::Baseline;
    };
    if previous.head != current.head {
        // A checkout, pull, commit or reset moved the tree. Every difference
        // would be attributed to the shell, so none is.
        return ShellDetection::Baseline;
    }
    let changed = current
        .entries
        .iter()
        .filter(|(path, now)| match previous.entries.get(*path) {
            // A tool edit was absorbed with a placeholder status; only the
            // content fingerprint says whether the shell touched it again.
            Some(before) => before.size != now.size || before.modified_ns != now.modified_ns,
            None => true,
        })
        .map(|(path, _)| path.clone())
        .collect();
    ShellDetection::Changed(changed)
}

async fn read_working_tree(checkout_root: &Path, budget: Duration) -> Result<Option<Snapshot>> {
    let mut child = tokio::process::Command::new("git")
        .arg("-C")
        .arg(checkout_root)
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--no-renames",
            "--untracked-files=all",
        ])
        // Never contend for index.lock with the agent's own git commands.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn git status")?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("git status has no stdout"))?;
    let read = async {
        let mut bytes = Vec::new();
        (&mut stdout)
            .take((MAX_STATUS_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > MAX_STATUS_BYTES {
            return Ok::<_, anyhow::Error>(None);
        }
        let status = child.wait().await?;
        Ok(status.success().then_some(bytes))
    };
    let bytes = match tokio::time::timeout(budget, read).await {
        Ok(Ok(Some(bytes))) => bytes,
        // Dropping `child` kills a git that is still walking a large tree.
        Ok(Ok(None)) | Err(_) => return Ok(None),
        Ok(Err(error)) => return Err(error),
    };
    Ok(parse_status(checkout_root, &bytes))
}

/// Parse `git status --porcelain=v2 --branch -z --no-renames`.
fn parse_status(checkout_root: &Path, bytes: &[u8]) -> Option<Snapshot> {
    let mut snapshot = Snapshot {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        head: None,
        entries: BTreeMap::new(),
    };
    let mut records = bytes.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        let record = std::str::from_utf8(record).ok()?;
        let (status, path) = if let Some(header) = record.strip_prefix("# ") {
            if let Some(oid) = header.strip_prefix("branch.oid ") {
                snapshot.head = Some(oid.to_string());
            }
            continue;
        } else if let Some(path) = record.strip_prefix("? ") {
            ("??".to_string(), path)
        } else if record.starts_with("! ") {
            continue;
        } else {
            let fields = match record.as_bytes().first()? {
                b'1' => 9,
                b'u' => 11,
                b'2' => {
                    // Renames are disabled, but never misread the original
                    // path of one as a record of its own.
                    records.next();
                    10
                }
                _ => return None,
            };
            let mut parts = record.splitn(fields, ' ');
            let status = parts.nth(1)?.to_string();
            (status, parts.last()?)
        };
        if snapshot.entries.len() >= MAX_DIRTY_ENTRIES {
            return None;
        }
        snapshot
            .entries
            .insert(path.to_string(), fingerprint(checkout_root, path, status));
    }
    Some(snapshot)
}

fn fingerprint(checkout_root: &Path, relative_path: &str, status: String) -> Fingerprint {
    let (size, modified_ns) = fs::symlink_metadata(checkout_root.join(relative_path))
        .map(|metadata| {
            let modified_ns = metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map_or(0, |elapsed| elapsed.as_nanos() as i128);
            (metadata.len(), modified_ns)
        })
        // A deleted file is a change too; it simply has nothing to stat.
        .unwrap_or((0, -1));
    Fingerprint {
        status,
        size,
        modified_ns,
    }
}

fn snapshot_path(snapshot_root: &Path, marker: &str) -> Result<PathBuf> {
    if marker.is_empty() || !marker.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(anyhow!("shell snapshot marker is invalid"));
    }
    Ok(snapshot_root.join(format!("shell-{marker}.json")))
}

fn load_snapshot(path: &Path) -> Option<Snapshot> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_SNAPSHOT_BYTES {
        return None;
    }
    #[cfg(unix)]
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return None;
    }
    let snapshot: Snapshot = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    (snapshot.schema_version == SNAPSHOT_SCHEMA_VERSION).then_some(snapshot)
}

fn store_snapshot(snapshot_root: &Path, path: &Path, snapshot: &Snapshot) -> Result<()> {
    fs::create_dir_all(snapshot_root)?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(snapshot_root)?.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(snapshot_root, permissions)?;
    }
    prune_snapshots(snapshot_root);
    let staged = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&staged)?;
    file.write_all(&serde_json::to_vec(snapshot)?)?;
    drop(file);
    fs::rename(&staged, path).inspect_err(|_| {
        let _ = fs::remove_file(&staged);
    })?;
    Ok(())
}

fn prune_snapshots(snapshot_root: &Path) {
    let Ok(entries) = fs::read_dir(snapshot_root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let expired = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > SNAPSHOT_RETENTION);
        let ours = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("shell-"));
        if expired && ours {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = "0123456789abcdef0123456789abcdef";
    const BUDGET: Duration = Duration::from_secs(10);

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
    }

    fn repository(label: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "lattice-shell-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = base.join("checkout");
        fs::create_dir_all(root.join("src")).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "lattice@example.test"]);
        git(&root, &["config", "user.name", "Lattice Test"]);
        fs::write(root.join("src/lib.rs"), "pub fn one() {}\n").unwrap();
        fs::write(root.join("src/other.rs"), "pub fn two() {}\n").unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "fixture"]);
        (root, base.join("state"))
    }

    async fn detect(root: &Path, state: &Path) -> ShellDetection {
        detect_shell_changes(root, state, MARKER, BUDGET)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn first_call_is_a_baseline_then_only_new_changes_are_reported() {
        let (root, state) = repository("baseline");
        // Dirty before the session looked: never attributed to the shell.
        fs::write(root.join("src/other.rs"), "pub fn two() { /* wip */ }\n").unwrap();
        assert_eq!(detect(&root, &state).await, ShellDetection::Baseline);
        assert_eq!(detect(&root, &state).await, ShellDetection::Changed(vec![]));

        fs::write(root.join("src/lib.rs"), "pub fn one() { 1; }\n").unwrap();
        fs::create_dir_all(root.join("src/new dir")).unwrap();
        fs::write(
            root.join("src/new dir/added file.rs"),
            "pub fn three() {}\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/ignored.o"), "binary").unwrap();
        assert_eq!(
            detect(&root, &state).await,
            ShellDetection::Changed(vec![
                "src/lib.rs".to_string(),
                "src/new dir/added file.rs".to_string()
            ])
        );
        assert_eq!(detect(&root, &state).await, ShellDetection::Changed(vec![]));
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn a_second_edit_to_an_already_dirty_file_and_a_deletion_are_both_seen() {
        let (root, state) = repository("re-edit");
        fs::write(root.join("src/lib.rs"), "pub fn one() { 1; }\n").unwrap();
        assert_eq!(detect(&root, &state).await, ShellDetection::Baseline);
        // Same status code (" M"), different content.
        fs::write(root.join("src/lib.rs"), "pub fn one() { 1; 2; }\n").unwrap();
        assert_eq!(
            detect(&root, &state).await,
            ShellDetection::Changed(vec!["src/lib.rs".to_string()])
        );
        fs::remove_file(root.join("src/other.rs")).unwrap();
        assert_eq!(
            detect(&root, &state).await,
            ShellDetection::Changed(vec!["src/other.rs".to_string()])
        );
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn a_moved_head_resets_the_baseline_instead_of_blaming_the_shell() {
        let (root, state) = repository("head");
        assert_eq!(detect(&root, &state).await, ShellDetection::Baseline);
        fs::write(root.join("src/lib.rs"), "pub fn one() { 1; }\n").unwrap();
        git(&root, &["commit", "-q", "-am", "moved"]);
        fs::write(root.join("src/other.rs"), "pub fn two() { 2; }\n").unwrap();
        assert_eq!(detect(&root, &state).await, ShellDetection::Baseline);
        fs::write(root.join("src/other.rs"), "pub fn two() { 2; 3; }\n").unwrap();
        assert_eq!(
            detect(&root, &state).await,
            ShellDetection::Changed(vec!["src/other.rs".to_string()])
        );
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn a_tool_edit_is_absorbed_and_not_attributed_to_the_next_shell_call() {
        let (root, state) = repository("absorb");
        // Absorbing with no snapshot is a no-op, not a fabricated baseline.
        absorb_tool_edit(&root, &state, MARKER, "src/lib.rs").unwrap();
        assert_eq!(detect(&root, &state).await, ShellDetection::Baseline);

        fs::write(root.join("src/lib.rs"), "pub fn one() { 1; }\n").unwrap();
        absorb_tool_edit(&root, &state, MARKER, "src/lib.rs").unwrap();
        assert_eq!(detect(&root, &state).await, ShellDetection::Changed(vec![]));
        // The shell touching that same file afterwards is still seen.
        fs::write(root.join("src/lib.rs"), "pub fn one() { 1; 2; 3; }\n").unwrap();
        assert_eq!(
            detect(&root, &state).await,
            ShellDetection::Changed(vec!["src/lib.rs".to_string()])
        );
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn an_exhausted_budget_or_an_oversized_dirty_set_degrades_and_keeps_the_snapshot() {
        let (root, state) = repository("degraded");
        assert_eq!(detect(&root, &state).await, ShellDetection::Baseline);
        let stored = fs::read(snapshot_path(&state, MARKER).unwrap()).unwrap();

        assert_eq!(
            detect_shell_changes(&root, &state, MARKER, Duration::ZERO)
                .await
                .unwrap(),
            ShellDetection::Degraded
        );
        assert_eq!(
            fs::read(snapshot_path(&state, MARKER).unwrap()).unwrap(),
            stored
        );

        let mut status = b"# branch.oid abc\0".to_vec();
        for index in 0..=MAX_DIRTY_ENTRIES {
            status.extend_from_slice(format!("? bulk/file-{index}\0").as_bytes());
        }
        assert_eq!(parse_status(&root, &status), None);
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn porcelain_v2_records_are_parsed_including_spaces_conflicts_and_renames() {
        let root = std::env::temp_dir();
        let status = b"# branch.oid 1111\0# branch.head main\0\
            1 .M N... 100644 100644 100644 aaa bbb src/with space.rs\0\
            u UU N... 100644 100644 100644 100644 a b c src/conflict.rs\0\
            2 R. N... 100644 100644 100644 aaa bbb R100 new name.rs\0old name.rs\0\
            ? untracked dir/file.rs\0! ignored.o\0";
        let snapshot = parse_status(&root, status).unwrap();
        assert_eq!(snapshot.head.as_deref(), Some("1111"));
        assert_eq!(
            snapshot.entries.keys().cloned().collect::<Vec<_>>(),
            vec![
                "new name.rs",
                "src/conflict.rs",
                "src/with space.rs",
                "untracked dir/file.rs"
            ]
        );
        assert_eq!(snapshot.entries["src/with space.rs"].status, ".M");
        assert_eq!(snapshot.entries["src/conflict.rs"].status, "UU");
        assert_eq!(parse_status(&root, b"x garbage\0"), None);
    }

    #[cfg(unix)]
    #[test]
    fn snapshots_are_private_named_by_digest_and_distrusted_when_loosened() {
        let state = std::env::temp_dir().join(format!(
            "lattice-shell-private-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = snapshot_path(&state, MARKER).unwrap();
        let snapshot = Snapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            head: Some("abc".into()),
            entries: BTreeMap::new(),
        };
        store_snapshot(&state, &path, &snapshot).unwrap();
        assert_eq!(fs::metadata(&state).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(load_snapshot(&path), Some(snapshot));

        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&path, permissions).unwrap();
        assert_eq!(load_snapshot(&path), None);

        for invalid in ["", "../escape", "session id", "abc/def"] {
            assert!(snapshot_path(&state, invalid).is_err());
        }
        fs::remove_dir_all(state).unwrap();
    }
}
