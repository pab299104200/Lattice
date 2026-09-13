use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::{params, Connection};
use serde_json::{json, Value};
use tempfile::tempdir;

use super::*;
use crate::events::{EventKind, EventPayload, EventQuery, EventReader, EventStore, EventWriter};
use crate::identity::OperatorId;
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType};

#[test]
fn session_scope_proposal_is_not_gated_and_auto_applies() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    runtime.submit(job_for_scope("session")).unwrap();

    let proposals = runtime
        .execute_ready(
            &fixture.memory_store,
            &fixture.event_writer,
            &fixture.operator("auto-policy"),
            &test_authority(),
        )
        .unwrap();

    assert_eq!(proposals.len(), 1);
    assert_eq!(
        proposal_decision(&fixture.db_path, "proposal-session"),
        "applied"
    );
    assert_eq!(fixture.memory_store.list_all().unwrap().len(), 1);
}

#[test]
fn branch_scope_proposal_is_not_gated_and_auto_applies() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    runtime.submit(job_for_scope("branch")).unwrap();

    runtime
        .execute_ready(
            &fixture.memory_store,
            &fixture.event_writer,
            &fixture.operator("auto-policy"),
            &test_authority(),
        )
        .unwrap();

    assert_eq!(
        proposal_decision(&fixture.db_path, "proposal-branch"),
        "applied"
    );
    assert_eq!(fixture.memory_store.list_all().unwrap().len(), 1);
}

#[test]
fn repo_scope_proposal_is_gated_and_stays_pending() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    runtime.submit(job_for_scope("repo")).unwrap();

    runtime
        .execute_ready(
            &fixture.memory_store,
            &fixture.event_writer,
            &fixture.operator("auto-policy"),
            &test_authority(),
        )
        .unwrap();

    assert_eq!(
        proposal_decision(&fixture.db_path, "proposal-repo"),
        "pending"
    );
    assert_eq!(fixture.memory_store.list_all().unwrap().len(), 0);
    let queue = fixture.review_queue();
    let items = queue
        .list_pending("workspace-main", &ReviewQueueFilter::default())
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].scope, crate::memory_graph::MemoryScope::Repo);
}

#[test]
fn organization_scope_proposal_is_gated_and_stays_pending() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    runtime.submit(job_for_scope("organization")).unwrap();

    runtime
        .execute_ready(
            &fixture.memory_store,
            &fixture.event_writer,
            &fixture.operator("auto-policy"),
            &test_authority(),
        )
        .unwrap();

    assert_eq!(
        proposal_decision(&fixture.db_path, "proposal-organization"),
        "pending"
    );
    assert_eq!(fixture.memory_store.list_all().unwrap().len(), 0);
}

