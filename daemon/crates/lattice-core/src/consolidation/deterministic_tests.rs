use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::Value;
use tempfile::tempdir;

use super::*;
use crate::graph::CodeGraph;
use crate::indexer::Indexer;
use crate::memory::{
    Memory, MemoryScope, MemoryScoreKind, MemoryScoreRecord, MemoryStore, MemoryType,
    MemoryVerificationStatus,
};

#[test]
fn duplicate_detector_emits_supersession_proposal() {
    let fixture = Fixture::new();
    let older_id = fixture.seed_memory(
        "Duplicate login memory",
        vec!["src/auth.ts"],
        vec!["loginUser"],
        10,
    );
    let newer_id = fixture.seed_memory(
        "Duplicate login memory",
        vec!["src/auth.ts"],
        vec!["loginUser"],
        20,
    );

    fixture.memory_store.reset_direct_write_count();
    let mut runtime = fixture.runtime();
    let mut detector = DuplicateDetector::new(&fixture.memory_store, &mut runtime);
    let report = detector
        .scan("workspace-main", Some(ProposalKind::Supersede))
        .unwrap();

    assert_eq!(report.proposals_enqueued, 1);
    assert_eq!(fixture.memory_store.direct_write_count(), 0);
    let proposal = fixture.load_only_proposal();
    assert_eq!(proposal.proposal_kind, ProposalKind::Supersede);
    assert_eq!(proposal.target.memory_id(), Some(older_id.as_str()));
    assert!(proposal.prior_state.get("memory").is_some());
    assert!(proposal.proposed_state.get("memory").is_some());
    assert_eq!(
        proposal
            .evidence
            .get("source_memory_ids")
            .and_then(Value::as_array)
            .map(|values| values.len()),
        Some(2)
    );
    assert!(proposal.proposed_state.to_string().contains(&newer_id));
}

#[test]
fn duplicate_detector_uses_typed_evidence_without_hash_similarity() {
    let fixture = Fixture::new();
    fixture.seed_memory(
        "Authentication failures are returned as HTTP 401.",
        vec!["src/auth.ts"],
        vec!["loginUser"],
        10,
    );
    fixture.seed_memory(
        "The login handler records an audit event after a successful request.",
        vec!["src/auth.ts"],
        vec!["loginUser"],
        20,
    );

    let mut runtime = fixture.runtime();
    let report = DuplicateDetector::new(&fixture.memory_store, &mut runtime)
        .scan("workspace-main", Some(ProposalKind::Supersede))
        .unwrap();

    assert_eq!(report.proposals_enqueued, 1);
    let proposal = fixture.load_only_proposal();
    assert_eq!(
        proposal.evidence["decision_basis"],
        "typed_evidence_overlap"
    );
    assert!(proposal.evidence.get("similarity").is_none());
}

#[test]
fn stale_marker_emits_mark_stale_proposal_for_deleted_anchor_file() {
    let fixture = Fixture::new();
    let memory_id = fixture.seed_memory("Auth notes", vec!["src/auth.ts"], vec![], 10);
    let mut runtime = fixture.runtime();
    let graph = CodeGraph::new();
    let mut marker = StaleMarker::new(
        &fixture.memory_store,
        &mut runtime,
        &graph,
        "workspace-main",
    );
    let report = marker
        .on_graph_change(vec!["src/auth.ts".to_string()])
        .unwrap();

    assert_eq!(report.proposals_enqueued, 1);
    let proposal = fixture.load_only_proposal();
    assert_eq!(proposal.proposal_kind, ProposalKind::MarkStale);
    assert_eq!(proposal.target.memory_id(), Some(memory_id.as_str()));
    assert!(proposal.prior_state.get("memory").is_some());
    assert!(proposal.proposed_state.get("memory").is_some());
}

#[test]
fn demotion_scanner_emits_demote_proposal_for_never_accessed_low_score_memory() {
    let fixture = Fixture::new();
    let memory_id = fixture.seed_memory("Cold memory", vec!["src/auth.ts"], vec![], 10);
    fixture
        .memory_store
        .write_memory_score(
            &memory_id,
            &MemoryScoreRecord {
                score_kind: MemoryScoreKind::UsefulnessPrior,
                value: 0.1,
                computed_at: 50,
                computed_from_window_secs: 60,
                sample_size: 1,
            },
        )
        .unwrap();

    let mut runtime = fixture.runtime();
    let mut scanner = DemotionScanner::new(&fixture.memory_store, &mut runtime);
    let report = scanner.scan("workspace-main", 100).unwrap();

    assert_eq!(report.proposals_enqueued, 1);
    let proposal = fixture.load_only_proposal();
    assert_eq!(proposal.proposal_kind, ProposalKind::Demote);
    assert_eq!(proposal.target.memory_id(), Some(memory_id.as_str()));
}

#[test]
fn refresh_scanner_emits_refresh_proposal_when_evidence_still_matches() {
    let fixture = Fixture::new();
    let memory_id = fixture.seed_memory(
        "Refresh auth note",
        vec!["src/auth.ts"],
        vec!["loginUser"],
        10,
    );
    fixture
        .memory_store
        .set_last_verified_at(&memory_id, 5)
        .unwrap();

    let graph = fixture.graph_with_auth();
    let mut runtime = fixture.runtime();
    let mut scanner = RefreshScanner::new(&fixture.memory_store, &mut runtime, &graph, 10);
    let report = scanner.scan("workspace-main", 25).unwrap();

    assert_eq!(report.proposals_enqueued, 1);
    let proposal = fixture.load_only_proposal();
    assert_eq!(proposal.proposal_kind, ProposalKind::Refresh);
    assert_eq!(proposal.target.memory_id(), Some(memory_id.as_str()));
}

