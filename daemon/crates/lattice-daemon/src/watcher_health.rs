use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WatcherHealthSnapshot {
    pub(crate) watch_degraded: bool,
    pub(crate) reason: Option<String>,
    pub(crate) polling_interval_secs: Option<u64>,
    pub(crate) last_poll_epoch_secs: Option<u64>,
    pub(crate) git_state: GitStateHealth,
    pub(crate) git_state_reason: Option<String>,
    pub(crate) git_state_retry_count: u64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GitStateHealth {
    Healthy,
    Unknown,
}

impl Default for GitStateHealth {
    fn default() -> Self {
        Self::Healthy
    }
}

#[derive(Debug, Default)]
pub(crate) struct WatcherHealth {
    degraded: AtomicBool,
    reason: Mutex<Option<String>>,
    polling_interval_secs: AtomicU64,
    last_poll_epoch_secs: AtomicU64,
    git_state_unknown: AtomicBool,
    git_state_reason: Mutex<Option<String>>,
    git_state_retry_count: AtomicU64,
}

impl WatcherHealth {
    pub(crate) fn mark_healthy(&self) {
        self.degraded.store(false, Ordering::Release);
        if let Ok(mut reason) = self.reason.lock() {
            *reason = None;
        }
        self.polling_interval_secs.store(0, Ordering::Release);
    }

    pub(crate) fn mark_degraded(&self, reason: impl Into<String>, polling_interval_secs: u64) {
        self.degraded.store(true, Ordering::Release);
        if let Ok(mut stored_reason) = self.reason.lock() {
            *stored_reason = Some(reason.into());
        }
        self.polling_interval_secs
            .store(polling_interval_secs, Ordering::Release);
    }

    pub(crate) fn mark_poll(&self) {
        self.last_poll_epoch_secs
            .store(now_epoch_secs(), Ordering::Release);
    }

    pub(crate) fn mark_git_state_healthy(&self) {
        self.git_state_unknown.store(false, Ordering::Release);
        if let Ok(mut reason) = self.git_state_reason.lock() {
            *reason = None;
        }
        self.git_state_retry_count.store(0, Ordering::Release);
    }

    pub(crate) fn mark_git_state_unknown(&self, reason: impl Into<String>, retry_count: u64) {
        self.git_state_unknown.store(true, Ordering::Release);
        if let Ok(mut stored_reason) = self.git_state_reason.lock() {
            *stored_reason = Some(reason.into());
        }
        self.git_state_retry_count
            .store(retry_count, Ordering::Release);
    }

    pub(crate) fn snapshot(&self) -> WatcherHealthSnapshot {
        let watch_degraded = self.degraded.load(Ordering::Acquire);
        let reason = self.reason.lock().ok().and_then(|reason| reason.clone());
        let polling_interval_secs = nonzero(self.polling_interval_secs.load(Ordering::Acquire));
        let last_poll_epoch_secs = nonzero(self.last_poll_epoch_secs.load(Ordering::Acquire));
        let git_state = if self.git_state_unknown.load(Ordering::Acquire) {
            GitStateHealth::Unknown
        } else {
            GitStateHealth::Healthy
        };
        let git_state_reason = self
            .git_state_reason
            .lock()
            .ok()
            .and_then(|reason| reason.clone());
        let git_state_retry_count = self.git_state_retry_count.load(Ordering::Acquire);
        WatcherHealthSnapshot {
            watch_degraded,
            reason,
            polling_interval_secs,
            last_poll_epoch_secs,
            git_state,
            git_state_reason,
            git_state_retry_count,
        }
    }
}

fn nonzero(value: u64) -> Option<u64> {
    if value == 0 {
        None
    } else {
        Some(value)
    }
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
