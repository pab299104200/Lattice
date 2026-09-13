use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use tempfile::tempdir;

use super::*;
use crate::consolidation::{
    ConsolidationConfig, ConsolidationJobMode, ConsolidationJobRuntime, ConsolidationJobSpec,
    ConsolidationMemoryState, ConsolidationSkipReason, ProposalDecision, ReviewQueue,
    ReviewQueueFilter,
};
use crate::events::{EventStore, EventWriter};
use crate::memory::{
    extract_default_session_digest_candidates, CheckOutcome, Memory, MemoryClass, MemoryScope,
    MemoryStore, MemoryType, SessionDigest, SessionDigestObservation,
    SESSION_DIGEST_EXTRACTOR_VERSION,
};
use crate::{DateTime, Utc};

const REPOSITORY_ID: &str = "repo-session-capture";
const RAW_SENTINEL: &str = "RAW_TRANSCRIPT_SENTINEL_DO_NOT_FORWARD";
const MOCK_KEY: &str = "mock-provider-key";

#[test]
fn disabled_records_content_free_skip_without_provider_invocation() {
    let mut fixture = Fixture::new(4);
    let config = config(false, None);

    let outcome = fixture.run(&config).expect("disabled run skips");

    assert_skip(outcome, ConsolidationSkipReason::Disabled);
    assert_eq!(fixture.driver.call_count(), 0);
    assert_eq!(fixture.proposal_count(), 0);
    fixture.assert_only_content_free_skip("disabled");
}

#[test]
fn enabled_without_usable_key_records_skip_before_provider_invocation() {
    let mut fixture = Fixture::new(4);
    let config = config(true, None);

    let outcome = fixture.run(&config).expect("missing-key run skips");

    assert_skip(outcome, ConsolidationSkipReason::MissingProviderKey);
    assert_eq!(fixture.driver.call_count(), 0);
    assert_eq!(fixture.proposal_count(), 0);
    fixture.assert_only_content_free_skip("missing_provider_key");
}

#[test]
fn full_bounded_queue_skips_before_loading_facts_or_invoking_provider() {
    let mut fixture = Fixture::new(1);
    fixture
        .runtime
        .submit(ConsolidationJobSpec {
            job_id: "already-queued".to_string(),
            workspace_id: REPOSITORY_ID.to_string(),
            kind: "deterministic-work".to_string(),
            mode: ConsolidationJobMode::Background,
            proposal: None,
        })
        .expect("queue seed persists");
    let config = config(true, Some(MOCK_KEY));

    let outcome = fixture.run(&config).expect("full queue skips");

    assert_skip(outcome, ConsolidationSkipReason::QueueFull);
    assert_eq!(fixture.driver.call_count(), 0);
    assert_eq!(fixture.proposal_count(), 0);
    assert_eq!(fixture.runtime.depth(), 1);
    fixture.assert_skip_reason_exists("queue_full");
}

#[test]
fn inapplicable_capture_facts_never_reach_the_provider() {
    for mutation in [
        "UPDATE session_digest_deliveries SET checkout_id='foreign-checkout'",
        "UPDATE session_digest_deliveries SET branch='foreign-branch'",
        "UPDATE memories SET applicable_checkout_id='foreign-checkout' WHERE source_query='automatic_session_digest'",
        "UPDATE memories SET scope='branch', branch='foreign-branch' WHERE source_query='automatic_session_digest'",
        "UPDATE memories SET is_invalidated=1 WHERE source_query='automatic_session_digest'",
        "UPDATE memories SET scope='session' WHERE source_query='automatic_session_digest'",
    ] {
        let mut fixture = Fixture::new(8);
        fixture.memory_store.with_connection(|connection| {
            connection.execute_batch(mutation).unwrap();
            Ok(())
        }).unwrap();
        let outcome = fixture.run(&config(true, Some(MOCK_KEY))).unwrap();
        assert_skip(outcome, ConsolidationSkipReason::NoEligibleCaptureFacts);
        assert_eq!(fixture.driver.call_count(), 0, "{mutation}");
        assert_eq!(fixture.proposal_count(), 0);
    }
}

