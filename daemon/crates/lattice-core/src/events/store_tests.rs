use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};

use super::{EventStore, InsertEnvelopeRow};

#[test]
fn schema_applies_on_fresh_database() {
    let store = EventStore::open_in_memory().expect("event store opens");
    let row_id = store
        .insert_envelope_row(&inline_event("event-1", "task-a", 10))
        .expect("event row inserts");

    assert_eq!(row_id, 1);
    assert_eq!(store.query_events_by_task("task-a", 10).unwrap().len(), 1);
}

#[test]
fn exact_identity_lookup_is_not_hidden_by_more_than_ten_thousand_events() {
    let store = EventStore::open_in_memory().expect("event store opens");
    store
        .insert_envelope_row(&inline_event("target-event", "task-a", 1))
        .expect("target inserts");
    for ordinal in 0..10_001 {
        let id = format!("noise-{ordinal:05}");
        store
            .insert_envelope_row(&inline_event(&id, "noise", ordinal + 2))
            .expect("noise inserts");
    }

    let target = store
        .query_event_by_identity("workspace-main", "target-event")
        .expect("exact lookup succeeds")
        .expect("target remains addressable");
    assert_eq!(target.event_id, 1);
    assert!(store
        .query_event_by_identity("workspace-foreign", "target-event")
        .expect("foreign lookup executes")
        .is_none());
}

#[test]
fn migration_is_idempotent_on_reopen() {
    let path = temp_db_path("idempotent");
    let first = EventStore::open(&path).expect("first open migrates");
    first
        .insert_envelope_row(&inline_event("event-1", "task-a", 10))
        .expect("event row inserts");
    drop(first);

    let second = EventStore::open(&path).expect("second open is idempotent");
    let rows = second.query_events_by_session("session-main", 10).unwrap();

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event_uuid, "event-1");
    cleanup_db_files(&path);
}

#[test]
fn newer_event_database_is_rejected_on_open() {
    let path = temp_db_path("future-version");
    let conn = Connection::open(&path).expect("database creates");
    conn.execute_batch(
        "CREATE TABLE event_schema_version (
           version INTEGER PRIMARY KEY,
           applied_ts_unix_micros INTEGER NOT NULL
         );
         INSERT INTO event_schema_version VALUES (5, 1);",
    )
    .expect("future marker writes");
    drop(conn);

    let error = match EventStore::open(&path) {
        Ok(_) => panic!("older binary must reject future schema"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("newer than this binary"));
    cleanup_db_files(&path);
}

#[test]
fn events_table_rejects_update_and_delete() {
    let store = EventStore::open_in_memory().expect("event store opens");
    store
        .insert_envelope_row(&inline_event("event-1", "task-a", 10))
        .expect("event row inserts");

    let (update, delete) = store.with_connection(|conn| {
        let update = conn.execute(
            "UPDATE events SET summary = ?1 WHERE event_uuid = ?2",
            params!["changed", "event-1"],
        );
        let delete = conn.execute(
            "DELETE FROM events WHERE event_uuid = ?1",
            params!["event-1"],
        );
        (update, delete)
    });

    assert!(update.unwrap_err().to_string().contains("append-only"));
    assert!(delete.unwrap_err().to_string().contains("append-only"));
}

#[test]
fn payload_spillover_row_ids_round_trip() {
    let store = EventStore::open_in_memory().expect("event store opens");
    let payload_hash = vec![9, 9, 9, 9];
    let spill_id = store
        .insert_or_get_payload(&payload_hash, b"large payload", 100)
        .expect("payload inserts");
    let mut row = spilled_event("event-1", "task-a", 10, spill_id);
    row.payload_hash = payload_hash.clone();
    store.insert_envelope_row(&row).expect("event row inserts");

    let event = store.query_events_by_task("task-a", 10).unwrap().remove(0);
    let payload = store.get_payload_row(spill_id).unwrap().unwrap();

    assert_eq!(event.payload_spill_id, Some(spill_id));
    assert_eq!(payload.payload_hash, payload_hash);
    assert_eq!(payload.bytes, b"large payload");
    assert_eq!(payload.bytes_len, 13);
}

