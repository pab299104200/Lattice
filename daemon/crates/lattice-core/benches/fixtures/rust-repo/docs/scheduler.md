# Scheduler Workflow

`Scheduler::schedule` records the job and calls `audit_schedule`.

`RetryPolicy::bounded` caps retry attempts before the job is stored.
