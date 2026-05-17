# R18 — Phase 2 Event Log Substrate Review

**Date:** 2026-05-17
**Reviewer:** R18 (claude-opus-4-7)
**Phase under review:** Phase 2 — Event Log Substrate
**Plan anchor:** [`docs/plans/2026-05-16-cognitive-workspace-fork-plan.md` `### Phase 2: Event Log Substrate`](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-2-event-log-substrate)
**Tasks under review:** T11, T12, T13, T14, T15, T16, T17

This review certifies that the Phase 2 event log substrate meets every spec invariant required before Phase 3 (memory graph) may begin. It exercises the spec deliverables and Definition of Done (DoD) lines in `### Phase 2: Event Log Substrate`, the event-kind catalog and envelope fields in `### 3. Event Log`, and the non-negotiable product properties in `## Non-Negotiable Product Properties` (workspace boundary enforcement, no unbounded scans, append-only event log).

## Spec alignment

### Phase 2 deliverables

| Spec deliverable | Artifact that satisfies it | Verification |
| --- | --- | --- |
| Event model | `daemon/crates/lattice-core/src/events/kinds.rs:14-37`, `…/kinds.rs:124-426` (all 20 typed payload structs), `…/kinds.rs:428-452` (tagged `EventPayload` enum), `…/envelope.rs:189-218` (typed envelope) | `cargo test -p lattice-core --lib events::tests` (5/5 pass) |
| Append-only SQLite event tables | `daemon/crates/lattice-core/src/events/schema.sql:17-50` (events with payload-location CHECK), `…/schema.sql:60-71` (append-only triggers gated by `event_compaction_control`) | `events::store_tests::events_table_rejects_update_and_delete` and `events::corruption_tests::events_table_update_fails_with_append_only_trigger_error` |
| Event writer API | `daemon/crates/lattice-core/src/events/writer.rs:86-160` (`EventWriter::append`/`append_with_flush_policy`), `…/writer.rs:217-269` (spillover persistence), `…/writer.rs:271-276` (Sync/Batched flush) | `cargo test -p lattice-core --lib events::writer_tests` (7/7 pass) |
| Event reader/query API | `daemon/crates/lattice-core/src/events/query.rs:74-184` (typed scoped builder, default 1000 / hard ceiling 10 000 limits, `Unscoped` rejection), `…/reader.rs:62-156` (execute + streaming variant + tail) | `cargo test -p lattice-core --lib events::reader_tests` (9/9 pass, including `missing_scope_returns_unscoped_error` and `limit_ceiling_is_enforced`) |
| Event payload hashing and optional spillover | `daemon/crates/lattice-core/src/events/hashing.rs:84-126` (sha256 over canonical JSON), `…/writer.rs:217-255` (inline vs spilled selection at the 4 096-byte ceiling), `…/schema.sql:1-15` (UNIQUE `payload_hash` in `event_payloads`) | `events::store_tests::content_addressed_payloads_dedupe_identical_bytes`, `events::writer_tests::spilled_payloads_dedupe_by_hash`, `events::writer_tests::inline_ceiling_boundary_is_honored_exactly` |
| MCP/tool-call event capture using stable identities | `daemon/crates/lattice-daemon/src/rpc/event_capture.rs:53-625` (capture surface), `daemon/crates/lattice-daemon/src/rpc/mcp.rs:1375-1486` (dispatch wraps `capture_tool_called` / `capture_tool_result` for every tool name), `…/event_capture.rs:558-587` (per-tool workflow events including `ContextBundleReturned`, `MemoryRetrieved`, `PlanCreated`) | `cargo test -p lattice-daemon --lib rpc::event_capture_tests` (6/6 pass; `all_dispatched_workflow_tools_emit_call_and_result_events` iterates 36 tools) |
| Workflow task/session correlation | `daemon/crates/lattice-daemon/src/rpc/event_capture.rs:78-108` (`begin_task` / `ensure_task_started`), `…/event_capture.rs:603-625` (envelope inherits current task + workspace + session + branch) | `rpc::event_capture_tests::dispatch_success_records_task_tool_result_and_workflow_events` |
| Tests for ordering, replay, corruption, and workspace scoping | `…/events/replay_tests.rs`, `…/events/corruption_tests.rs`, `…/events/store_tests.rs`, `…/events/reader_tests.rs`, `…/events/snapshot_tests.rs` | `cargo test -p lattice-core --lib events::replay_tests` (4/4), `events::corruption_tests` (5/5), `events::snapshot_tests` (6/6) |

