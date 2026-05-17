CREATE TABLE IF NOT EXISTS consolidation_jobs (
    job_id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    mode TEXT NOT NULL CHECK (
        mode IN ('synchronous_post_task', 'background', 'manual_review', 'replay')
    ),
    status TEXT NOT NULL CHECK (
        status IN ('queued', 'running', 'proposed', 'applied', 'rejected', 'failed', 'dropped')
    ),
    enqueued_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER,
    proposal_id TEXT,
    error_kind TEXT
);

CREATE INDEX IF NOT EXISTS idx_consolidation_jobs_workspace_status
    ON consolidation_jobs(workspace_id, status);

CREATE INDEX IF NOT EXISTS idx_consolidation_jobs_kind_status
    ON consolidation_jobs(kind, status);

CREATE TABLE IF NOT EXISTS consolidation_proposals (
    proposal_id TEXT PRIMARY KEY,
    job_id TEXT NOT NULL,
    target_memory_id TEXT,
    proposal_kind TEXT NOT NULL,
    prior_state TEXT NOT NULL CHECK (json_valid(prior_state)),
    proposed_state TEXT NOT NULL CHECK (json_valid(proposed_state)),
    evidence TEXT NOT NULL CHECK (json_valid(evidence)),
    provenance_json TEXT CHECK (provenance_json IS NULL OR json_valid(provenance_json)),
    decided_at INTEGER,
    decision TEXT NOT NULL CHECK (decision IN ('pending', 'applied', 'rejected', 'reverted')),
    decided_by TEXT,
    decision_reason TEXT,
    FOREIGN KEY (job_id) REFERENCES consolidation_jobs(job_id)
);
