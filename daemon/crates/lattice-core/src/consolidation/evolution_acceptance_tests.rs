//! End-to-end recovery assertions for public memory-evolution persistence.

use std::sync::Arc;

use rusqlite::{params, Connection};
use serde_json::json;
use tempfile::tempdir;

use super::proposal::apply_memory_state;
use super::*;
use crate::events::{EventPayload, EventReader, EventStore, EventWriter, FlushPolicy};
use crate::memory::{
    Memory, MemoryLinkRecord, MemoryScope, MemoryStore, MemoryStructuredFields, MemoryType,
    MemoryVerificationStatus,
};

const WORKSPACE: &str = "workspace-evolution-acceptance";

#[test]
fn old_outbox_schema_migration_preserves_pending_event_and_drains_after_reopen() {
    let fixture = Fixture::new();
    let proposal = fixture.create_proposal("migration", "mem-migration", "migrated");
    fixture.insert_pending(&proposal);
    fixture.apply_without_drain(&proposal);

    let (event_uuid, event_ts, envelope): (String, i64, String) = fixture.memory.with_connection(|conn| {
        conn.query_row(
            "SELECT event_uuid,event_ts_unix_micros,envelope_json FROM consolidation_event_outbox WHERE proposal_id=?1",
            [&proposal.proposal_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).map_err(|e| crate::LatticeError::Storage(e.to_string()))
    }).unwrap();
    fixture.memory.with_connection(|conn| {
        conn.execute_batch("DROP TABLE consolidation_event_outbox;
            CREATE TABLE consolidation_event_outbox (
              outbox_id TEXT PRIMARY KEY, proposal_id TEXT NOT NULL UNIQUE, workspace_id TEXT NOT NULL,
              event_uuid TEXT NOT NULL UNIQUE, event_ts_unix_micros INTEGER NOT NULL,
              envelope_json TEXT NOT NULL CHECK (json_valid(envelope_json)), created_at INTEGER NOT NULL,
              delivered_at INTEGER, attempt_count INTEGER NOT NULL DEFAULT 0, last_error TEXT,
              FOREIGN KEY (proposal_id) REFERENCES consolidation_proposals(proposal_id) ON DELETE CASCADE);")
            .map_err(|e| crate::LatticeError::Storage(e.to_string()))?;
        conn.execute("INSERT INTO consolidation_event_outbox(outbox_id,proposal_id,workspace_id,event_uuid,event_ts_unix_micros,envelope_json,created_at) VALUES('legacy-row',?1,?2,?3,?4,?5,77)",
            params![proposal.proposal_id, WORKSPACE, event_uuid, event_ts, envelope])
            .map_err(|e| crate::LatticeError::Storage(e.to_string()))?;
        initialize_schema(conn)
    }).unwrap();

    let reopened = MemoryStore::open(fixture.memory_path()).unwrap();
    let reopened_events = Arc::new(EventStore::open(fixture.event_path()).unwrap());
    let writer = EventWriter::new(reopened_events.clone(), WORKSPACE.to_string(), 4096)
        .with_flush_policy(FlushPolicy::Sync);
    reopened.with_connection(|conn| {
        let (transition, pending): (String, i64) = conn.query_row(
            "SELECT transition,COUNT(*) OVER () FROM consolidation_event_outbox WHERE proposal_id=?1",
            [&proposal.proposal_id], |r| Ok((r.get(0)?, r.get(1)?)),
        ).map_err(|e| crate::LatticeError::Storage(e.to_string()))?;
        assert_eq!(transition, "applied");
        assert_eq!(pending, 1);
        Ok(())
    }).unwrap();
    assert_eq!(
        drain_event_outbox(&reopened, &writer, 64)
            .unwrap()
            .delivered,
        1
    );
    assert_eq!(reopened_events.latest_event_row_id().unwrap(), 1);
    reopened
        .with_connection(|conn| {
            let pending: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM consolidation_event_outbox WHERE proposal_id=?1",
                    [&proposal.proposal_id],
                    |r| r.get(0),
                )
                .map_err(|e| crate::LatticeError::Storage(e.to_string()))?;
            assert_eq!(pending, 0);
            Ok(())
        })
        .unwrap();
}

#[test]
fn replay_scans_over_ten_thousand_events_and_processes_tail_transition() {
    let fixture = Fixture::new();
    for index in 0..10_240 {
        fixture.append_noise(index);
    }
    let proposal = fixture.create_proposal("tail", "mem-tail", "tail-state");
    fixture.insert_pending(&proposal);
    fixture.apply_and_drain(&proposal);

    let driver = fixture.replay();
    let report = driver.replay(ReplayMode::FromGenesis).unwrap();
    assert_eq!(report.events_replayed, 1);
    assert_eq!(report.events_skipped, 0);
    assert_eq!(
        driver.derived_memory("mem-tail").unwrap().unwrap().content,
        "tail-state"
    );
}

#[test]
fn replay_keeps_unrelated_canonical_memory_and_acknowledged_retention_byte_exact() {
    let fixture = Fixture::new();
    let sentinel = fixture.memory_state("mem-sentinel", "canonical sentinel");
    apply_memory_state(&fixture.memory, &sentinel).unwrap();
    fixture.memory.with_connection(|conn| {
        conn.execute("UPDATE memories SET last_recalled_at=424240 WHERE id='mem-sentinel'", [])
            .map_err(|e| crate::LatticeError::Storage(e.to_string()))?;
        conn.execute("INSERT INTO memory_deliveries(delivery_id,repository_id,session_id,payload_hash,memory_set_hash,attempted_at,acknowledged_at) VALUES('delivery-sentinel',?1,'session','payload','set',424241,424242)", [WORKSPACE])
            .map_err(|e| crate::LatticeError::Storage(e.to_string()))?;
        conn.execute("INSERT INTO memory_delivery_items(delivery_id,memory_id) VALUES('delivery-sentinel','mem-sentinel')", [])
            .map_err(|e| crate::LatticeError::Storage(e.to_string()))?;
        Ok(())
    }).unwrap();
    let before_memory =
        serde_json::to_vec(&fixture.memory.get_by_id("mem-sentinel").unwrap().unwrap()).unwrap();
    let before_retention = fixture.memory.with_connection(|conn| conn.query_row(
        "SELECT m.last_recalled_at,d.acknowledged_at FROM memories m JOIN memory_delivery_items i ON i.memory_id=m.id JOIN memory_deliveries d ON d.delivery_id=i.delivery_id WHERE m.id='mem-sentinel'",
        [], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    ).map_err(|e| crate::LatticeError::Storage(e.to_string()))).unwrap();
    let proposal = fixture.create_proposal("replay-sentinel", "mem-replay", "replay-only");
    fixture.insert_pending(&proposal);
    fixture.apply_and_drain(&proposal);

    fixture.replay().replay(ReplayMode::FromGenesis).unwrap();
    assert_eq!(
        serde_json::to_vec(&fixture.memory.get_by_id("mem-sentinel").unwrap().unwrap()).unwrap(),
        before_memory
    );
    let after_retention = fixture.memory.with_connection(|conn| conn.query_row(
        "SELECT m.last_recalled_at,d.acknowledged_at FROM memories m JOIN memory_delivery_items i ON i.memory_id=m.id JOIN memory_deliveries d ON d.delivery_id=i.delivery_id WHERE m.id='mem-sentinel'",
        [], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    ).map_err(|e| crate::LatticeError::Storage(e.to_string()))).unwrap();
    assert_eq!(after_retention, before_retention);
}

#[test]
fn failed_publication_recovers_apply_then_revert_in_causal_exactly_once_order() {
    let fixture = Fixture::new();
    fixture.fail_event_inserts();
    let proposal = fixture.create_proposal("failure-reverse", "mem-failure-reverse", "applied");
    fixture.insert_pending(&proposal);
    fixture.apply_without_drain(&proposal);
    assert!(drain_event_outbox(&fixture.memory, &fixture.writer, 64).is_err());
    assert_eq!(fixture.decision(&proposal.proposal_id), "applied");

    let reverse_error = fixture
        .replay()
        .reverse(&proposal.proposal_id)
        .expect_err("committed reversal reports unavailable publication");
    assert!(matches!(
        reverse_error,
        ReverseError::EventWriteFailed { .. }
    ));
    assert_eq!(fixture.decision(&proposal.proposal_id), "reverted");
    fixture.restore_event_inserts();
    let restarted_memory = MemoryStore::open(fixture.memory_path()).unwrap();
    let restarted_events = Arc::new(EventStore::open(fixture.event_path()).unwrap());
    let writer = EventWriter::new(restarted_events.clone(), WORKSPACE.to_string(), 4096)
        .with_flush_policy(FlushPolicy::Sync);
    // The causal gate publishes the applied event first; the reverted event is
    // eligible on the following recovery pass after its predecessor retires.
    let first_recovery = drain_event_outbox(&restarted_memory, &writer, 64).unwrap();
    assert_eq!(first_recovery.delivered, 1);
    assert!(first_recovery.has_more);
    let second_recovery = drain_event_outbox(&restarted_memory, &writer, 64).unwrap();
    assert_eq!(second_recovery.delivered, 1);
    assert!(!second_recovery.has_more);
    assert_eq!(restarted_events.latest_event_row_id().unwrap(), 2);
    assert_eq!(
        drain_event_outbox(&restarted_memory, &writer, 64)
            .unwrap()
            .delivered,
        0
    );
    let transitions = restarted_events
        .query_events_after_row_id(0, 8)
        .unwrap()
        .into_iter()
        .map(|row| {
            match serde_json::from_slice::<EventPayload>(row.payload_inline.as_deref().unwrap())
                .unwrap()
            {
                EventPayload::MemoryConsolidated(payload) => payload.transition.unwrap(),
                _ => unreachable!("outbox published another payload"),
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(transitions, ["applied", "reverted"]);

    let restarted_reader = EventReader::new(restarted_events.clone());
    let driver = ReplayDriver::new(
        &restarted_reader,
        restarted_events.clone(),
        &restarted_memory,
        &writer,
        &fixture.clock,
    )
    .unwrap();
    let report = driver.replay(ReplayMode::FromGenesis).unwrap();
    assert_eq!(report.events_replayed, 2);
    assert!(driver
        .derived_memory("mem-failure-reverse")
        .unwrap()
        .is_none());
    assert!(restarted_memory
        .get_by_id("mem-failure-reverse")
        .unwrap()
        .is_none());
}

#[test]
fn reverse_after_later_content_edit_rejects_without_overwriting_or_emitting_revert() {
    let fixture = Fixture::new();
    let proposal = fixture.create_proposal("reverse-cas", "mem-reverse-cas", "applied content");
    fixture.insert_pending(&proposal);
    fixture.apply_and_drain(&proposal);
    let mut changed = fixture
        .memory
        .get_by_id("mem-reverse-cas")
        .unwrap()
        .unwrap();
    changed.content = "later canonical edit".to_string();
    fixture.memory.store(changed).unwrap();
    let before = fixture.serialized_memory("mem-reverse-cas");

    let error = fixture
        .replay()
        .reverse(&proposal.proposal_id)
        .expect_err("CAS must reject later edits");
    assert!(
        matches!(error, ReverseError::RestoreFailed { ref reason, .. } if reason.contains("changed after apply"))
    );
    assert_eq!(fixture.serialized_memory("mem-reverse-cas"), before);
    assert_eq!(fixture.decision(&proposal.proposal_id), "applied");
    assert_eq!(fixture.outbox_count(&proposal.proposal_id, "reverted"), 0);
}

#[test]
fn deletion_receipt_prevents_reverse_and_preserves_applied_decision_without_revert_event() {
    let fixture = Fixture::new();
    let proposal = fixture.create_proposal(
        "reverse-deleted",
        "mem-reverse-deleted",
        "deliberately deleted",
    );
    fixture.insert_pending(&proposal);
    fixture.apply_and_drain(&proposal);
    let before = fixture.serialized_memory("mem-reverse-deleted");
    fixture.memory.with_connection(|conn| conn.execute(
        "INSERT INTO memory_deletion_receipts(memory_id,deleted_at) VALUES('mem-reverse-deleted',99)", [],
    ).map(|_| ()).map_err(|e| crate::LatticeError::Storage(e.to_string()))).unwrap();

    let error = fixture
        .replay()
        .reverse(&proposal.proposal_id)
        .expect_err("deletion fence must prevent restore");
    assert!(
        matches!(error, ReverseError::RestoreFailed { ref reason, .. } if reason.contains("deliberately deleted"))
    );
    assert_eq!(fixture.serialized_memory("mem-reverse-deleted"), before);
    assert_eq!(fixture.decision(&proposal.proposal_id), "applied");
    assert_eq!(fixture.outbox_count(&proposal.proposal_id, "reverted"), 0);
}

#[test]
fn reverted_event_is_rejected_when_canonical_proposal_is_only_applied() {
    let fixture = Fixture::new();
    let proposal = fixture.create_proposal(
        "applied-not-reverted",
        "mem-applied-not-reverted",
        "applied",
    );
    fixture.insert_pending(&proposal);
    fixture.apply_and_drain(&proposal);
    fixture.append_memory_transition(
        &proposal.proposal_id,
        "mem-applied-not-reverted",
        WORKSPACE,
        "reverted",
    );

    let error = fixture
        .replay()
        .replay(ReplayMode::FromGenesis)
        .expect_err("reverted event needs a reverted decision");
    assert!(
        matches!(error, ReplayError::Memory(ref reason) if reason.contains("cannot authorize a 'reverted'"))
    );
    assert_eq!(fixture.decision(&proposal.proposal_id), "applied");
}

#[test]
fn local_event_row_with_foreign_nested_memory_identity_is_rejected() {
    let fixture = Fixture::new();
    let proposal = fixture.create_proposal("foreign-nested", "mem-foreign-nested", "applied");
    fixture.insert_pending(&proposal);
    fixture.apply_and_drain(&proposal);
    fixture.append_memory_transition(
        &proposal.proposal_id,
        "mem-foreign-nested",
        "workspace-foreign",
        "applied",
    );

    let error = fixture
        .replay()
        .replay(ReplayMode::FromGenesis)
        .expect_err("foreign nested identity cannot be authorized by local row");
    assert!(
        matches!(error, ReplayError::Memory(ref reason) if reason.contains("row belongs to repository") && reason.contains("payload belongs"))
    );
    assert_eq!(fixture.decision(&proposal.proposal_id), "applied");
}

#[test]
fn reverse_supersede_does_not_traverse_unrelated_replacement_graph() {
    let fixture = Fixture::new();
    let source = fixture.memory_state("mem-source", "source");
    let third = fixture.memory_state("mem-third", "third");
    let mut replacement = fixture.memory_state("mem-replacement", "replacement");
    replacement.memory_links.push(MemoryLinkRecord {
        link_id: "replacement-to-third".to_string(),
        source_memory_id: "mem-replacement".to_string(),
        target_memory_id: "mem-third".to_string(),
        link_type: "relates_to".to_string(),
        reason: "unrelated replacement graph".to_string(),
        created_at: 1,
        verification_status: "verified".to_string(),
    });
    apply_memory_state(&fixture.memory, &third).unwrap();
    apply_memory_state(&fixture.memory, &replacement).unwrap();
    apply_memory_state(&fixture.memory, &source).unwrap();
    let mut proposed = source.clone();
    proposed.structured_fields.verification_status = MemoryVerificationStatus::Superseded;
    proposed.structured_fields.superseded_by_memory_id = Some("mem-replacement".to_string());
    proposed.memory_links.push(MemoryLinkRecord {
        link_id: "source-to-replacement".to_string(),
        source_memory_id: "mem-source".to_string(),
        target_memory_id: "mem-replacement".to_string(),
        link_type: "supersedes".to_string(),
        reason: "supersession".to_string(),
        created_at: 2,
        verification_status: "verified".to_string(),
    });
    let proposal = ConsolidationProposal {
        proposal_id: "proposal-supersede-graph".to_string(),
        job_id: "job-supersede-graph".to_string(),
        target: ProposalTarget::ExistingMemory("mem-source".to_string()),
        proposal_kind: ProposalKind::Supersede,
        prior_state: encode_memory_state(&source),
        proposed_state: encode_memory_state(&proposed),
        evidence: json!({"repository_id": WORKSPACE, "checkout_id": "", "source_memory_ids": ["mem-source", "mem-replacement"], "superseded_by_memory_id": "mem-replacement", "replacement_state_hash": state_hash_for_memory(&fixture.memory, "mem-replacement").unwrap()}),
        provenance: None,
    };
    fixture.insert_pending(&proposal);
    fixture.apply_and_drain(&proposal);

    assert_eq!(
        fixture.replay().reverse(&proposal.proposal_id).unwrap(),
        ReverseOutcome::Reverted {
            memory_id: "mem-source".to_string()
        }
    );
    assert_eq!(
        fixture
            .memory
            .get_structured_fields("mem-source")
            .unwrap()
            .unwrap()
            .verification_status,
        MemoryVerificationStatus::Unverified
    );
    assert_eq!(
        fixture
            .memory
            .list_memory_links_from("mem-source")
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        fixture
            .memory
            .list_memory_links_from("mem-replacement")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn reverted_create_is_absent_from_derived_replay_without_erasing_canonical_authority() {
    let fixture = Fixture::new();
    let proposal = fixture.create_proposal("create-revert", "mem-create-revert", "temporary");
    fixture.insert_pending(&proposal);
    fixture.apply_and_drain(&proposal);
    fixture.replay().reverse(&proposal.proposal_id).unwrap();
    drain_event_outbox(&fixture.memory, &fixture.writer, 64).unwrap();
    let driver = fixture.replay();
    let report = driver.replay(ReplayMode::FromGenesis).unwrap();
    assert_eq!(report.events_replayed, 2);
    assert!(driver
        .derived_memory("mem-create-revert")
        .unwrap()
        .is_none());
    assert!(fixture
        .memory
        .get_by_id("mem-create-revert")
        .unwrap()
        .is_none());
    assert_eq!(fixture.decision(&proposal.proposal_id), "reverted");
}

#[test]
fn transitionless_historical_event_is_audit_only_and_never_authorizes_replay() {
    let fixture = Fixture::new();
    let proposal = fixture.create_proposal("audit-only", "mem-audit-only", "must-not-derive");
    fixture.insert_pending(&proposal);
    fixture.apply_without_drain(&proposal);
    fixture.memory.with_connection(|conn| {
        conn.execute(
            "UPDATE consolidation_event_outbox SET envelope_json=replace(envelope_json, '\"transition\":\"applied\",', '') WHERE proposal_id=?1",
            [&proposal.proposal_id],
        ).map_err(|e| crate::LatticeError::Storage(e.to_string()))?;
        Ok(())
    }).unwrap();
    drain_event_outbox(&fixture.memory, &fixture.writer, 64).unwrap();
    fixture
        .memory
        .with_connection(|conn| {
            conn.execute(
                "UPDATE consolidation_proposals SET decision='pending' WHERE proposal_id=?1",
                [&proposal.proposal_id],
            )
            .map_err(|e| crate::LatticeError::Storage(e.to_string()))
        })
        .unwrap();
    let report = fixture.replay().replay(ReplayMode::FromGenesis).unwrap();
    assert_eq!(report.events_replayed, 0);
    assert_eq!(report.events_skipped, 1);
}

struct Fixture {
    _dir: tempfile::TempDir,
    memory_path: std::path::PathBuf,
    event_path: std::path::PathBuf,
    memory: MemoryStore,
    events: Arc<EventStore>,
    reader: EventReader,
    writer: EventWriter,
    clock: FixedReplayClock,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let memory_path = dir.path().join("memory.sqlite");
        let event_path = dir.path().join("events.sqlite");
        let memory = MemoryStore::open(&memory_path).unwrap();
        memory.with_connection(initialize_schema).unwrap();
        let events = Arc::new(EventStore::open(&event_path).unwrap());
        let reader = EventReader::new(events.clone());
        let writer = EventWriter::new(events.clone(), WORKSPACE.to_string(), 4096)
            .with_flush_policy(FlushPolicy::Sync);
        Self {
            _dir: dir,
            memory_path,
            event_path,
            memory,
            events,
            reader,
            writer,
            clock: FixedReplayClock::default(),
        }
    }

    fn memory_path(&self) -> &std::path::Path {
        &self.memory_path
    }
    fn event_path(&self) -> &std::path::Path {
        &self.event_path
    }

    fn memory_state(&self, id: &str, content: &str) -> ConsolidationMemoryState {
        ConsolidationMemoryState {
            memory: Memory {
                id: id.to_string(),
                session_id: "session".to_string(),
                content: content.to_string(),
                memory_type: MemoryType::Observation,
                scope: MemoryScope::Session,
                confidence: 1.0,
                linked_symbols: vec![],
                linked_files: vec![],
                workspace_id: Some(WORKSPACE.to_string()),
                branch: Some("main".to_string()),
                scope_organization_id: None,
                refresh_key: None,
                source_query: None,
                created_at: 1,
                last_accessed: 1,
                access_count: 0,
                is_stale: false,
                stale_reason: None,
                verification_status: MemoryVerificationStatus::Unverified,
            },
            structured_fields: MemoryStructuredFields::default(),
            last_verified_at: None,
            last_verified_graph_snapshot_id: None,
            expires_at: None,
            memory_links: vec![],
        }
    }

    fn create_proposal(
        &self,
        suffix: &str,
        memory_id: &str,
        content: &str,
    ) -> ConsolidationProposal {
        ConsolidationProposal {
            proposal_id: format!("proposal-{suffix}"),
            job_id: format!("job-{suffix}"),
            target: ProposalTarget::NewMemory,
            proposal_kind: ProposalKind::CreateMemory,
            prior_state: empty_state(),
            proposed_state: encode_memory_state(&self.memory_state(memory_id, content)),
            evidence: json!({"repository_id": WORKSPACE, "checkout_id": "", "source_memory_ids": []}),
            provenance: None,
        }
    }

    fn insert_pending(&self, proposal: &ConsolidationProposal) {
        self.memory.with_connection(|conn| {
            conn.execute("INSERT INTO consolidation_jobs(job_id,workspace_id,kind,mode,status,enqueued_at) VALUES(?1,?2,'acceptance','background','queued',1)", params![proposal.job_id, WORKSPACE]).map_err(|e| crate::LatticeError::Storage(e.to_string()))?;
            proposal.insert_pending(conn)
        }).unwrap();
    }

    fn apply_without_drain(&self, proposal: &ConsolidationProposal) {
        self.memory
            .with_connection(|conn| {
                let tx = conn
                    .unchecked_transaction()
                    .map_err(|e| crate::LatticeError::Storage(e.to_string()))?;
                proposal.apply_transactional(
                    &tx,
                    &self.memory,
                    &EvolutionAuthority {
                        repository_id: WORKSPACE,
                        checkout_id: "",
                        branch: "main",
                    },
                    "acceptance",
                    None,
                )?;
                tx.commit()
                    .map_err(|e| crate::LatticeError::Storage(e.to_string()))
            })
            .unwrap();
    }

    fn apply_and_drain(&self, proposal: &ConsolidationProposal) {
        self.apply_without_drain(proposal);
        drain_event_outbox(&self.memory, &self.writer, 64).unwrap();
    }
    fn replay(&self) -> ReplayDriver<'_> {
        ReplayDriver::new(
            &self.reader,
            self.events.clone(),
            &self.memory,
            &self.writer,
            &self.clock,
        )
        .unwrap()
    }
    fn decision(&self, id: &str) -> String {
        self.memory
            .with_connection(|c| {
                c.query_row(
                    "SELECT decision FROM consolidation_proposals WHERE proposal_id=?1",
                    [id],
                    |r| r.get(0),
                )
                .map_err(|e| crate::LatticeError::Storage(e.to_string()))
            })
            .unwrap()
    }
    fn serialized_memory(&self, id: &str) -> Vec<u8> {
        serde_json::to_vec(&self.memory.get_by_id(id).unwrap().unwrap()).unwrap()
    }
    fn outbox_count(&self, proposal_id: &str, transition: &str) -> i64 {
        self.memory.with_connection(|conn| conn.query_row("SELECT COUNT(*) FROM consolidation_event_outbox WHERE proposal_id=?1 AND transition=?2", params![proposal_id, transition], |row| row.get(0)).map_err(|e| crate::LatticeError::Storage(e.to_string()))).unwrap()
    }

    fn append_memory_transition(
        &self,
        proposal_id: &str,
        memory_id: &str,
        payload_workspace: &str,
        transition: &str,
    ) {
        self.writer
            .append(crate::events::PartialEnvelope {
                workspace_id: Some(WORKSPACE.to_string()),
                branch: crate::events::BranchRef {
                    name: "main".to_string(),
                },
                session_id: crate::events::SessionId {
                    value: "authority-negative".to_string(),
                },
                task_id: None,
                actor: crate::events::Actor::Daemon,
                kind: crate::events::EventKind::MemoryConsolidated,
                references: vec![],
                summary: crate::events::CompactSummary::new("authority-negative").unwrap(),
                payload: EventPayload::MemoryConsolidated(
                    crate::events::MemoryConsolidatedPayload {
                        source_memory_ids: vec![],
                        consolidated_memory_id: crate::identity::MemoryId {
                            workspace_id: payload_workspace.to_string(),
                            ulid: memory_id.to_string(),
                        },
                        source_event_ids: vec![],
                        consolidation_summary: "authority-negative".to_string(),
                        proposal_id: Some(proposal_id.to_string()),
                        transition: Some(transition.to_string()),
                        prior_state_json: None,
                        proposed_state_json: None,
                        post_apply_state_hash: [0; 32],
                        decided_by: None,
                        decision_reason: None,
                    },
                ),
            })
            .unwrap();
    }

    fn append_noise(&self, index: usize) {
        self.writer
            .append(crate::events::PartialEnvelope {
                workspace_id: Some(WORKSPACE.to_string()),
                branch: crate::events::BranchRef {
                    name: "main".to_string(),
                },
                session_id: crate::events::SessionId {
                    value: format!("noise-{index}"),
                },
                task_id: None,
                actor: crate::events::Actor::Daemon,
                kind: crate::events::EventKind::WorkflowSucceeded,
                references: vec![],
                summary: crate::events::CompactSummary::new("noise").unwrap(),
                payload: crate::events::EventPayload::WorkflowSucceeded(
                    crate::events::WorkflowSucceededPayload {
                        workflow_name: format!("noise-{index}"),
                        terminal_event_id: None,
                        output_context_handle_id: None,
                        memory_ids: vec![],
                        result_summary: "noise".to_string(),
                    },
                ),
            })
            .unwrap();
    }
    fn fail_event_inserts(&self) {
        Connection::open(self.event_path()).unwrap().execute_batch("CREATE TRIGGER reject_evolution BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'publication offline'); END;").unwrap();
    }
    fn restore_event_inserts(&self) {
        Connection::open(self.event_path())
            .unwrap()
            .execute_batch("DROP TRIGGER reject_evolution;")
            .unwrap();
    }
}