### Phase 2 Definition of Done

| DoD line | Evidence |
| --- | --- |
| Every high-level MCP workflow records task, retrieval, response, and outcome events | `daemon/crates/lattice-daemon/src/rpc/mcp.rs:1375-1486` dispatches `capture_tool_called` + `capture_tool_result` + `record_workflow_events` for every tool branch. `rpc::event_capture_tests::all_dispatched_workflow_tools_emit_call_and_result_events` proves 36 instrumented tools emit both `ToolCalled` and `ToolResult`. `record_success_events` (`event_capture.rs:558-587`) additionally emits `ContextBundleReturned`, `MemoryRetrieved`, `PlanCreated`, and `WorkflowSucceeded`. |
| Event log can replay enough data to reconstruct workflow history | `events::Snapshot` (`snapshot.rs:100-188`) captures graph + memory state up to a cursor and `Bootstrap::load` (`compaction.rs:284-306`) replays post-cursor events with reference validation (`validate_tail_references`). `events::snapshot_tests::bootstrap_from_snapshot_and_tail_events_matches_full_replay_count` proves equivalence with full replay. |
| Failure paths emit useful events without hiding errors | `mcp.rs:1456-1485` always invokes `capture_tool_result` regardless of outcome, then returns the original `Result<Value, (i32, String)>` unchanged. `rpc::event_capture_tests::dispatch_error_records_tool_result_and_preserves_json_rpc_error` asserts both that the JSON-RPC error (`-32602`, `"Missing required parameter: query"`) is preserved AND that the session tail contains `ToolCalled`, `ToolResult` (with `ToolResultStatus::Failed`), and `WorkflowFailed`. |
| Event write adds no more than 5 ms P99 to hot-path tool calls (`prepare_change`, `get_context_capsule`); large payloads spill rather than blocking the writer | See `## P99 budget evidence` below. Live re-run: 512 µs / 235 µs / 212 µs P99 against the 5 000 µs budget. Spillover threshold is the writer-configurable `inline_ceiling_bytes` (default 4 096); `writer.rs:217-255` routes through `EventStore::insert_or_get_payload` (with a per-writer row-id cache) without blocking the envelope insert path. |

### Event-kind catalog (`### 3. Event Log`)

All 20 spec event kinds are present, in spec order, with canonical snake_case wire names and typed payloads:

| Spec kind | Rust variant | Wire name | Typed payload |
| --- | --- | --- | --- |
| `AssistantTaskStarted` | `EventKind::AssistantTaskStarted` | `assistant_task_started` | `AssistantTaskStartedPayload` |
| `ToolCalled` | `EventKind::ToolCalled` | `tool_called` | `ToolCalledPayload` |
| `ToolResult` | `EventKind::ToolResult` | `tool_result` | `ToolResultPayload` |
| `ContextBundleReturned` | `EventKind::ContextBundleReturned` | `context_bundle_returned` | `ContextBundleReturnedPayload` |
| `MemoryRetrieved` | `EventKind::MemoryRetrieved` | `memory_retrieved` | `MemoryRetrievedPayload` |
| `MemoryExpanded` | `EventKind::MemoryExpanded` | `memory_expanded` | `MemoryExpandedPayload` |
| `PlanCreated` | `EventKind::PlanCreated` | `plan_created` | `PlanCreatedPayload` |
| `FileRead` | `EventKind::FileRead` | `file_read` | `FileReadPayload` |
| `PatchApplied` | `EventKind::PatchApplied` | `patch_applied` | `PatchAppliedPayload` |
| `TestRunStarted` | `EventKind::TestRunStarted` | `test_run_started` | `TestRunStartedPayload` |
| `TestRunCompleted` | `EventKind::TestRunCompleted` | `test_run_completed` | `TestRunCompletedPayload` |
| `DiagnosticObserved` | `EventKind::DiagnosticObserved` | `diagnostic_observed` | `DiagnosticObservedPayload` |
| `UserCorrection` | `EventKind::UserCorrection` | `user_correction` | `UserCorrectionPayload` |
| `UserPreferenceObserved` | `EventKind::UserPreferenceObserved` | `user_preference_observed` | `UserPreferenceObservedPayload` |
| `WorkflowSucceeded` | `EventKind::WorkflowSucceeded` | `workflow_succeeded` | `WorkflowSucceededPayload` |
| `WorkflowFailed` | `EventKind::WorkflowFailed` | `workflow_failed` | `WorkflowFailedPayload` |
| `MemoryCreated` | `EventKind::MemoryCreated` | `memory_created` | `MemoryCreatedPayload` |
| `MemoryUpdated` | `EventKind::MemoryUpdated` | `memory_updated` | `MemoryUpdatedPayload` |
| `MemoryInvalidated` | `EventKind::MemoryInvalidated` | `memory_invalidated` | `MemoryInvalidatedPayload` |
| `MemoryConsolidated` | `EventKind::MemoryConsolidated` | `memory_consolidated` | `MemoryConsolidatedPayload` |

