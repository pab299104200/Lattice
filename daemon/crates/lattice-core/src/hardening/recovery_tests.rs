//! Phase 11 hardening tests for recovery paths.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Storage Design`: snapshots are versioned and independently readable.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `### 3. Event Log`: the event log is append-only and replayable.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 2: Event Log Substrate`: failure paths emit useful events.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 11: Hardening`: recovery behavior is regression-tested.

use crate::events::{Bootstrap, EventPayload, EventQueryError, QueryOrder, Snapshot};

use super::support::{
    capture_logs, event_envelope, sample_graph, sample_memory_store, truncate_file,
    HardeningFixture,
};

#[test]
fn test_replay_from_snapshot_plus_tail_reconstructs_state() {
    let fixture = HardeningFixture::new();
    fixture
        .writer(4096)
        .append(event_envelope("task-snapshot-tail", "session-a", "before"))
        .expect("pre-snapshot event appends");
    let graph = sample_graph();
    let memory = sample_memory_store();
    let snapshot_path = fixture.snapshot_path("snapshot-1-1.bin");
    Snapshot::write(&snapshot_path, &graph, &memory, 1).expect("snapshot writes");
    fixture
        .writer(4096)
        .append(event_envelope("task-snapshot-tail", "session-b", "after"))
        .expect("tail event appends");

    let (bootstrapped, logs) =
        capture_logs(|| Bootstrap::load(&snapshot_path, &fixture.reopen_store()));
    let bootstrapped = bootstrapped.expect("snapshot bootstrap succeeds");
    let expected = Snapshot::capture(&graph, &memory, 2).expect("full state captures");

    assert_eq!(bootstrapped.replayed_event_rows, 1);
    assert_eq!(bootstrapped.snapshot.graph_state, expected.graph_state);
    assert_eq!(bootstrapped.snapshot.memory_state, expected.memory_state);
    assert!(logs.contains("event log recovery completed from snapshot plus tail"));
}

#[test]
fn test_replay_from_event_log_only_survives_without_snapshot() {
    let fixture = HardeningFixture::new();
    let writer = fixture.writer(4096);
    writer
        .append(event_envelope("task-log-only", "session-a", "one"))
        .expect("first event appends");
    writer
        .append(event_envelope("task-log-only", "session-b", "two"))
        .expect("second event appends");

    let events = fixture
        .reopen_reader()
        .execute(fixture.query_task("task-log-only"))
        .expect("full event-log replay query succeeds without snapshot");

    assert_eq!(objectives(&events), vec!["one", "two"]);
}

#[test]
fn test_partial_snapshot_falls_back_to_full_replay_without_data_loss() {
    let fixture = HardeningFixture::new();
    let writer = fixture.writer(4096);
    writer
        .append(event_envelope(
            "task-partial-snapshot",
            "session-a",
            "before",
        ))
        .expect("first event appends");
    let valid = fixture.snapshot_path("snapshot-1-1.bin");
    Snapshot::write(&valid, &sample_graph(), &sample_memory_store(), 1)
        .expect("valid snapshot writes");
    writer
        .append(event_envelope(
            "task-partial-snapshot",
            "session-b",
            "after",
        ))
        .expect("tail event appends");
    let corrupt = fixture.snapshot_path("snapshot-2-2.bin");
    Snapshot::write(&corrupt, &sample_graph(), &sample_memory_store(), 2)
        .expect("corruptible snapshot writes");
    truncate_file(&corrupt);

    let (bootstrapped, logs) = capture_logs(|| Bootstrap::load(&corrupt, &fixture.reopen_store()));
    let bootstrapped = bootstrapped.expect("bootstrap falls back to valid snapshot");
    let full_events = fixture
        .reopen_store()
        .query_events_after_row_id(0, 100)
        .expect("full event log remains readable");

    assert_eq!(bootstrapped.snapshot.up_to_event_id, 1);
    assert_eq!(bootstrapped.replayed_event_rows, full_events.len() - 1);
    assert!(logs.contains("ignoring corrupt snapshot candidate"));
}

#[test]
fn test_partial_event_write_emits_recovery_and_stream_continues() {
    let fixture = HardeningFixture::new();
    let writer = fixture.writer(1);
    writer
        .append(event_envelope(
            "task-partial-event",
            "session-a",
            "missing payload",
        ))
        .expect("spilled event appends");
    let spill_id = fixture.spill_row_id_for("task-partial-event");
    fixture.delete_spill_row(spill_id);
    writer
        .append(event_envelope(
            "task-partial-event",
            "session-b",
            "survivor",
        ))
        .expect("surviving event appends");

    let (streamed, logs) = capture_logs(|| {
        fixture
            .reopen_reader()
            .stream(
                fixture
                    .query_task("task-partial-event")
                    .order(QueryOrder::OldestFirst),
            )
            .collect::<Vec<_>>()
    });

    assert!(matches!(
        streamed.first(),
        Some(Err(EventQueryError::Storage(_)))
    ));
    let survivor = streamed
        .into_iter()
        .filter_map(Result::ok)
        .next()
        .expect("stream continues after quarantined dangling event");
    assert_eq!(objective(&survivor.payload), "survivor");
    assert!(logs.contains("event log recovery: spilled payload row is missing"));
}

#[test]
fn test_wal_restart_reopens_cleanly_without_losing_committed_events() {
    let fixture = HardeningFixture::new();
    {
        let writer = fixture.writer(4096);
        writer
            .append(event_envelope("task-wal", "session-a", "before crash"))
            .expect("wal event appends");
    }

    let store = fixture.reopen_store();
    store.checkpoint_wal().expect("wal checkpoint succeeds");
    let events = fixture
        .reopen_reader()
        .execute(fixture.query_task("task-wal"))
        .expect("event log remains queryable after restart");

    assert_eq!(objectives(&events), vec!["before crash"]);
}

fn objectives(events: &[crate::events::EventEnvelope]) -> Vec<&str> {
    events
        .iter()
        .map(|event| objective(&event.payload))
        .collect()
}

fn objective(payload: &EventPayload) -> &str {
    match payload {
        EventPayload::AssistantTaskStarted(payload) => payload.objective.as_str(),
        other => panic!("expected assistant task payload, got {other:?}"),
    }
}
