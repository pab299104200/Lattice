//! Runtime orchestration for bounded Git-intelligence refreshes.
//!
//! Watchers only publish desired HEAD states. One latest-wins worker per
//! repository serializes mining, shares the process-wide index-work admission
//! control, and delegates publication to the transactional core store.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use lattice_core::git_intelligence::{mine_repository, repository_commit_window, GitMiningLimits};
use lattice_core::storage::{
    GitIntelligenceStore, HistoryObjectCache, StoredGitIntelligenceSnapshot,
};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::index_work::IndexWorkCoordinator;
use crate::rpc::mcp::GitIntelligenceSnapshotHandle;

const HISTORY_CACHE_RETENTION_SECS: i64 = 7 * 24 * 60 * 60;
const HISTORY_CACHE_GC_BATCH: usize = 128;

trait GitIntelligenceSnapshotPublisher: Send + Sync {
    fn publish(
        &self,
        snapshot: StoredGitIntelligenceSnapshot,
        is_fresh: bool,
    ) -> Result<(), String>;

    fn mark_stale(&self, repository_id: &str) -> Result<(), String>;
}

impl GitIntelligenceSnapshotPublisher for GitIntelligenceSnapshotHandle {
    fn publish(
        &self,
        snapshot: StoredGitIntelligenceSnapshot,
        is_fresh: bool,
    ) -> Result<(), String> {
        GitIntelligenceSnapshotHandle::publish(self, snapshot, is_fresh)
    }

