use std::fmt;

use chrono::Utc;
use rusqlite::Connection;

use crate::events::{EventQuery, EventReader, QueryOrder};
use crate::memory_graph::replay_events;
use crate::memory_graph::tests_common::{idem, memory_evidence, stable_ulid, TestHarness};
use crate::memory_graph::{
    get_links_to, initialize_schema, insert_link, transition_status, AssertionType, MemoryClass,
    MemoryLink, MemoryLinkId, MemoryLinkReference, MemoryLinkTarget, MemoryLinkType,
    VerificationStatus,
};

#[test]
fn supersedes_transitions_prior_memory_to_superseded_and_sets_successor() {
    let harness = TestHarness::new();
    let prior = verified_memory(&harness, "prior");
    let replacement = created_memory(&harness, "replacement");

    apply_supersedes(&harness, &replacement, &prior, "prior-replacement")
        .expect("supersedes applies");

    let loaded = harness
        .store
        .get(&prior.memory_id, &harness.branch_scope())
        .expect("prior reloads");
    assert_eq!(loaded.verification_status, VerificationStatus::Superseded);
    assert_eq!(loaded.superseded_by, Some(replacement.memory_id));
}

#[test]
fn supersession_chain_is_traversable_and_cycles_are_rejected() {
    let harness = TestHarness::new();
    let a = verified_memory(&harness, "a");
    let b = verified_memory(&harness, "b");
    let c = verified_memory(&harness, "c");

    apply_supersedes(&harness, &b, &a, "b-a").expect("b supersedes a");
    apply_supersedes(&harness, &c, &b, "c-b").expect("c supersedes b");

    assert_eq!(
        current_claim(&harness.conn(), &a.memory_id).expect("current claim resolves"),
        c.memory_id
    );
    assert_eq!(
        get_links_to(
            &harness.conn(),
            &MemoryLinkTarget::Memory(a.memory_id.clone())
        )
        .unwrap()
        .len(),
        1
    );
    let error = apply_supersedes(&harness, &a, &c, "a-c").expect_err("cycle rejects");
    assert!(matches!(
        error,
        SupersessionApplyError::CycleDetected { .. }
    ));
}