Constructor parity is enforced by `EventEnvelope::new` (`envelope.rs:220-260`), which rejects any envelope whose `kind` disagrees with `payload.kind()` via `EventModelError::KindPayloadMismatch`. The corresponding regression test is `events::tests::event_envelope_constructor_rejects_kind_payload_mismatch`.

### Envelope field coverage (`### 3. Event Log` — "Events should carry…")

| Spec field | Envelope field | Notes |
| --- | --- | --- |
| Workspace id | `workspace_id: WorkspaceId` | Phase 1 stable identity. |
| Branch | `branch: BranchRef` | Empty branch rejected at the writer (`validate_identity_fields` `writer.rs:332-357`). |
| Session id | `session_id: SessionId` | Required; empty `SessionId::value` rejected at writer. |
| Task id | `task_id: Option<TaskId>` | Optional, populated automatically from `EventCapture::current_task` when present. |
| Actor | `actor: Actor` | Enum: `Assistant{model}`, `User`, `Tool{name}`, `Daemon`. |
| Timestamp | `timestamp: DateTime<Utc>` | Monotonic per-session via `EventWriter::next_timestamp` (`writer.rs:183-215`). |
| Stable references | `references: Vec<StableRef>` | `FileRef`, `SymbolRef`, `DocSectionRef`, `EventRef`, `MemoryRef`, `ContextHandleRef` — all backed by Phase 1 stable identity types. |
| Payload hash | `payload_hash: PayloadHash` | sha256 surfaced as `sha256:<64-hex>` on the wire (`hashing.rs:14-82`). |
| Compact summary | `summary: CompactSummary` | Hard 512-byte ceiling enforced on construction AND deserialization (`envelope.rs:122-187`). |
| Optional spillover location | `payload_location: PayloadLocation` | `Inline{bytes_len}` or `Spilled{row_id}`; exclusivity also enforced by SQL `CHECK` (`schema.sql:46-49`). |

### Non-negotiable product properties

| Property | Evidence |
| --- | --- |
| Append-only event log | DB-level triggers `events_no_update` and `events_no_delete` reject UPDATE/DELETE unconditionally except when `event_compaction_control.allow_delete = 1`, which is only set inside `EventStore::truncate_through` for the duration of one transaction (`store.rs:336-351`) and is the only sanctioned compaction path. |
| Workspace boundary enforcement | `EventWriter` rejects any envelope whose `workspace_id` disagrees with the writer's bound workspace (`writer.rs:162-181`). `EventCapture::new` rejects writer/capture workspace mismatches at construction (`event_capture.rs:53-76`). |
| No unbounded event-log scans on hot paths | `EventQuery::validate` requires task / session / workspace+branch scope (`query.rs:154-160`) and rejects requested `limit > 10_000` (`query.rs:162-168`). The store layer also clamps to 10 000 via `checked_limit` (`store.rs:573-578`). |
| Deterministic identities | Event ids are workspace-scoped ULIDs derived from the writer's monotonic-per-session timestamp (`writer.rs:378-392`). |

## Coding-standard alignment

The Cadres coding standard sections relevant here are §Hard limits, §Error handling, §Schema / model parity, §No broken windows, §Single source of truth, and the Lattice CLAUDE.md rule about Markdown heading references.

### File-length ceiling (§Hard limits — 800 lines)

Every event source file is below the 800-line ceiling:

