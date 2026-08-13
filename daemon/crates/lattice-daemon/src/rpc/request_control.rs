//! Per-request retrieval control.
//!
//! This is deliberately daemon-owned: callers cannot select a larger deadline,
//! and ranking code receives only the cooperative stop signal it needs.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

/// The bounded server-side budget for ranked read workflows.
pub const RETRIEVAL_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct RequestControl {
    accepted_at: Instant,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl RequestControl {
    pub fn retrieval() -> Self {
        Self::with_budget(RETRIEVAL_DEADLINE)
    }

    #[cfg(test)]
    pub fn with_budget(budget: Duration) -> Self {
        let accepted_at = Instant::now();
        Self {
            accepted_at,
            deadline: accepted_at + budget,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(not(test))]
    fn with_budget(budget: Duration) -> Self {
        let accepted_at = Instant::now();
        Self {
            accepted_at,
            deadline: accepted_at + budget,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn deadline_reached(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn elapsed(&self) -> Duration {
        self.accepted_at.elapsed()
    }
}

tokio::task_local! {
    static ACTIVE_REQUEST_CONTROL: RequestControl;
}

pub async fn scope<T>(control: RequestControl, future: impl std::future::Future<Output = T>) -> T {
    ACTIVE_REQUEST_CONTROL.scope(control, future).await
}

pub fn current() -> Option<RequestControl> {
    ACTIVE_REQUEST_CONTROL.try_with(Clone::clone).ok()
}

#[cfg(test)]
mod tests {
    use super::{scope, RequestControl};
    use std::time::Duration;

    #[tokio::test]
    async fn deadline_and_cancellation_are_independent() {
        let control = RequestControl::with_budget(Duration::ZERO);
        assert!(control.deadline_reached());
        assert!(!control.is_cancelled());
        control.cancel();
        assert!(control.is_cancelled());
        let observed = scope(control, async {
            super::current().expect("request control")
        })
        .await;
        assert!(observed.deadline_reached());
    }
}