#[test]
fn every_scanner_is_no_op_on_empty_store() {
    let fixture = Fixture::new();
    let graph = fixture.graph_with_auth();

    let mut runtime = fixture.runtime();
    let duplicate = DuplicateDetector::new(&fixture.memory_store, &mut runtime)
        .scan("workspace-main", None)
        .unwrap();
    assert_eq!(duplicate.proposals_enqueued, 0);

    let mut runtime = fixture.runtime();
    let supersession = SupersessionCandidates::new(&fixture.memory_store, &mut runtime)
        .scan("workspace-main")
        .unwrap();
    assert_eq!(supersession.proposals_enqueued, 0);

    let mut runtime = fixture.runtime();
    let demotion = DemotionScanner::new(&fixture.memory_store, &mut runtime)
        .scan("workspace-main", 100)
        .unwrap();
    assert_eq!(demotion.proposals_enqueued, 0);

    let mut runtime = fixture.runtime();
    let refresh = RefreshScanner::new(&fixture.memory_store, &mut runtime, &graph, 10)
        .scan("workspace-main", 25)
        .unwrap();
    assert_eq!(refresh.proposals_enqueued, 0);

    let mut runtime = fixture.runtime();
    let stale = StaleMarker::new(
        &fixture.memory_store,
        &mut runtime,
        &graph,
        "workspace-main",
    )
    .on_graph_change(vec!["src/auth.ts".to_string()])
    .unwrap();
    assert_eq!(stale.proposals_enqueued, 0);
}

#[test]
fn scanners_route_through_runtime_without_direct_writes() {
    let fixture = Fixture::new();
    fixture.seed_memory(
        "Duplicate login memory",
        vec!["src/auth.ts"],
        vec!["loginUser"],
        10,
    );
    fixture.seed_memory(
        "Duplicate login memory",
        vec!["src/auth.ts"],
        vec!["loginUser"],
        20,
    );
    fixture.memory_store.reset_direct_write_count();

    let mut runtime = fixture.runtime();
    let mut detector = DuplicateDetector::new(&fixture.memory_store, &mut runtime);
    detector
        .scan("workspace-main", Some(ProposalKind::Supersede))
        .unwrap();

    assert_eq!(fixture.memory_store.direct_write_count(), 0);
    assert_eq!(fixture.memory_store.list_all().unwrap().len(), 2);
    let proposal = fixture.load_only_proposal();
    assert!(proposal.prior_state.get("memory").is_some());
    assert!(proposal.proposed_state.get("memory").is_some());
}

struct Fixture {
    _dir: tempfile::TempDir,
    consolidation_db: PathBuf,
    memory_store: MemoryStore,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let memory_store = MemoryStore::open(&dir.path().join("memory.sqlite")).unwrap();
        Self {
            consolidation_db: dir.path().join("consolidation.sqlite"),
            _dir: dir,
            memory_store,
        }
    }

    fn runtime(&self) -> ConsolidationJobRuntime {
        ConsolidationJobRuntime::new(
            Connection::open(&self.consolidation_db).unwrap(),
            ConsolidationConfig::default(),
        )
        .unwrap()
    }

    fn seed_memory(
        &self,
        content: &str,
        linked_files: Vec<&str>,
        linked_symbols: Vec<&str>,
        created_at: u64,
    ) -> String {
        let memory = Memory {
            id: String::new(),
            session_id: "session-main".to_string(),
            content: content.to_string(),
            memory_type: MemoryType::Observation,
            scope: MemoryScope::Repo,
            confidence: 0.9,
            linked_symbols: linked_symbols
                .into_iter()
                .map(|value| value.to_string())
                .collect(),
            linked_files: linked_files
                .into_iter()
                .map(|value| value.to_string())
                .collect(),
            workspace_id: Some("workspace-main".to_string()),
            branch: Some("main".to_string()),
            scope_organization_id: None,
            refresh_key: None,
            source_query: None,
            created_at,
            last_accessed: created_at,
            access_count: 0,
            is_stale: false,
            stale_reason: None,
            verification_status: crate::memory::MemoryVerificationStatus::Unverified,
        };
        let id = self.memory_store.store(memory).unwrap();
        let mut fields = self
            .memory_store
            .get_structured_fields(&id)
            .unwrap()
            .unwrap_or_default();
        fields.verification_status = MemoryVerificationStatus::Verified;
        self.memory_store
            .update_structured_fields(&id, &fields)
            .unwrap();
        id
    }

    fn load_only_proposal(&self) -> ConsolidationProposal {
        let conn = Connection::open(&self.consolidation_db).unwrap();
        let proposal_id: String = conn
            .query_row(
                "SELECT proposal_id FROM consolidation_proposals ORDER BY proposal_id LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        ConsolidationProposal::load(&conn, &proposal_id)
            .unwrap()
            .unwrap()
    }

    fn graph_with_auth(&self) -> CodeGraph {
        let mut indexer = Indexer::new(PathBuf::from("/workspace"));
        indexer
            .index_file_content(
                "src/auth.ts",
                "export function loginUser(): boolean { return true; }",
            )
            .unwrap();
        indexer.graph().clone()
    }
}