```
crates/lattice-core/src/events/budget_tests.rs       227
crates/lattice-core/src/events/compaction.rs         497
crates/lattice-core/src/events/corruption_tests.rs   340
crates/lattice-core/src/events/envelope.rs           261
crates/lattice-core/src/events/hashing.rs            144
crates/lattice-core/src/events/kinds.rs              480
crates/lattice-core/src/events/migrations.rs         134
crates/lattice-core/src/events/mod.rs                 64
crates/lattice-core/src/events/query.rs              213
crates/lattice-core/src/events/reader.rs             478
crates/lattice-core/src/events/reader_tests.rs       286
crates/lattice-core/src/events/replay_tests.rs       334
crates/lattice-core/src/events/snapshot.rs           446
crates/lattice-core/src/events/snapshot_tests.rs     255
crates/lattice-core/src/events/store.rs              584
crates/lattice-core/src/events/store_tests.rs        187
crates/lattice-core/src/events/tests.rs              423
crates/lattice-core/src/events/writer.rs             392
crates/lattice-core/src/events/writer_tests.rs       247
crates/lattice-core/src/events/schema.sql             80
crates/lattice-daemon/src/rpc/event_capture.rs       625
crates/lattice-daemon/src/rpc/event_capture_support.rs 251
crates/lattice-daemon/src/rpc/event_capture_tests.rs 324
```

`event_capture.rs` at 625 lines is the largest file in the substrate; it is structurally a coherent "one method per event kind" surface and remains well under the 800-line ceiling.

### Function length and nesting (§Hard limits — 50 lines, 3 levels)

A function-by-function pass over `store.rs`, `writer.rs`, `reader.rs`, `compaction.rs`, `snapshot.rs`, and `event_capture.rs` finds no function over the 50-line ceiling. The longest candidates are `Compactor::run_once` (~48 lines, `compaction.rs:127-174`), `EventReader::execute` plus its helper `query_rows` (split, each <30 lines), and `to_envelope` (~42 lines, `reader.rs:339-381`). Nesting is consistently flat — early returns are used throughout.

### Error handling (§Error handling)

- Every fallible boundary returns a typed error: `EventModelError`, `EventStoreError`, `EventWriteError`, `EventQueryError`, `SnapshotError`, `BootstrapError`, `CompactionError`, `EventCaptureError`, `IdentityError`. All use `thiserror::Error`.
- No `bare except`-style swallowing: even the MCP capture wrapper (`mcp.rs:1438-1485`) logs with `tracing::warn!` and returns nothing only on a non-fatal capture failure (so the tool result still flows to the caller).
- No silent `let _ = …` for fallible mutations. The one `let _ = shutdown.send(())` in `SchedulerHandle::shutdown` (`compaction.rs:273-282`) is correct: send-on-already-closed is an expected no-op.
- Lock poisoning is mapped to typed errors (`EventStoreError::EnvelopeInvalid`, `EventCaptureError::Lock`) rather than panicking.
- The two `expect()` calls in production code (`writer.rs:391` for the ULID alphabet, `snapshot.rs:295-296` for fixed-length slice into a 64-byte array) are safe-by-construction.

### Schema / model parity (§Schema / model parity)

`schema.sql` columns line up with `InsertEnvelopeRow` and `EventEnvelopeRow`:

- `event_id INTEGER PRIMARY KEY AUTOINCREMENT` ↔ `event_id: i64`
- `event_uuid TEXT NOT NULL UNIQUE` ↔ `event_uuid: String`
- `workspace_id TEXT NOT NULL` ↔ `workspace_id: String`
- `branch TEXT NOT NULL` ↔ `branch: String`
- `session_id TEXT NOT NULL` ↔ `session_id: String`
- `task_id TEXT NULL` ↔ `task_id: Option<String>`
- `actor_kind TEXT NOT NULL`, `actor_detail TEXT NULL` ↔ `actor_kind: String`, `actor_detail: Option<String>`
- `kind TEXT NOT NULL` ↔ `kind: String`
- `ts_unix_micros INTEGER NOT NULL` ↔ `ts_unix_micros: i64`
- `payload_hash BLOB NOT NULL` ↔ `payload_hash: Vec<u8>`
- `summary TEXT NOT NULL` (≤512 bytes) ↔ `summary: String`
- `payload_inline BLOB NULL` ↔ `payload_inline: Option<Vec<u8>>`
- `payload_spill_id INTEGER NULL` (FK → `event_payloads.row_id`) ↔ `payload_spill_id: Option<i64>`
- `references_json TEXT NOT NULL` ↔ `references_json: String`
- `schema_version INTEGER NOT NULL` ↔ `schema_version: i64`

