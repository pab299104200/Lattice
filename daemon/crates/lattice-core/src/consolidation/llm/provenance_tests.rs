use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use super::*;
use crate::consolidation::{
    initialize_schema, BoundedJobQueue, ConsolidationJobMode, ConsolidationJobSpec, EnqueueOutcome,
};
use crate::events::{EventKind, EventQuery, EventReader, EventStore, EventWriter, QueryOrder};

#[test]
fn successful_job_provenance_records_deterministic_hashes() {
    let prompt = b"Summarize this task";
    let response = br#"{"summary":"done"}"#;
    let provenance = LlmProvenance::record(
        "test-model",
        prompt,
        response,
        3,
        2,
        Duration::from_millis(42),
    )
    .expect("provenance records");

    assert_eq!(provenance.model, "test-model");
    assert_eq!(provenance.prompt_sha256, sha256(prompt));
    assert_eq!(provenance.response_sha256, sha256(response));
    assert_eq!(provenance.prompt_token_count, 3);
    assert_eq!(provenance.response_token_count, 2);
    assert_eq!(provenance.latency_ms, 42);
}

#[test]
fn identical_prompt_and_response_reproduce_hashes() {
    let first = LlmProvenance::record(
        "test-model",
        b"same prompt",
        b"same response",
        2,
        2,
        Duration::from_millis(7),
    )
    .expect("first provenance");
    let second = LlmProvenance::record(
        "test-model",
        b"same prompt",
        b"same response",
        2,
        2,
        Duration::from_millis(7),
    )
    .expect("second provenance");

    assert_eq!(first.prompt_sha256, second.prompt_sha256);
    assert_eq!(first.response_sha256, second.response_sha256);
}

#[test]
fn budget_overruns_route_to_required_outcomes() {
    let catalog = BudgetCatalog::default();

    assert_eq!(
        catalog.evaluate(ConsolidationJobKind::EpisodeSummary, over_budget_usage()),
        BudgetOutcome::ExceededFallback(DeterministicFallback::EpisodeTemplate)
    );
    assert_eq!(
        catalog.evaluate(
            ConsolidationJobKind::ContradictionDetection,
            over_budget_usage()
        ),
        BudgetOutcome::ExceededFallback(DeterministicFallback::ContradictionSupersessionCandidate)
    );
    assert_eq!(
        catalog.evaluate(
            ConsolidationJobKind::ProcedureExtraction,
            over_budget_usage()
        ),
        BudgetOutcome::ExceededSkipWithStale
    );
    assert_eq!(
        catalog.evaluate(
            ConsolidationJobKind::FailurePatternExtraction,
            over_budget_usage()
        ),
        BudgetOutcome::ExceededSkipWithStale
    );
}

#[test]
fn per_kind_queue_overflow_drops_job_and_emits_failure_event() {
    let conn = Connection::open_in_memory().expect("db opens");
    initialize_schema(&conn).expect("schema initializes");
    let conn = Arc::new(Mutex::new(conn));
    let mut queue = BoundedJobQueue::new(10, conn).with_per_kind_max_depth(1);
    let event_store = Arc::new(EventStore::open_in_memory().expect("event store opens"));
    let event_writer = EventWriter::new(event_store.clone(), "workspace-main".to_string(), 4096);

    let first = queue
        .enqueue_llm(
            job("job-a"),
            &event_writer,
            "test-model",
            &crate::consolidation::EvolutionAuthority {
                repository_id: "workspace-main",
                checkout_id: "checkout-main",
                branch: "main",
            },
        )
        .expect("first job queues");
    let second = queue
        .enqueue_llm(
            job("job-b"),
            &event_writer,
            "test-model",
            &crate::consolidation::EvolutionAuthority {
                repository_id: "workspace-main",
                checkout_id: "checkout-main",
                branch: "main",
            },
        )
        .expect("second job drops");

    assert!(matches!(first, EnqueueOutcome::Queued { depth: 1 }));
    assert!(matches!(second, EnqueueOutcome::Dropped { .. }));
    let events = EventQuery::new()
        .workspace("workspace-main")
        .branch("main")
        .kind(EventKind::ConsolidationFailed)
        .order(QueryOrder::OldestFirst)
        .execute(&EventReader::new(event_store))
        .expect("failure event queries");
    assert_eq!(events.len(), 1);
    match &events[0].payload {
        crate::events::EventPayload::ConsolidationFailed(payload) => {
            assert_eq!(payload.error_kind, "queue_full");
            assert_eq!(payload.job_kind, "episode_summary");
        }
        payload => panic!("unexpected payload {payload:?}"),
    }
}

#[test]
fn budget_architecture_doc_contains_required_headings() {
    let doc =
        include_str!("../../../../../../docs/architecture/2026-05-16-consolidation-llm-budgets.md");
    for heading in [
        "## Scope",
        "## Per-job budgets",
        "## Bounded queue depth",
        "## Failure handling",
        "## Provenance record",
        "## Operator overrides",
        "## Spec citation",
    ] {
        assert!(doc.contains(heading), "missing heading {heading}");
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(&digest);
    hash
}

fn over_budget_usage() -> BudgetUsage {
    BudgetUsage {
        prompt_tokens: u32::MAX,
        response_tokens: 0,
        latency_ms: 0,
        cost_micro_usd: 0,
    }
}

fn job(job_id: &str) -> ConsolidationJobSpec {
    ConsolidationJobSpec {
        job_id: job_id.to_string(),
        workspace_id: "workspace-main".to_string(),
        kind: ConsolidationJobKind::EpisodeSummary.as_str().to_string(),
        mode: ConsolidationJobMode::Background,
        proposal: None,
    }
}
