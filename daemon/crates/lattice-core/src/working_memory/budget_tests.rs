//! Working-memory budget, pin, and eviction regressions.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 5: Working Memory` and `## 5. Working Memory` require explicit
//! tests for token budgets, pinned context, eviction, and excluded-context
//! tracking.

use std::cell::RefCell;

use super::event_hooks::{
    emit_memory_retrieved, MutationEventSummary, WorkingMemoryEventAppender,
    WorkingMemoryEventContext,
};
use super::operations::{
    compress, evict, pin, retrieve, CompressArgs, EvictArgs, PinArgs, RetrievalExecution,
};
use super::tests_common::{
    file_identity, huge_bundle_result, long_bundle_result, retrieval_args, sample_bundle,
    sample_bundle_result, sample_identity, sample_state, FakeRetriever, Harness,
};
use crate::events::{
    Actor, BranchRef, EventWriteError, FlushPolicy, PartialEnvelope, SessionId, TaskId,
};

#[test]
fn compress_honors_token_budget_and_updates_budget_decisions() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();
    state.selected_memories = vec![
        huge_bundle_result("memory-a", 0.9),
        huge_bundle_result("memory-b", 0.8),
        huge_bundle_result("memory-c", 0.7),
    ];

    let outcome = compress(
        &mut state,
        CompressArgs { token_budget: 120 },
        &harness.ctx(),
    )
    .expect("compress");

    assert!(outcome.budget_report.estimated_tokens <= 120);
    assert_eq!(state.budget_decisions.token_cap, 120);
    assert!(outcome.budget_report.truncated);
}

#[test]
fn retrieve_over_budget_bundle_keeps_truncated_report() {
    let execution = RetrievalExecution {
        bundle: sample_bundle(vec![sample_bundle_result("memory-a", 0.9)], 96, true, 1, 0),
        excluded_memories: Vec::new(),
    };
    let harness = Harness::new(FakeRetriever::with_retrieve_execution(execution));
    let mut state = sample_state();

    let outcome = retrieve(&mut state, retrieval_args(), &harness.ctx()).expect("retrieve");

    assert!(outcome.bundle.budget_report.truncated);
    assert_eq!(state.budget_decisions.token_cap, 96);
    assert_eq!(state.budget_decisions.truncated_results, 1);
}

#[test]
fn retrieve_moves_truncated_results_to_excluded_with_budget_reason() {
    let execution = RetrievalExecution {
        bundle: sample_bundle(vec![sample_bundle_result("memory-a", 0.9)], 64, true, 0, 1),
        excluded_memories: vec![super::state::ExcludedMemory {
            result: sample_bundle_result("memory-b", 0.5),
            exclusion_reason: "compressed: budget=64 tokens".to_string(),
        }],
    };
    let harness = Harness::new(FakeRetriever::with_retrieve_execution(execution));
    let mut state = sample_state();

    let outcome = retrieve(&mut state, retrieval_args(), &harness.ctx()).expect("retrieve");

    assert_eq!(outcome.excluded_count, 1);
    assert_eq!(
        state.excluded_memories[0].exclusion_reason,
        "compressed: budget=64 tokens"
    );
}

#[test]
fn budget_changes_only_affect_subsequent_operations() {
    let mut state = sample_state();
    retrieve(
        &mut state,
        retrieval_args(),
        &Harness::new(FakeRetriever::default()).ctx(),
    )
    .expect("first retrieve");
    let second = RetrievalExecution {
        bundle: sample_bundle(vec![sample_bundle_result("memory-d", 0.9)], 80, true, 0, 1),
        excluded_memories: vec![super::state::ExcludedMemory {
            result: sample_bundle_result("memory-e", 0.4),
            exclusion_reason: "compressed: budget=80 tokens".to_string(),
        }],
    };
    retrieve(
        &mut state,
        retrieval_args(),
        &Harness::new(FakeRetriever::with_retrieve_execution(second)).ctx(),
    )
    .expect("second retrieve");

    let reasons = state
        .excluded_memories
        .iter()
        .map(|item| item.exclusion_reason.as_str())
        .collect::<Vec<_>>();
    assert!(reasons.contains(&"compressed: budget=240 tokens"));
    assert!(reasons.contains(&"compressed: budget=80 tokens"));
}

#[test]
fn huge_candidate_set_is_bounded_by_compress() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();
    state.selected_memories = (0..12)
        .map(|index| huge_bundle_result(&format!("memory-{index}"), 1.0 - (index as f32 * 0.01)))
        .collect();

    let outcome = compress(
        &mut state,
        CompressArgs { token_budget: 160 },
        &harness.ctx(),
    )
    .expect("compress");

    assert!(outcome.removed_count > 0);
    assert!(outcome.budget_report.estimated_tokens <= 160);
}

#[test]
fn pin_survives_compress() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();
    let pinned = file_identity("src/pinned.rs");
    state.selected_memories = vec![
        long_bundle_result(pinned.clone(), "Pinned", 0.9),
        long_bundle_result(file_identity("src/other-a.rs"), "Other A", 0.8),
        long_bundle_result(file_identity("src/other-b.rs"), "Other B", 0.7),
    ];
    pin(
        &mut state,
        PinArgs {
            identities: vec![pinned.clone()],
        },
        &harness.ctx(),
    )
    .expect("pin");

    compress(
        &mut state,
        CompressArgs { token_budget: 70 },
        &harness.ctx(),
    )
    .expect("compress");

    assert!(state
        .selected_memories
        .iter()
        .any(|result| result.identity == pinned));
}

