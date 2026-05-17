use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{params, Connection};
use serde_json::json;
use tempfile::tempdir;

use super::llm::{LlmDriver, LlmDriverError, LlmProvenance, LlmRequest, LlmResponse};
use super::proposal::{apply_memory_state, state_hash_from_json};
use super::*;
use crate::events::{EventReader, EventStore, EventWriter, FlushPolicy};
use crate::memory::model::MemoryProvenance;
use crate::memory::{
    Memory, MemoryLinkRecord, MemoryScope, MemoryStore, MemoryStructuredFields, MemoryType,
    MemoryVerificationStatus,
};

#[test]
fn replay_from_genesis_reconstructs_same_state_for_fifty_mixed_proposals() {
    let fixture = Fixture::new();
    let mut cache = HashMap::new();

    for index in 0..50 {
        let proposal_id = format!("proposal-{index}");
        let job_id = format!("job-{index}");
        let memory_id = format!("mem-{index:02}");
        let response_bytes = format!("response-{index}").into_bytes();
        let prompt_bytes = format!("prompt-{index}").into_bytes();
        let provenance = (index % 2 == 0).then(|| {
            let provenance = LlmProvenance::record(
                "fake-llm",
                &prompt_bytes,
                &response_bytes,
                8,
                4,
                Duration::from_millis(12),
            )
            .expect("provenance records");
            cache.insert(provenance.prompt_sha256, response_bytes.clone());
            provenance
        });
        let state = memory_state(&memory_id, &format!("memory {index}"));
        let proposal = ConsolidationProposal {
            proposal_id: proposal_id.clone(),
            job_id: job_id.clone(),
            target: ProposalTarget::NewMemory,
            proposal_kind: ProposalKind::CreateMemory,
            prior_state: empty_state(),
            proposed_state: encode_memory_state(&state),
            evidence: json!({ "source_memory_ids": [memory_id.clone()] }),
            provenance,
        };
        fixture.insert_job(&job_id);
        proposal
            .insert_pending(&fixture.conn)
            .expect("proposal inserts");
        proposal
            .apply(
                &fixture.conn,
                &fixture.memory_store,
                &fixture.event_writer,
                "operator",
                Some("apply for replay"),
            )
            .expect("proposal applies");
    }

    let expected_hash = fixture.store_hash();
    let driver = ReplayDriver::new(
        &fixture.event_reader,
        fixture.event_store.clone(),
        &fixture.conn,
        &fixture.memory_store,
        &fixture.event_writer,
        &fixture.clock,
    )
    .with_cached_responses(cache);
    let report = driver
        .replay(ReplayMode::FromGenesis)
        .expect("replay succeeds");

    assert_eq!(report.events_replayed, 50);
    assert_eq!(fixture.store_hash(), expected_hash);
}

#[test]
fn reverse_refresh_restores_last_verified_at_exactly() {
    let fixture = Fixture::new();
    let prior = memory_state("mem-refresh", "refresh candidate");
    apply_memory_state(&fixture.memory_store, &prior).expect("prior state stores");
    let mut proposed = prior.clone();
    proposed.last_verified_at = Some(999);
    let proposal = ConsolidationProposal {
        proposal_id: "proposal-refresh".to_string(),
        job_id: "job-refresh".to_string(),
        target: ProposalTarget::ExistingMemory("mem-refresh".to_string()),
        proposal_kind: ProposalKind::Refresh,
        prior_state: encode_memory_state(&prior),
        proposed_state: encode_memory_state(&proposed),
        evidence: json!({ "source_memory_ids": ["mem-refresh"] }),
        provenance: None,
    };
    fixture.insert_job("job-refresh");
    proposal
        .insert_pending(&fixture.conn)
        .expect("proposal inserts");
    proposal
        .apply(
            &fixture.conn,
            &fixture.memory_store,
            &fixture.event_writer,
            "operator",
            None,
        )
        .expect("proposal applies");

    let driver = fixture.replay_driver();
    let outcome = driver
        .reverse("proposal-refresh")
        .expect("reverse succeeds");

    assert_eq!(
        outcome,
        ReverseOutcome::Reverted {
            memory_id: "mem-refresh".to_string()
        }
    );
    assert_eq!(
        fixture
            .memory_store
            .get_last_verified_at("mem-refresh")
            .expect("timestamp loads"),
        prior.last_verified_at
    );
}

