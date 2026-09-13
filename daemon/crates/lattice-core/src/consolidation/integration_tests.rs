//! Phase 6 consolidation integration tests.
//!
//! Spec invariants from `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 6. Consolidation Engine`, `## Phase 6: Consolidation Engine`, and
//! `## Testing Requirements`:
//!
//! - "consolidation is auditable, reversible, and test-covered"
//! - "Every background consolidation pass is recoverable, replayable, and observable."
//! - "failed or malformed LLM responses must leave the prior memory state unchanged and emit a consolidation failure event"
//! - "consolidation queue depth must be bounded; when the queue is full, new jobs are dropped with a log warning rather than stalling the daemon"

use std::sync::{Arc, Mutex};

use super::integration_test_support::*;
use super::*;
use crate::events::EventPayload;
use crate::memory::{MemoryScope, MemoryVerificationStatus};

#[test]
fn test_synchronous_session_consolidation_emits_proposal_not_silent_write() {
    let harness = ConsolidationHarness::new();
    harness.append_small_task("task-sync");
    let outcome = harness
        .session_consolidator()
        .on_task_complete(
            &EvolutionAuthority {
                repository_id: "workspace-main",
                checkout_id: "checkout-main",
                branch: "main",
            },
            &task_id("task-sync"),
            EpisodeOutcome::Success,
        )
        .expect("session consolidation succeeds");
    let proposal_id = expect_session_proposal(outcome);

    assert_pending_create_memory(&harness, &proposal_id);
    assert_eq!(harness.memory_count(), 0);
    harness.apply_proposal(&proposal_id);
    assert_eq!(harness.memory_count(), 1);
    assert_eq!(harness.consolidated_events().len(), 1);
}

#[test]
fn test_deterministic_supersession_applies_and_checkout_bound_reverse_fails_closed() {
    let harness = ConsolidationHarness::new();
    let older_id = harness.seed_memory(
        "mem-older",
        "Duplicate login memory",
        MemoryScope::Repo,
        10,
        vec!["src/auth.ts"],
        Vec::new(),
    );
    let newer_id = harness.seed_memory(
        "mem-newer",
        "Duplicate login memory",
        MemoryScope::Repo,
        20,
        vec!["src/auth.ts"],
        Vec::new(),
    );
    let proposal_id = harness.run_supersession_scan();

    harness.apply_proposal(&proposal_id);
    assert_superseded_state(&harness, &older_id, &newer_id);
    let error = harness.reverse_proposal_result(&proposal_id).unwrap_err();
    assert!(error.to_string().contains("checkout-bound"));
    assert_superseded_state(&harness, &older_id, &newer_id);
}

#[test]
fn test_llm_episode_job_well_formed_response_emits_proposal_with_provenance() {
    let mut harness = ConsolidationHarness::new();
    harness.driver.push_ok(episode_response());
    let slice = harness.task_slice("task-llm-provenance");
    let proposal = harness
        .run_episode_job(background_ctx(), &slice)
        .expect("episode job succeeds")
        .expect("proposal emitted");
    let request = harness.driver.last_request().expect("request recorded");
    let provenance = proposal.provenance.expect("provenance recorded");

    assert_eq!(provenance.model, "fake-llm");
    assert_eq!(provenance.prompt_sha256, sha256(request.prompt.as_bytes()));
    assert_eq!(
        provenance.response_sha256,
        sha256(episode_response().as_bytes())
    );
}

#[test]
fn test_llm_malformed_response_leaves_memory_unchanged_and_emits_failure_event() {
    let mut harness = ConsolidationHarness::new();
    harness.driver.push_ok("{not json".to_string());
    let slice = harness.task_slice("task-llm-malformed");

    let result = harness.run_episode_job(background_ctx(), &slice);

    assert!(matches!(
        result,
        Err(super::llm::LlmJobError::MalformedResponse(_))
    ));
    assert_eq!(harness.proposal_ids().len(), 0);
    assert_eq!(harness.memory_count(), 0);
    assert_single_failure_kind(&harness, "malformed_response");
}

#[test]
fn test_llm_synchronous_mode_is_forbidden_and_driver_never_called() {
    let mut harness = ConsolidationHarness::new();
    harness.driver.push_ok(episode_response());
    let slice = harness.task_slice("task-llm-sync-guard");

    let result = harness.run_episode_job(synchronous_ctx(), &slice);

    assert!(matches!(
        result,
        Err(super::llm::LlmJobError::ForbiddenOnHotPath(
            "episode_summary"
        ))
    ));
    assert_eq!(harness.driver.call_count(), 0);
}