#[test]
fn decide_apply_routes_through_proposal_apply_and_records_operator() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    runtime.submit(job_for_scope("repo")).unwrap();
    runtime
        .execute_ready(
            &fixture.memory_store,
            &fixture.event_writer,
            &fixture.operator("auto-policy"),
            &test_authority(),
        )
        .unwrap();
    let queue = fixture.review_queue();

    let outcome = queue
        .decide(
            "proposal-repo",
            ProposalDecision::Applied,
            &fixture.operator("reviewer-1"),
            Some("promote durable fact".to_string()),
            &test_authority(),
        )
        .unwrap();

    match outcome {
        ReviewDecisionOutcome::Applied { outcome } => {
            assert_eq!(
                outcome,
                ApplyOutcome::Applied {
                    memory_id: "mem-proposal-repo".to_string()
                }
            );
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    assert_eq!(
        proposal_decision(&fixture.db_path, "proposal-repo"),
        "applied"
    );
    assert_eq!(
        proposal_decision_reason(&fixture.db_path, "proposal-repo").as_deref(),
        Some("promote durable fact")
    );
    let event = fixture.memory_consolidated_event();
    match event.payload {
        EventPayload::MemoryConsolidated(payload) => {
            assert_eq!(payload.decided_by.as_deref(), Some("reviewer-1"));
            assert_eq!(
                payload.decision_reason.as_deref(),
                Some("promote durable fact")
            );
            assert_eq!(payload.proposal_id.as_deref(), Some("proposal-repo"));
        }
        _ => panic!("expected memory_consolidated"),
    }
}

#[test]
fn decide_reject_leaves_memory_unchanged_and_records_rejection() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    runtime.submit(job_for_scope("repo")).unwrap();
    runtime
        .execute_ready(
            &fixture.memory_store,
            &fixture.event_writer,
            &fixture.operator("auto-policy"),
            &test_authority(),
        )
        .unwrap();
    let queue = fixture.review_queue();

    let outcome = queue
        .decide(
            "proposal-repo",
            ProposalDecision::Rejected,
            &fixture.operator("reviewer-2"),
            Some("insufficient evidence".to_string()),
            &test_authority(),
        )
        .unwrap();

    match outcome {
        ReviewDecisionOutcome::Rejected { outcome } => {
            assert_eq!(outcome, RejectOutcome::Rejected);
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    assert_eq!(fixture.memory_store.list_all().unwrap().len(), 0);
    assert_eq!(
        proposal_decision(&fixture.db_path, "proposal-repo"),
        "rejected"
    );
    assert_eq!(
        proposal_decision_reason(&fixture.db_path, "proposal-repo").as_deref(),
        Some("insufficient evidence")
    );
}

#[test]
fn list_pending_returns_oldest_first_and_honors_filters() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    runtime.submit(job_for_scope("repo")).unwrap();
    runtime.submit(job_for_scope("organization")).unwrap();
    runtime
        .execute_ready(
            &fixture.memory_store,
            &fixture.event_writer,
            &fixture.operator("auto-policy"),
            &test_authority(),
        )
        .unwrap();
    fixture.set_enqueued_at("job-repo", 10);
    fixture.set_enqueued_at("job-organization", 20);

    let queue = fixture.review_queue();
    let items = queue
        .list_pending("workspace-main", &ReviewQueueFilter::default())
        .unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].proposal_id, "proposal-repo");
    assert_eq!(items[1].proposal_id, "proposal-organization");

    let repo_only = queue
        .list_pending(
            "workspace-main",
            &ReviewQueueFilter {
                scope: Some(crate::memory_graph::MemoryScope::Repo),
                ..ReviewQueueFilter::default()
            },
        )
        .unwrap();
    assert_eq!(repo_only.len(), 1);
    assert_eq!(repo_only[0].proposal_id, "proposal-repo");

    let older_only = queue
        .list_pending(
            "workspace-main",
            &ReviewQueueFilter {
                older_than: Some(10),
                ..ReviewQueueFilter::default()
            },
        )
        .unwrap();
    assert_eq!(older_only.len(), 1);
    assert_eq!(older_only[0].proposal_id, "proposal-repo");
}

#[test]
fn second_decision_is_idempotent() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    runtime.submit(job_for_scope("repo")).unwrap();
    runtime
        .execute_ready(
            &fixture.memory_store,
            &fixture.event_writer,
            &fixture.operator("auto-policy"),
            &test_authority(),
        )
        .unwrap();
    let queue = fixture.review_queue();
    queue
        .decide(
            "proposal-repo",
            ProposalDecision::Applied,
            &fixture.operator("reviewer-1"),
            Some("accept".to_string()),
            &test_authority(),
        )
        .unwrap();

    let second = queue
        .decide(
            "proposal-repo",
            ProposalDecision::Applied,
            &fixture.operator("reviewer-1"),
            Some("duplicate".to_string()),
            &test_authority(),
        )
        .unwrap();

    assert_eq!(
        second,
        ReviewDecisionOutcome::AlreadyDecided {
            decision: ProposalDecision::Applied
        }
    );
}

#[test]
fn decision_reason_column_round_trips() {
    let fixture = Fixture::new();
    let conn = Connection::open(&fixture.db_path).unwrap();
    initialize_schema(&conn).unwrap();
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('consolidation_proposals') WHERE name = 'decision_reason'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(exists, 1);

    conn.execute(
        "INSERT INTO consolidation_jobs
            (job_id, workspace_id, kind, mode, status, enqueued_at)
         VALUES ('job-schema', 'workspace-main', 'schema', 'manual_review', 'proposed', 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO consolidation_proposals
            (proposal_id, job_id, target_memory_id, proposal_kind, prior_state, proposed_state,
             evidence, provenance_json, decision, decided_by, decision_reason)
         VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, NULL, 'rejected', 'reviewer', 'schema-check')",
        params![
            "proposal-schema",
            "job-schema",
            ProposalKind::CreateMemory.as_str(),
            "{}",
            "{\"scope\":\"repo\"}",
            "{}"
        ],
    )
    .unwrap();

    let record = ConsolidationProposal::load_record(&conn, "proposal-schema")
        .unwrap()
        .unwrap();
    assert_eq!(record.decision_reason.as_deref(), Some("schema-check"));
}

