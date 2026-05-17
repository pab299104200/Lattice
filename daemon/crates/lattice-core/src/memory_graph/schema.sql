-- Memory graph schema, T19.
--
-- Primary memory classes: Observation, Decision, Constraint, Pattern,
-- AntiPattern, WorkflowOutcome, FailurePattern, Procedure, Preference,
-- ArchitectureInvariant, DocsContract, OpenQuestion, CounterMemory.
--
-- Each memory record requires: stable id, content, memory class, assertion
-- type, scope: session, branch, repo, user, organization, verification status,
-- confidence and confidence reason, freshness policy, validity conditions,
-- invalidation triggers, provenance events, evidence references, linked files,
-- linked symbols, linked docs, linked tests, linked memories, contradiction
-- links, supersession links, access history, usefulness scores, last verified
-- state.

CREATE TABLE IF NOT EXISTS memories (
    memory_id TEXT PRIMARY KEY,
    content TEXT NOT NULL,
    class TEXT NOT NULL CHECK (
        class IN (
            'observation',
            'decision',
            'constraint',
            'pattern',
            'anti_pattern',
            'workflow_outcome',
            'failure_pattern',
            'procedure',
            'preference',
            'architecture_invariant',
            'docs_contract',
            'open_question',
            'counter_memory'
        )
    ),
    assertion_type TEXT NOT NULL CHECK (
        assertion_type IN (
            'observation',
            'decision',
            'constraint',
            'hypothesis',
            'procedure',
            'outcome',
            'preference',
            'question',
            'counter'
        )
    ),
    scope TEXT NOT NULL CHECK (
        scope IN ('session', 'branch', 'repo', 'user', 'organization')
    ),
    scope_session_id TEXT NULL,
    scope_branch TEXT NULL,
    scope_workspace_id TEXT NULL,
    scope_user_id TEXT NULL,
    scope_org_id TEXT NULL,
    verification_status TEXT NOT NULL CHECK (
        verification_status IN (
            'unverified',
            'in_review',
            'verified',
            'stale',
            'contradicted',
            'superseded',
            'expired',
            'invalidated'
        )
    ),
    confidence REAL NOT NULL CHECK(confidence BETWEEN 0 AND 1),
    confidence_reason TEXT NOT NULL,
    freshness_policy_json TEXT NOT NULL,
    validity_conditions_json TEXT NOT NULL,
    invalidation_triggers_json TEXT NOT NULL,
    provenance_event_ids_json TEXT NOT NULL,
    evidence_references_json TEXT NOT NULL,
    linked_files_json TEXT NOT NULL,
    linked_symbols_json TEXT NOT NULL,
    linked_docs_json TEXT NOT NULL,
    linked_tests_json TEXT NOT NULL,
    linked_memories_json TEXT NOT NULL,
    contradiction_links_json TEXT NOT NULL,
    supersession_links_json TEXT NOT NULL,
    access_history_json TEXT NOT NULL,
    last_verified_event_id INTEGER NULL,
    last_verified_state TEXT NULL,
    usefulness_score REAL NOT NULL DEFAULT 0,
    usefulness_score_updated_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    updated_by TEXT NOT NULL,
    superseded_by TEXT NULL,
    schema_version INTEGER NOT NULL,
    CHECK (
        (
            scope = 'session'
            AND scope_session_id IS NOT NULL
            AND scope_branch IS NULL
            AND scope_workspace_id IS NULL
            AND scope_user_id IS NULL
            AND scope_org_id IS NULL
        )
        OR (
            scope = 'branch'
            AND scope_session_id IS NULL
            AND scope_branch IS NOT NULL
            AND scope_workspace_id IS NOT NULL
            AND scope_user_id IS NULL
            AND scope_org_id IS NULL
        )
        OR (
            scope = 'repo'
            AND scope_session_id IS NULL
            AND scope_branch IS NULL
            AND scope_workspace_id IS NOT NULL
            AND scope_user_id IS NULL
            AND scope_org_id IS NULL
        )
        OR (
            scope = 'user'
            AND scope_session_id IS NULL
            AND scope_branch IS NULL
            AND scope_workspace_id IS NULL
            AND scope_user_id IS NOT NULL
            AND scope_org_id IS NULL
        )
        OR (
            scope = 'organization'
            AND scope_session_id IS NULL
            AND scope_branch IS NULL
            AND scope_workspace_id IS NULL
            AND scope_user_id IS NULL
            AND scope_org_id IS NOT NULL
        )
    )
);

CREATE TABLE IF NOT EXISTS memory_idempotency (
    idempotency_key TEXT PRIMARY KEY,
    memory_id TEXT NOT NULL REFERENCES memories(memory_id)
);