#[test]
fn content_addressed_payloads_dedupe_identical_bytes() {
    let store = EventStore::open_in_memory().expect("event store opens");
    let first = store
        .insert_or_get_payload(&[1, 2, 3, 4], b"same payload", 100)
        .expect("first payload inserts");
    let second = store
        .insert_or_get_payload(&[1, 2, 3, 4], b"same payload", 200)
        .expect("duplicate payload returns row");

    assert_eq!(first, second);
}

#[test]
fn scoped_queries_are_bounded_and_ordered() {
    let store = EventStore::open_in_memory().expect("event store opens");
    store
        .insert_envelope_row(&inline_event("event-2", "task-a", 20))
        .expect("second event inserts");
    store
        .insert_envelope_row(&inline_event("event-1", "task-a", 10))
        .expect("first event inserts");
    store
        .insert_envelope_row(&inline_event("event-3", "task-b", 30))
        .expect("third event inserts");

    let task_rows = store.query_events_by_task("task-a", 1).unwrap();
    let session_rows = store.query_events_by_session("session-main", 10).unwrap();
    let branch_rows = store
        .query_events_by_workspace_branch("workspace-main", "main", 10)
        .unwrap();

    assert_eq!(task_rows.len(), 1);
    assert_eq!(task_rows[0].event_uuid, "event-1");
    assert_eq!(session_rows.len(), 3);
    assert_eq!(branch_rows.len(), 3);
}

#[test]
fn required_indexes_exist() {
    let store = EventStore::open_in_memory().expect("event store opens");
    store.with_connection(|conn| {
        assert_index_exists(conn, "events", "idx_events_task");
        assert_index_exists(conn, "events", "idx_events_session");
        assert_index_exists(conn, "events", "idx_events_workspace_branch");
        assert_index_exists(conn, "events", "idx_events_kind_ts");
        assert_index_exists(conn, "event_payloads", "sqlite_autoindex_event_payloads_1");
    });
}

fn inline_event(event_uuid: &str, task_id: &str, ts_unix_micros: i64) -> InsertEnvelopeRow {
    InsertEnvelopeRow {
        event_uuid: event_uuid.to_string(),
        workspace_id: "workspace-main".to_string(),
        branch: "main".to_string(),
        session_id: "session-main".to_string(),
        task_id: Some(task_id.to_string()),
        actor_kind: "assistant".to_string(),
        actor_detail: Some("gpt-5".to_string()),
        kind: "assistant_task_started".to_string(),
        ts_unix_micros,
        payload_hash: vec![1, 2, 3, event_uuid.as_bytes()[event_uuid.len() - 1]],
        summary: format!("summary for {event_uuid}"),
        payload_inline: Some(br#"{"kind":"assistant_task_started"}"#.to_vec()),
        payload_spill_id: None,
        references_json: "[]".to_string(),
        schema_version: 1,
    }
}

fn spilled_event(
    event_uuid: &str,
    task_id: &str,
    ts_unix_micros: i64,
    spill_id: i64,
) -> InsertEnvelopeRow {
    let mut row = inline_event(event_uuid, task_id, ts_unix_micros);
    row.payload_inline = None;
    row.payload_spill_id = Some(spill_id);
    row
}

fn assert_index_exists(conn: &Connection, table: &str, index_name: &str) {
    let mut statement = conn
        .prepare("SELECT name FROM pragma_index_list(?1) WHERE name = ?2")
        .expect("index lookup prepares");
    let exists = statement
        .exists(params![table, index_name])
        .expect("index lookup executes");
    assert!(exists, "missing index {index_name}");
}

fn temp_db_path(test_name: &str) -> PathBuf {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    std::env::temp_dir().join(format!(
        "lattice-events-{test_name}-{}-{micros}.db",
        std::process::id()
    ))
}

fn cleanup_db_files(path: &PathBuf) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("db-wal"));
    let _ = fs::remove_file(path.with_extension("db-shm"));
}

