use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const VALIDATION_CACHE_TTL: Duration = Duration::from_millis(75);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoStateSnapshot {
    pub(crate) git_dir: PathBuf,
    pub(crate) head_ref: Option<String>,
    pub(crate) head_oid: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RepoStateTracker {
    observed: Option<RepoStateSnapshot>,
    indexed: Option<RepoStateSnapshot>,
    current_epoch: u64,
    indexed_epoch: u64,
    branch_switching: bool,
    pending_workspace_change: bool,
    last_validated_at: Option<Instant>,
    last_validated_snapshot: Option<Option<RepoStateSnapshot>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValidationOutcome {
    Fresh,
    BranchSwitch,
    WorkspaceChange,
}

impl RepoStateTracker {
    pub(crate) fn new(workspace_root: &Path) -> Self {
        let snapshot = resolve_repo_state(workspace_root);
        let epoch = u64::from(snapshot.is_some());
        Self {
            observed: snapshot.clone(),
            indexed: snapshot,
            current_epoch: epoch,
            indexed_epoch: epoch,
            branch_switching: false,
            pending_workspace_change: false,
            last_validated_at: None,
            last_validated_snapshot: None,
        }
    }

    pub(crate) fn validate(&mut self, workspace_root: &Path) -> ValidationOutcome {
        let current = self.read_current_snapshot(workspace_root);
        match current {
            Some(snapshot) => {
                if self.observed.as_ref() != Some(&snapshot) {
                    self.current_epoch = self.current_epoch.saturating_add(1).max(1);
                    self.observed = Some(snapshot.clone());
                }
                self.branch_switching = self.indexed.as_ref() != Some(&snapshot);
            }
            None => {
                self.branch_switching = false;
            }
        }

        if self.branch_switching {
            ValidationOutcome::BranchSwitch
        } else if self.pending_workspace_change {
            ValidationOutcome::WorkspaceChange
        } else {
            ValidationOutcome::Fresh
        }
    }

    pub(crate) fn current_epoch(&self) -> u64 {
        self.current_epoch
    }

    pub(crate) fn indexed_epoch(&self) -> u64 {
        self.indexed_epoch
    }

    pub(crate) fn branch_switching(&self) -> bool {
        self.branch_switching
    }

    pub(crate) fn can_publish_epoch(&self, epoch: u64) -> bool {
        self.current_epoch == epoch
    }

    #[allow(dead_code)]
    pub(crate) fn mark_workspace_change(&mut self) -> u64 {
        self.current_epoch = self.current_epoch.saturating_add(1).max(1);
        self.pending_workspace_change = true;
        self.last_validated_at = None;
        self.current_epoch
    }

    pub(crate) fn mark_published_epoch(&mut self, epoch: u64) {
        if self.current_epoch != epoch {
            return;
        }
        self.indexed = self.observed.clone();
        self.indexed_epoch = epoch;
        self.branch_switching = false;
        self.pending_workspace_change = false;
    }

    #[allow(dead_code)]
    pub(crate) fn current_branch(&self) -> Option<String> {
        self.observed
            .as_ref()
            .and_then(|snapshot| snapshot.head_ref.as_deref())
            .and_then(|head_ref| head_ref.strip_prefix("refs/heads/"))
            .map(ToString::to_string)
    }

    fn read_current_snapshot(&mut self, workspace_root: &Path) -> Option<RepoStateSnapshot> {
        if self
            .last_validated_at
            .is_some_and(|checked| checked.elapsed() <= VALIDATION_CACHE_TTL)
        {
            return self.last_validated_snapshot.clone().flatten();
        }

        let snapshot = resolve_repo_state(workspace_root);
        self.last_validated_at = Some(Instant::now());
        self.last_validated_snapshot = Some(snapshot.clone());
        snapshot
    }
}

pub(crate) fn resolve_repo_state(workspace_root: &Path) -> Option<RepoStateSnapshot> {
    let git_dir = resolve_git_dir(workspace_root)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let trimmed = head.trim();
    if let Some(head_ref) = trimmed.strip_prefix("ref:").map(str::trim) {
        let head_oid = resolve_ref_oid(&git_dir, head_ref);
        Some(RepoStateSnapshot {
            git_dir,
            head_ref: Some(head_ref.to_string()),
            head_oid,
        })
    } else if !trimmed.is_empty() {
        Some(RepoStateSnapshot {
            git_dir,
            head_ref: None,
            head_oid: Some(trimmed.to_string()),
        })
    } else {
        None
    }
}

fn resolve_git_dir(workspace_root: &Path) -> Option<PathBuf> {
    let git_path = workspace_root.join(".git");
    if git_path.is_dir() {
        return Some(git_path);
    }
    if git_path.is_file() {
        let gitdir = std::fs::read_to_string(&git_path).ok()?;
        let relative = gitdir.trim().strip_prefix("gitdir:")?.trim();
        return Some(workspace_root.join(relative));
    }
    None
}

fn resolve_ref_oid(git_dir: &Path, head_ref: &str) -> Option<String> {
    let ref_path = git_dir.join(head_ref);
    if let Ok(contents) = std::fs::read_to_string(ref_path) {
        let oid = contents.trim();
        if !oid.is_empty() {
            return Some(oid.to_string());
        }
    }

    let packed_refs = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
    for line in packed_refs.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('^') {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let oid = parts.next()?;
        let packed_ref = parts.next()?;
        if packed_ref == head_ref {
            return Some(oid.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lattice-repo-state-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(path.join(".git").join("refs").join("heads")).expect("git dirs");
        path
    }

    #[test]
    fn tracker_detects_branch_switch_and_advances_epoch() {
        let repo = temp_repo("switch");
        std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/main\n").expect("head");
        std::fs::write(
            repo.join(".git").join("refs").join("heads").join("main"),
            "aaa\n",
        )
        .expect("main ref");
        std::fs::write(
            repo.join(".git").join("refs").join("heads").join("feature"),
            "bbb\n",
        )
        .expect("feature ref");

        let mut tracker = RepoStateTracker::new(&repo);
        assert_eq!(tracker.indexed_epoch(), 1);
        assert_eq!(tracker.validate(&repo), ValidationOutcome::Fresh);

        std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/feature\n").expect("head");
        tracker.last_validated_at = None;
        assert_eq!(tracker.validate(&repo), ValidationOutcome::BranchSwitch);
        assert!(tracker.branch_switching());
        assert_eq!(tracker.current_epoch(), 2);
    }

    #[test]
    fn stale_publish_is_rejected_until_latest_epoch_publishes() {
        let repo = temp_repo("publish");
        std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/main\n").expect("head");
        std::fs::write(
            repo.join(".git").join("refs").join("heads").join("main"),
            "aaa\n",
        )
        .expect("main ref");
        std::fs::write(
            repo.join(".git").join("refs").join("heads").join("feature"),
            "bbb\n",
        )
        .expect("feature ref");
        std::fs::write(
            repo.join(".git")
                .join("refs")
                .join("heads")
                .join("feature2"),
            "ccc\n",
        )
        .expect("feature2 ref");

        let mut tracker = RepoStateTracker::new(&repo);
        std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/feature\n").expect("head");
        tracker.last_validated_at = None;
        tracker.validate(&repo);
        let epoch_feature = tracker.current_epoch();

        std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/feature2\n").expect("head");
        tracker.last_validated_at = None;
        tracker.validate(&repo);
        let epoch_feature2 = tracker.current_epoch();

        assert!(epoch_feature2 > epoch_feature);
        assert!(!tracker.can_publish_epoch(epoch_feature));
        tracker.mark_published_epoch(epoch_feature);
        assert!(tracker.branch_switching());
        tracker.mark_published_epoch(epoch_feature2);
        assert!(!tracker.branch_switching());
        assert_eq!(tracker.indexed_epoch(), epoch_feature2);
    }

    #[test]
    fn workspace_change_requires_publish_before_state_is_fresh_again() {
        let repo = temp_repo("workspace-change");
        std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/main\n").expect("head");
        std::fs::write(
            repo.join(".git").join("refs").join("heads").join("main"),
            "aaa\n",
        )
        .expect("main ref");

        let mut tracker = RepoStateTracker::new(&repo);
        assert_eq!(tracker.validate(&repo), ValidationOutcome::Fresh);

        let epoch = tracker.mark_workspace_change();
        assert_eq!(tracker.validate(&repo), ValidationOutcome::WorkspaceChange);
        tracker.mark_published_epoch(epoch);
        assert_eq!(tracker.validate(&repo), ValidationOutcome::Fresh);
    }
}
