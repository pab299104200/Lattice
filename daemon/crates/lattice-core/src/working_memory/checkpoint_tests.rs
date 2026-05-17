//! Working-memory checkpoint restore regressions.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 5: Working Memory` and `## 5. Working Memory` require
//! checkpoint save/load tests, including coherent restore and version checks.

use rusqlite::params;

use super::state::{
    canonical_state_json, load_checkpoint, save_checkpoint, state_hash, BudgetDecisions,
    WORKING_MEMORY_STATE_VERSION,
};
use super::tests_common::{
    sample_bundle_result, sample_identity, sample_state, Harness, WORKSPACE,
};
use crate::error::LatticeError;

#[test]
fn checkpoint_round_trip_is_byte_identical() {
    let harness = Harness::new(Default::default());
    let state = sample_state();

    let checkpoint_id = save_checkpoint(&state, "before-compress", &harness.conn).expect("save");
    let loaded = load_checkpoint(checkpoint_id, &harness.conn).expect("load");

    assert_eq!(
        canonical_state_json(&loaded).unwrap(),
        canonical_state_json(&state).unwrap()
    );
    assert_eq!(state_hash(&loaded).unwrap(), state_hash(&state).unwrap());
}

#[test]
fn checkpoints_with_same_name_get_distinct_ids_and_round_trip() {
    let harness = Harness::new(Default::default());
    let state = sample_state();

    let first = save_checkpoint(&state, "same-name", &harness.conn).expect("first");
    let second = save_checkpoint(&state, "same-name", &harness.conn).expect("second");

    assert!(second > first);
    assert_eq!(load_checkpoint(first, &harness.conn).unwrap(), state);
    assert_eq!(load_checkpoint(second, &harness.conn).unwrap(), state);
}

#[test]
fn loading_unknown_checkpoint_id_returns_storage_error() {
    let harness = Harness::new(Default::default());

    let error = load_checkpoint(999_999, &harness.conn).expect_err("unknown checkpoint");

    assert!(matches!(error, LatticeError::Storage(_)));
    assert!(error.to_string().contains("was not found"));
}

#[test]
fn loading_newer_state_version_is_rejected_clearly() {
    let harness = Harness::new(Default::default());
    let state = sample_state();
    let state_json = canonical_state_json(&state).expect("json");
    let state_hash = state_hash(&state).expect("hash");
    harness
        .conn
        .execute(
            "INSERT INTO working_memory_checkpoints
            (workspace_id, session_id, task_id, checkpoint_name, created_at,
             state_version, state_json, state_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                WORKSPACE,
                "session-a",
                "task-a",
                "future",
                1_u64,
                WORKING_MEMORY_STATE_VERSION + 1,
                state_json,
                state_hash
            ],
        )
        .expect("insert");

    let error = load_checkpoint(harness.conn.last_insert_rowid(), &harness.conn)
        .expect_err("future version");

    assert!(matches!(error, LatticeError::Storage(_)));
    assert!(error
        .to_string()
        .contains("Unknown working memory state_version"));
}

#[test]
fn loading_checkpoint_restores_selected_excluded_and_budget_together() {
    let harness = Harness::new(Default::default());
    let mut original = sample_state();
    original.selected_memories = vec![sample_bundle_result("memory-a", 0.9)];
    original.excluded_memories = vec![super::state::ExcludedMemory {
        result: sample_bundle_result("memory-b", 0.4),
        exclusion_reason: "compressed: budget=80 tokens".to_string(),
    }];
    original.budget_decisions = BudgetDecisions {
        token_cap: 80,
        dropped_count: 1,
        truncated_results: 1,
        pinned_identities: [sample_identity("memory-a")].into_iter().collect(),
    };
    let checkpoint_id = save_checkpoint(&original, "coherent", &harness.conn).expect("save");
    let mut mutated = original.clone();
    mutated.selected_memories.clear();
    mutated.excluded_memories.clear();
    mutated.budget_decisions = BudgetDecisions::default();

    let restored = load_checkpoint(checkpoint_id, &harness.conn).expect("restore");

    assert_ne!(restored.selected_memories, mutated.selected_memories);
    assert_ne!(restored.excluded_memories, mutated.excluded_memories);
    assert_ne!(restored.budget_decisions, mutated.budget_decisions);
    assert_eq!(restored, original);
}