#[test]
fn mismatched_capture_evidence_fails_before_provider_invocation() {
    for field in ["repository_id", "checkout_id", "branch"] {
        let mut fixture = Fixture::new(8);
        fixture.memory_store.with_connection(|connection| {
            connection.execute(
                "UPDATE memory_evidence SET detail=json_set(detail, ?1, 'foreign') WHERE kind='session_digest'",
                [format!("$.{field}")],
            ).unwrap();
            Ok(())
        }).unwrap();
        let error = fixture.run(&config(true, Some(MOCK_KEY))).unwrap_err();
        assert!(error.to_string().contains("authority"), "{error}");
        assert_eq!(fixture.driver.call_count(), 0, "{field}");
        assert_eq!(fixture.proposal_count(), 0);
    }
}

#[test]
fn valid_mocked_key_creates_repo_review_proposals_only_from_persisted_sanitized_facts() {
    let mut fixture = Fixture::new(8);
    fixture.driver.push_ok(valid_response());
    let config = config(true, Some(MOCK_KEY));
    let memories_before = fixture.memory_store.list_all().unwrap().len();
    fixture.memory_store.reset_direct_write_count();

    let outcome = fixture.run(&config).expect("valid run succeeds");

    let SessionDigestConsolidationOutcome::Proposed {
        proposal_ids,
        source_fact_count,
    } = outcome
    else {
        panic!("expected proposals");
    };
    assert_eq!(proposal_ids.len(), 2);
    assert_eq!(
        source_fact_count, 1,
        "one validated failure episode supplies one reusable source fact"
    );
    assert_eq!(fixture.driver.call_count(), 1);
    assert_eq!(fixture.memory_store.direct_write_count(), 0);
    assert_eq!(
        fixture.memory_store.list_all().unwrap().len(),
        memories_before
    );

    let request = fixture.driver.only_request();
    assert_eq!(
        request.job_kind,
        ConsolidationJobKind::SessionDigestConsolidation
    );
    assert!(!request.prompt.contains(RAW_SENTINEL));
    assert!(!request.prompt.contains(MOCK_KEY));
    assert!(!request.prompt.contains("unrelated ordinary memory"));

    let conn = Connection::open(&fixture.database_path).unwrap();
    let review_queue = ReviewQueue::new(&conn, &fixture.memory_store, &fixture.event_writer);
    let items = review_queue
        .list_pending(REPOSITORY_ID, &ReviewQueueFilter::default())
        .unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(review_queue.pending_count(REPOSITORY_ID).unwrap(), 2);
    let classes = items
        .iter()
        .map(|item| {
            assert_eq!(item.scope, crate::memory_graph::MemoryScope::Repo);
            assert_eq!(item.decision, ProposalDecision::Pending);
            assert!(item.target_memory_id.is_none());
            assert_eq!(item.kind, crate::consolidation::ProposalKind::CreateMemory);
            assert_eq!(item.provenance.as_ref().unwrap().model, "mock-model");
            let state: ConsolidationMemoryState =
                serde_json::from_value(item.proposed_state.clone()).unwrap();
            assert_eq!(state.memory.scope, MemoryScope::Repo);
            assert_eq!(state.memory.workspace_id.as_deref(), Some(REPOSITORY_ID));
            assert_eq!(
                state.structured_fields.verification_status,
                crate::memory::MemoryVerificationStatus::InReview
            );
            assert!(state
                .structured_fields
                .provenance
                .iter()
                .any(|entry| entry.source == "lattice.session_digest.llm_consolidation.v1"));
            state.structured_fields.memory_class
        })
        .collect::<Vec<_>>();
    assert!(classes.contains(&MemoryClass::Decision));
    assert!(classes.contains(&MemoryClass::Constraint));

    let proposal_text = items
        .iter()
        .map(|item| serde_json::to_string(item).unwrap())
        .collect::<String>();
    assert!(!proposal_text.contains(RAW_SENTINEL));
    assert!(!proposal_text.contains(MOCK_KEY));
    assert!(!proposal_text.contains(r#"\"scope\":\"organization\""#));
}

fn config(enabled: bool, key: Option<&str>) -> SessionDigestConsolidationConfig {
    SessionDigestConsolidationConfig::from_daemon_config(
        enabled,
        Some(SessionDigestLlmProvider::OpenAi),
        key,
        Duration::from_secs(24 * 60 * 60),
    )
    .unwrap()
    .with_bounds(16, 2, 8)
    .unwrap()
}

fn assert_skip(outcome: SessionDigestConsolidationOutcome, reason: ConsolidationSkipReason) {
    assert!(matches!(
        outcome,
        SessionDigestConsolidationOutcome::Skipped {
            reason: actual,
            ..
        } if actual == reason
    ));
}

struct Fixture {
    _dir: tempfile::TempDir,
    database_path: std::path::PathBuf,
    memory_store: MemoryStore,
    runtime: ConsolidationJobRuntime,
    event_writer: EventWriter,
    driver: MockDriver,
}

impl Fixture {
    fn new(max_queue_depth: usize) -> Self {
        let dir = tempdir().unwrap();
        let database_path = dir.path().join("repository.sqlite");
        let memory_store = MemoryStore::open(&database_path).unwrap();
        persist_sanitized_capture(&memory_store);
        memory_store
            .store(Memory {
                id: "raw-decoy-memory".to_string(),
                session_id: "ordinary-session".to_string(),
                content: format!("unrelated ordinary memory {RAW_SENTINEL}"),
                memory_type: MemoryType::Observation,
                scope: MemoryScope::Session,
                confidence: 0.2,
                linked_symbols: Vec::new(),
                linked_files: Vec::new(),
                workspace_id: Some(REPOSITORY_ID.to_string()),
                branch: Some("main".to_string()),
                scope_organization_id: None,
                refresh_key: None,
                source_query: Some("ordinary_memory".to_string()),
                created_at: now_seconds() as u64,
                last_accessed: now_seconds() as u64,
                access_count: 0,
                is_stale: false,
                stale_reason: None,
                verification_status: crate::memory::MemoryVerificationStatus::Unverified,
            })
            .unwrap();
        let runtime = ConsolidationJobRuntime::new(
            Connection::open(&database_path).unwrap(),
            ConsolidationConfig {
                max_queue_depth,
                llm_budget_catalog: None,
            },
        )
        .unwrap();
        let event_store = Arc::new(EventStore::open_in_memory().unwrap());
        let event_writer = EventWriter::new(event_store, REPOSITORY_ID.to_string(), 4096);
        Self {
            _dir: dir,
            database_path,
            memory_store,
            runtime,
            event_writer,
            driver: MockDriver::default(),
        }
    }

    fn run(
        &mut self,
        config: &SessionDigestConsolidationConfig,
    ) -> Result<SessionDigestConsolidationOutcome, LlmJobError> {
        let mut services = LlmJobServices {
            driver: &self.driver,
            runtime: &mut self.runtime,
            memory_store: &self.memory_store,
            event_writer: &self.event_writer,
            authority: &crate::consolidation::EvolutionAuthority {
                repository_id: REPOSITORY_ID,
                checkout_id: "checkout-main",
                branch: "main",
            },
        };
        SessionDigestLlmConsolidator::run(config, REPOSITORY_ID, &mut services)
    }

    fn proposal_count(&self) -> i64 {
        self.memory_store
            .with_connection(|conn| {
                conn.query_row("SELECT COUNT(*) FROM consolidation_proposals", [], |row| {
                    row.get(0)
                })
                .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))
            })
            .unwrap()
    }

    fn assert_only_content_free_skip(&self, expected_reason: &str) {
        let row = self
            .memory_store
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT kind, mode, status, proposal_id, error_kind
                     FROM consolidation_jobs",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))
            })
            .unwrap();
        assert_eq!(row.0, "session_digest_llm_consolidation");
        assert_eq!(row.1, "background");
        assert_eq!(row.2, "dropped");
        assert!(row.3.is_none());
        assert_eq!(row.4, expected_reason);
        let encoded = format!("{row:?}");
        assert!(!encoded.contains(RAW_SENTINEL));
        assert!(!encoded.contains(MOCK_KEY));
    }

    fn assert_skip_reason_exists(&self, expected_reason: &str) {
        let count = self
            .memory_store
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM consolidation_jobs WHERE error_kind = ?1",
                    [expected_reason],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))
            })
            .unwrap();
        assert_eq!(count, 1);
    }
}

