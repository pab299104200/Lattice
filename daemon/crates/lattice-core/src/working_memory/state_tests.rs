use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, Connection};

use super::state::{
    canonical_state_json, initialize_schema, load_checkpoint, save_checkpoint,
    save_checkpoint_for_scope, state_hash, BudgetDecisions, CheckpointScope, ExcludedMemory,
    FailureRecord, Hypothesis, PlanRef, WorkingMemoryState, WorkingMemoryVerification,
    WORKING_MEMORY_STATE_VERSION,
};
use crate::identity::{FileId, Identity, IdentityKind, MemoryId, SymbolId};
use crate::memory::MemoryVerificationStatus;
use crate::retrieval_v1::{
    schema::BundleResult, IntentClassification, IntentFeature, IntentFeatureKind, IntentLabel,
};

#[test]
fn default_constructed_state_has_required_fields() {
    let state = WorkingMemoryState::default();

    assert_eq!(state.task_statement, "");
    assert_eq!(state.interpreted_intent.primary_label, IntentLabel::Unknown);
    assert!(state.active_files.is_empty());
    assert!(state.active_symbols.is_empty());
    assert!(state.active_hypotheses.is_empty());
    assert!(state.active_failures.is_empty());
    assert!(state.current_plan.is_none());
    assert!(state.selected_memories.is_empty());
    assert!(state.excluded_memories.is_empty());
    assert_eq!(state.budget_decisions, BudgetDecisions::default());
    assert!(state.unresolved_questions.is_empty());
    assert_eq!(
        state.verification_status,
        WorkingMemoryVerification::default()
    );
}

#[test]
fn serde_round_trips_state_without_data_loss() {
    let state = sample_state();
    let encoded = serde_json::to_string(&state).expect("state serializes");
    let decoded: WorkingMemoryState = serde_json::from_str(&encoded).expect("state deserializes");

    assert_eq!(decoded, state);
}

#[test]
fn save_then_load_checkpoint_returns_byte_identical_state() {
    let conn = initialized_connection();
    let state = sample_state();
    let scope = CheckpointScope::new("workspace-a", "session-a", "task-a");

    let id = save_checkpoint_for_scope(&state, "before-compress", &scope, &conn).expect("save");
    let loaded = load_checkpoint(id, &conn).expect("load");

    assert_eq!(
        canonical_state_json(&loaded).unwrap(),
        canonical_state_json(&state).unwrap()
    );
    assert_eq!(loaded, state);
}

#[test]
fn state_hash_matches_between_save_and_load() {
    let conn = initialized_connection();
    let state = sample_state();
    let id = save_checkpoint(&state, "hash-check", &conn).expect("save");
    let stored_hash: String = conn
        .query_row(
            "SELECT state_hash FROM working_memory_checkpoints WHERE checkpoint_id = ?1",
            [id],
            |row| row.get(0),
        )
        .expect("stored hash");

    let loaded = load_checkpoint(id, &conn).expect("load");

    assert_eq!(stored_hash, state_hash(&state).expect("state hash"));
    assert_eq!(stored_hash, state_hash(&loaded).expect("loaded state hash"));
}

#[test]
fn loading_checkpoint_with_unknown_state_version_is_rejected() {
    let conn = initialized_connection();
    let state = sample_state();
    let state_json = canonical_state_json(&state).expect("json");
    let state_hash = state_hash(&state).expect("hash");
    conn.execute(
        "INSERT INTO working_memory_checkpoints
            (workspace_id, session_id, task_id, checkpoint_name, created_at,
             state_version, state_json, state_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            "workspace-a",
            "session-a",
            "task-a",
            "future",
            1_u64,
            WORKING_MEMORY_STATE_VERSION + 1,
            state_json,
            state_hash,
        ],
    )
    .expect("insert future checkpoint");

    let error = load_checkpoint(conn.last_insert_rowid(), &conn).expect_err("version rejected");

    assert!(error
        .to_string()
        .contains("Unknown working memory state_version"));
}