#[test]
fn compaction_reclaims_only_last_payload_reference_and_allows_reappend() {
    let store = EventStore::open_in_memory().unwrap();
    let mut first = inline_event("spill-1", "task-a", 10);
    first.payload_inline = Some(vec![42; 8192]);
    let mut second = first.clone();
    second.event_uuid = "spill-2".into();
    let first_id = store
        .insert_envelope_with_payload(first.clone(), 32)
        .unwrap();
    let second_id = store.insert_envelope_with_payload(second, 32).unwrap();
    let spill = store.query_events_by_task("task-a", 10).unwrap()[0]
        .payload_spill_id
        .unwrap();
    assert_eq!(store.truncate_through(first_id).unwrap(), 1);
    assert!(store.get_payload_row(spill).unwrap().is_some());
    assert_eq!(store.truncate_through(second_id).unwrap(), 1);
    assert!(store.get_payload_row(spill).unwrap().is_none());
    first.event_uuid = "spill-3".into();
    store.insert_envelope_with_payload(first, 32).unwrap();
    let replacement = store.query_events_by_task("task-a", 10).unwrap()[0]
        .payload_spill_id
        .unwrap();
    assert_eq!(
        store.get_payload_row(replacement).unwrap().unwrap().bytes,
        vec![42; 8192]
    );
}

#[test]
fn failed_atomic_append_does_not_leave_payload_and_failed_compaction_rolls_back() {
    let store = EventStore::open_in_memory().unwrap();
    let first = inline_event("same-id", "task-a", 10);
    store
        .insert_envelope_with_payload(first.clone(), 4096)
        .unwrap();
    let mut duplicate = first;
    duplicate.payload_hash = vec![7; 32];
    duplicate.payload_inline = Some(vec![7; 8192]);
    assert!(store.insert_envelope_with_payload(duplicate, 32).is_err());
    store.with_connection(|conn| {
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM event_payloads", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        conn.execute_batch("CREATE TRIGGER reject_compaction AFTER DELETE ON events BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
    });
    assert!(store.truncate_through(i64::MAX).is_err());
    assert_eq!(store.query_events_by_task("task-a", 10).unwrap().len(), 1);
    store.with_connection(|conn| {
        assert_eq!(
            conn.query_row(
                "SELECT allow_delete FROM event_compaction_control",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    });
}

#[test]
fn physical_reclamation_is_bounded_and_reader_backlog_is_visible() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let store = EventStore::open(&path).unwrap();
    let mut event = inline_event("large", "task-a", 10);
    event.payload_inline = Some(vec![42; 1024 * 1024]);
    let id = store.insert_envelope_with_payload(event, 32).unwrap();
    store.checkpoint_wal().unwrap();
    let reader = Connection::open(&path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT * FROM events;")
        .unwrap();
    store.truncate_through(id).unwrap();
    let (released, backlog) = store.reclaim_free_pages(16).unwrap();
    assert!(released > 0 && released <= 16);
    assert!(backlog > 0);
    reader.execute_batch("ROLLBACK").unwrap();
    let before = fs::metadata(&path).unwrap().len();
    let (released, backlog) = store.reclaim_free_pages(4096).unwrap();
    assert!(released > 0);
    assert_eq!(backlog, 0);
    assert!(fs::metadata(&path).unwrap().len() < before);
    assert!(store.reclaim_free_pages(0).is_err());
    assert!(store.reclaim_free_pages(4097).is_err());
}

#[test]
fn concurrent_atomic_append_and_compaction_preserve_every_surviving_payload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let store = EventStore::open(&path).unwrap();
    let writer = EventStore::open(&path).unwrap();
    let thread = std::thread::spawn(move || {
        for i in 0..100 {
            let mut row = inline_event(&format!("concurrent-{i}"), "task-a", i);
            row.payload_hash = vec![42; 32];
            row.payload_inline = Some(vec![42; 8192]);
            writer.insert_envelope_with_payload(row, 32).unwrap();
        }
    });
    for id in 0..100 {
        store.truncate_through(id).unwrap();
    }
    thread.join().unwrap();
    for row in store.query_events_by_task("task-a", 1000).unwrap() {
        assert_eq!(
            store
                .get_payload_row(row.payload_spill_id.unwrap())
                .unwrap()
                .unwrap()
                .bytes
                .len(),
            8192
        );
    }
}

#[test]
fn atomic_append_rejects_payload_hash_collision() {
    let store = EventStore::open_in_memory().unwrap();
    let mut row = inline_event("collision-a", "task-a", 1);
    row.payload_inline = Some(vec![1; 100]);
    store.insert_envelope_with_payload(row.clone(), 1).unwrap();
    row.event_uuid = "collision-b".into();
    row.payload_inline = Some(vec![2; 100]);
    assert!(store
        .insert_envelope_with_payload(row, 1)
        .unwrap_err()
        .to_string()
        .contains("different bytes"));
    assert_eq!(store.query_events_by_task("task-a", 10).unwrap().len(), 1);
}