#[test]
fn pin_twice_is_idempotent() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();
    let pinned = sample_identity("memory-a");

    let first = pin(
        &mut state,
        PinArgs {
            identities: vec![pinned.clone()],
        },
        &harness.ctx(),
    )
    .expect("first pin");
    let second = pin(
        &mut state,
        PinArgs {
            identities: vec![pinned],
        },
        &harness.ctx(),
    )
    .expect("second pin");

    assert_eq!(first.added_count, 1);
    assert_eq!(second.added_count, 0);
}

#[test]
fn evict_refuses_pinned_identity_without_force() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();
    let pinned = sample_identity("memory-a");
    state.selected_memories = vec![sample_bundle_result("memory-a", 0.8)];
    state
        .budget_decisions
        .pinned_identities
        .insert(pinned.clone());

    let error = evict(
        &mut state,
        EvictArgs {
            identities: vec![pinned],
            reason: "manual trim".to_string(),
            force: false,
        },
        &harness.ctx(),
    )
    .expect_err("pinned eviction should fail");

    assert!(error.to_string().contains("Cannot evict pinned identity"));
}

#[test]
fn forced_evict_records_force_in_exclusion_reason() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();
    let pinned = sample_identity("memory-a");
    state.selected_memories = vec![sample_bundle_result("memory-a", 0.8)];
    state
        .budget_decisions
        .pinned_identities
        .insert(pinned.clone());

    evict(
        &mut state,
        EvictArgs {
            identities: vec![pinned],
            reason: "manual trim".to_string(),
            force: true,
        },
        &harness.ctx(),
    )
    .expect("forced evict");

    assert_eq!(
        state.excluded_memories[0].exclusion_reason,
        "evicted: force=true; manual trim"
    );
}

#[test]
fn pin_survives_checkpoint_round_trip() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();
    let pinned = file_identity("src/pinned.rs");
    state
        .budget_decisions
        .pinned_identities
        .insert(pinned.clone());

    let checkpoint = super::operations::checkpoint(
        &mut state,
        super::operations::CheckpointArgs {
            name: "pins".to_string(),
        },
        &harness.ctx(),
    )
    .expect("checkpoint");
    let loaded = super::state::load_checkpoint(checkpoint.checkpoint_id, &harness.conn)
        .expect("load checkpoint");

    assert!(loaded.budget_decisions.pinned_identities.contains(&pinned));
}

#[test]
fn evict_present_identity_moves_it_to_excluded_with_reason() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();
    state.selected_memories = vec![sample_bundle_result("memory-a", 0.8)];

    evict(
        &mut state,
        EvictArgs {
            identities: vec![sample_identity("memory-a")],
            reason: "operator request".to_string(),
            force: false,
        },
        &harness.ctx(),
    )
    .expect("evict");

    assert_eq!(
        state.excluded_memories[0].exclusion_reason,
        "evicted: operator request"
    );
}

#[test]
fn evict_absent_identity_is_a_documented_no_op() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();

    let outcome = evict(
        &mut state,
        EvictArgs {
            identities: vec![sample_identity("missing")],
            reason: "operator request".to_string(),
            force: false,
        },
        &harness.ctx(),
    )
    .expect("evict");

    assert_eq!(outcome.evicted_count, 0);
    assert_eq!(outcome.skipped_absent_count, 1);
}

#[test]
fn evict_no_op_emits_no_mutation_or_event() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();
    let before = state.clone();
    evict(
        &mut state,
        EvictArgs {
            identities: vec![sample_identity("missing")],
            reason: "operator request".to_string(),
            force: false,
        },
        &harness.ctx(),
    )
    .expect("evict");
    let writer = NoopWriter::default();
    let summary = MutationEventSummary {
        op_name: "evict".to_string(),
        summary: "evicted=0 requested=1".to_string(),
    };

    let event =
        emit_memory_retrieved(&before, &state, &summary, &event_context(), &writer).expect("emit");

    assert!(harness.observer.records.borrow().is_empty());
    assert!(event.is_none());
}

#[test]
fn evicted_identity_can_be_retrieved_again_in_same_session() {
    let mut state = sample_state();
    state.selected_memories = vec![sample_bundle_result("memory-a", 0.8)];
    evict(
        &mut state,
        EvictArgs {
            identities: vec![sample_identity("memory-a")],
            reason: "operator request".to_string(),
            force: false,
        },
        &Harness::new(FakeRetriever::default()).ctx(),
    )
    .expect("evict");
    let retrieval = RetrievalExecution {
        bundle: sample_bundle(vec![sample_bundle_result("memory-a", 0.9)], 96, false, 0, 0),
        excluded_memories: Vec::new(),
    };
    retrieve(
        &mut state,
        retrieval_args(),
        &Harness::new(FakeRetriever::with_retrieve_execution(retrieval)).ctx(),
    )
    .expect("retrieve");

    assert!(state
        .selected_memories
        .iter()
        .any(|result| result.identity == sample_identity("memory-a")));
}

#[derive(Default)]
struct NoopWriter {
    appended: RefCell<usize>,
}

impl WorkingMemoryEventAppender for NoopWriter {
    fn append(
        &self,
        _envelope: PartialEnvelope,
        _flush_policy: FlushPolicy,
    ) -> Result<crate::identity::EventId, EventWriteError> {
        *self.appended.borrow_mut() += 1;
        panic!("no event should be appended for a no-op mutation");
    }
}

fn event_context() -> WorkingMemoryEventContext {
    WorkingMemoryEventContext {
        workspace_id: "workspace-test".to_string(),
        branch: BranchRef {
            name: "test".to_string(),
        },
        session_id: SessionId {
            value: "session-test".to_string(),
        },
        task_id: TaskId {
            value: "task-working-memory".to_string(),
        },
        actor: Actor::Daemon,
    }
}
