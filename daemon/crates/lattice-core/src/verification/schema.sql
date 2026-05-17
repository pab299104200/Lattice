CREATE TABLE IF NOT EXISTS verification_jobs (
    job_id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    target_memory_id TEXT NOT NULL,
    check_kind TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'completed', 'failed')),
    verdict TEXT NULL CHECK (
        verdict IS NULL
        OR verdict IN (
            'verified',
            'unverified',
            'in_review',
            'stale',
            'contradicted',
            'superseded',
            'expired',
            'invalidated'
        )
    ),
    reason TEXT,
    queued_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER
);

CREATE INDEX IF NOT EXISTS idx_verification_jobs_workspace_status
    ON verification_jobs(workspace_id, status);
CREATE INDEX IF NOT EXISTS idx_verification_jobs_target_finished
    ON verification_jobs(target_memory_id, finished_at);
