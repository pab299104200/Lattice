use std::collections::BTreeMap;

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::events::{EventPayload, EventQuery, EventReader, QueryOrder, StableRef};
use crate::memory_graph::tests_common::{file_id, idem, memory_evidence, TestHarness};
use crate::memory_graph::{
    get_links_from, initialize_schema, replay_events, transition_status, AssertionType,
    FreshnessKind, FreshnessPolicy, InvalidationTrigger, MemoryClass, MemoryLink, MemoryLinkId,
    MemoryLinkTarget, MemoryLinkType, TriggerKind, VerificationStatus,
};

#[test]
fn event_log_replay_matches_original_memory_tables_byte_for_byte() {
    init_tracing();
    let harness = TestHarness::new();
    let before_midpoint = seed_replay_scenario(&harness);
    assert!(!before_midpoint.is_empty());

    let original = table_fingerprint(&harness.conn());
    let replay_conn = open_replay_db();
    replay_events(&replay_conn, &all_memory_events(&harness)).expect("full replay succeeds");

    assert_eq!(original, table_fingerprint(&replay_conn));
}

#[test]
fn snapshot_plus_tail_replay_matches_full_replay_state() {
    init_tracing();
    let harness = TestHarness::new();
    let midpoint = seed_replay_scenario(&harness);
    let all_events = all_memory_events(&harness);

    let full_replay = open_replay_db();
    replay_events(&full_replay, &all_events).expect("full replay succeeds");

    let snapshot_replay = open_replay_db();
    replay_events(&snapshot_replay, &midpoint).expect("midpoint snapshot applies");
    replay_events(&snapshot_replay, &all_events[midpoint.len()..]).expect("tail replay succeeds");

    assert_eq!(
        table_fingerprint(&full_replay),
        table_fingerprint(&snapshot_replay)
    );
}

#[test]
fn corrupted_replay_snapshot_returns_typed_error_and_preserves_partial_state() {
    init_tracing();
    let harness = TestHarness::new();
    seed_replay_scenario(&harness);
    let clean_events = all_memory_events(&harness);
    let mut corrupted = clean_events.clone();
    corrupt_second_memory_update(&mut corrupted);

    let replay_conn = open_replay_db();
    let error = replay_events(&replay_conn, &corrupted).expect_err("corrupted replay fails");
    assert!(matches!(
        error,
        crate::memory_graph::MemoryReplayError::SnapshotDecode { .. }
    ));

    let partial = table_fingerprint(&replay_conn);
    let prefix_conn = open_replay_db();
    replay_events(&prefix_conn, &clean_events[..3]).expect("clean prefix replays");
    assert_eq!(partial, table_fingerprint(&prefix_conn));
}

