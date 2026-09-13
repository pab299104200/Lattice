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

CREATE TABLE IF NOT EXISTS consolidation_event_outbox (
    outbox_id TEXT PRIMARY KEY,
    proposal_id TEXT NOT NULL,
    transition TEXT NOT NULL CHECK (transition IN ('applied', 'reverted')),
    workspace_id TEXT NOT NULL,
    event_uuid TEXT NOT NULL UNIQUE,
    event_ts_unix_micros INTEGER NOT NULL,
    envelope_json TEXT NOT NULL CHECK (json_valid(envelope_json)),
    created_at INTEGER NOT NULL,
    delivered_at INTEGER,
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    last_error TEXT,
    FOREIGN KEY (proposal_id) REFERENCES consolidation_proposals(proposal_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS consolidation_capture_receipts (
    delivery_key TEXT PRIMARY KEY REFERENCES session_digest_deliveries(delivery_key) ON DELETE CASCADE,
    repository_id TEXT NOT NULL,
    checkout_id TEXT NOT NULL,
    branch TEXT NOT NULL,
    proposal_ids TEXT NOT NULL CHECK (json_valid(proposal_ids)),
    completed_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS consolidation_proposal_memory_refs (
    proposal_id TEXT NOT NULL REFERENCES consolidation_proposals(proposal_id) ON DELETE CASCADE,
    memory_id TEXT NOT NULL,
    reference_kind TEXT NOT NULL CHECK (reference_kind IN ('target','prior_state','proposed_state','evidence')),
    PRIMARY KEY (proposal_id, memory_id, reference_kind)
);

CREATE INDEX IF NOT EXISTS idx_consolidation_proposal_memory_refs_memory
    ON consolidation_proposal_memory_refs(memory_id, proposal_id);

CREATE INDEX IF NOT EXISTS idx_consolidation_manual_review_admission
    ON consolidation_proposals(decision,job_id,proposal_id);

CREATE TABLE IF NOT EXISTS consolidation_proposal_admission (
    proposal_id TEXT PRIMARY KEY REFERENCES consolidation_proposals(proposal_id) ON DELETE CASCADE,
    workspace_id TEXT NOT NULL,
    authority_repository_id TEXT,
    manual_review INTEGER NOT NULL CHECK(manual_review IN (0,1))
);
CREATE INDEX IF NOT EXISTS idx_consolidation_proposal_admission_workspace
    ON consolidation_proposal_admission(workspace_id,manual_review,proposal_id);

CREATE TRIGGER IF NOT EXISTS consolidation_proposal_admission_insert
AFTER INSERT ON consolidation_proposals
WHEN json_valid(NEW.prior_state) AND json_valid(NEW.proposed_state) AND json_valid(NEW.evidence)
BEGIN
  INSERT OR REPLACE INTO consolidation_proposal_admission
  SELECT NEW.proposal_id,j.workspace_id,json_extract(NEW.evidence,'$.repository_id'),
    COALESCE(lower(COALESCE(json_extract(NEW.proposed_state,'$.memory.scope'),json_extract(NEW.proposed_state,'$.scope'),json_extract(NEW.prior_state,'$.memory.scope'),json_extract(NEW.prior_state,'$.scope'))) IN ('repo','organization'),0)
  FROM consolidation_jobs j WHERE j.job_id=NEW.job_id AND NEW.decision='pending';
END;
CREATE TRIGGER IF NOT EXISTS consolidation_proposal_admission_update
AFTER UPDATE OF job_id,prior_state,proposed_state,evidence,decision ON consolidation_proposals
WHEN json_valid(NEW.prior_state) AND json_valid(NEW.proposed_state) AND json_valid(NEW.evidence)
BEGIN
  DELETE FROM consolidation_proposal_admission WHERE proposal_id=NEW.proposal_id;
  INSERT OR REPLACE INTO consolidation_proposal_admission
  SELECT NEW.proposal_id,j.workspace_id,json_extract(NEW.evidence,'$.repository_id'),
    COALESCE(lower(COALESCE(json_extract(NEW.proposed_state,'$.memory.scope'),json_extract(NEW.proposed_state,'$.scope'),json_extract(NEW.prior_state,'$.memory.scope'),json_extract(NEW.prior_state,'$.scope'))) IN ('repo','organization'),0)
  FROM consolidation_jobs j WHERE j.job_id=NEW.job_id AND NEW.decision='pending';
END;

CREATE TABLE IF NOT EXISTS consolidation_proposal_ref_backfill (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    next_rowid INTEGER NOT NULL DEFAULT 0,
    complete INTEGER NOT NULL DEFAULT 0 CHECK (complete IN (0,1)),
    blocked_proposal_id TEXT,
    blocked_payload_bytes INTEGER,
    last_error TEXT
);

INSERT OR IGNORE INTO consolidation_proposal_ref_backfill(id,complete)
SELECT 1,CASE WHEN EXISTS(SELECT 1 FROM consolidation_proposals) THEN 0 ELSE 1 END;

UPDATE consolidation_proposal_ref_backfill
SET next_rowid=0,complete=0,blocked_proposal_id=NULL,blocked_payload_bytes=NULL,last_error=NULL
WHERE complete=1 AND EXISTS(
  SELECT 1 FROM consolidation_proposals p
  LEFT JOIN consolidation_proposal_admission a ON a.proposal_id=p.proposal_id
  WHERE (p.decision='pending' AND a.proposal_id IS NULL)
     OR (p.decision!='pending' AND a.proposal_id IS NOT NULL)
);

CREATE TRIGGER IF NOT EXISTS consolidation_proposal_payload_limit_insert
BEFORE INSERT ON consolidation_proposals
WHEN octet_length(NEW.prior_state)+octet_length(NEW.proposed_state)+octet_length(NEW.evidence) > 1048576
BEGIN SELECT RAISE(ABORT, 'consolidation proposal payload exceeds 1048576 bytes'); END;

CREATE TRIGGER IF NOT EXISTS consolidation_proposal_payload_limit_update
BEFORE UPDATE OF prior_state,proposed_state,evidence ON consolidation_proposals
WHEN octet_length(NEW.prior_state)+octet_length(NEW.proposed_state)+octet_length(NEW.evidence) > 1048576
BEGIN SELECT RAISE(ABORT, 'consolidation proposal payload exceeds 1048576 bytes'); END;

CREATE TRIGGER IF NOT EXISTS consolidation_proposal_reference_limit_insert
BEFORE INSERT ON consolidation_proposals
WHEN CASE WHEN octet_length(NEW.prior_state)+octet_length(NEW.proposed_state)+octet_length(NEW.evidence)>1048576 THEN 0 ELSE (CASE WHEN NEW.target_memory_id IS NULL THEN 0 ELSE 1 END + (SELECT COUNT(*) FROM json_tree(NEW.prior_state) WHERE type='text' AND (((fullkey='$.id' OR (key='id' AND path='$.memory')) OR key IN ('memory_id','source_memory_id','replacement_memory_id','supersedes_memory_id','superseded_by_memory_id','source','target') OR path LIKE '%.source_memory_ids' OR path LIKE '%.memory_ids' OR path LIKE '%.contradicts_memory_ids' OR path LIKE '%.contradicted_by_memory_ids' OR path LIKE '%.linked_memories')))
   + (SELECT COUNT(*) FROM json_tree(NEW.proposed_state) WHERE type='text' AND (((fullkey='$.id' OR (key='id' AND path='$.memory')) OR key IN ('memory_id','source_memory_id','replacement_memory_id','supersedes_memory_id','superseded_by_memory_id','source','target') OR path LIKE '%.source_memory_ids' OR path LIKE '%.memory_ids' OR path LIKE '%.contradicts_memory_ids' OR path LIKE '%.contradicted_by_memory_ids' OR path LIKE '%.linked_memories')))
   + (SELECT COUNT(*) FROM json_tree(NEW.evidence) WHERE type='text' AND (key IN ('memory_id','source_memory_id','replacement_memory_id','supersedes_memory_id','superseded_by_memory_id','source','target') OR path LIKE '%.source_memory_ids' OR path LIKE '%.memory_ids' OR path LIKE '%.contradicts_memory_ids' OR path LIKE '%.contradicted_by_memory_ids' OR path LIKE '%.linked_memories')) > 4096) END
BEGIN SELECT RAISE(ABORT, 'consolidation proposal exceeds 4096 memory references'); END;

CREATE TRIGGER IF NOT EXISTS consolidation_proposal_reference_limit_update
BEFORE UPDATE OF target_memory_id,prior_state,proposed_state,evidence ON consolidation_proposals
WHEN CASE WHEN octet_length(NEW.prior_state)+octet_length(NEW.proposed_state)+octet_length(NEW.evidence)>1048576 THEN 0 ELSE (CASE WHEN NEW.target_memory_id IS NULL THEN 0 ELSE 1 END + (SELECT COUNT(*) FROM json_tree(NEW.prior_state) WHERE type='text' AND (((fullkey='$.id' OR (key='id' AND path='$.memory')) OR key IN ('memory_id','source_memory_id','replacement_memory_id','supersedes_memory_id','superseded_by_memory_id','source','target') OR path LIKE '%.source_memory_ids' OR path LIKE '%.memory_ids' OR path LIKE '%.contradicts_memory_ids' OR path LIKE '%.contradicted_by_memory_ids' OR path LIKE '%.linked_memories')))
   + (SELECT COUNT(*) FROM json_tree(NEW.proposed_state) WHERE type='text' AND (((fullkey='$.id' OR (key='id' AND path='$.memory')) OR key IN ('memory_id','source_memory_id','replacement_memory_id','supersedes_memory_id','superseded_by_memory_id','source','target') OR path LIKE '%.source_memory_ids' OR path LIKE '%.memory_ids' OR path LIKE '%.contradicts_memory_ids' OR path LIKE '%.contradicted_by_memory_ids' OR path LIKE '%.linked_memories')))
   + (SELECT COUNT(*) FROM json_tree(NEW.evidence) WHERE type='text' AND (key IN ('memory_id','source_memory_id','replacement_memory_id','supersedes_memory_id','superseded_by_memory_id','source','target') OR path LIKE '%.source_memory_ids' OR path LIKE '%.memory_ids' OR path LIKE '%.contradicts_memory_ids' OR path LIKE '%.contradicted_by_memory_ids' OR path LIKE '%.linked_memories')) > 4096) END
BEGIN SELECT RAISE(ABORT, 'consolidation proposal exceeds 4096 memory references'); END;

CREATE TRIGGER IF NOT EXISTS consolidation_proposal_refs_insert
AFTER INSERT ON consolidation_proposals BEGIN
  INSERT OR IGNORE INTO consolidation_proposal_memory_refs SELECT NEW.proposal_id,NEW.target_memory_id,'target' WHERE NEW.target_memory_id IS NOT NULL;
  INSERT OR IGNORE INTO consolidation_proposal_memory_refs SELECT NEW.proposal_id,value,'prior_state' FROM json_tree(NEW.prior_state) WHERE type='text' AND (((fullkey='$.id' OR (key='id' AND path='$.memory')) OR key IN ('memory_id','source_memory_id','replacement_memory_id','supersedes_memory_id','superseded_by_memory_id','source','target') OR path LIKE '%.source_memory_ids' OR path LIKE '%.memory_ids' OR path LIKE '%.contradicts_memory_ids' OR path LIKE '%.contradicted_by_memory_ids' OR path LIKE '%.linked_memories'));
  INSERT OR IGNORE INTO consolidation_proposal_memory_refs SELECT NEW.proposal_id,value,'proposed_state' FROM json_tree(NEW.proposed_state) WHERE type='text' AND (((fullkey='$.id' OR (key='id' AND path='$.memory')) OR key IN ('memory_id','source_memory_id','replacement_memory_id','supersedes_memory_id','superseded_by_memory_id','source','target') OR path LIKE '%.source_memory_ids' OR path LIKE '%.memory_ids' OR path LIKE '%.contradicts_memory_ids' OR path LIKE '%.contradicted_by_memory_ids' OR path LIKE '%.linked_memories'));
  INSERT OR IGNORE INTO consolidation_proposal_memory_refs SELECT NEW.proposal_id,value,'evidence' FROM json_tree(NEW.evidence) WHERE type='text' AND (key IN ('memory_id','source_memory_id','replacement_memory_id','supersedes_memory_id','superseded_by_memory_id','source','target') OR path LIKE '%.source_memory_ids' OR path LIKE '%.memory_ids' OR path LIKE '%.contradicts_memory_ids' OR path LIKE '%.contradicted_by_memory_ids' OR path LIKE '%.linked_memories');
END;

CREATE TRIGGER IF NOT EXISTS consolidation_proposal_refs_update
AFTER UPDATE OF target_memory_id,prior_state,proposed_state,evidence ON consolidation_proposals BEGIN
  DELETE FROM consolidation_proposal_memory_refs WHERE proposal_id=NEW.proposal_id;
  INSERT OR IGNORE INTO consolidation_proposal_memory_refs SELECT NEW.proposal_id,NEW.target_memory_id,'target' WHERE NEW.target_memory_id IS NOT NULL;
  INSERT OR IGNORE INTO consolidation_proposal_memory_refs SELECT NEW.proposal_id,value,'prior_state' FROM json_tree(NEW.prior_state) WHERE type='text' AND (((fullkey='$.id' OR (key='id' AND path='$.memory')) OR key IN ('memory_id','source_memory_id','replacement_memory_id','supersedes_memory_id','superseded_by_memory_id','source','target') OR path LIKE '%.source_memory_ids' OR path LIKE '%.memory_ids' OR path LIKE '%.contradicts_memory_ids' OR path LIKE '%.contradicted_by_memory_ids' OR path LIKE '%.linked_memories'));
  INSERT OR IGNORE INTO consolidation_proposal_memory_refs SELECT NEW.proposal_id,value,'proposed_state' FROM json_tree(NEW.proposed_state) WHERE type='text' AND (((fullkey='$.id' OR (key='id' AND path='$.memory')) OR key IN ('memory_id','source_memory_id','replacement_memory_id','supersedes_memory_id','superseded_by_memory_id','source','target') OR path LIKE '%.source_memory_ids' OR path LIKE '%.memory_ids' OR path LIKE '%.contradicts_memory_ids' OR path LIKE '%.contradicted_by_memory_ids' OR path LIKE '%.linked_memories'));
  INSERT OR IGNORE INTO consolidation_proposal_memory_refs SELECT NEW.proposal_id,value,'evidence' FROM json_tree(NEW.evidence) WHERE type='text' AND (key IN ('memory_id','source_memory_id','replacement_memory_id','supersedes_memory_id','superseded_by_memory_id','source','target') OR path LIKE '%.source_memory_ids' OR path LIKE '%.memory_ids' OR path LIKE '%.contradicts_memory_ids' OR path LIKE '%.contradicted_by_memory_ids' OR path LIKE '%.linked_memories');
END;
