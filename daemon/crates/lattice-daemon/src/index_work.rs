use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

/// Tracks synchronous workers whose `JoinHandle` can outlive an aborted async
/// task. Runtime shutdown waits for these guards before releasing storage
/// authority to lifecycle GC.
#[derive(Debug, Default)]
pub(crate) struct RuntimeWorkTracker {
    active: AtomicUsize,
    idle: Notify,
}

impl RuntimeWorkTracker {
    pub(crate) fn begin(self: &Arc<Self>) -> RuntimeWorkGuard {
        self.active.fetch_add(1, Ordering::AcqRel);
        RuntimeWorkGuard(Arc::clone(self))
    }

    pub(crate) fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) async fn wait_idle(&self) {
        loop {
            let idle = self.idle.notified();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            idle.await;
        }
    }
}

pub(crate) struct RuntimeWorkGuard(Arc<RuntimeWorkTracker>);

impl Drop for RuntimeWorkGuard {
    fn drop(&mut self) {
        if self.0.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

pub(crate) const INDEX_CONCURRENCY_ENV: &str = "LATTICE_MAX_CONCURRENT_INDEX_JOBS";
pub(crate) const INDEX_JOB_RESERVATION_ENV: &str = "LATTICE_INDEX_JOB_RESERVATION_BYTES";
const DEFAULT_INDEX_CONCURRENCY: usize = 1;
const DEFAULT_INDEX_JOB_RESERVATION_BYTES: u64 = 256 * 1024 * 1024;

/// Process-wide admission control for memory- and CPU-heavy graph rebuilds.
///
/// Every shard shares one coordinator. This prevents independent workspace
/// refreshes from simultaneously materializing full replacement graphs.
#[derive(Debug)]
pub(crate) struct IndexWorkCoordinator {
    permits: Arc<Semaphore>,
    capacity: usize,
    active_jobs: AtomicUsize,
    queued_jobs: AtomicUsize,
    completed_jobs: AtomicU64,
    active_by_workspace: StdMutex<HashMap<String, usize>>,
    queued_by_workspace: StdMutex<HashMap<String, usize>>,
    resource_budget: Arc<crate::resource_budget::ResourceBudget>,
    job_reservation_bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct IndexWorkSnapshot {
    pub(crate) state: &'static str,
    pub(crate) capacity: usize,
    pub(crate) active_jobs: usize,
    pub(crate) queued_jobs: usize,
    pub(crate) completed_jobs: u64,
    pub(crate) env_var: &'static str,
}

impl IndexWorkCoordinator {
    pub(crate) fn from_env() -> Arc<Self> {
        Self::from_env_with_budget(crate::resource_budget::ResourceBudget::from_env())
    }

    pub(crate) fn from_env_with_budget(
        resource_budget: Arc<crate::resource_budget::ResourceBudget>,
    ) -> Arc<Self> {
        let capacity = std::env::var(INDEX_CONCURRENCY_ENV)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_INDEX_CONCURRENCY);
        let bytes = std::env::var(INDEX_JOB_RESERVATION_ENV)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_INDEX_JOB_RESERVATION_BYTES);
        Self::with_resource_budget(capacity, resource_budget, bytes)
    }

    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        Self::with_resource_budget(
            capacity,
            Arc::new(crate::resource_budget::ResourceBudget::new(u64::MAX)),
            1,
        )
    }