CREATE TABLE IF NOT EXISTS memory_tombstones (
    memory_id TEXT PRIMARY KEY REFERENCES memories(memory_id),
    reason TEXT NOT NULL,
    deleted_at INTEGER NOT NULL,
    deleted_by_event_id TEXT NULL
);

CREATE TABLE IF NOT EXISTS memory_links (
    link_id TEXT PRIMARY KEY,
    source_memory_id TEXT NOT NULL REFERENCES memories(memory_id),
    target_kind TEXT NOT NULL CHECK (
        target_kind IN ('memory', 'file', 'symbol', 'doc_section', 'test')
    ),
    target_id TEXT NOT NULL,
    link_type TEXT NOT NULL CHECK (
        link_type IN (
            'supports',
            'contradicts',
            'supersedes',
            'refines',
            'generalizes',
            'specializes',
            'co_occurs_with',
            'derived_from',
            'applies_to',
            'validated_by',
            'invalidated_by'
        )
    ),
    strength REAL NOT NULL CHECK(strength BETWEEN 0 AND 1),
    reason TEXT NOT NULL,
    evidence_event_id TEXT NULL,
    created_by_kind TEXT NOT NULL CHECK (
        created_by_kind IN ('assistant', 'user', 'tool', 'daemon')
    ),
    created_by_detail TEXT NULL,
    created_at INTEGER NOT NULL,
    verification_status TEXT NOT NULL CHECK (
        verification_status IN (
            'unverified',
            'in_review',
            'verified',
            'stale',
            'contradicted',
            'superseded',
            'expired',
            'invalidated'
        )
    )
);

CREATE TABLE IF NOT EXISTS memory_evidence (
    evidence_id TEXT PRIMARY KEY,
    memory_id TEXT NOT NULL REFERENCES memories(memory_id),
    event_id TEXT NULL,
    anchor_kind TEXT NOT NULL CHECK (
        anchor_kind IN (
            'file_span',
            'symbol_ref',
            'doc_section',
            'test_result',
            'event_reference'
        )
    ),
    anchor_json TEXT NOT NULL,
    captured_at INTEGER NOT NULL,
    captured_by_kind TEXT NOT NULL CHECK (
        captured_by_kind IN ('assistant', 'user', 'tool', 'daemon')
    ),
    captured_by_detail TEXT NULL
);

CREATE TABLE IF NOT EXISTS memory_accesses (
    access_id TEXT PRIMARY KEY,
    memory_id TEXT NOT NULL REFERENCES memories(memory_id),
    accessed_at INTEGER NOT NULL,
    accessed_in_event TEXT NOT NULL,
    accessor_kind TEXT NOT NULL CHECK (
        accessor_kind IN ('assistant', 'user', 'tool', 'daemon')
    ),
    accessor_detail TEXT NULL,
    inclusion_reason TEXT NOT NULL,
    was_used INTEGER NULL CHECK (was_used IN (0, 1)),
    downstream_outcome_event TEXT NULL
);

CREATE TABLE IF NOT EXISTS memory_scores (
    memory_id TEXT NOT NULL REFERENCES memories(memory_id),
    score_kind TEXT NOT NULL CHECK (
        score_kind IN (
            'usefulness_prior',
            'recent_usefulness',
            'retrieval_accuracy',
            'regression_risk'
        )
    ),
    value REAL NOT NULL,
    computed_at INTEGER NOT NULL,
    computed_from_window_secs INTEGER NOT NULL CHECK (computed_from_window_secs >= 0),
    sample_size INTEGER NOT NULL CHECK (sample_size >= 0),
    PRIMARY KEY (memory_id, score_kind, computed_at)
);

CREATE INDEX IF NOT EXISTS idx_memory_links_source
    ON memory_links(source_memory_id);
CREATE INDEX IF NOT EXISTS idx_memory_links_target
    ON memory_links(target_kind, target_id);
CREATE INDEX IF NOT EXISTS idx_memory_links_type
    ON memory_links(link_type);

CREATE INDEX IF NOT EXISTS idx_memory_evidence_memory
    ON memory_evidence(memory_id);
CREATE INDEX IF NOT EXISTS idx_memory_evidence_event
    ON memory_evidence(event_id);

CREATE INDEX IF NOT EXISTS idx_memory_accesses_memory_time
    ON memory_accesses(memory_id, accessed_at DESC);
CREATE INDEX IF NOT EXISTS idx_memory_accesses_event
    ON memory_accesses(accessed_in_event);

CREATE INDEX IF NOT EXISTS idx_memory_scores_memory_kind
    ON memory_scores(memory_id, score_kind, computed_at DESC);

CREATE INDEX IF NOT EXISTS idx_memory_idempotency_memory
    ON memory_idempotency(memory_id);
