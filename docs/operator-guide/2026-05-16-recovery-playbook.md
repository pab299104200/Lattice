# Recovery Playbook

This playbook covers recovery, replay, rollback, and incident communication for the cognitive workspace successor. It is the failure-path companion to the daily [Operator Runbook](./2026-05-16-runbook.md#daily-operations). The governing contracts are [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design), [### 3. Event Log](../plans/2026-05-16-cognitive-workspace-fork-plan.md#3-event-log), and [## Phase 11: Hardening](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-11-hardening), plus the snapshot and rollback details in [Storage Migration Policy](../architecture/2026-05-16-storage-migration-policy.md#compaction-snapshot-policy).

## D3a hook-session capture recovery

D3a capture is repository-local and exact-checkout-bound. The adapter admits
only an opaque host session ID, an allowlisted structured edit path, and a
close marker. It never opens `transcript_path` and never forwards prompts, raw
tool input/output, commands, terminal output, environment values, or caller
scope/repository claims. A successful authenticated close reduces the bounded
journal into deterministic repository-store memories; a failed Stop is a
truthful best-effort failure and does not block agent shutdown.

If capture is missing from task recall, first check that the host supplied the
same session ID to `SessionStart`, the edit hook ran inside the intended
checkout, and `lattice status --scope index --json` reports a healthy daemon.
Then inspect the daemon's content-free hook health/log outcomes for
`binding_missing`, `checkout_mismatch`, `daemon_unavailable`, or
`registry_unavailable`. Do not inspect or recover a transcript path: it is not
part of the capture contract. Re-run the next session after the daemon and
workspace are healthy; a later Stop cannot silently rebind or infer the missed
session.

Protected state is bounded and retry-safe. The client stores capability
metadata and sanitized pending deliveries under the user's private
`$XDG_STATE_HOME/lattice/hook-bindings` (or
`~/.local/state/lattice/hook-bindings`) directory. It caps pending state at 256
deliveries/256 KiB, expires pending entries after 24 hours, and retains a
close retry for five minutes. Delivery IDs are reused on retry; never copy or
edit capability files. The daemon binding registry uses 30-minute idle and
12-hour absolute lifetimes, retains receipt/audit metadata for seven days, and
the repository `.lattice/hook-capture.db` journal prunes rows older than seven
days. These stores contain normalized facts or opaque metadata only—never raw
host envelopes.

For suspected protected-state corruption, stop the daemon, preserve the exact
state directory with ownership/mode metadata, and move only the affected
binding record or registry after confirming it is not a symlink. Do not delete
`memories.db` or `.lattice/hook-capture.db` as a first response. A fresh
authenticated `SessionStart` creates a new binding; an old Stop cannot reopen
one. If a capture journal or memory transaction is unavailable, the close is
retried by delivery ID and either commits once or reports a typed failure.

Capture deletion is repository-scoped and transactional. The memory-store
deletion path removes capture evidence, links, idempotency rows, and queued
capture work, while retaining derived durable records as content-free
tombstone provenance and preserving required audit history. There is no
transcript deletion or transcript recovery step. Use the repository-scoped
session-capture deletion workflow when exposed by the operator tooling; never
delete by a caller-supplied organization or sibling-checkout identity.

Deterministic D3a capture makes no LLM call. The separate consolidation tools
are explicit and proposal-only; they do not run as a Stop side effect. A
capture incident therefore requires no provider/key investigation unless an
operator explicitly invoked consolidation.

## Recovery procedures

### Derived graph corruption

`.lattice/graph.db` is a disposable current-workspace index, not historical or audit authority. On every file-backed open, Lattice runs SQLite `quick_check`. Confirmed `SQLITE_CORRUPT`, `SQLITE_NOTADB`, or a non-`ok` integrity result causes Lattice to remove exactly `graph.db`, `graph.db-wal`, and `graph.db-shm`, create a clean graph store, and rebuild it from workspace files. It does not remove `memories.db`, `vectors.db`, `events.db`, snapshots, or context handles. Symlinked graph database paths and ordinary permission/I/O failures are rejected rather than deleted.

Run `lattice status --scope index --json` after restart. `graph_storage_state` is `healthy` for an ordinary open, `rebuilt_corrupt` when startup replaced a corrupt graph, and `unhealthy` if status cannot query the store. A rebuilt store may report indexing until the source scan finishes; that is recovery progress, not an empty successful graph.

Do not apply the graph reset policy to memory, vectors, event history, or snapshots. Those stores can carry durable evidence and follow the quarantine/replay procedures below.

1. Daemon will not start.
Check binary deployment first with the [runbook deploy procedure](./2026-05-16-runbook.md#deploy-sequence), then inspect daemon stderr or service logs for startup failures. If the error references SQLite open, snapshot load, or migration order, continue to [Rollback](#rollback) or [Replay from snapshot](#replay-from-snapshot).

2. Daemon starts but indexing fails.
Open `indexingHealthView`, identify the failing parser or pipeline section, then inspect daemon logs for watcher, parser, or SQLite errors. If the graph is partially usable, quarantine the current state and continue with [Quarantine and forensics](#quarantine-and-forensics) before replay.

3. Indexing succeeds but queries return empty.
Check `workspaceGraphHealthView`, FTS row counts, vector freshness, and event-log ingestion. If graph counts are present but retrieval is empty, continue with [Replay from event log only](#replay-from-event-log-only) or [Replay from snapshot](#replay-from-snapshot), depending on snapshot availability.

4. Queries return stale data.
Inspect stale-edge, orphan-symbol, and stale-memory signals first. If a full rescan does not clear the stale state, continue with [Replay from snapshot](#replay-from-snapshot) to rebuild derived state from the latest valid snapshot and post-snapshot tail events.

5. Memory state is contradictory at scale.
Review the contradiction queue, stale-memory view, and recent consolidation jobs. If contradictions are localized, resolve them through normal review. If contradictions are systemic across many recent memories, quarantine the current stores and continue with [Replay from snapshot](#replay-from-snapshot) or [Replay from event log only](#replay-from-event-log-only).

6. Snapshot file is missing or corrupted.
If a newer valid sibling snapshot exists, use it. If no valid snapshot exists, continue with [Replay from event log only](#replay-from-event-log-only). Use [Corruption recovery](#corruption-recovery) for the exact failure class.

7. A context request times out.
Run `lattice status --scope index --json`. Status should still return while context work is active because query traversal runs from an immutable graph snapshot outside the live engine lock. A workflow response with `reason: query_capacity` means both bounded CPU query slots are still occupied; retry after one completes. Repeated status timeouts indicate a daemon or transport fault, not normal query backpressure: capture the daemon PID, CPU usage, and logs, restart the daemon, and verify status plus one representative context request. Do not delete any database to address query saturation.

8. Indexing remains CPU- or memory-heavy during a large change storm.
Inspect `index_status.index_work`. `active_jobs` must not exceed `capacity`; the default capacity is one. A non-zero `queued_jobs` value is bounded backpressure, while a steadily growing queue or repeated jobs for the same workspace indicates a scheduling defect. `graph_storage_state: "busy"` is a transient snapshot publication state and must not make status block. Confirm `LATTICE_MAX_CONCURRENT_INDEX_JOBS` was not raised without a measured memory budget, and confirm multi-root prewarming was not explicitly enabled. Restarting is appropriate after capturing PID, RSS, CPU, status, and lifecycle logs, but deleting `graph.db` does not fix rebuild amplification and is not a resource-remediation step.

## Replay from snapshot

This is the binding recovery path proved by `test_replay_from_snapshot_plus_tail_reconstructs_state` in [daemon/crates/lattice-core/src/hardening/recovery_tests.rs](../../daemon/crates/lattice-core/src/hardening/recovery_tests.rs:21). It also relies on the snapshot invariants in [## Storage Design](../plans/2026-05-16-cognitive-workspace-fork-plan.md#storage-design) and the append-only replay contract in [### 3. Event Log](../plans/2026-05-16-cognitive-workspace-fork-plan.md#3-event-log).

1. Stop the daemon with `pkill -f lattice`.
2. Resolve the newest snapshot payload under `.lattice/snapshots/`; if your operational wrapper publishes a convenience alias such as `.lattice/snapshots/<latest>.snap`, use that alias, otherwise use the current implementation path `snapshot-<event_row_id>-<unix_micros>.bin`.
3. Verify the snapshot file exists and record its absolute path, size, and mtime.
4. Verify the 64-byte header is readable and that the format version is supported before reading the body.
5. Verify the snapshot checksum with the stored content hash before attempting bootstrap.
6. Move the current `graph.db` and `memories.db` into a timestamped quarantine directory so recovery does not destroy forensic evidence.
7. Start the daemon using the installed/current Lattice launcher in snapshot-bootstrap mode with the resolved snapshot path; when your launcher exposes the flag directly, the operator-facing form is `lattice --bootstrap-from-snapshot <path>`, and the underlying code path is `Bootstrap::load(...)` in [daemon/crates/lattice-core/src/events/compaction.rs](../../daemon/crates/lattice-core/src/events/compaction.rs:284).
8. Confirm bootstrap evidence; prefer a dedicated `SnapshotBootstrap` event if your deployment wrapper emits one, but the checked codebase currently proves success with the log line `event log recovery completed from snapshot plus tail`.
9. Run a scoped query and confirm it returns the expected pre-snapshot state plus post-snapshot tail events, which is the behavior asserted by `test_replay_from_snapshot_plus_tail_reconstructs_state`.
10. Record the post-recovery audit results: snapshot path, bootstrap evidence, replayed tail count, query spot checks, and any remaining stale or contradiction findings.

Suggested integrity checks:

```bash
SNAPSHOT=".lattice/snapshots/snapshot-<event_row_id>-<unix_micros>.bin"
test -f "$SNAPSHOT"
stat "$SNAPSHOT"
xxd -l 64 "$SNAPSHOT"
sha256sum "$SNAPSHOT"
```

Post-recovery audit checklist:

1. Confirm `pgrep -fa lattice` shows only the recovered daemon.
2. Confirm `indexingHealthView` no longer shows the pre-recovery failure.
3. Confirm `workspaceGraphHealthView` no longer shows the triggering broken-reference or stale-edge symptom, or record any remaining rows.
4. Confirm one representative workflow query, one memory query, and one event-trace query all succeed.
5. Confirm the quarantine copy is preserved until the incident is closed.

## Replay from event log only

Use this path when no valid snapshot is available. It is proved by `test_replay_from_event_log_only_survives_without_snapshot` in [daemon/crates/lattice-core/src/hardening/recovery_tests.rs](../../daemon/crates/lattice-core/src/hardening/recovery_tests.rs:49).

1. Stop the daemon with `pkill -f lattice`.
2. Quarantine the current `graph.db` and `memories.db`.
3. Preserve `events.db` in place because it is now the only recovery source of truth under [### 3. Event Log](../plans/2026-05-16-cognitive-workspace-fork-plan.md#3-event-log).
4. Start the daemon without a snapshot so it rebuilds from the append-only event history.
5. Confirm the daemon logs a clean event-store open and does not emit snapshot-load errors.
6. Run a scoped event query and confirm the recovered sequence matches the expected task/session history.
7. Re-run the same spot checks used in the snapshot procedure and record the results.

## Quarantine and forensics

1. Create a quarantine directory under `.lattice/quarantine/<timestamp>/`.
2. Move corrupted or superseded `graph.db`, `memories.db`, snapshot files, and any extracted diagnostic artifacts into that directory.
3. Record the incident id, workspace path, daemon version, and trigger symptom in a `README.md` inside the quarantine directory.
4. Capture checksums for each quarantined file before any manual inspection.
5. Keep the quarantined copy read-only after capture so later analysis does not alter evidence.
6. Link the quarantine directory from the incident record and the post-incident review.

## Corruption recovery

The automatic derived-graph reset above is the complete normal recovery for isolated `graph.db` corruption. The procedures in this section apply to durable event, memory, and snapshot corruption or to incidents that span multiple stores.

1. Corrupted payload.
Expect a quarantined row and continued stream progress, as proved by `test_corrupted_payload_quarantines_row_without_panic` in [daemon/crates/lattice-core/src/hardening/corruption_tests.rs](../../daemon/crates/lattice-core/src/hardening/corruption_tests.rs:21). Preserve `events.db`, note the affected event id, and replay from the newest valid snapshot or full event log.

2. Mismatched hash.
Expect the reader to log a hash-mismatch corruption signal and continue, as proved by `test_mismatched_payload_hash_logs_corruption_and_continues` in [daemon/crates/lattice-core/src/hardening/corruption_tests.rs](../../daemon/crates/lattice-core/src/hardening/corruption_tests.rs:49). Preserve the damaged spill row, then replay from a valid snapshot or the full log.

3. Invalid kind.
Expect the reader to reject the bad row with a clear error instead of panicking, as proved by `test_invalid_event_kind_returns_clear_error_without_panic` in [daemon/crates/lattice-core/src/hardening/corruption_tests.rs](../../daemon/crates/lattice-core/src/hardening/corruption_tests.rs:77). Quarantine the store, then rebuild from an earlier valid snapshot or full replay.

4. Invalid stable reference.
Expect bootstrap to reject the tail with a dangling-reference signal, as proved by `test_invalid_stable_reference_emits_dangling_reference_signal` in [daemon/crates/lattice-core/src/hardening/corruption_tests.rs](../../daemon/crates/lattice-core/src/hardening/corruption_tests.rs:95). Fix or remove the invalid producer only after evidence is preserved.

5. Truncated file.
Expect a recovery-relevant SQLite or replay error without invented rows, as proved by `test_truncated_event_log_file_reports_recovery_or_sqlite_error_without_panic` in [daemon/crates/lattice-core/src/hardening/corruption_tests.rs](../../daemon/crates/lattice-core/src/hardening/corruption_tests.rs:133). Quarantine the truncated file and replay from the last valid snapshot or a clean backup.

6. Snapshot version mismatch.
Expect bootstrap refusal with a clear `snapshot format too new` signal, as proved by `test_snapshot_version_mismatch_refuses_bootstrap_with_clear_error` in [daemon/crates/lattice-core/src/hardening/corruption_tests.rs](../../daemon/crates/lattice-core/src/hardening/corruption_tests.rs:161). Use an older daemon that supports that format or fall back to event-log-only replay.

7. Partial snapshot or missing sibling snapshot.
Expect bootstrap to ignore the corrupt candidate and fall back to the newest valid sibling, as proved by `test_partial_snapshot_falls_back_to_full_replay_without_data_loss` in [daemon/crates/lattice-core/src/hardening/recovery_tests.rs](../../daemon/crates/lattice-core/src/hardening/recovery_tests.rs:65). Record which sibling was selected and why.

## Rollback

Use the inverse-migration policy in [Storage Migration Policy](../architecture/2026-05-16-storage-migration-policy.md#rollback) and the hardening evidence in [T84 Result](../plans/2026-05-16-cognitive-workspace-fork-build/results/T84.md).

1. Stop the daemon.
2. Identify the last known-good migration id from the failing deployment window.
3. Take a fresh full backup of `.lattice/` before rollback.
4. Run the policy-defined rollback command with `--archive-newer` so post-target rows are preserved for forensics.
5. Restore the previous daemon binary only after the rollback archive is confirmed.
6. Restart the daemon and run the same spot checks used for snapshot recovery.
7. Record the rollback archive path, migration id, and verification results in the incident record.

## Communicating incidents

1. Log the incident start time, workspace, branch, daemon version, and user-visible symptom.
2. Record every operator action taken, including replay path, quarantine path, rollback command, and validation commands.
3. Notify the maintainer or on-call owner as soon as recovery leaves the normal review-panel workflow and enters replay, quarantine, or rollback.
4. Capture the exact log evidence that justified the chosen path, including bootstrap success, corruption errors, and queue or indexing symptoms.
5. Write a post-incident note that links the quarantine directory, recovered snapshot or event-log evidence, and any follow-up code or docs work.