#[test]
fn reverse_supersede_restores_status_and_clears_memory_links() {
    let fixture = Fixture::new();
    let older = memory_state("mem-older", "older memory");
    let newer = memory_state("mem-newer", "newer memory");
    apply_memory_state(&fixture.memory_store, &older).expect("older stores");
    apply_memory_state(&fixture.memory_store, &newer).expect("newer stores");

    let mut proposed = older.clone();
    proposed.structured_fields.verification_status = MemoryVerificationStatus::Superseded;
    proposed.structured_fields.superseded_by_memory_id = Some("mem-newer".to_string());
    proposed.memory_links.push(MemoryLinkRecord {
        link_id: "dup:mem-older:mem-newer".to_string(),
        source_memory_id: "mem-older".to_string(),
        target_memory_id: "mem-newer".to_string(),
        link_type: "supersedes".to_string(),
        reason: "duplicate detector".to_string(),
        created_at: 2,
        verification_status: "verified".to_string(),
    });
    let proposal = ConsolidationProposal {
        proposal_id: "proposal-supersede".to_string(),
        job_id: "job-supersede".to_string(),
        target: ProposalTarget::ExistingMemory("mem-older".to_string()),
        proposal_kind: ProposalKind::Supersede,
        prior_state: encode_memory_state(&older),
        proposed_state: encode_memory_state(&proposed),
        evidence: json!({ "source_memory_ids": ["mem-older", "mem-newer"] }),
        provenance: None,
    };
    fixture.insert_job("job-supersede");
    proposal
        .insert_pending(&fixture.conn)
        .expect("proposal inserts");
    proposal
        .apply(
            &fixture.conn,
            &fixture.memory_store,
            &fixture.event_writer,
            "operator",
            None,
        )
        .expect("proposal applies");

    fixture
        .replay_driver()
        .reverse("proposal-supersede")
        .expect("reverse succeeds");

    let restored = fixture
        .memory_store
        .get_structured_fields("mem-older")
        .expect("fields load")
        .expect("fields exist");
    assert_eq!(
        restored.verification_status,
        MemoryVerificationStatus::Unverified
    );
    assert!(restored.superseded_by_memory_id.is_none());
    assert!(fixture
        .memory_store
        .list_memory_links_from("mem-older")
        .expect("links load")
        .is_empty());
}

#[test]
fn replay_succeeds_with_cached_llm_responses_and_fails_cleanly_without_them() {
    let fixture = Fixture::new();
    let (proposal, cache) = llm_backed_create("proposal-cache", "job-cache", "mem-cache");
    fixture.insert_job("job-cache");
    proposal
        .insert_pending(&fixture.conn)
        .expect("proposal inserts");
    proposal
        .apply(
            &fixture.conn,
            &fixture.memory_store,
            &fixture.event_writer,
            "operator",
            None,
        )
        .expect("proposal applies");

    fixture
        .replay_driver()
        .with_cached_responses(cache.clone())
        .replay(ReplayMode::FromGenesis)
        .expect("cache-backed replay succeeds");

    let error = fixture
        .replay_driver()
        .replay(ReplayMode::FromGenesis)
        .expect_err("replay without cache fails");
    assert!(matches!(error, ReplayError::CachedResponseMissing { .. }));
}

#[test]
fn replay_never_calls_live_llm_driver() {
    let fixture = Fixture::new();
    let (proposal, cache) = llm_backed_create("proposal-live", "job-live", "mem-live");
    fixture.insert_job("job-live");
    proposal
        .insert_pending(&fixture.conn)
        .expect("proposal inserts");
    proposal
        .apply(
            &fixture.conn,
            &fixture.memory_store,
            &fixture.event_writer,
            "operator",
            None,
        )
        .expect("proposal applies");

    fixture
        .replay_driver()
        .with_cached_responses(cache)
        .with_live_llm_driver(&PanicDriver)
        .replay(ReplayMode::FromGenesis)
        .expect("replay ignores live LLM driver");
}

#[test]
fn reverse_is_idempotent_for_already_reverted_proposals() {
    let fixture = Fixture::new();
    let prior = memory_state("mem-idempotent", "idempotent memory");
    apply_memory_state(&fixture.memory_store, &prior).expect("prior stores");
    let mut proposed = prior.clone();
    proposed.last_verified_at = Some(77);
    let proposal = ConsolidationProposal {
        proposal_id: "proposal-idempotent".to_string(),
        job_id: "job-idempotent".to_string(),
        target: ProposalTarget::ExistingMemory("mem-idempotent".to_string()),
        proposal_kind: ProposalKind::Refresh,
        prior_state: encode_memory_state(&prior),
        proposed_state: encode_memory_state(&proposed),
        evidence: json!({ "source_memory_ids": ["mem-idempotent"] }),
        provenance: None,
    };
    fixture.insert_job("job-idempotent");
    proposal
        .insert_pending(&fixture.conn)
        .expect("proposal inserts");
    proposal
        .apply(
            &fixture.conn,
            &fixture.memory_store,
            &fixture.event_writer,
            "operator",
            None,
        )
        .expect("proposal applies");

    let driver = fixture.replay_driver();
    assert!(matches!(
        driver.reverse("proposal-idempotent"),
        Ok(ReverseOutcome::Reverted { .. })
    ));
    assert_eq!(
        driver
            .reverse("proposal-idempotent")
            .expect("second reverse handled"),
        ReverseOutcome::AlreadyReverted
    );
}