    pub(crate) fn with_resource_budget(
        capacity: usize,
        resource_budget: Arc<crate::resource_budget::ResourceBudget>,
        job_reservation_bytes: u64,
    ) -> Arc<Self> {
        let capacity = capacity.max(1);
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(capacity)),
            capacity,
            active_jobs: AtomicUsize::new(0),
            queued_jobs: AtomicUsize::new(0),
            completed_jobs: AtomicU64::new(0),
            active_by_workspace: StdMutex::new(HashMap::new()),
            queued_by_workspace: StdMutex::new(HashMap::new()),
            resource_budget,
            job_reservation_bytes: job_reservation_bytes.max(1),
        })
    }

    pub(crate) async fn acquire(
        self: &Arc<Self>,
        workspace: impl Into<String>,
        kind: &'static str,
    ) -> anyhow::Result<IndexWorkPermit> {
        let workspace = workspace.into();
        let waiting = WaitingJob::new(Arc::clone(self), workspace.clone());
        let queued_at = Instant::now();
        let permit = Arc::clone(&self.permits).acquire_owned().await?;
        let resource_reservation = self
            .resource_budget
            .try_reserve("index_staging_generation", self.job_reservation_bytes)?;
        waiting.promote();
        self.active_jobs.fetch_add(1, Ordering::AcqRel);
        adjust_workspace_count(&self.active_by_workspace, &workspace, 1);
        tracing::info!(
            workspace,
            kind,
            queued_ms = queued_at.elapsed().as_millis() as u64,
            active_jobs = self.active_jobs.load(Ordering::Acquire),
            capacity = self.capacity,
            "Index work started"
        );
        Ok(IndexWorkPermit {
            coordinator: Arc::clone(self),
            _permit: permit,
            workspace,
            kind,
            started_at: Instant::now(),
            _resource_reservation: resource_reservation,
        })
    }

    pub(crate) fn snapshot(&self) -> IndexWorkSnapshot {
        let active_jobs = self.active_jobs.load(Ordering::Acquire);
        let queued_jobs = self.queued_jobs.load(Ordering::Acquire);
        let state = if active_jobs > 0 {
            "active"
        } else if queued_jobs > 0 {
            "queued"
        } else {
            "idle"
        };
        IndexWorkSnapshot {
            state,
            capacity: self.capacity,
            active_jobs,
            queued_jobs,
            completed_jobs: self.completed_jobs.load(Ordering::Acquire),
            env_var: INDEX_CONCURRENCY_ENV,
        }
    }

    pub(crate) fn workspace_is_busy(&self, workspace: &str) -> bool {
        workspace_count(&self.active_by_workspace, workspace) > 0
            || workspace_count(&self.queued_by_workspace, workspace) > 0
    }

    pub(crate) fn resource_snapshot(&self) -> crate::resource_budget::ResourceBudgetSnapshot {
        self.resource_budget.snapshot()
    }

    pub(crate) fn resource_budget(&self) -> Arc<crate::resource_budget::ResourceBudget> {
        Arc::clone(&self.resource_budget)
    }
}

struct WaitingJob {
    coordinator: Arc<IndexWorkCoordinator>,
    workspace: String,
    is_waiting: AtomicBool,
}

impl WaitingJob {
    fn new(coordinator: Arc<IndexWorkCoordinator>, workspace: String) -> Self {
        coordinator.queued_jobs.fetch_add(1, Ordering::AcqRel);
        adjust_workspace_count(&coordinator.queued_by_workspace, &workspace, 1);
        Self {
            coordinator,
            workspace,
            is_waiting: AtomicBool::new(true),
        }
    }

    fn promote(&self) {
        if self.is_waiting.swap(false, Ordering::AcqRel) {
            self.coordinator.queued_jobs.fetch_sub(1, Ordering::AcqRel);
            adjust_workspace_count(&self.coordinator.queued_by_workspace, &self.workspace, -1);
        }
    }
}

impl Drop for WaitingJob {
    fn drop(&mut self) {
        self.promote();
    }
}

pub(crate) struct IndexWorkPermit {
    coordinator: Arc<IndexWorkCoordinator>,
    _permit: OwnedSemaphorePermit,
    workspace: String,
    kind: &'static str,
    started_at: Instant,
    _resource_reservation: crate::resource_budget::ResourceReservation,
}

impl Drop for IndexWorkPermit {
    fn drop(&mut self) {
        self.coordinator.active_jobs.fetch_sub(1, Ordering::AcqRel);
        adjust_workspace_count(&self.coordinator.active_by_workspace, &self.workspace, -1);
        let completed_jobs = self
            .coordinator
            .completed_jobs
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        tracing::info!(
            workspace = self.workspace,
            kind = self.kind,
            elapsed_ms = self.started_at.elapsed().as_millis() as u64,
            completed_jobs,
            "Index work finished"
        );
    }
}