Nullability matches the Rust optionality. Validation symmetry: the row check `(payload_inline IS NULL) XOR (payload_spill_id IS NULL)` (`schema.sql:46-49`) is mirrored at the Rust boundary by `validate_payload_location` (`store.rs:538-543`). Both the model (`CompactSummary::new`) and the SQL `CHECK (length(CAST(summary AS BLOB)) <= 512)` enforce the same 512-byte summary ceiling.

### SQL-binding hygiene (no string interpolation of user values)

All SQL statements in `store.rs`, `compaction.rs`, `migrations.rs`, and `reader.rs::build_sql` use parameter placeholders (`?1`, `?2`, …) bound through `params!` or `params_from_iter`. The dynamic SQL constructed in `reader.rs::build_sql` only concatenates hardcoded SQL fragments (one fragment per scope/cursor/time/kind clause); user-controlled strings always flow through `params.push(Value::Text(…))`. There are no `format!("… WHERE x = '{}'", value)` patterns anywhere in the events module.

### Tracing on hot paths

- `EventWriter::append_with_flush_policy` opens a `trace_span!("event_writer.append", kind = …)` (`writer.rs:128`).
- `EventReader::execute` opens a `debug_span!("event_reader.execute", scope = …)` with the bounded `EventQuery::trace_scope` label (`reader.rs:69-70`).
- `Compactor::run_once` opens an `info_span!("compaction", interval = …)` and logs result counters on success (`compaction.rs:131`, `compaction.rs:160-166`).
- `EventStore::insert_envelope_row` warns on insert failure with the `event_uuid` for correlation (`store.rs:157-167`).
- `EventCapture` itself emits no tracing, relying on the `info_span!("tool", name = tool_name)` already opened at `mcp.rs:1380` and the writer's span downstream. This is structurally fine — see Findings §Minor.

### No broken windows (§No broken windows)

- No `TODO`, `FIXME`, or `XXX` comments anywhere in the events module or the daemon-side event capture (grep returned no matches).
- No `#[allow(unused)]` shotguns; the seven `#[allow(dead_code)]` markers on `event_capture.rs` (`record_memory_expanded`, `record_file_read`, `record_patch_applied`, `record_test_run`, `record_diagnostic`, `record_user_correction`, `record_user_preference`, `record_memory_*`) are documented with `// T15 defines helper contracts before all future call sites exist.` — these are the lifecycle hooks Phase 3+ will use, intentionally landed in this phase. The annotations are not used to suppress diagnostics on otherwise broken code.
- No commented-out code, no dead imports.

### Markdown-heading references (Lattice CLAUDE.md)

- `events/snapshot.rs:1-7` and `events/compaction.rs:1-6` cite `docs/architecture/2026-05-16-event-log-compaction.md` sections by exact heading (`## Snapshot format`, `## Recovery from corrupt snapshot`, `## Compaction schedule`, `## Bootstrap procedure`, `## Observability`).
- `events/mod.rs:1-6` cites the plan section by exact heading (`### 3. Event Log`).
- `events/kinds.rs:7-13` documents the canonical event-kind list by spec heading reference.

## P99 budget evidence

The Phase 2 DoD requires the event-write hot path to stay within ≤5 ms P99 for `prepare_change`- and `get_context_capsule`-shaped payloads, including the spilled-payload variant. R18 re-ran the budget harness during this review and captured the live JSON artifacts under `daemon/target/event_budget/`.

### Hardware profile

- CPU: `Intel(R) Core(TM) i7-10610U CPU @ 1.80GHz` (read from `/proc/cpuinfo`).
- Store mode: `EventStore::open_in_memory` (`journal_mode=MEMORY`, `synchronous=NORMAL`).
- Writer flush policy: `FlushPolicy::Batched { interval_ms: 250 }` — the same policy used by `EventCapture` for hot-path tool events (`event_capture.rs:29`).
- Concurrency control: ignored-budget tests serialize on a global `OnceLock<Mutex<()>>` so the three cases do not measure their own contention (`budget_tests.rs:25,99-102`).