    fn mark_stale(&self, repository_id: &str) -> Result<(), String> {
        GitIntelligenceSnapshotHandle::mark_stale(self, repository_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RefreshRequest {
    sequence: u64,
    expected_head: Option<String>,
}

/// Non-blocking watcher-side trigger for a repository refresh.
#[derive(Clone)]
pub(crate) struct GitIntelligenceRefreshHandle {
    sender: watch::Sender<RefreshRequest>,
    next_sequence: Arc<AtomicU64>,
    repository_id: String,
    snapshots: Arc<dyn GitIntelligenceSnapshotPublisher>,
}

impl GitIntelligenceRefreshHandle {
    /// Replaces any queued request with the newest observed HEAD.
    pub(crate) fn request(&self, expected_head: Option<String>) {
        if let Err(error) = self.snapshots.mark_stale(&self.repository_id) {
            tracing::warn!(
                repository_id = self.repository_id.as_str(),
                %error,
                "Failed to mark Git-intelligence snapshot stale"
            );
        }
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        self.sender.send_replace(RefreshRequest {
            sequence,
            expected_head,
        });
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let (sender, _receiver) = watch::channel(RefreshRequest {
            sequence: 0,
            expected_head: None,
        });
        Self {
            sender,
            next_sequence: Arc::new(AtomicU64::new(1)),
            repository_id: "test-repository".to_string(),
            snapshots: Arc::new(GitIntelligenceSnapshotHandle::default()),
        }
    }

    #[cfg(test)]
    pub(crate) fn latest_request_for_test(&self) -> (u64, Option<String>) {
        let request = self.sender.borrow();
        (request.sequence, request.expected_head.clone())
    }
}

/// A validated worker which owns the only publication path for its repository.
pub(crate) struct GitIntelligenceRuntime {
    repository_path: PathBuf,
    repository_id: String,
    store: Arc<StdMutex<GitIntelligenceStore>>,
    history_cache: Arc<StdMutex<HistoryObjectCache>>,
    requests: watch::Receiver<RefreshRequest>,
    index_work: Arc<IndexWorkCoordinator>,
    snapshots: Arc<dyn GitIntelligenceSnapshotPublisher>,
    runtime_work: Arc<crate::index_work::RuntimeWorkTracker>,
}

impl GitIntelligenceRuntime {
    /// Opens and validates the persistence surface before a workspace runtime
    /// is admitted. Mining remains deferred until the watcher captures HEAD.
    pub(crate) fn open(
        repository_path: PathBuf,
        graph_path: &Path,
        history_cache_path: &Path,
        repository_id: String,
        index_work: Arc<IndexWorkCoordinator>,
        snapshots: GitIntelligenceSnapshotHandle,
    ) -> Result<(GitIntelligenceRefreshHandle, Self)> {
        Self::open_with_snapshot_publisher(
            repository_path,
            graph_path,
            history_cache_path,
            repository_id,
            index_work,
            Arc::new(snapshots),
        )
    }

    fn open_with_snapshot_publisher(
        repository_path: PathBuf,
        graph_path: &Path,
        history_cache_path: &Path,
        repository_id: String,
        index_work: Arc<IndexWorkCoordinator>,
        snapshots: Arc<dyn GitIntelligenceSnapshotPublisher>,
    ) -> Result<(GitIntelligenceRefreshHandle, Self)> {
        let store = GitIntelligenceStore::open(graph_path).with_context(|| {
            format!(
                "failed to initialize Git intelligence for `{}`",
                repository_path.display()
            )
        })?;
        let history_cache = HistoryObjectCache::open(history_cache_path).with_context(|| {
            format!(
                "failed to initialize shared Git history cache for `{}`",
                repository_path.display()
            )
        })?;
        // Audit the active pointer while construction can still fail cleanly.
        // Recovery is intentionally not automatic here: an unreadable prior
        // generation must be retained for diagnosis rather than erased.
        let active = store.load_active(&repository_id).with_context(|| {
            format!("failed to validate Git intelligence for repository `{repository_id}`")
        })?;
        if let Some(active) = active {
            snapshots
                .publish(active, false)
                .map_err(anyhow::Error::msg)
                .context("failed to hydrate the Git-intelligence snapshot handoff")?;
        }

        let initial = RefreshRequest {
            sequence: 0,
            expected_head: None,
        };
        let (sender, requests) = watch::channel(initial);
        let handle = GitIntelligenceRefreshHandle {
            sender,
            next_sequence: Arc::new(AtomicU64::new(1)),
            repository_id: repository_id.clone(),
            snapshots: snapshots.clone(),
        };
        Ok((
            handle,
            Self {
                repository_path,
                repository_id,
                store: Arc::new(StdMutex::new(store)),
                history_cache: Arc::new(StdMutex::new(history_cache)),
                requests,
                index_work,
                snapshots,
                runtime_work: Arc::new(crate::index_work::RuntimeWorkTracker::default()),
            },
        ))
    }

    pub(crate) fn with_runtime_work_tracker(
        mut self,
        runtime_work: Arc<crate::index_work::RuntimeWorkTracker>,
    ) -> Self {
        self.runtime_work = runtime_work;
        self
    }

    pub(crate) fn spawn(mut self) -> JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }

    async fn run(&mut self) {
        while self.requests.changed().await.is_ok() {
            let request = self.requests.borrow_and_update().clone();
            if let Err(error) = self.refresh(request).await {
                tracing::warn!(
                    workspace = %self.repository_path.display(),
                    repository_id = self.repository_id.as_str(),
                    %error,
                    "Git-intelligence refresh failed; retaining the prior generation"
                );
            }
        }
    }

    async fn refresh(&self, request: RefreshRequest) -> Result<()> {
        let active = {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("Git-intelligence store lock was poisoned"))?;
            store
                .load_active(&self.repository_id)
                .context("failed to read the active Git-intelligence generation")?
        };
        if let Some(active) = active.filter(|active| active.head_commit_id == request.expected_head)
        {
            self.publish_snapshot(active, true)?;
            tracing::debug!(
                workspace = %self.repository_path.display(),
                head = ?request.expected_head,
                "Skipping Git-intelligence refresh for an already-published HEAD"
            );
            return Ok(());
        }

        let permit = Arc::new(
            self.index_work
                .acquire(
                    self.repository_path.to_string_lossy().to_string(),
                    "git_intelligence",
                )
                .await
                .context("index work coordinator closed before Git mining")?,
        );

        // A queued request can become redundant while waiting behind indexing.
        let active = {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("Git-intelligence store lock was poisoned"))?;
            store
                .load_active(&self.repository_id)
                .context("failed to re-read the active Git-intelligence generation")?
        };
        if let Some(active) = active.filter(|active| active.head_commit_id == request.expected_head)
        {
            self.publish_snapshot(active, true)?;
            return Ok(());
        }

        let repository_path = self.repository_path.clone();
        let history_cache = Arc::clone(&self.history_cache);
        let runtime_work = self.runtime_work.begin();
        let child_permit = Arc::clone(&permit);
        let mined = tokio::task::spawn_blocking(move || -> Result<_> {
            let (_runtime_work, _child_permit) = (runtime_work, child_permit);
            let limits = GitMiningLimits::default();
            let commit_window = repository_commit_window(&repository_path, limits)
                .context("read Git history cache identity")?;
            if let Some(snapshot) = history_cache
                .lock()
                .map_err(|_| anyhow::anyhow!("history cache lock was poisoned"))?
                .get(&commit_window, limits)
                .context("read shared Git history cache")?
                .1
            {
                return Ok(lattice_core::git_intelligence::RepositoryGitMiningResult {
                    report: snapshot.report.clone(),
                    snapshot,
                });
            }
            let mined = mine_repository(&repository_path, limits)
                .context("bounded repository mining failed")?;
            // The commit window is checked again by `put`; a HEAD transition
            // yields a distinct immutable object and cannot widen another
            // checkout's active generation.
            history_cache
                .lock()
                .map_err(|_| anyhow::anyhow!("history cache lock was poisoned"))?
                .put(&mined.snapshot)
                .context("publish shared Git history cache")?;
            let oldest = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .saturating_sub(HISTORY_CACHE_RETENTION_SECS as u64)
                as i64;
            let removed = history_cache
                .lock()
                .map_err(|_| anyhow::anyhow!("history cache lock was poisoned"))?
                .gc_before(oldest, HISTORY_CACHE_GC_BATCH)
                .context("collect expired shared Git history cache entries")?;
            if removed > 0 {
                tracing::info!(removed, "Shared Git history cache maintenance completed");
            }
            Ok(mined)
        })
        .await
        .context("Git mining worker panicked")??;

        // Coalesce rapid transitions. If the mined snapshot already represents
        // the latest requested HEAD it is safe to publish; otherwise the next
        // loop iteration will mine the newest state directly.
        let latest = self.requests.borrow().clone();
        let mined_head = mined.snapshot.head_commit_id().map(str::to_owned);
        if latest.sequence > request.sequence && latest.expected_head != mined_head {
            tracing::debug!(
                workspace = %self.repository_path.display(),
                mined_head = ?mined_head,
                requested_head = ?latest.expected_head,
                "Discarding superseded Git-intelligence candidate"
            );
            return Ok(());
        }

        let store = Arc::clone(&self.store);
        let repository_id = self.repository_id.clone();
        let sampled_commits = mined.report.sampled_commits;
        let runtime_work = self.runtime_work.begin();
        let child_permit = Arc::clone(&permit);
        let published = tokio::task::spawn_blocking(move || -> Result<_> {
            let (_runtime_work, _child_permit) = (runtime_work, child_permit);
            let store = store
                .lock()
                .map_err(|_| anyhow::anyhow!("Git-intelligence store lock was poisoned"))?;
            let published = store
                .publish(&repository_id, unix_timestamp()?, &mined.snapshot)
                .context("atomic Git-intelligence publication failed")?;
            Ok(published)
        })
        .await
        .context("Git-intelligence publication worker panicked")??;
        let generation = published.generation;
        self.publish_snapshot(published, true)?;

        tracing::info!(
            workspace = %self.repository_path.display(),
            repository_id = self.repository_id.as_str(),
            generation,
            sampled_commits,
            head = ?mined_head,
            "Published Git-intelligence generation"
        );
        Ok(())
    }

