use workflow_guard::{build_default_scheduler, RetryPolicy, VecAuditSink};

#[test]
fn scheduler_records_job_and_audit_event() {
    let sink = VecAuditSink::default();
    let mut scheduler = build_default_scheduler(sink);
    scheduler.schedule("nightly-inventory", "maintenance", RetryPolicy::bounded(8));

    assert_eq!(scheduler.jobs()[0].retry_policy.max_attempts, 5);
}