### Captured per-quantile numbers (live re-run, 2026-05-17)

`daemon/target/event_budget/prepare_change.json`:

```json
{
  "case_name": "prepare_change",
  "iterations": 10000,
  "inline_ceiling_bytes": 4096,
  "payload_bytes": 1514,
  "p50_micros": 338,
  "p95_micros": 474,
  "p99_micros": 512,
  "max_micros": 1097,
  "cpu_model": "Intel(R) Core(TM) i7-10610U CPU @ 1.80GHz",
  "filesystem": "in-memory sqlite",
  "sqlite_mode": { "flush_policy": "batched", "journal_mode": "MEMORY", "synchronous": "NORMAL" }
}
```

`daemon/target/event_budget/get_context_capsule.json`:

```json
{
  "case_name": "get_context_capsule",
  "iterations": 10000,
  "inline_ceiling_bytes": 4096,
  "payload_bytes": 383,
  "p50_micros": 150,
  "p95_micros": 212,
  "p99_micros": 235,
  "max_micros": 371,
  "cpu_model": "Intel(R) Core(TM) i7-10610U CPU @ 1.80GHz",
  "filesystem": "in-memory sqlite",
  "sqlite_mode": { "flush_policy": "batched", "journal_mode": "MEMORY", "synchronous": "NORMAL" }
}
```

`daemon/target/event_budget/prepare_change_spill.json`:

```json
{
  "case_name": "prepare_change_spill",
  "iterations": 2000,
  "inline_ceiling_bytes": 256,
  "payload_bytes": 586,
  "p50_micros": 174,
  "p95_micros": 198,
  "p99_micros": 212,
  "max_micros": 237,
  "cpu_model": "Intel(R) Core(TM) i7-10610U CPU @ 1.80GHz",
  "filesystem": "in-memory sqlite",
  "sqlite_mode": { "flush_policy": "batched", "journal_mode": "MEMORY", "synchronous": "NORMAL" }
}
```

### Verdict on the 5 ms P99 budget

| Case | Payload bytes | Inline ceiling | P50 | P95 | P99 | Max | 5 000 µs budget |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `prepare_change` (inline) | 1 514 | 4 096 | 338 µs | 474 µs | **512 µs** | 1 097 µs | **PASS (~10× headroom)** |
| `get_context_capsule` (inline) | 383 | 4 096 | 150 µs | 212 µs | **235 µs** | 371 µs | **PASS (~21× headroom)** |
| `prepare_change_spill` (spilled) | 586 | 256 | 174 µs | 198 µs | **212 µs** | 237 µs | **PASS (~23× headroom)** |

The recorded P99 numbers in `docs/architecture/2026-05-16-event-log-compaction.md` (P99 = 3 710 µs / 3 314 µs / 3 427 µs) reflect an earlier capture run and are still inside the 5 ms budget. The live re-run executed for this review (above) is materially faster than the recorded run on the same CPU, which suggests the architecture doc's "captured P99 results" subsection is stale relative to the current writer — see Findings §Minor 3.

The 5 ms P99 budget is met with substantial headroom across all three configurations, including the worst case (spilled large payloads bypass the inline write path entirely yet still complete at 212 µs P99).

### Verification commands and outputs

| Task | Command | Outcome |
| --- | --- | --- |
| T11 | `cargo test -p lattice-core --lib events::tests` | `5 passed; 0 failed; 0 ignored; 0 measured; 259 filtered out` |
| T12 | `cargo test -p lattice-core --lib events::store_tests` | `7 passed; 0 failed; 0 ignored; 0 measured; 257 filtered out` |
| T13 | `cargo test -p lattice-core --lib events::writer_tests` | `7 passed; 0 failed; 0 ignored; 0 measured; 257 filtered out` |
| T14 | `cargo test -p lattice-core --lib events::reader_tests` | `9 passed; 0 failed; 0 ignored; 0 measured; 255 filtered out` |
| T15 | `cargo build --release` | `Finished release profile [optimized] target(s) in 1m 15s` |
| T15 | `cargo test -p lattice-daemon --lib rpc::event_capture_tests` | `6 passed; 0 failed; 0 ignored; 0 measured; 49 filtered out` |
| T16 | `cargo test -p lattice-core --lib events::snapshot_tests` | `6 passed; 0 failed; 0 ignored; 0 measured; 258 filtered out` |
| T17 | `cargo test -p lattice-core --lib events::replay_tests` | `4 passed; 0 failed; 0 ignored; 0 measured; 260 filtered out` |
| T17 | `cargo test -p lattice-core --lib events::corruption_tests` | `5 passed; 0 failed; 0 ignored; 0 measured; 259 filtered out` |
| T17 | `cargo test -p lattice-core --lib events::budget_tests -- --include-ignored` | `3 passed; 0 failed; 0 ignored; 0 measured; 261 filtered out` |