    fn publish_snapshot(
        &self,
        snapshot: StoredGitIntelligenceSnapshot,
        is_fresh: bool,
    ) -> Result<()> {
        self.snapshots
            .publish(snapshot, is_fresh)
            .map_err(anyhow::Error::msg)
            .context("failed to publish the Git-intelligence snapshot handoff")
    }
}

fn unix_timestamp() -> Result<i64> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs();
    i64::try_from(seconds).context("Unix timestamp exceeds SQLite integer range")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    #[derive(Default)]
    struct RecordingSnapshotPublisher {
        publications: StdMutex<Vec<(String, i64, bool)>>,
        stale_repositories: StdMutex<Vec<String>>,
    }

    impl GitIntelligenceSnapshotPublisher for RecordingSnapshotPublisher {
        fn publish(
            &self,
            snapshot: StoredGitIntelligenceSnapshot,
            is_fresh: bool,
        ) -> Result<(), String> {
            self.publications
                .lock()
                .expect("publication recorder lock")
                .push((snapshot.repository_id, snapshot.generation, is_fresh));
            Ok(())
        }

        fn mark_stale(&self, repository_id: &str) -> Result<(), String> {
            self.stale_repositories
                .lock()
                .expect("stale recorder lock")
                .push(repository_id.to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn refresh_handle_coalesces_queued_heads_to_the_latest_generation() {
        let root = unique_test_root("git-runtime-coalescing");
        init_repository(&root);
        let first_head = git_output(&root, &["rev-parse", "HEAD"]);
        std::fs::write(root.join("lib.rs"), "pub fn value() -> u8 { 2 }\n")
            .expect("update fixture source");
        run_git(&root, &["add", "lib.rs"]);
        run_git(
            &root,
            &[
                "-c",
                "user.name=Lattice Test",
                "-c",
                "user.email=lattice@example.invalid",
                "commit",
                "-q",
                "-m",
                "second",
            ],
        );
        let latest_head = git_output(&root, &["rev-parse", "HEAD"]);
        let graph_path = root.join("graph.db");
        let history_cache_path = root.join("history-object-cache.db");
        let repository_id = "coalesced-repository".to_string();
        let coordinator = IndexWorkCoordinator::new(1);
        let (handle, runtime) = GitIntelligenceRuntime::open(
            root.clone(),
            &graph_path,
            &history_cache_path,
            repository_id.clone(),
            Arc::clone(&coordinator),
            GitIntelligenceSnapshotHandle::default(),
        )
        .expect("open runtime");
        let task = runtime.spawn();

        handle.request(Some(first_head));
        handle.request(Some(latest_head.clone()));

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let published_head = GitIntelligenceStore::open(&graph_path)
                    .expect("open observer")
                    .load_active(&repository_id)
                    .expect("load observer")
                    .and_then(|active| active.head_commit_id);
                if published_head.as_deref() == Some(latest_head.as_str())
                    && coordinator.snapshot().completed_jobs == 1
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("latest generation publication");

        assert_eq!(coordinator.snapshot().completed_jobs, 1);
        task.abort();
        let _ = task.await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stable_head_is_a_noop_and_failed_mining_retains_prior_generation() {
        let root = unique_test_root("git-runtime");
        init_repository(&root);
        let graph_path = root.join("graph.db");
        let history_cache_path = root.join("history-object-cache.db");
        let repository_id = "fixture-repository".to_string();
        let coordinator = IndexWorkCoordinator::new(1);
        let snapshots = Arc::new(RecordingSnapshotPublisher::default());
        let (handle, runtime) = GitIntelligenceRuntime::open_with_snapshot_publisher(
            root.clone(),
            &graph_path,
            &history_cache_path,
            repository_id.clone(),
            Arc::clone(&coordinator),
            snapshots.clone(),
        )
        .expect("open runtime");
        let head = git_output(&root, &["rev-parse", "HEAD"]);

        runtime
            .refresh(RefreshRequest {
                sequence: 1,
                expected_head: Some(head.clone()),
            })
            .await
            .expect("publish initial generation");
        let first = runtime
            .store
            .lock()
            .expect("store lock")
            .load_active(&repository_id)
            .expect("load active")
            .expect("active generation");
        assert_eq!(first.head_commit_id.as_deref(), Some(head.as_str()));
        assert_eq!(coordinator.snapshot().completed_jobs, 1);
        assert_eq!(
            snapshots
                .publications
                .lock()
                .expect("publication recorder lock")
                .as_slice(),
            [(repository_id.clone(), first.generation, true)]
        );

        handle.request(Some(head.clone()));
        runtime
            .refresh(RefreshRequest {
                sequence: 2,
                expected_head: Some(head),
            })
            .await
            .expect("skip stable head");
        assert_eq!(coordinator.snapshot().completed_jobs, 1);
        assert_eq!(
            snapshots
                .publications
                .lock()
                .expect("publication recorder lock")
                .as_slice(),
            [
                (repository_id.clone(), first.generation, true),
                (repository_id.clone(), first.generation, true),
            ]
        );
        assert_eq!(
            snapshots
                .stale_repositories
                .lock()
                .expect("stale recorder lock")
                .as_slice(),
            [repository_id.clone()]
        );

        std::fs::rename(root.join(".git"), root.join("git-disabled"))
            .expect("make repository unreadable");
        handle.request(Some("new-head".to_string()));
        let error = runtime
            .refresh(RefreshRequest {
                sequence: 3,
                expected_head: Some("new-head".to_string()),
            })
            .await
            .expect_err("mining must fail");
        assert!(error
            .to_string()
            .contains("read Git history cache identity"));
        let retained = runtime
            .store
            .lock()
            .expect("store lock")
            .load_active(&repository_id)
            .expect("load retained active")
            .expect("retained generation");
        assert_eq!(retained.generation, first.generation);
        assert_eq!(retained.snapshot, first.snapshot);
        assert_eq!(
            snapshots
                .publications
                .lock()
                .expect("publication recorder lock")
                .len(),
            2,
            "failed refresh must not republish the retained generation as fresh"
        );
        assert_eq!(
            snapshots
                .stale_repositories
                .lock()
                .expect("stale recorder lock")
                .as_slice(),
            [repository_id.clone(), repository_id]
        );

        let _ = std::fs::remove_dir_all(root);
    }

    fn init_repository(root: &Path) {
        run_git(root, &["init", "-q"]);
        std::fs::write(root.join("lib.rs"), "pub fn value() -> u8 { 1 }\n")
            .expect("write fixture source");
        run_git(root, &["add", "lib.rs"]);
        run_git(
            root,
            &[
                "-c",
                "user.name=Lattice Test",
                "-c",
                "user.email=lattice@example.invalid",
                "commit",
                "-q",
                "-m",
                "initial",
            ],
        );
    }

    fn run_git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .expect("execute git");
        assert!(status.success(), "git command failed: {args:?}");
    }

    fn git_output(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("execute git");
        assert!(output.status.success(), "git command failed: {args:?}");
        String::from_utf8(output.stdout)
            .expect("utf-8 git output")
            .trim()
            .to_string()
    }

    fn unique_test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lattice-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create test root");
        path
    }
}
