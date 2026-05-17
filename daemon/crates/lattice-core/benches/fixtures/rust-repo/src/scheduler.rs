use crate::audit::{audit_schedule, AuditSink};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u8,
    pub backoff_seconds: u64,
}

impl RetryPolicy {
    pub fn bounded(max_attempts: u8) -> Self {
        Self {
            max_attempts: max_attempts.clamp(1, 5),
            backoff_seconds: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledJob {
    pub name: String,
    pub queue: String,
    pub retry_policy: RetryPolicy,
}

pub struct Scheduler<S: AuditSink> {
    sink: S,
    jobs: Vec<ScheduledJob>,
}

impl<S: AuditSink> Scheduler<S> {
    pub fn new(sink: S) -> Self {
        Self {
            sink,
            jobs: Vec::new(),
        }
    }

    pub fn schedule(&mut self, name: &str, queue: &str, retry_policy: RetryPolicy) {
        audit_schedule(&mut self.sink, name);
        self.jobs.push(ScheduledJob {
            name: name.to_string(),
            queue: queue.to_string(),
            retry_policy,
        });
    }

    pub fn jobs(&self) -> &[ScheduledJob] {
        &self.jobs
    }
}

pub fn build_default_scheduler<S: AuditSink>(sink: S) -> Scheduler<S> {
    Scheduler::new(sink)
}