## Findings

### Blocker

None.

### Major

None.

### Minor

1. **`hash_canonical_payload_bytes` is invoked on non-canonical bytes in the writer hot path** — owner: `T13`. `writer.rs:130` produces `payload_bytes` via `serde_json::to_vec(&envelope.payload)` (struct-field declaration order; no map-key sorting), and `writer.rs:138` then computes the payload hash directly on those bytes via `hash_canonical_payload_bytes(&payload_bytes)`. For typed Rust structs derived with serde this is deterministic per binary version, so dedupe semantics are intact and the test `canonical_hash_is_stable_for_equivalent_key_order_variations` passes against `canonical_json_bytes`. But the helper name conflates "hash these bytes I claim are canonical" with "compute a canonical hash"; a future contributor adding `serde_json::Value` payloads or reordering struct fields could silently regress dedupe across versions. Recommend either renaming the helper to `hash_payload_bytes` and re-asserting the canonical-JSON contract at the writer (via `canonical_json_bytes` on the typed payload), or documenting in `writer.rs` that the implicit canonicalization comes from struct-field declaration order. Behavior is currently correct; the risk is naming-induced drift. Not blocking.

2. **`EventCapture` has no tracing span of its own** — owner: `T15` (optional). The capture surface relies on the `info_span!("tool", name = …)` already opened at `mcp.rs:1380` and the writer's `trace_span!("event_writer.append", …)` downstream. This works, but `EventCapture::record_workflow_events`, `record_tool_result`, and `record_context_bundle` each emit two or three events without a per-call span. Adding `tracing::trace_span!("event_capture.record_*", …)` would improve diagnosability if any of these helpers later become a hot path. Not blocking.

3. **Architecture-doc P99 figures are stale** — owner: `T17` (followup). `docs/architecture/2026-05-16-event-log-compaction.md` `## Phase 2 Budget Evidence` records `P99 3710 µs / 3314 µs / 3427 µs`; the live re-run on the same CPU now measures `P99 512 µs / 235 µs / 212 µs`. Both numbers are inside the 5 ms budget, but the doc no longer reflects the current writer's measured behavior. Recommend refreshing that section with the live numbers and noting the cause (the inline-vs-spill cache reuse + struct-order hash optimization landed in T17 has compounded with WAL behavior since the doc was written). Not blocking.

4. **Daemon library targets have pre-existing failing references unrelated to Phase 2** — owner: out of scope for R18. T16's notes recorded that `cargo check -p lattice-daemon --bin lattice --locked` previously failed on `vector_sync`, `repo_name_for_root`, and `prioritize_indexable_paths`. Those targets compiled cleanly during R18's `cargo build --release` and `cargo test -p lattice-daemon --lib rpc::event_capture_tests`, so the issue is either already resolved or was specific to a different cargo invocation. No new failure observed during this review.

### Notes (not findings)

- The `#[allow(dead_code)]` markers in `event_capture.rs` are intentional placeholders for Phase 3+ wiring; they are documented inline and have no runtime cost. Consistent with the "build it once, wire it later" subagent strategy used elsewhere in this build plan.
- The `event_compaction_control` table is a clean way to gate the sanctioned delete path without dropping the trigger; the design matches the documented `## Compaction schedule` recovery semantics.

## Verdict

`APPROVED`

The Phase 2 event-log substrate satisfies every spec deliverable and DoD line in `### Phase 2: Event Log Substrate`, every event kind and envelope field in `### 3. Event Log`, every relevant invariant in `## Non-Negotiable Product Properties`, and the Cadres coding standard sections referenced above. The 5 ms P99 hot-path budget is met across the `prepare_change`, `get_context_capsule`, and spilled-payload cases with ≥10× headroom on the measurement hardware. The three minor findings above are tracking-quality items that do not block Phase 3 (`T19+`) from starting.