#[test]
fn current_claim_walks_to_latest_successor_and_historical_lookup_preserves_prior_state() {
    let harness = TestHarness::new();
    let a = verified_memory(&harness, "historical-a");
    let historical_before = harness
        .store
        .get(&a.memory_id, &harness.branch_scope())
        .expect("a loads before supersession");
    let b = verified_memory(&harness, "historical-b");
    let c = verified_memory(&harness, "historical-c");

    let events_before = all_memory_events(&harness);
    apply_supersedes(&harness, &b, &a, "historical-b-a").expect("b supersedes a");
    apply_supersedes(&harness, &c, &b, "historical-c-b").expect("c supersedes b");

    assert_eq!(
        current_claim(&harness.conn(), &a.memory_id).expect("current claim resolves"),
        c.memory_id
    );

    let historical_db = Connection::open_in_memory().expect("historical DB opens");
    historical_db
        .execute_batch("PRAGMA foreign_keys = ON;")
        .expect("foreign keys enable");
    initialize_schema(&historical_db).expect("schema initializes");
    replay_events(&historical_db, &events_before).expect("historical replay succeeds");
    let restored = historical_db
        .query_row(
            "SELECT verification_status, superseded_by FROM memories WHERE memory_id = ?1",
            rusqlite::params![crate::identity::encode_identity(
                &crate::identity::Identity::Memory(a.memory_id.clone())
            )],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .expect("historical row loads");

    assert_eq!(restored.0, historical_before.verification_status.as_str());
    assert!(restored.1.is_none());
}

#[derive(Debug)]
enum SupersessionApplyError {
    CycleDetected { source: String, target: String },
    Link(crate::memory_graph::MemoryLinkError),
    Transition(crate::memory_graph::MemoryTransitionError),
    Store(crate::memory_graph::MemoryStoreError),
}

impl fmt::Display for SupersessionApplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CycleDetected { source, target } => {
                write!(
                    formatter,
                    "supersession cycle `{source}` -> `{target}` is not allowed"
                )
            }
            Self::Link(error) => error.fmt(formatter),
            Self::Transition(error) => error.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SupersessionApplyError {}

fn apply_supersedes(
    harness: &TestHarness,
    source: &crate::memory_graph::MemoryRecord,
    target: &crate::memory_graph::MemoryRecord,
    seed: &str,
) -> Result<(), SupersessionApplyError> {
    if current_claim(&harness.conn(), &source.memory_id).map_err(SupersessionApplyError::Store)?
        == target.memory_id
    {
        return Err(SupersessionApplyError::CycleDetected {
            source: source.memory_id.to_string(),
            target: target.memory_id.to_string(),
        });
    }

    let link = MemoryLink::try_new(
        MemoryLinkId(format!("supersedes-{}", stable_ulid(seed))),
        source.memory_id.clone(),
        MemoryLinkTarget::Memory(target.memory_id.clone()),
        MemoryLinkType::Supersedes,
        0.95,
        "newer memory supersedes prior claim".to_string(),
        None,
        crate::events::Actor::Assistant {
            model: "gpt-test".to_string(),
        },
        Utc::now(),
        VerificationStatus::Verified,
    )
    .map_err(SupersessionApplyError::Link)?;
    insert_link(&harness.conn(), &link).map_err(SupersessionApplyError::Link)?;

    let source_loaded = harness
        .store
        .get(&source.memory_id, &harness.branch_scope())
        .map_err(SupersessionApplyError::Store)?;
    harness
        .store
        .write_transition(
            &source_loaded,
            source_loaded.clone(),
            "supersession link inserted",
            Some(memory_evidence(&format!("source-{seed}"))),
        )
        .map_err(SupersessionApplyError::Store)?;

    let mut target_loaded = harness
        .store
        .get(&target.memory_id, &harness.branch_scope())
        .map_err(SupersessionApplyError::Store)?;
    target_loaded.supersession_links.push(MemoryLinkReference {
        memory_id: source.memory_id.clone(),
        reason: "replacement memory".to_string(),
    });
    target_loaded.superseded_by = Some(source.memory_id.clone());
    harness
        .store
        .write_transition(
            target,
            target_loaded,
            "newer memory superseded prior claim",
            Some(memory_evidence(&format!("target-{seed}"))),
        )
        .map_err(SupersessionApplyError::Store)?;
    let _ = transition_status(
        &harness.store,
        target.memory_id.clone(),
        VerificationStatus::Superseded,
        "newer memory superseded prior claim",
        Some(memory_evidence(&format!("status-{seed}"))),
    )
    .map_err(SupersessionApplyError::Transition)?;
    Ok(())
}

fn current_claim(
    conn: &Connection,
    start: &crate::identity::MemoryId,
) -> Result<crate::identity::MemoryId, crate::memory_graph::MemoryStoreError> {
    let mut current = start.clone();
    loop {
        let links =
            get_links_to(conn, &MemoryLinkTarget::Memory(current.clone())).map_err(|error| {
                crate::memory_graph::MemoryStoreError::ConstraintViolation {
                    detail: error.to_string(),
                }
            })?;
        let Some(next) = links
            .into_iter()
            .find(|link| link.link_type == MemoryLinkType::Supersedes)
            .map(|link| link.source)
        else {
            return Ok(current);
        };
        current = next;
    }
}

fn all_memory_events(harness: &TestHarness) -> Vec<crate::events::EventEnvelope> {
    EventReader::new(harness.event_store.clone())
        .execute(
            EventQuery::new()
                .session("memory-graph")
                .workspace("workspace")
                .branch("main")
                .order(QueryOrder::OldestFirst)
                .limit(200),
        )
        .expect("memory graph events load")
}

fn verified_memory(harness: &TestHarness, seed: &str) -> crate::memory_graph::MemoryRecord {
    let created = created_memory(harness, seed);
    transition_status(
        &harness.store,
        created.memory_id.clone(),
        VerificationStatus::Verified,
        "seed verified",
        Some(memory_evidence(&format!("verify-{seed}"))),
    )
    .expect("seed verifies")
}

fn created_memory(harness: &TestHarness, seed: &str) -> crate::memory_graph::MemoryRecord {
    harness
        .store
        .create(
            harness
                .draft(seed)
                .with_class(MemoryClass::Observation, AssertionType::Observation)
                .into(),
            idem(&format!("create-{seed}")),
        )
        .expect("memory creates")
}