#[test]
fn test_bounded_queue_overflow_drops_jobs_with_warning_event() {
    let harness = ConsolidationHarness::new();
    let logs = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufferWriter(logs.clone()))
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let mut queue = harness.llm_queue(2);

    let outcomes = (0..5)
        .map(|index| {
            queue.enqueue_llm(
                llm_queue_job(index),
                &harness.event_writer,
                "fake-llm",
                &EvolutionAuthority {
                    repository_id: "workspace-main",
                    checkout_id: "checkout-main",
                    branch: "main",
                },
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .expect("enqueue succeeds");

    assert_eq!(count_dropped(&outcomes), 3);
    assert_failure_count(&harness, "queue_full", 3);
    assert!(String::from_utf8(logs.lock().unwrap().clone())
        .expect("logs are utf8")
        .contains("LLM consolidation queue slice full; dropping job"));
}

#[test]
fn test_high_scope_proposal_routes_to_manual_review_queue() {
    let harness = ConsolidationHarness::new();
    let memory_id = harness.seed_memory(
        "mem-repo-review",
        "Repo memory",
        MemoryScope::Repo,
        1,
        vec!["src/repo.rs"],
        vec!["repoFact"],
    );
    let proposal_id = harness.execute_ready(repo_scope_update_job(&memory_id));
    let pending = harness.pending_repo_review_items();

    assert_eq!(
        harness.proposal_decision(&proposal_id),
        ProposalDecision::Pending
    );
    assert_eq!(harness.memory_count(), 1);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].proposal_id, proposal_id);
}

#[test]
fn test_replay_from_genesis_reconstructs_memory_state() {
    let harness = ConsolidationHarness::new();
    let cache = harness.apply_replay_mix();
    let mut expected = harness.memories();
    expected.sort_by(|left, right| left.id.cmp(&right.id));
    let driver = CountingDriver::default();

    let replay = harness.replay_driver_with_cache(cache, &driver);
    replay
        .replay(ReplayMode::FromGenesis)
        .expect("replay succeeds");
    let mut derived = replay.derived_memories().expect("derived memories list");
    derived.sort_by(|left, right| left.id.cmp(&right.id));
    assert_eq!(derived, expected);
    assert_eq!(driver.call_count(), 0);
}

fn expect_session_proposal(outcome: SessionConsolidationOutcome) -> String {
    match outcome {
        SessionConsolidationOutcome::Proposed { proposal_id, .. } => proposal_id,
        other => panic!("expected proposal outcome, got {other:?}"),
    }
}

fn assert_pending_create_memory(harness: &ConsolidationHarness, proposal_id: &str) {
    let proposal = harness.load_proposal(proposal_id);
    let record = harness.proposal_record(proposal_id);
    assert_eq!(proposal.proposal_kind, ProposalKind::CreateMemory);
    assert_eq!(record.decision, ProposalDecision::Pending);
}

fn assert_superseded_state(harness: &ConsolidationHarness, older_id: &str, newer_id: &str) {
    let fields = harness.structured_fields(older_id);
    assert_eq!(
        fields.verification_status,
        MemoryVerificationStatus::Superseded
    );
    assert_eq!(fields.superseded_by_memory_id.as_deref(), Some(newer_id));
    assert!(harness.memory_links_from(older_id).is_empty());
}

fn assert_restored_state(harness: &ConsolidationHarness, older_id: &str) {
    let fields = harness.structured_fields(older_id);
    assert_eq!(
        fields.verification_status,
        MemoryVerificationStatus::Verified
    );
    assert!(fields.superseded_by_memory_id.is_none());
    assert!(harness.memory_links_from(older_id).is_empty());
}

fn assert_single_failure_kind(harness: &ConsolidationHarness, error_kind: &str) {
    assert_failure_count(harness, error_kind, 1);
    assert_eq!(harness.failure_events().len(), 1);
}

fn assert_failure_count(harness: &ConsolidationHarness, error_kind: &str, expected: usize) {
    let actual = harness
        .failure_events()
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::ConsolidationFailed(payload) => Some(payload.error_kind),
            _ => None,
        })
        .filter(|kind| kind == error_kind)
        .count();
    assert_eq!(actual, expected);
}

fn count_dropped(outcomes: &[EnqueueOutcome]) -> usize {
    outcomes
        .iter()
        .filter(|outcome| matches!(outcome, EnqueueOutcome::Dropped { .. }))
        .count()
}
