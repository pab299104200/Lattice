use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use super::accesses::{
    list_accesses_for, mark_used, record_access, MemoryAccess, MemoryAccessError, MemoryAccessId,
};
use super::classes::{AssertionType, FreshnessKind, FreshnessPolicy, MemoryClass, MemoryScope};
use super::evidence::{
    get_evidence_for, insert_evidence, EvidenceAnchor, MemoryEvidence, MemoryEvidenceId,
};
use super::links::{
    get_links_by_type, get_links_from, get_links_to, insert_link, MemoryLink, MemoryLinkError,
    MemoryLinkId, MemoryLinkTarget, MemoryLinkType,
};
use super::scores::{latest_score, score_history, write_score, MemoryScore, ScoreKind};
use super::{initialize_schema, TestId, VerificationStatus};
use crate::events::{Actor, DocSectionId};
use crate::identity::{DocId, EventId, FileId, MemoryId, SectionId, SymbolId};

#[test]
fn every_link_type_round_trips() {
    let conn = open_schema();
    let source = memory_id("source");
    insert_memory_row(&conn, &source);

    for link_type in [
        MemoryLinkType::Supports,
        MemoryLinkType::Contradicts,
        MemoryLinkType::Supersedes,
        MemoryLinkType::Refines,
        MemoryLinkType::Generalizes,
        MemoryLinkType::Specializes,
        MemoryLinkType::CoOccursWith,
        MemoryLinkType::DerivedFrom,
        MemoryLinkType::AppliesTo,
        MemoryLinkType::ValidatedBy,
        MemoryLinkType::InvalidatedBy,
    ] {
        let target = memory_id(link_type.as_str());
        insert_memory_row(&conn, &target);
        let link = MemoryLink::try_new(
            MemoryLinkId(format!("link-{}", link_type.as_str())),
            source.clone(),
            MemoryLinkTarget::Memory(target),
            link_type,
            0.6,
            format!("{} reason", link_type.as_str()),
            Some(event_id(link_type.as_str())),
            Actor::Assistant {
                model: "gpt-test".to_string(),
            },
            timestamp(100),
            VerificationStatus::Verified,
        )
        .unwrap();
        insert_link(&conn, &link).unwrap();
    }

    for link_type in [
        MemoryLinkType::Supports,
        MemoryLinkType::Contradicts,
        MemoryLinkType::Supersedes,
        MemoryLinkType::Refines,
        MemoryLinkType::Generalizes,
        MemoryLinkType::Specializes,
        MemoryLinkType::CoOccursWith,
        MemoryLinkType::DerivedFrom,
        MemoryLinkType::AppliesTo,
        MemoryLinkType::ValidatedBy,
        MemoryLinkType::InvalidatedBy,
    ] {
        let links = get_links_by_type(&conn, link_type).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].link_type, link_type);
    }
}

#[test]
fn strength_outside_unit_interval_is_rejected() {
    let error = MemoryLink::try_new(
        MemoryLinkId("invalid".to_string()),
        memory_id("source"),
        MemoryLinkTarget::Memory(memory_id("target")),
        MemoryLinkType::Supports,
        1.1,
        "bad".to_string(),
        None,
        Actor::User,
        timestamp(10),
        VerificationStatus::Unverified,
    )
    .unwrap_err();

    assert!(matches!(error, MemoryLinkError::InvalidStrength { .. }));
}

#[test]
fn target_dispatch_round_trips_memory_and_graph_nodes() {
    let conn = open_schema();
    let source = memory_id("source");
    let target_memory = memory_id("target");
    insert_memory_row(&conn, &source);
    insert_memory_row(&conn, &target_memory);

    let memory_link = MemoryLink::try_new(
        MemoryLinkId("link-memory".to_string()),
        source.clone(),
        MemoryLinkTarget::Memory(target_memory.clone()),
        MemoryLinkType::Supports,
        0.7,
        "memory target".to_string(),
        None,
        Actor::User,
        timestamp(20),
        VerificationStatus::Verified,
    )
    .unwrap();
    insert_link(&conn, &memory_link).unwrap();

    let symbol = symbol_id("target_fn");
    let symbol_link = MemoryLink::try_new(
        MemoryLinkId("link-symbol".to_string()),
        source.clone(),
        MemoryLinkTarget::Symbol(symbol.clone()),
        MemoryLinkType::AppliesTo,
        0.4,
        "symbol target".to_string(),
        None,
        Actor::Tool {
            name: "search".to_string(),
        },
        timestamp(21),
        VerificationStatus::InReview,
    )
    .unwrap();
    insert_link(&conn, &symbol_link).unwrap();

    let links_from = get_links_from(&conn, &source).unwrap();
    assert_eq!(links_from.len(), 2);
    assert_eq!(links_from[0], memory_link);
    assert_eq!(links_from[1], symbol_link);

    let links_to_memory = get_links_to(&conn, &MemoryLinkTarget::Memory(target_memory)).unwrap();
    assert_eq!(links_to_memory, vec![memory_link]);

    let links_to_symbol = get_links_to(&conn, &MemoryLinkTarget::Symbol(symbol)).unwrap();
    assert_eq!(links_to_symbol, vec![symbol_link]);
}