fn adjust_workspace_count(
    counts: &StdMutex<HashMap<String, usize>>,
    workspace: &str,
    delta: isize,
) {
    let Ok(mut counts) = counts.lock() else {
        return;
    };
    let count = counts.entry(workspace.to_string()).or_default();
    if delta >= 0 {
        *count = count.saturating_add(delta as usize);
    } else {
        *count = count.saturating_sub(delta.unsigned_abs());
    }
    if *count == 0 {
        counts.remove(workspace);
    }
}

fn workspace_count(counts: &StdMutex<HashMap<String, usize>>, workspace: &str) -> usize {
    counts
        .lock()
        .ok()
        .and_then(|counts| counts.get(workspace).copied())
        .unwrap_or_default()
}

/// Per-shard startup barrier. Watchers start immediately so they cannot miss
/// file-system events, but they publish only after the initial graph snapshot.
#[derive(Debug, Default)]
pub(crate) struct IndexReadiness {
    ready: AtomicBool,
    notify: Notify,
}

impl IndexReadiness {
    pub(crate) async fn wait(&self) {
        while !self.ready.load(Ordering::Acquire) {
            let notified = self.notify.notified();
            if self.ready.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn mark_ready(&self) {
        self.ready.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn coordinator_serializes_jobs_and_reports_queue_state() {
        let coordinator = IndexWorkCoordinator::new(1);
        let first = coordinator
            .acquire("/workspace/a", "startup")
            .await
            .expect("first permit");
        let entered = Arc::new(AtomicUsize::new(0));
        let task = {
            let coordinator = Arc::clone(&coordinator);
            let entered = Arc::clone(&entered);
            tokio::spawn(async move {
                let _second = coordinator
                    .acquire("/workspace/b", "watcher")
                    .await
                    .expect("second permit");
                entered.fetch_add(1, Ordering::AcqRel);
            })
        };

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(entered.load(Ordering::Acquire), 0);
        assert_eq!(
            coordinator.snapshot(),
            IndexWorkSnapshot {
                state: "active",
                capacity: 1,
                active_jobs: 1,
                queued_jobs: 1,
                completed_jobs: 0,
                env_var: INDEX_CONCURRENCY_ENV,
            }
        );

        drop(first);
        task.await.expect("queued task");
        assert_eq!(entered.load(Ordering::Acquire), 1);
        let snapshot = coordinator.snapshot();
        assert_eq!(snapshot.state, "idle");
        assert_eq!(snapshot.active_jobs, 0);
        assert_eq!(snapshot.queued_jobs, 0);
        assert_eq!(snapshot.completed_jobs, 2);
    }

    #[tokio::test]
    async fn readiness_waits_until_initial_snapshot_is_published() {
        let readiness = Arc::new(IndexReadiness::default());
        let observed = Arc::new(AtomicBool::new(false));
        let task = {
            let readiness = Arc::clone(&readiness);
            let observed = Arc::clone(&observed);
            tokio::spawn(async move {
                readiness.wait().await;
                observed.store(true, Ordering::Release);
            })
        };

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!observed.load(Ordering::Acquire));
        readiness.mark_ready();
        task.await.expect("readiness waiter");
        assert!(observed.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn staging_generation_is_rejected_before_overcommit_and_releases_on_cancel() {
        let budget = Arc::new(crate::resource_budget::ResourceBudget::new(10));
        let coordinator = IndexWorkCoordinator::with_resource_budget(2, Arc::clone(&budget), 6);
        let first = coordinator
            .acquire("/workspace/a", "startup")
            .await
            .unwrap();
        let error = match coordinator.acquire("/workspace/b", "startup").await {
            Ok(_) => panic!("second staging generation exceeded the byte budget"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("resource-limited"));
        assert_eq!(budget.snapshot().reserved_bytes, 6);
        drop(first);
        assert_eq!(budget.snapshot().reserved_bytes, 0);
    }
}