fn persist_sanitized_capture(memory_store: &MemoryStore) {
    let now = DateTime::<Utc>::from_unix_seconds(now_seconds());
    let digest = SessionDigest {
        schema_version: crate::memory::SESSION_DIGEST_SCHEMA_VERSION,
        session_id: "captured-session".to_string(),
        repository_id: REPOSITORY_ID.to_string(),
        checkout_id: Some("checkout-main".to_string()),
        branch: Some("main".to_string()),
        revision: "abc123".to_string(),
        segment: 0,
        ended_at: now,
        received_at: now,
        edited_paths: vec!["daemon/src/main.rs".to_string()],
        final_summary: Some(
            "Corrected the missing import and kept the change scoped to the parser.".to_string(),
        ),
        observations: vec![
            SessionDigestObservation::Error {
                category: "compiler".to_string(),
                fingerprint:
                    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                        .to_string(),
                status: crate::memory::ErrorStatus::Observed,
                summary: Some("missing import".to_string()),
            },
            SessionDigestObservation::Error {
                category: "compiler".to_string(),
                fingerprint:
                    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                        .to_string(),
                status: crate::memory::ErrorStatus::Resolved,
                summary: Some("missing import".to_string()),
            },
            SessionDigestObservation::Check {
                label: "lattice core tests".to_string(),
                outcome: CheckOutcome::Passed,
            },
        ],
        payload_hash: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .to_string(),
        dropped_observation_count: 0,
    };
    let candidates = extract_default_session_digest_candidates(&digest);
    memory_store
        .persist_session_digest_candidate_batch(
            &digest,
            &candidates,
            SESSION_DIGEST_EXTRACTOR_VERSION,
        )
        .unwrap();
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

#[derive(Clone, Default)]
struct MockDriver {
    responses: Arc<Mutex<VecDeque<String>>>,
    calls: Arc<Mutex<Vec<LlmRequest>>>,
}

impl MockDriver {
    fn push_ok(&self, response: String) {
        self.responses.lock().unwrap().push_back(response);
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn only_request(&self) -> LlmRequest {
        let calls = self.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        calls[0].clone()
    }
}

impl LlmDriver for MockDriver {
    fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmDriverError> {
        self.calls.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .map(|content| LlmResponse { content })
            .ok_or_else(|| LlmDriverError::Failed("missing mock response".to_string()))
    }

    fn name(&self) -> &str {
        "mock-model"
    }
}

fn valid_response() -> String {
    r#"{
      "proposals": [
        {
          "memory_class": "decision",
          "content": "The repository uses focused verification for capture changes.",
          "evidence_summary": "Persisted edited paths and a passing check support this proposal.",
          "uncertainty": "Evidence comes from one captured repository session.",
          "confidence": 0.82
        },
        {
          "memory_class": "constraint",
          "content": "Session capture consolidation must remain review gated.",
          "evidence_summary": "The persisted workflow facts support a repository constraint proposal.",
          "uncertainty": "A reviewer must confirm the intended durability.",
          "confidence": 0.76
        }
      ]
    }"#
    .to_string()
}