#[test]
fn evidence_anchors_round_trip_including_file_span_sha256() {
    let conn = open_schema();
    let memory = memory_id("memory");
    insert_memory_row(&conn, &memory);

    let anchors = vec![
        EvidenceAnchor::FileSpan {
            file: file_id("src/lib.rs"),
            byte_start: 4,
            byte_end: 12,
            sha256: [7_u8; 32],
        },
        EvidenceAnchor::SymbolRef(symbol_id("parse")),
        EvidenceAnchor::DocSection {
            id: doc_section_id("docs/guide.md"),
            sha256: [9_u8; 32],
        },
        EvidenceAnchor::TestResult {
            test: test_id("tests/memory.rs", "test_anchor"),
            passed: true,
            run_event: event_id("test-run"),
        },
        EvidenceAnchor::EventReference(event_id("event-ref")),
    ];

    for (index, anchor) in anchors.iter().cloned().enumerate() {
        let evidence = MemoryEvidence {
            evidence_id: MemoryEvidenceId(format!("evidence-{index}")),
            memory_id: memory.clone(),
            event_id: Some(event_id(&format!("capture-{index}"))),
            anchor,
            captured_at: timestamp(30 + index as i64),
            captured_by: Actor::Daemon,
        };
        insert_evidence(&conn, &evidence).unwrap();
    }

    let stored = get_evidence_for(&conn, &memory).unwrap();
    assert_eq!(stored.len(), anchors.len());
    assert_eq!(stored[0].anchor, anchors[0]);
    assert_eq!(stored[1].anchor, anchors[1]);
    assert_eq!(stored[2].anchor, anchors[2]);
    assert_eq!(stored[3].anchor, anchors[3]);
    assert_eq!(stored[4].anchor, anchors[4]);
}

#[test]
fn accesses_can_be_marked_used_after_the_fact() {
    let conn = open_schema();
    let memory = memory_id("memory");
    insert_memory_row(&conn, &memory);

    let access = MemoryAccess {
        access_id: MemoryAccessId("access-1".to_string()),
        memory_id: memory.clone(),
        accessed_at: timestamp(40),
        accessed_in_event: event_id("retrieval"),
        accessor: Actor::Assistant {
            model: "gpt-test".to_string(),
        },
        inclusion_reason: "top retrieval result".to_string(),
        was_used: None,
        downstream_outcome_event: None,
    };
    record_access(&conn, &access).unwrap();
    mark_used(&conn, &access.access_id, true, Some(&event_id("outcome"))).unwrap();

    let accesses = list_accesses_for(&conn, &memory).unwrap();
    assert_eq!(accesses.len(), 1);
    assert_eq!(accesses[0].was_used, Some(true));
    assert_eq!(
        accesses[0].downstream_outcome_event,
        Some(event_id("outcome"))
    );
}

#[test]
fn access_outcome_resolution_is_idempotent_but_never_overwrites_first_decision() {
    let conn = open_schema();
    let memory = memory_id("memory");
    insert_memory_row(&conn, &memory);
    let access = MemoryAccess {
        access_id: MemoryAccessId("access-immutable-outcome".to_string()),
        memory_id: memory.clone(),
        accessed_at: timestamp(41),
        accessed_in_event: event_id("retrieval"),
        accessor: Actor::Daemon,
        inclusion_reason: "selected for response".to_string(),
        was_used: None,
        downstream_outcome_event: None,
    };
    record_access(&conn, &access).unwrap();

    let outcome = event_id("response-complete");
    mark_used(&conn, &access.access_id, true, Some(&outcome)).unwrap();
    mark_used(&conn, &access.access_id, true, Some(&outcome)).unwrap();

    let error = mark_used(
        &conn,
        &access.access_id,
        false,
        Some(&event_id("different-response")),
    )
    .unwrap_err();
    assert!(matches!(error, MemoryAccessError::OutcomeConflict { .. }));

    let stored = list_accesses_for(&conn, &memory).unwrap();
    assert_eq!(stored[0].was_used, Some(true));
    assert_eq!(stored[0].downstream_outcome_event, Some(outcome));
}