#[test]
fn workflow_outcome_stream_classification_survives_replay() {
    init_tracing();
    let harness = TestHarness::new();
    let workflow = harness
        .store
        .create(
            harness
                .draft("workflow-outcome")
                .with_class(MemoryClass::WorkflowOutcome, AssertionType::Outcome)
                .into(),
            idem("create-workflow-outcome"),
        )
        .expect("workflow outcome creates");

    let replay_conn = open_replay_db();
    replay_events(&replay_conn, &all_memory_events(&harness)).expect("replay succeeds");
    let rows = replay_conn
        .query_row(
            "SELECT class, assertion_type FROM memories WHERE memory_id = ?1",
            rusqlite::params![crate::identity::encode_identity(
                &crate::identity::Identity::Memory(workflow.memory_id)
            )],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("workflow memory loads");

    let class = rows.0.parse().expect("class parses");
    let assertion = rows.1.parse().expect("assertion parses");
    assert_eq!(
        crate::memory_graph::classify_stream(class, assertion),
        crate::memory_graph::MemoryStream::WorkflowEpisodes
    );
}

#[test]
fn stale_transition_is_persisted_and_visible_through_scope_filtered_reads() {
    init_tracing();
    let harness = TestHarness::new();
    let draft = harness
        .draft("stale-file")
        .with_linked_file(file_id("src/stale.rs"))
        .build();
    let memory = harness
        .store
        .create(draft, idem("create-stale"))
        .expect("stale candidate creates");
    let verified = transition_status(
        &harness.store,
        memory.memory_id.clone(),
        VerificationStatus::Verified,
        "initial verification passed",
        Some(memory_evidence("stale-verified")),
    )
    .expect("verification succeeds");

    let mut stale = verified.clone();
    stale.freshness_policy = FreshnessPolicy {
        kind: FreshnessKind::EventTriggered,
        ttl: None,
        recheck_interval: None,
    };
    stale.invalidation_triggers = vec![InvalidationTrigger {
        kind: TriggerKind::FileChanged,
        target: StableRef::FileRef(file_id("src/stale.rs")),
    }];
    harness
        .store
        .write_transition(
            &verified,
            stale,
            "file changed invalidation trigger fired",
            Some(memory_evidence("stale-trigger")),
        )
        .expect("stale event writes");
    let updated = transition_status(
        &harness.store,
        memory.memory_id.clone(),
        VerificationStatus::Stale,
        "file changed invalidation trigger fired",
        Some(memory_evidence("stale-status")),
    )
    .expect("stale transition succeeds");

    assert_eq!(updated.verification_status, VerificationStatus::Stale);
    assert_eq!(
        harness
            .store
            .get(&memory.memory_id, &harness.branch_scope())
            .expect("scope-filtered read")
            .verification_status,
        VerificationStatus::Stale
    );
}

fn seed_replay_scenario(harness: &TestHarness) -> Vec<crate::events::EventEnvelope> {
    let a = harness
        .store
        .create(harness.draft("alpha").into(), idem("create-alpha"))
        .expect("alpha creates");
    let _verified = transition_status(
        &harness.store,
        a.memory_id.clone(),
        VerificationStatus::Verified,
        "alpha verified",
        Some(memory_evidence("alpha-verified")),
    )
    .expect("alpha verifies");
    let midpoint = all_memory_events(harness);

    let b = harness
        .store
        .create(
            harness
                .draft("beta")
                .with_class(MemoryClass::CounterMemory, AssertionType::Counter)
                .into(),
            idem("create-beta"),
        )
        .expect("beta creates");
    insert_supporting_link(
        harness,
        &b.memory_id,
        &a.memory_id,
        "beta-contradicts-alpha",
    );
    let beta_loaded = harness
        .store
        .get(&b.memory_id, &harness.branch_scope())
        .expect("beta loads");
    harness
        .store
        .write_transition(
            &beta_loaded,
            beta_loaded.clone(),
            "contradiction link inserted",
            Some(memory_evidence("beta-link")),
        )
        .expect("beta event writes");
    let _contradicted = transition_status(
        &harness.store,
        a.memory_id.clone(),
        VerificationStatus::Contradicted,
        "counter memory contradicts alpha",
        Some(memory_evidence("alpha-contradicted")),
    )
    .expect("alpha contradicts");
    harness
        .store
        .delete(&b.memory_id, "counter memory withdrawn")
        .expect("beta invalidates");

    midpoint
}

fn insert_supporting_link(
    harness: &TestHarness,
    source: &crate::identity::MemoryId,
    target: &crate::identity::MemoryId,
    seed: &str,
) {
    let link = MemoryLink::try_new(
        MemoryLinkId(format!("replay-link-{seed}")),
        source.clone(),
        MemoryLinkTarget::Memory(target.clone()),
        MemoryLinkType::Contradicts,
        0.8,
        "replay contradiction".to_string(),
        None,
        crate::events::Actor::Assistant {
            model: "gpt-test".to_string(),
        },
        chrono::Utc::now(),
        VerificationStatus::Verified,
    )
    .expect("link validates");
    crate::memory_graph::insert_link(&harness.conn(), &link).expect("link inserts");
    assert_eq!(
        get_links_from(&harness.conn(), source).expect("links load"),
        vec![link]
    );
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

fn corrupt_second_memory_update(events: &mut [crate::events::EventEnvelope]) {
    let mut seen_updates = 0_usize;
    for event in events {
        if let EventPayload::MemoryUpdated(payload) = &mut event.payload {
            seen_updates += 1;
            if seen_updates == 2 {
                payload.replay_snapshot_json = Some("{not-json".to_string());
                return;
            }
        }
    }
    panic!("expected two memory_updated events to corrupt");
}

fn open_replay_db() -> Connection {
    let conn = Connection::open_in_memory().expect("replay DB opens");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("foreign keys enable");
    initialize_schema(&conn).expect("schema initializes");
    conn
}

fn table_fingerprint(conn: &Connection) -> BTreeMap<&'static str, (usize, String)> {
    BTreeMap::from([
        ("memories", table_digest(conn, "memories")),
        ("memory_links", table_digest(conn, "memory_links")),
        ("memory_evidence", table_digest(conn, "memory_evidence")),
    ])
}

fn table_digest(conn: &Connection, table: &'static str) -> (usize, String) {
    let mut statement = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY 1, 2, 3"))
        .expect("statement prepares");
    let column_count = statement.column_count();
    let rows = statement
        .query_map([], |row| {
            let mut values = Vec::new();
            for index in 0..column_count {
                values.push(format!("{:?}", row.get_ref(index)?));
            }
            Ok(values.join("|"))
        })
        .expect("rows query");
    let values = rows.collect::<Result<Vec<_>, _>>().expect("rows collect");
    let mut hasher = Sha256::new();
    for value in &values {
        hasher.update(value.as_bytes());
        hasher.update(b"\n");
    }
    (values.len(), format!("{:x}", hasher.finalize()))
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt::try_init();
}