#[test]
fn excluded_memories_require_exclusion_reason() {
    let excluded = ExcludedMemory {
        result: sample_bundle_result(),
        exclusion_reason: "stale after current retrieval".to_string(),
    };

    assert_eq!(excluded.exclusion_reason, "stale after current retrieval");
}

fn initialized_connection() -> Connection {
    let conn = Connection::open_in_memory().expect("in-memory db opens");
    initialize_schema(&conn).expect("schema initializes");
    conn
}

fn sample_state() -> WorkingMemoryState {
    let file = sample_file();
    let symbol = SymbolId {
        file: file.clone(),
        qualified_name: "working_memory::state::WorkingMemoryState".to_string(),
        byte_offset: 128,
        kind: "struct".to_string(),
    };
    let mut active_files = BTreeSet::new();
    active_files.insert(file.clone());
    let mut active_symbols = BTreeSet::new();
    active_symbols.insert(symbol.clone());
    let mut pinned_identities = BTreeSet::new();
    pinned_identities.insert(Identity::File(file));
    pinned_identities.insert(Identity::Symbol(symbol));

    WorkingMemoryState {
        task_statement: "Add working memory checkpoints".to_string(),
        interpreted_intent: sample_intent(),
        active_files,
        active_symbols,
        active_hypotheses: vec![Hypothesis {
            text: "Memory checkpoints need deterministic hashes".to_string(),
            evidence_refs: vec!["docs/plans/spec.md#5-working-memory".to_string()],
            confidence: 0.92,
        }],
        active_failures: vec![FailureRecord {
            kind: "test".to_string(),
            message: "checkpoint restore failed before schema existed".to_string(),
            observed_at: 42,
            evidence_refs: vec!["cargo test".to_string()],
        }],
        current_plan: Some(PlanRef {
            plan_id: "T34".to_string(),
            version: 1,
        }),
        selected_memories: vec![sample_bundle_result()],
        excluded_memories: vec![ExcludedMemory {
            result: sample_bundle_result(),
            exclusion_reason: "outside the task scope".to_string(),
        }],
        budget_decisions: BudgetDecisions {
            token_cap: 4096,
            dropped_count: 2,
            truncated_results: 1,
            pinned_identities,
        },
        unresolved_questions: vec!["Should restore expose checkpoint names?".to_string()],
        verification_status: WorkingMemoryVerification {
            last_verified_at: Some(123),
            status: MemoryVerificationStatus::InReview,
            notes: vec!["Round-trip verified".to_string()],
        },
    }
}

fn sample_intent() -> IntentClassification {
    let feature = IntentFeature {
        kind: IntentFeatureKind::ImperativeVerb,
        value: "add".to_string(),
    };
    let mut feature_scores = BTreeMap::new();
    feature_scores.insert(IntentLabel::AddFeature, 3.0);
    let mut contributing_features = BTreeMap::new();
    contributing_features.insert(IntentLabel::AddFeature, vec![feature.clone()]);
    IntentClassification {
        primary_label: IntentLabel::AddFeature,
        secondary_labels: vec![IntentLabel::Migration],
        feature_scores,
        contributing_features,
        fired_features: vec![feature],
        inspected_text: "Add working memory checkpoints".to_string(),
        input_was_truncated: false,
    }
}

fn sample_bundle_result() -> BundleResult {
    BundleResult {
        identity: Identity::Memory(MemoryId {
            workspace_id: "workspace-a".to_string(),
            ulid: "01HXWORKINGMEMORY0000000000".to_string(),
        }),
        kind: IdentityKind::Memory,
        headline: "Checkpoint schema decision".to_string(),
        snippet: "Use the memory DB for checkpoint rows.".to_string(),
        inclusion_reason: "selected memory".to_string(),
        expansion_handle: "memory:checkpoint-schema".to_string(),
        source: Vec::new(),
        score: 0.87,
    }
}

fn sample_file() -> FileId {
    FileId {
        workspace_id: "workspace-a".to_string(),
        repo_relative_path: "daemon/crates/lattice-core/src/working_memory/state.rs".to_string(),
        content_hash: "abc123".to_string(),
    }
}