#[test]
fn access_outcomes_require_a_distinct_same_workspace_event() {
    let conn = open_schema();
    let memory = memory_id("memory");
    insert_memory_row(&conn, &memory);
    let access = MemoryAccess {
        access_id: MemoryAccessId("access-outcome-validation".to_string()),
        memory_id: memory.clone(),
        accessed_at: timestamp(42),
        accessed_in_event: event_id("retrieval"),
        accessor: Actor::Daemon,
        inclusion_reason: "selected for response".to_string(),
        was_used: None,
        downstream_outcome_event: None,
    };
    record_access(&conn, &access).unwrap();

    let missing = mark_used(&conn, &access.access_id, false, None).unwrap_err();
    assert!(matches!(
        missing,
        MemoryAccessError::OutcomeEventRequired { .. }
    ));

    let same_event = mark_used(
        &conn,
        &access.access_id,
        true,
        Some(&access.accessed_in_event),
    )
    .unwrap_err();
    assert!(matches!(
        same_event,
        MemoryAccessError::OutcomeMustFollowAccess { .. }
    ));

    let foreign_event = EventId {
        workspace_id: "other-workspace".to_string(),
        ulid: stable_ulid("foreign-outcome"),
    };
    let wrong_workspace =
        mark_used(&conn, &access.access_id, true, Some(&foreign_event)).unwrap_err();
    assert!(matches!(
        wrong_workspace,
        MemoryAccessError::WorkspaceMismatch {
            field: "downstream_outcome_event",
            ..
        }
    ));
}

#[test]
fn initial_access_must_be_unresolved_and_workspace_scoped() {
    let conn = open_schema();
    let memory = memory_id("memory");
    insert_memory_row(&conn, &memory);

    let already_resolved = MemoryAccess {
        access_id: MemoryAccessId("access-resolved-on-insert".to_string()),
        memory_id: memory.clone(),
        accessed_at: timestamp(43),
        accessed_in_event: event_id("retrieval"),
        accessor: Actor::Daemon,
        inclusion_reason: "selected for response".to_string(),
        was_used: Some(true),
        downstream_outcome_event: Some(event_id("response")),
    };
    assert!(matches!(
        record_access(&conn, &already_resolved),
        Err(MemoryAccessError::OutcomeConflict { .. })
    ));

    let foreign_access_event = MemoryAccess {
        access_id: MemoryAccessId("access-foreign-event".to_string()),
        memory_id: memory,
        accessed_at: timestamp(44),
        accessed_in_event: EventId {
            workspace_id: "other-workspace".to_string(),
            ulid: stable_ulid("foreign-access"),
        },
        accessor: Actor::Daemon,
        inclusion_reason: "selected for response".to_string(),
        was_used: None,
        downstream_outcome_event: None,
    };
    assert!(matches!(
        record_access(&conn, &foreign_access_event),
        Err(MemoryAccessError::WorkspaceMismatch {
            field: "accessed_in_event",
            ..
        })
    ));
}

#[test]
fn score_history_orders_newest_first() {
    let conn = open_schema();
    let memory = memory_id("memory");
    insert_memory_row(&conn, &memory);

    let older = MemoryScore {
        memory_id: memory.clone(),
        score_kind: ScoreKind::RecentUsefulness,
        value: 0.25,
        computed_at: timestamp(50),
        computed_from_window: Duration::from_secs(3600),
        sample_size: 3,
    };
    let newer = MemoryScore {
        memory_id: memory.clone(),
        score_kind: ScoreKind::RecentUsefulness,
        value: 0.75,
        computed_at: timestamp(60),
        computed_from_window: Duration::from_secs(7200),
        sample_size: 6,
    };
    write_score(&conn, &older).unwrap();
    write_score(&conn, &newer).unwrap();

    let history = score_history(&conn, &memory, ScoreKind::RecentUsefulness).unwrap();
    assert_eq!(history, vec![newer.clone(), older]);
    assert_eq!(
        latest_score(&conn, &memory, ScoreKind::RecentUsefulness).unwrap(),
        Some(newer)
    );
}

