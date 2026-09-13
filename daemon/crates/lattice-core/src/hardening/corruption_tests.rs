//! Phase 11 hardening tests for corrupted event handling.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Storage Design`: derived state can be rebuilt from source files and events.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `### 3. Event Log`: event payloads carry stable references and payload hashes.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 2: Event Log Substrate`: corruption handling is observable.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 11: Hardening`: corrupted-event handling is regression-tested.

use std::fs;

use crate::events::{Bootstrap, BootstrapError, EventPayload, EventQueryError, Snapshot};

use super::support::{
    capture_logs, event_envelope, event_envelope_with_refs, missing_memory_ref, missing_symbol_ref,
    overwrite_snapshot_version, sample_graph, HardeningFixture,
};

#[test]
fn test_corrupted_payload_quarantines_row_without_panic() {
    let fixture = HardeningFixture::new();
    let writer = fixture.writer(1);
    writer
        .append(event_envelope("task-corrupt-payload", "session-a", "bad"))
        .expect("spilled event appends");
    writer
        .append(event_envelope("task-corrupt-payload", "session-b", "good"))
        .expect("surviving event appends");
    let bytes = b"not-json";
    fixture.overwrite_spill_bytes_preserving_hash(
        fixture.spill_row_id_for("task-corrupt-payload"),
        bytes,
    );

    let (streamed, logs) = capture_logs(|| {
        fixture
            .reopen_reader()
            .stream(fixture.query_task("task-corrupt-payload"))
            .collect::<Vec<_>>()
    });

    assert!(matches!(
        streamed.first(),
        Some(Err(EventQueryError::PayloadCorruption { .. }))
    ));
    assert_eq!(surviving_objectives(streamed), vec!["good"]);
    assert!(logs.contains("event payload corruption: hash mismatch"));
}

#[test]
fn test_mismatched_payload_hash_logs_corruption_and_continues() {
    let fixture = HardeningFixture::new();
    let writer = fixture.writer(1);
    writer
        .append(event_envelope("task-hash-mismatch", "session-a", "bad"))
        .expect("spilled event appends");
    writer
        .append(event_envelope("task-hash-mismatch", "session-b", "good"))
        .expect("surviving event appends");
    let row_id = fixture.spill_row_id_for("task-hash-mismatch");
    fixture.overwrite_spill_hash(row_id, &[7_u8; 32]);

    let (streamed, logs) = capture_logs(|| {
        fixture
            .reopen_reader()
            .stream(fixture.query_task("task-hash-mismatch"))
            .collect::<Vec<_>>()
    });

    assert!(matches!(
        streamed.first(),
        Some(Err(EventQueryError::Storage(_)))
    ));
    assert_eq!(surviving_objectives(streamed), vec!["good"]);
    assert!(logs.contains("spill row hash does not match envelope hash"));
}

#[test]
fn test_invalid_event_kind_returns_clear_error_without_panic() {
    let fixture = HardeningFixture::new();
    fixture
        .writer(4096)
        .append(event_envelope("task-invalid-kind", "session-a", "bad kind"))
        .expect("event appends");
    fixture.set_event_kind("task-invalid-kind", "invalid_event_kind");

    let (error, logs) = capture_logs(|| {
        fixture
            .reopen_reader()
            .execute(fixture.query_task("task-invalid-kind"))
            .expect_err("invalid event kind rejects row")
    });

    assert!(error.to_string().contains("unknown event kind"));
    assert!(logs.contains("event payload corruption: invalid event kind"));
}

#[test]
fn test_invalid_stable_reference_emits_dangling_reference_signal() {
    let fixture = HardeningFixture::new();
    fixture
        .writer(4096)
        .append(event_envelope("task-dangling-ref", "session-a", "seed"))
        .expect("seed event appends");
    let snapshot_path = fixture.snapshot_path("snapshot-1-1.bin");
    Snapshot::write(&snapshot_path, &sample_graph(), 1).expect("snapshot writes");
    fixture
        .writer(4096)
        .append(event_envelope_with_refs(
            "task-dangling-ref",
            "session-b",
            "dangling",
            vec![
                missing_memory_ref("missing-memory"),
                missing_symbol_ref("src/missing.rs", "missing"),
            ],
        ))
        .expect("dangling ref event appends");

    let (result, logs) = capture_logs(|| Bootstrap::load(&snapshot_path, &fixture.reopen_store()));
    let error = result.expect_err("dangling reference rejects bootstrap");

    assert!(matches!(error, BootstrapError::ReferenceUnresolved { .. }));
    assert!(logs.contains("event_type=\"dangling-reference\""));
}

#[test]
fn test_truncated_event_log_file_reports_recovery_or_sqlite_error_without_panic() {
    let fixture = HardeningFixture::new();
    fixture
        .writer(4096)
        .append(event_envelope("task-truncated-db", "session-a", "before"))
        .expect("event appends");
    truncate_database_file(&fixture.db_path);

    let (opened, logs) = capture_logs(|| fixture.reopen_store());
    let result = opened.query_events_after_row_id(0, 100);

    match result {
        Ok(rows) => assert!(
            rows.len() <= 1,
            "truncated database recovery must not invent event rows"
        ),
        Err(error) => assert!(
            error.to_string().contains("SQLite"),
            "truncated database should surface SQLite recovery error, got {error}"
        ),
    }
    assert!(
        logs.contains("configured event store SQLite connection") || logs.contains("recovery"),
        "opening truncated event store should emit a recovery-relevant log, got {logs}"
    );
}

#[test]
fn test_snapshot_version_mismatch_refuses_bootstrap_with_clear_error() {
    let fixture = HardeningFixture::new();
    fixture
        .writer(4096)
        .append(event_envelope("task-version", "session-a", "seed"))
        .expect("event appends");
    let snapshot_path = fixture.snapshot_path("snapshot-1-1.bin");
    Snapshot::write(&snapshot_path, &sample_graph(), 1).expect("snapshot writes");
    overwrite_snapshot_version(&snapshot_path, 3);

    let (result, logs) = capture_logs(|| Bootstrap::load(&snapshot_path, &fixture.reopen_store()));
    let error = result.expect_err("snapshot version mismatch rejects bootstrap");

    assert!(matches!(
        error,
        BootstrapError::FormatVersionUnsupported { found: 3, max: 2 }
    ));
    assert!(logs.contains("snapshot format too new"));
}

fn surviving_objectives(
    streamed: Vec<Result<crate::events::EventEnvelope, EventQueryError>>,
) -> Vec<String> {
    streamed
        .into_iter()
        .filter_map(Result::ok)
        .map(|event| match event.payload {
            EventPayload::AssistantTaskStarted(payload) => payload.objective,
            other => panic!("expected assistant task payload, got {other:?}"),
        })
        .collect()
}

fn truncate_database_file(path: &std::path::Path) {
    let bytes = fs::read(path).expect("event database reads");
    let truncated_len = bytes.len().saturating_sub(64).max(bytes.len() / 2);
    fs::write(path, &bytes[..truncated_len]).expect("event database truncates");
}
