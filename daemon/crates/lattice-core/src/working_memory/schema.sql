-- Working memory checkpoints live in the existing memories DB
-- (`.lattice/memories.db`) per
-- `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
-- `## Storage Design` and `## Phase 5: Working Memory`, so checkpoint rows
-- share the same workspace/session/task identity scope as memory state.
CREATE TABLE IF NOT EXISTS working_memory_checkpoints (
    checkpoint_id INTEGER PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    checkpoint_name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    state_version INTEGER NOT NULL,
    state_json TEXT NOT NULL,
    state_hash TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_working_memory_checkpoints_scope_created
    ON working_memory_checkpoints(workspace_id, session_id, task_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_working_memory_checkpoints_state_hash
    ON working_memory_checkpoints(state_hash);