fn open_schema() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    initialize_schema(&conn).unwrap();
    conn
}

fn insert_memory_row(conn: &Connection, memory_id: &MemoryId) {
    let freshness_policy = serde_json::to_string(&FreshnessPolicy {
        kind: FreshnessKind::SessionScoped,
        ttl: None,
        recheck_interval: None,
    })
    .unwrap();
    let encoded_memory_id =
        crate::identity::encode_identity(&crate::identity::Identity::Memory(memory_id.clone()));
    conn.execute(
        "INSERT INTO memories
            (memory_id, content, class, assertion_type, scope, scope_session_id, scope_branch,
             scope_workspace_id, scope_user_id, scope_org_id, verification_status, confidence,
             confidence_reason, freshness_policy_json, validity_conditions_json,
             invalidation_triggers_json, provenance_event_ids_json, evidence_references_json,
             linked_files_json, linked_symbols_json, linked_docs_json, linked_tests_json,
             linked_memories_json, contradiction_links_json, supersession_links_json,
             access_history_json, last_verified_event_id, last_verified_state, usefulness_score,
             usefulness_score_updated_at, created_at, created_by, updated_at, updated_by,
             superseded_by, schema_version)
         VALUES
            (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL, NULL, ?7, ?8, ?9, ?10, '[]', '[]', '[]',
             '[]', '[]', '[]', '[]', '[]', '[]', '[]', '[]', '[]', NULL, NULL, 0.0, 0, 0, ?11,
             0, ?12, NULL, 1)",
        params![
            encoded_memory_id,
            format!("memory {}", memory_id.ulid),
            MemoryClass::Observation.as_str(),
            AssertionType::Observation.as_str(),
            MemoryScope::Session.as_str(),
            "session-1",
            VerificationStatus::Unverified.as_str(),
            0.5_f64,
            "seed",
            freshness_policy,
            "user",
            "assistant",
        ],
    )
    .unwrap();
}

fn file_id(path: &str) -> FileId {
    FileId {
        workspace_id: "workspace".to_string(),
        repo_relative_path: path.to_string(),
        content_hash: "deadbeef".to_string(),
    }
}

fn symbol_id(name: &str) -> SymbolId {
    SymbolId {
        file: file_id("src/lib.rs"),
        qualified_name: name.to_string(),
        byte_offset: 12,
        kind: "function".to_string(),
    }
}

fn doc_section_id(path: &str) -> DocSectionId {
    SectionId {
        doc: DocId {
            workspace_id: "workspace".to_string(),
            repo_relative_path: path.to_string(),
            content_hash: "feedface".to_string(),
        },
        heading_path: vec!["Guide".to_string(), "Section".to_string()],
        byte_offset: 44,
    }
}

fn memory_id(suffix: &str) -> MemoryId {
    MemoryId {
        workspace_id: "workspace".to_string(),
        ulid: stable_ulid(suffix),
    }
}

fn event_id(suffix: &str) -> EventId {
    EventId {
        workspace_id: "workspace".to_string(),
        ulid: stable_ulid(&format!("event-{suffix}")),
    }
}

fn test_id(test_path: &str, test_name: &str) -> TestId {
    TestId {
        workspace_id: "workspace".to_string(),
        test_path: test_path.to_string(),
        test_name: test_name.to_string(),
    }
}

fn timestamp(seconds: i64) -> DateTime<Utc> {
    DateTime::from_unix_seconds(seconds)
}

fn stable_ulid(seed: &str) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut bytes = [b'0'; 26];
    bytes[..24].copy_from_slice(b"01ARZ3NDEKTSV4RRFFQ69G5F");
    let checksum = seed.bytes().fold(0_u32, |acc, byte| {
        acc.wrapping_mul(33).wrapping_add(u32::from(byte))
    });
    bytes[24] = ALPHABET[((checksum >> 5) & 31) as usize];
    bytes[25] = ALPHABET[(checksum & 31) as usize];
    String::from_utf8(bytes.to_vec()).unwrap()
}