struct Fixture {
    _dir: tempfile::TempDir,
    conn: Connection,
    memory_store: MemoryStore,
    event_store: Arc<EventStore>,
    event_reader: EventReader,
    event_writer: EventWriter,
    clock: FixedReplayClock,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempdir().expect("tempdir");
        let conn = Connection::open(dir.path().join("consolidation.sqlite")).expect("db opens");
        initialize_schema(&conn).expect("schema initializes");
        let memory_store = MemoryStore::open(&dir.path().join("memory.sqlite")).unwrap();
        let event_store = Arc::new(EventStore::open_in_memory().unwrap());
        let event_reader = EventReader::new(event_store.clone());
        let event_writer =
            EventWriter::new(event_store.clone(), "workspace-main".to_string(), 4096)
                .with_flush_policy(FlushPolicy::Sync);
        Self {
            _dir: dir,
            conn,
            memory_store,
            event_store,
            event_reader,
            event_writer,
            clock: FixedReplayClock::default(),
        }
    }

    fn insert_job(&self, job_id: &str) {
        self.conn
            .execute(
                "INSERT INTO consolidation_jobs
                    (job_id, workspace_id, kind, mode, status, enqueued_at)
                 VALUES (?1, 'workspace-main', 'replay-test', 'background', 'queued', 1)",
                params![job_id],
            )
            .expect("job inserts");
    }

    fn replay_driver(&self) -> ReplayDriver<'_> {
        ReplayDriver::new(
            &self.event_reader,
            self.event_store.clone(),
            &self.conn,
            &self.memory_store,
            &self.event_writer,
            &self.clock,
        )
    }

    fn store_hash(&self) -> [u8; 32] {
        let mut states = self
            .memory_store
            .query_unscoped_admin(None, usize::MAX)
            .expect("memories list");
        states.sort_by(|left, right| left.id.cmp(&right.id));
        let payload = states
            .into_iter()
            .map(|memory| {
                let state =
                    capture_memory_state(&self.memory_store, &memory).expect("state captures");
                serde_json::to_value(state).expect("state serializes")
            })
            .collect::<Vec<_>>();
        state_hash_from_json(&serde_json::Value::Array(payload)).expect("hash computes")
    }
}

#[derive(Clone)]
struct PanicDriver;

impl LlmDriver for PanicDriver {
    fn complete(&self, _request: LlmRequest) -> Result<LlmResponse, LlmDriverError> {
        panic!("replay must not call the live LLM driver");
    }

    fn name(&self) -> &str {
        "panic-driver"
    }
}

fn llm_backed_create(
    proposal_id: &str,
    job_id: &str,
    memory_id: &str,
) -> (ConsolidationProposal, HashMap<[u8; 32], Vec<u8>>) {
    let prompt_bytes = format!("prompt-{proposal_id}").into_bytes();
    let response_bytes = format!("response-{proposal_id}").into_bytes();
    let provenance = LlmProvenance::record(
        "fake-llm",
        &prompt_bytes,
        &response_bytes,
        10,
        5,
        Duration::from_millis(7),
    )
    .expect("provenance records");
    let mut cache = HashMap::new();
    cache.insert(provenance.prompt_sha256, response_bytes);
    (
        ConsolidationProposal {
            proposal_id: proposal_id.to_string(),
            job_id: job_id.to_string(),
            target: ProposalTarget::NewMemory,
            proposal_kind: ProposalKind::CreateMemory,
            prior_state: empty_state(),
            proposed_state: encode_memory_state(&memory_state(memory_id, "llm-backed memory")),
            evidence: json!({ "source_memory_ids": [memory_id] }),
            provenance: Some(provenance),
        },
        cache,
    )
}

fn memory_state(memory_id: &str, content: &str) -> ConsolidationMemoryState {
    ConsolidationMemoryState {
        memory: Memory {
            id: memory_id.to_string(),
            session_id: "session-main".to_string(),
            content: content.to_string(),
            memory_type: MemoryType::Observation,
            scope: MemoryScope::Session,
            confidence: 0.9,
            linked_symbols: Vec::new(),
            linked_files: Vec::new(),
            workspace_id: Some("workspace-main".to_string()),
            branch: Some("main".to_string()),
            scope_organization_id: None,
            refresh_key: None,
            source_query: None,
            created_at: 1,
            last_accessed: 1,
            access_count: 0,
            is_stale: false,
            stale_reason: None,
            verification_status: crate::memory::MemoryVerificationStatus::Unverified,
        },
        structured_fields: MemoryStructuredFields {
            provenance: vec![MemoryProvenance {
                source: "replay-tests".to_string(),
                reference: None,
                captured_at: Some(1),
                note: Some("fixture".to_string()),
            }],
            ..MemoryStructuredFields::default()
        },
        last_verified_at: Some(10),
        last_verified_graph_snapshot_id: None,
        expires_at: None,
        memory_links: Vec::new(),
    }
}