struct Fixture {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    memory_store: MemoryStore,
    event_store: Arc<EventStore>,
    event_writer: EventWriter,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("consolidation.sqlite");
        let event_store = Arc::new(EventStore::open_in_memory().unwrap());
        Self {
            memory_store: MemoryStore::open(&db_path).unwrap(),
            event_store: event_store.clone(),
            event_writer: EventWriter::new(event_store, "workspace-main".to_string(), 4096),
            db_path,
            _dir: dir,
        }
    }

    fn runtime(&self) -> ConsolidationJobRuntime {
        ConsolidationJobRuntime::new(
            Connection::open(&self.db_path).unwrap(),
            ConsolidationConfig::default(),
        )
        .unwrap()
    }

    fn review_queue(&self) -> ReviewQueue<'_> {
        let conn = Box::leak(Box::new(Connection::open(&self.db_path).unwrap()));
        ReviewQueue::new(conn, &self.memory_store, &self.event_writer)
    }

    fn operator(&self, value: &str) -> OperatorId {
        OperatorId {
            value: value.to_string(),
        }
    }

    fn set_enqueued_at(&self, job_id: &str, enqueued_at: i64) {
        let conn = Connection::open(&self.db_path).unwrap();
        conn.execute(
            "UPDATE consolidation_jobs SET enqueued_at = ?1 WHERE job_id = ?2",
            params![enqueued_at, job_id],
        )
        .unwrap();
    }

    fn memory_consolidated_event(&self) -> crate::events::EventEnvelope {
        let reader = EventReader::new(self.event_store.clone());
        EventQuery::new()
            .workspace("workspace-main")
            .branch("main")
            .kind(EventKind::MemoryConsolidated)
            .execute(&reader)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    }
}

fn job_for_scope(scope: &str) -> ConsolidationJobSpec {
    ConsolidationJobSpec {
        job_id: format!("job-{scope}"),
        workspace_id: "workspace-main".to_string(),
        kind: format!("scope-{scope}"),
        mode: ConsolidationJobMode::ManualReview,
        proposal: Some(PendingProposalSpec {
            proposal_id: format!("proposal-{scope}"),
            target_memory_id: None,
            proposal_kind: ProposalKind::CreateMemory,
            prior_state: json!({}),
            proposed_state: proposal_state_for_scope(scope),
            evidence: json!({"source_memory_ids":[]}),
            provenance: None,
        }),
    }
}

fn proposal_state_for_scope(scope: &str) -> Value {
    match scope {
        "organization" => json!({
            "scope": "organization",
            "memory": { "scope": "organization" }
        }),
        _ => {
            let legacy_scope = match scope {
                "session" => MemoryScope::Session,
                "branch" => MemoryScope::Branch,
                "repo" => MemoryScope::Repo,
                other => panic!("unsupported scope: {other}"),
            };
            let mut value = serde_json::to_value(ConsolidationMemoryState {
                memory: Memory {
                    id: format!("mem-proposal-{scope}"),
                    session_id: "session-main".to_string(),
                    content: format!("{scope} memory"),
                    memory_type: MemoryType::Observation,
                    scope: legacy_scope,
                    confidence: 0.9,
                    linked_symbols: Vec::new(),
                    linked_files: Vec::new(),
                    workspace_id: Some("workspace-main".to_string()),
                    branch: Some("main".to_string()),
                    scope_organization_id: None,
                    refresh_key: Some(format!("refresh-{scope}")),
                    source_query: Some("review queue test".to_string()),
                    created_at: 1,
                    last_accessed: 1,
                    access_count: 0,
                    is_stale: false,
                    stale_reason: None,
                    verification_status: crate::memory::MemoryVerificationStatus::Unverified,
                },
                structured_fields: crate::memory::MemoryStructuredFields::default(),
                last_verified_at: None,
                last_verified_graph_snapshot_id: None,
                expires_at: None,
                memory_links: Vec::new(),
            })
            .unwrap();
            value["scope"] = Value::String(scope.to_string());
            value
        }
    }
}

fn proposal_decision(path: &PathBuf, proposal_id: &str) -> String {
    let conn = Connection::open(path).unwrap();
    conn.query_row(
        "SELECT decision FROM consolidation_proposals WHERE proposal_id = ?1",
        params![proposal_id],
        |row| row.get(0),
    )
    .unwrap()
}

fn proposal_decision_reason(path: &PathBuf, proposal_id: &str) -> Option<String> {
    let conn = Connection::open(path).unwrap();
    conn.query_row(
        "SELECT decision_reason FROM consolidation_proposals WHERE proposal_id = ?1",
        params![proposal_id],
        |row| row.get(0),
    )
    .unwrap()
}
