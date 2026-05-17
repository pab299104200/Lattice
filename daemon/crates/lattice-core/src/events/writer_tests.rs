use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde_json::json;

use super::{
    canonical_json_bytes, canonicalize_json_value, hash_payload, Actor,
    AssistantTaskStartedPayload, BranchRef, CompactSummary, EventPayload, EventStore,
    EventWriteError, EventWriter, FlushPolicy, PartialEnvelope, SessionId,
};

#[test]
fn canonical_hash_is_stable_for_equivalent_key_order_variations() {
    let left = json!({
        "payload": {
            "zeta": 1,
            "alpha": {"k2": "v2", "k1": "v1"}
        },
        "kind": "assistant_task_started"
    });
    let right = json!({
        "kind": "assistant_task_started",
        "payload": {
            "alpha": {"k1": "v1", "k2": "v2"},
            "zeta": 1
        }
    });

    let left_bytes = canonical_json_bytes(&canonicalize_json_value(left)).unwrap();
    let right_bytes = canonical_json_bytes(&canonicalize_json_value(right)).unwrap();

    assert_eq!(left_bytes, right_bytes);
}

#[test]
fn spilled_payloads_dedupe_by_hash() {
    let store = Arc::new(EventStore::open_in_memory().expect("event store opens"));
    let writer = EventWriter::new(store.clone(), "workspace-main".to_string(), 1)
        .with_flush_policy(FlushPolicy::Sync);

    writer.append(large_payload_envelope("session-a")).unwrap();
    writer.append(large_payload_envelope("session-b")).unwrap();

    let payload_rows = store.with_connection(|conn| payload_row_count(conn));
    assert_eq!(payload_rows, 1);
}

#[test]
fn inline_ceiling_boundary_is_honored_exactly() {
    let store = Arc::new(EventStore::open_in_memory().expect("event store opens"));
    let payload = assistant_task_payload();
    let inline_len = canonical_json_bytes(&payload).unwrap().len();
    let writer = EventWriter::new(store.clone(), "workspace-main".to_string(), inline_len)
        .with_flush_policy(FlushPolicy::Sync);

    writer.append(base_envelope(payload)).unwrap();

    let rows = store.query_events_by_session("session-main", 10).unwrap();
    assert!(rows[0].payload_inline.is_some());
    assert!(rows[0].payload_spill_id.is_none());

    let spill_writer =
        EventWriter::new(store.clone(), "workspace-main".to_string(), inline_len - 1)
            .with_flush_policy(FlushPolicy::Sync);
    spill_writer
        .append(base_envelope(assistant_task_payload()))
        .unwrap();

    let rows = store.query_events_by_session("session-main", 10).unwrap();
    assert!(rows[1].payload_inline.is_none());
    assert!(rows[1].payload_spill_id.is_some());
}

#[test]
fn workspace_mismatch_is_rejected() {
    let store = Arc::new(EventStore::open_in_memory().expect("event store opens"));
    let writer = EventWriter::new(store, "workspace-main".to_string(), 4096);
    let mut envelope = base_envelope(assistant_task_payload());
    envelope.workspace_id = Some("workspace-other".to_string());

    let error = writer
        .append(envelope)
        .expect_err("workspace mismatch should fail");

    match error {
        EventWriteError::WorkspaceMismatch { expected, actual } => {
            assert_eq!(expected, "workspace-main");
            assert_eq!(actual, "workspace-other");
        }
        other => panic!("expected workspace mismatch, got {other:?}"),
    }
}

#[test]
fn concurrent_appends_preserve_monotonic_row_ids() {
    let store = Arc::new(EventStore::open_in_memory().expect("event store opens"));
    let writer = Arc::new(
        EventWriter::new(store.clone(), "workspace-main".to_string(), 4096)
            .with_flush_policy(FlushPolicy::Sync),
    );

    let first = {
        let writer = writer.clone();
        thread::spawn(move || {
            for index in 0..10 {
                let mut envelope = base_envelope(assistant_task_payload());
                envelope.session_id = SessionId {
                    value: format!("thread-a-{index}"),
                };
                writer.append(envelope).unwrap();
            }
        })
    };
    let second = {
        let writer = writer.clone();
        thread::spawn(move || {
            for index in 0..10 {
                let mut envelope = base_envelope(assistant_task_payload());
                envelope.session_id = SessionId {
                    value: format!("thread-b-{index}"),
                };
                writer.append(envelope).unwrap();
            }
        })
    };

    first.join().unwrap();
    second.join().unwrap();

    let rows = store.with_connection(|conn| {
        let mut statement = conn
            .prepare("SELECT event_id FROM events ORDER BY event_id ASC")
            .unwrap();
        statement
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect::<Vec<_>>()
    });

    assert_eq!(rows.len(), 20);
    assert!(rows.windows(2).all(|window| window[0] < window[1]));
}

#[test]
fn sync_flush_makes_rows_visible_to_new_connection() {
    let path = temp_db_path("sync-flush");
    let store = Arc::new(EventStore::open(&path).expect("event store opens"));
    let writer = EventWriter::new(store, "workspace-main".to_string(), 4096)
        .with_flush_policy(FlushPolicy::Sync);

    let event_id = writer
        .append(base_envelope(assistant_task_payload()))
        .unwrap();
    let conn = Connection::open(&path).expect("second connection opens");
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE event_uuid = ?1",
            [event_id.ulid.as_str()],
            |row| row.get(0),
        )
        .expect("query succeeds");

    assert_eq!(count, 1);
    cleanup_db_files(&path);
}

#[test]
fn hash_payload_is_deterministic_for_typed_payloads() {
    let payload = assistant_task_payload();

    let left = hash_payload(&payload).unwrap();
    let right = hash_payload(&payload).unwrap();

    assert_eq!(left, right);
    assert!(left.to_string().starts_with("sha256:"));
}

fn base_envelope(payload: EventPayload) -> PartialEnvelope {
    PartialEnvelope {
        workspace_id: None,
        branch: BranchRef {
            name: "main".to_string(),
        },
        session_id: SessionId {
            value: "session-main".to_string(),
        },
        task_id: None,
        actor: Actor::Assistant {
            model: "gpt-5.5".to_string(),
        },
        kind: payload.kind(),
        references: Vec::new(),
        summary: CompactSummary::new("event summary").unwrap(),
        payload,
    }
}

fn assistant_task_payload() -> EventPayload {
    EventPayload::AssistantTaskStarted(AssistantTaskStartedPayload {
        context_handle_id: None,
        seed_event_ids: Vec::new(),
        initial_memory_ids: Vec::new(),
        objective: "measure writer".to_string(),
    })
}

fn large_payload_envelope(session_id: &str) -> PartialEnvelope {
    let mut envelope = base_envelope(EventPayload::AssistantTaskStarted(
        AssistantTaskStartedPayload {
            context_handle_id: None,
            seed_event_ids: Vec::new(),
            initial_memory_ids: Vec::new(),
            objective: "x".repeat(5000),
        },
    ));
    envelope.session_id = SessionId {
        value: session_id.to_string(),
    };
    envelope
}

fn payload_row_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM event_payloads", [], |row| row.get(0))
        .unwrap()
}

fn temp_db_path(test_name: &str) -> PathBuf {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    std::env::temp_dir().join(format!(
        "lattice-event-writer-{test_name}-{}-{micros}.db",
        std::process::id()
    ))
}

fn cleanup_db_files(path: &PathBuf) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("db-wal"));
    let _ = fs::remove_file(path.with_extension("db-shm"));
}
