# R38 — Phase 5 Working Memory Review

**Phase:** 5
**Reviewer:** R38 (build harness)
**Date:** 2026-05-17
**Tasks reviewed:** T34, T35, T36, T37
**Spec anchors:**
- `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md` `## 5. Working Memory`
- `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md` `## Phase 5: Working Memory`
- `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md` `## 3. Event Log`
- `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md` `## 9. MCP Surface`
- `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md` `## Storage Design`

## Spec alignment

### Phase 5 deliverables

| Deliverable (`## Phase 5: Working Memory`) | Owning task | Implementing file | Status |
|---|---|---|---|
| Explicit per-task working memory state | T34 | `daemon/crates/lattice-core/src/working_memory/state.rs` | Implemented — `WorkingMemoryState` carries all eleven spec-named fields. |
| Operations: retrieve, summarize, filter, pin, evict, expand, compress, checkpoint | T35 | `daemon/crates/lattice-core/src/working_memory/operations.rs` | Implemented — eight free functions plus a `WorkingMemoryOp` enum that match-arms each operation. |
| MCP surface for inspecting working memory | T36 | `daemon/crates/lattice-daemon/src/rpc/working_memory_tool.rs` (registration), `daemon/crates/lattice-daemon/src/rpc/mcp.rs:3819` (handler) | Implemented — `inspect_working_memory` tool with `compact` / `diagnostic` modes and `include_excluded` switch; registered alongside the existing tools via `working_memory_tool::tool_definition()` in `mcp.rs:1213`. |
| Automatic event capture for included and excluded context | T36 | `daemon/crates/lattice-core/src/working_memory/event_hooks.rs` | Implemented — `emit_memory_retrieved` and `emit_memory_expanded` compute include/exclude diffs and write `EventKind::MemoryRetrieved` / `EventKind::MemoryExpanded` envelopes. `IncludedContextDelta` / `ExcludedContextDelta` are re-exported from `crate::events` so the working-memory module does not declare a parallel diff type. |
| Tests for token budgets, pinned context, eviction, and checkpoint restore | T37 | `working_memory/budget_tests.rs` (14 tests) and `working_memory/checkpoint_tests.rs` (5 tests) | Implemented. |

### Definition-of-done assertions (`## Phase 5: Working Memory`)

| DoD item | Where verified |
|---|---|
| Workflow tools can show what context is active | `inspect_working_memory` compact/diagnostic responses expose `selected_memories`, intent label, active file/symbol counts (`mcp.rs:3837-3858`). Verified by `compact_mode_returns_summary_and_snapshot_handle` and `diagnostic_mode_returns_full_state_and_include_excluded_controls_visibility`. |
| Workflow tools can show why context is active | `BundleResult.inclusion_reason` (Phase 4 shape) is preserved through the embedded `Vec<BundleResult>` in `selected_memories` and surfaces in the diagnostic mode response. |
| Workflow tools can show what was intentionally excluded | `ExcludedMemory { result: BundleResult, exclusion_reason: String }` is a typed field; the `include_excluded=true` mode returns it; events carry `ExcludedContextDelta { identity, headline, exclusion_reason }`. Verified by `diagnostic_mode_returns_full_state_and_include_excluded_controls_visibility` and `retrieve_and_expand_emit_event_diffs_through_single_mutation_observer`. |
| Context management is no longer just prompt accumulation | `record_state_mutation` runs after every mutation, hashes before/after canonical-JSON states, and routes through a `StateMutationObserver`. The MCP handler holds a `working_memory_states` cache and falls back to `working_memory_checkpoints` so a session that rolls can still recover state — verified by `tool_falls_back_to_latest_checkpoint_when_session_cache_is_empty`. |

### State-field coverage (spec §5 enumeration)

All eleven spec-named fields are present on `WorkingMemoryState` (`state.rs:37-51`):

| Spec field | Implementation field | Type |
|---|---|---|
| task statement | `task_statement` | `String` |
| interpreted intent | `interpreted_intent` | `IntentClassification` (Phase 4) |
| active files and symbols | `active_files` + `active_symbols` (split per T34 direction) | `BTreeSet<FileIdentity>` + `BTreeSet<SymbolIdentity>` |
| active hypotheses | `active_hypotheses` | `Vec<Hypothesis>` |
| active failures | `active_failures` | `Vec<FailureRecord>` |
| current plan | `current_plan` | `Option<PlanRef>` |
| selected memories | `selected_memories` | `Vec<BundleResult>` |
| excluded memories and reasons | `excluded_memories` | `Vec<ExcludedMemory>` (reason required by type system) |
| budget decisions | `budget_decisions` | `BudgetDecisions` |
| unresolved questions | `unresolved_questions` | `Vec<String>` |
| verification status | `verification_status` | `WorkingMemoryVerification` |

The "active files and symbols" split into two fields is per T34's own steps and does not constitute a deviation; both subfields are typed against the canonical Phase 1 identity types.

### Operation coverage (spec §5 "Required operations")

| Spec operation | Implementing fn | Test in `operations_tests.rs` | Drop-path exclusion reason |
|---|---|---|---|
| `retrieve` | `retrieve` (`operations.rs:344`) | `retrieve_populates_selected_and_excluded_memories` | `"compressed: budget=<n> tokens"` (shaper-driven drops in `PhaseFourRetriever::run_pipeline`) |
| `summarize` | `summarize` + `summarize_state` (`operations.rs:384`) | `summarize_returns_expected_fields` | n/a — non-mutating |
| `filter` | `filter` (`operations.rs:415`) | `filter_records_predicate_description` | `"filtered: <predicate description>"` |
| `pin` | `pin` (`operations.rs:430`) | `pin_is_idempotent_and_compress_preserves_pinned_identity` | n/a — does not drop |
| `evict` | `evict` (`operations.rs:460`) | `evict_of_pinned_identity_without_force_is_refused`, `evict_forced_identity_records_reason_and_absent_count`, plus eviction regressions in `budget_tests.rs` | `"evicted: <reason>"` / `"evicted: force=true; <reason>"` |
| `expand` | `expand` (`operations.rs:485`) | `expand_appends_new_high_ranked_candidates` | `"compressed: budget=<n> tokens"` (same shaper) |
| `compress` | `compress` (`operations.rs:517`) | `pin_is_idempotent_and_compress_preserves_pinned_identity`, plus budget regressions in `budget_tests.rs` | `"compressed: budget=<n> tokens"` |
| `checkpoint` | `checkpoint` (`operations.rs:554`) | `checkpoint_round_trips_state_bytes`, `checkpoint_name_versions_instead_of_overwriting` | n/a — write-only |

### Phase 4 forward compatibility

`retrieval_v1::shaper::schema::BundleResult` and `RetrievalBundle` are unchanged (`shaper.rs:30-59`). They are embedded under `selected_memories: Vec<BundleResult>` and under `ExcludedMemory.result: BundleResult` with no wrapper that mutates field shape, names, or nullability. The Phase 4 stability contract is preserved — no Phase 4 rework required.

### Event Log alignment (spec §3)

Events emitted from working memory use the existing `EventKind::MemoryRetrieved` / `EventKind::MemoryExpanded` variants and the existing `MemoryRetrievedPayload` / `MemoryExpandedPayload` shapes (`kinds.rs:190-232`). Both payloads already carry `included_context: Vec<IncludedContextDelta>` and `excluded_context: Vec<ExcludedContextDelta>` so the Phase 5 contract is satisfied without changing Phase 2 schema.

## Coding-standard alignment

Hard limits from `/home/pete/cadres/shared/templates/coding.md` evaluated per file:

| File | Lines | ≤800 cap | Longest fn (lines) | ≤50 cap | Notes |
|---|---|---|---|---|---|
| `working_memory/mod.rs` | 40 | ✅ | n/a (re-exports only) | ✅ | |
| `working_memory/state.rs` | 342 | ✅ | `save_checkpoint_for_scope` (~30) | ✅ | |
| `working_memory/schema.sql` | 22 | ✅ | n/a | n/a | |
| `working_memory/operations.rs` | **848** | ❌ **over cap by 48 lines** | `PhaseFourRetriever::run_pipeline` (60) | ❌ | T35's coding-standard check explicitly says: "If `operations.rs` approaches the cap, split per-operation modules under `operations/` along operation boundaries." Required follow-up: see T35 follow-up below. |
| `working_memory/event_hooks.rs` | 284 | ✅ | `emit_memory_expanded` (53) | ❌ **over cap by 3 lines** | Borderline — `emit_memory_retrieved` (44) is under the cap. Extracting the common payload-assembly shared with `emit_memory_expanded` would bring both under cap. |
| `working_memory/state_tests.rs` | 227 | ✅ | All tests ≤30 | ✅ | |
| `working_memory/operations_tests.rs` | 465 | ✅ | `pin_is_idempotent_and_compress_preserves_pinned_identity` (41) | ❌ **test >30 cap** | One test over the 30-line "tests as documentation" cap (`/home/pete/cadres/shared/templates/coding.md` §"Tests as documentation"). |
| `working_memory/budget_tests.rs` | 247 | ✅ | All tests ≤16 | ✅ | |
| `working_memory/checkpoint_tests.rs` | 98 | ✅ | All tests ≤27 | ✅ | |
| `working_memory/tests_common.rs` | 257 | ✅ | n/a (fixtures) | ✅ | |
| `daemon/src/rpc/working_memory_tool.rs` | 103 | ✅ | All ≤32 | ✅ | |
| `daemon/src/rpc/working_memory_tool_tests.rs` | 562 | ✅ | `retrieve_and_expand_emit_event_diffs_through_single_mutation_observer` (76) | ❌ **test >30 cap** | This is the integration test asserting both `MemoryRetrieved` and `MemoryExpanded` payloads in one arrangement; could split into two tests sharing a fixture builder. |

Other coding-standard checks across the file set:

- `TODO` / `FIXME` / `XXX` / `todo!()` / `unimplemented!()`: **none found** (`grep -rn` returns empty).
- `#[allow(...)]` suppressions: **none found**.
- Commented-out code blocks: **none found** by inspection.
- Schema parity (T34): every column in `working_memory_checkpoints` (`schema.sql:6-16`) maps to a `WorkingMemoryState` wrapper field. `state_json` mirrors the serde shape exactly; round-trip canonical-JSON hash test (`state_tests.rs:64-80`) is the parity gate.
- Single source of truth — identity types reuse Phase 1 `FileId` / `SymbolId` / `Identity`; the event-diff types reuse Phase 2 `IncludedContextDelta` / `ExcludedContextDelta`; the checkpoint writer routes through `MemoryStore::save_working_memory_checkpoint_for_scope` (`memory/store.rs:296-303`) so there is no parallel checkpoint writer.
- Single source of truth — test fixtures: **violation noted.** `operations_tests.rs` declares its own `FakeRetriever`, `Harness`, `Observer`, `sample_state`, `sample_bundle_result`, `sample_bundle`, `sample_identity`, `file_identity`, `long_bundle_result` (lines 254-465) that overlap with `tests_common.rs` (lines 22-247). T35 and T37 explicitly required fixture reuse. Required follow-up: see T35 / T37 follow-ups below.
- Doc citations: `state.rs`, `operations.rs`, `event_hooks.rs`, `schema.sql`, `budget_tests.rs`, `checkpoint_tests.rs` all cite the spec headings they implement, satisfying `/home/pete/cadres/lattice/CLAUDE.md` "Markdown Heading References".

## Excluded-context auditability

This section is the heart of the review — it certifies the spec phrase "excluded memories and reasons" is enforced as a contract, not a convention.

### `ExcludedMemory.exclusion_reason` is non-optional

`ExcludedMemory` (`state.rs:74-78`) is defined as:

```
pub struct ExcludedMemory {
    pub result: BundleResult,
    pub exclusion_reason: String,
}
```

`exclusion_reason` is `String`, not `Option<String>`. Constructing the struct without supplying a reason is a compile error. `state_tests.rs:114-121` (`excluded_memories_require_exclusion_reason`) anchors this as a regression.

### Every drop/skip path writes a populated reason

Sampled one entry from each drop/skip path through the eight operations:

| Drop path | Source | Reason format | Sample test asserting it |
|---|---|---|---|
| `filter` removal | `operations.rs:609-628` (`filter_selected_memories`) | `"filtered: <predicate description>"` | `filter_records_predicate_description` — asserts `"filtered: headline contains \`keep\`"` |
| `evict` (cooperative) | `operations.rs:672-678` (`eviction_reason`) + `evict_selected_memories` (`operations.rs:650-670`) | `"evicted: <reason>"` | `evict_present_identity_moves_it_to_excluded_with_reason` in `budget_tests.rs:172-181` — asserts `"evicted: operator request"` |
| `evict` (forced over pinned) | same | `"evicted: force=true; <reason>"` | `evict_forced_identity_records_reason_and_absent_count` in `operations_tests.rs:148-172` — asserts `"evicted: force=true; manual trim"` |
| `compress` shaper drop | `operations.rs:517-550` via `compress_bundle_results` → `to_excluded_memories` | `"compressed: budget=<n> tokens"` | `retrieve_moves_truncated_results_to_excluded_with_budget_reason` in `budget_tests.rs:53-69` — asserts `"compressed: budget=64 tokens"` |
| `retrieve` shaper drop (Phase 4 pipeline) | `operations.rs:280-339` (`PhaseFourRetriever::run_pipeline`) → `to_excluded_memories` (`operations.rs:807-815`) | `"compressed: budget=<n> tokens"` | `retrieve_populates_selected_and_excluded_memories` in `operations_tests.rs:27-40` |
| `expand` shaper drop | Same retriever as `retrieve`, called from `expand` (`operations.rs:485-513`) | `"compressed: budget=<n> tokens"` | `retrieve_and_expand_emit_event_diffs_through_single_mutation_observer` in `working_memory_tool_tests.rs:217-292` — expand-side excluded payload contains `"filtered: low confidence"` (test fixture supplies it directly, exercising the carry-through path) |

`pin` and `summarize` do not drop or skip results, so there is no exclusion path to cover. `checkpoint` writes the full state including `excluded_memories` so reasons survive restore (verified by `loading_checkpoint_restores_selected_excluded_and_budget_together` in `checkpoint_tests.rs:71-98`).

### MCP `include_excluded` behaviour

The `inspect_working_memory` handler (`mcp.rs:3819-3860`) builds the diagnostic response by cloning the loaded `WorkingMemoryState` and clearing `excluded_memories` when `include_excluded=false`:

```
let mut diagnostic_state = state.clone();
if !parsed.include_excluded {
    diagnostic_state.excluded_memories.clear();
}
```

This is the only conditional that changes the visible shape — `selected_memories`, `budget_decisions`, `interpreted_intent`, and the rest of the state are unaffected by the flag. Verified by `diagnostic_mode_returns_full_state_and_include_excluded_controls_visibility`:

- With `include_excluded` omitted (defaults to `false`), `state.excluded_memories.len() == 0` in the response payload.
- With `include_excluded: true`, the excluded entries appear with their reasons (test asserts `excluded[0]["exclusion_reason"] == "compressed: budget=120 tokens"`).

Excluded entries do not leak into any other field in either mode — the test reads `state.excluded_memories` directly, and the only producer of that field in the response is the line above.

### Event-payload diffs carry inclusion and exclusion reasons

`emit_memory_retrieved` (`event_hooks.rs:54-97`) and `emit_memory_expanded` (`event_hooks.rs:99-151`) both compute their diffs via `diff_selected_memories` (`event_hooks.rs:169-188`) and `diff_excluded_memories` (`event_hooks.rs:190-209`). Each diff entry preserves the reason — `IncludedContextDelta.inclusion_reason` from `BundleResult.inclusion_reason`, `ExcludedContextDelta.exclusion_reason` from `ExcludedMemory.exclusion_reason`. The MemoryRetrieved/MemoryExpanded payload variants both have `included_context` / `excluded_context` non-`skip_serializing_if_empty` when populated.

Three event payloads sampled from the test capture (`retrieve_and_expand_emit_event_diffs_through_single_mutation_observer`):

1. `events[0]` — `kind == "memory_retrieved"`; `included_context.len() == 1`; `excluded_context.len() == 1`; `excluded_context[0].exclusion_reason == "compressed: budget=64 tokens"`.
2. `events[1]` — `kind == "memory_expanded"`; `included_context.len() == 1`; `excluded_context.len() == 1`; `excluded_context[0].exclusion_reason == "filtered: low confidence"`; `memory_id` populated.
3. `events[0].included_context[0]` — pulled from `IncludedContextDelta { identity: Memory(...), headline: "Memory memory-retrieved", inclusion_reason: "retrieved from query" }` (asserted implicitly by the assertion on `included_context.len()` combined with the fixture's unique reason string).

No-op mutations do not emit events: `evict_no_op_emits_no_mutation_or_event` in `budget_tests.rs:195-207` confirms the observer records nothing and `emit_memory_retrieved` returns `Ok(None)` when there is no diff.

## Findings

### Verification commands

All commands from T34–T37 run from `daemon/`:

1. `test -f daemon/crates/lattice-core/src/working_memory/mod.rs` — **pass** (40 lines).
2. `test -f daemon/crates/lattice-core/src/working_memory/state.rs` — **pass** (342 lines).
3. `cargo test -p lattice-core --lib working_memory::state_tests` — **pass**, 6 / 6 tests:

   ```
   test working_memory::state_tests::excluded_memories_require_exclusion_reason ... ok
   test working_memory::state_tests::default_constructed_state_has_required_fields ... ok
   test working_memory::state_tests::serde_round_trips_state_without_data_loss ... ok
   test working_memory::state_tests::loading_checkpoint_with_unknown_state_version_is_rejected ... ok
   test working_memory::state_tests::save_then_load_checkpoint_returns_byte_identical_state ... ok
   test working_memory::state_tests::state_hash_matches_between_save_and_load ... ok
   test result: ok. 6 passed; 0 failed; 0 ignored
   ```

4. `test -f daemon/crates/lattice-core/src/working_memory/operations.rs` — **pass** (848 lines — over cap, see Coding-standard section).
5. `cargo test -p lattice-core --lib working_memory::operations_tests` — **pass**, 9 / 9 tests:

   ```
   test working_memory::operations_tests::evict_of_pinned_identity_without_force_is_refused ... ok
   test working_memory::operations_tests::checkpoint_name_versions_instead_of_overwriting ... ok
   test working_memory::operations_tests::evict_forced_identity_records_reason_and_absent_count ... ok
   test working_memory::operations_tests::filter_records_predicate_description ... ok
   test working_memory::operations_tests::expand_appends_new_high_ranked_candidates ... ok
   test working_memory::operations_tests::retrieve_populates_selected_and_excluded_memories ... ok
   test working_memory::operations_tests::checkpoint_round_trips_state_bytes ... ok
   test working_memory::operations_tests::summarize_returns_expected_fields ... ok
   test working_memory::operations_tests::pin_is_idempotent_and_compress_preserves_pinned_identity ... ok
   test result: ok. 9 passed; 0 failed; 0 ignored
   ```

6. `test -f daemon/crates/lattice-daemon/src/rpc/working_memory_tool.rs` — **pass** (103 lines).
7. `cargo build --release` — **pass** (whole workspace compiles cleanly; 1m20s).
8. `cargo test -p lattice-daemon --lib rpc::working_memory_tool_tests` — **pass**, 5 / 5 tests:

   ```
   test rpc::working_memory_tool_tests::compact_mode_returns_summary_and_snapshot_handle ... ok
   test rpc::working_memory_tool_tests::tools_list_registers_inspect_working_memory_schema ... ok
   test rpc::working_memory_tool_tests::retrieve_and_expand_emit_event_diffs_through_single_mutation_observer ... ok
   test rpc::working_memory_tool_tests::tool_falls_back_to_latest_checkpoint_when_session_cache_is_empty ... ok
   test rpc::working_memory_tool_tests::diagnostic_mode_returns_full_state_and_include_excluded_controls_visibility ... ok
   test result: ok. 5 passed; 0 failed; 0 ignored
   ```

9. `test -f daemon/crates/lattice-core/src/working_memory/budget_tests.rs` — **pass** (247 lines).
10. `test -f daemon/crates/lattice-core/src/working_memory/checkpoint_tests.rs` — **pass** (98 lines).
11. `cargo test -p lattice-core --lib working_memory::budget_tests` — **pass**, 14 / 14 tests:

    ```
    test working_memory::budget_tests::evict_refuses_pinned_identity_without_force ... ok
    test working_memory::budget_tests::evict_absent_identity_is_a_documented_no_op ... ok
    test working_memory::budget_tests::evict_no_op_emits_no_mutation_or_event ... ok
    test working_memory::budget_tests::forced_evict_records_force_in_exclusion_reason ... ok
    test working_memory::budget_tests::evict_present_identity_moves_it_to_excluded_with_reason ... ok
    test working_memory::budget_tests::evicted_identity_can_be_retrieved_again_in_same_session ... ok
    test working_memory::budget_tests::pin_survives_checkpoint_round_trip ... ok
    test working_memory::budget_tests::retrieve_moves_truncated_results_to_excluded_with_budget_reason ... ok
    test working_memory::budget_tests::budget_changes_only_affect_subsequent_operations ... ok
    test working_memory::budget_tests::pin_twice_is_idempotent ... ok
    test working_memory::budget_tests::retrieve_over_budget_bundle_keeps_truncated_report ... ok
    test working_memory::budget_tests::pin_survives_compress ... ok
    test working_memory::budget_tests::compress_honors_token_budget_and_updates_budget_decisions ... ok
    test working_memory::budget_tests::huge_candidate_set_is_bounded_by_compress ... ok
    test result: ok. 14 passed; 0 failed; 0 ignored
    ```

12. `cargo test -p lattice-core --lib working_memory::checkpoint_tests` — **pass**, 5 / 5 tests:

    ```
    test working_memory::checkpoint_tests::loading_unknown_checkpoint_id_returns_storage_error ... ok
    test working_memory::checkpoint_tests::loading_newer_state_version_is_rejected_clearly ... ok
    test working_memory::checkpoint_tests::loading_checkpoint_restores_selected_excluded_and_budget_together ... ok
    test working_memory::checkpoint_tests::checkpoints_with_same_name_get_distinct_ids_and_round_trip ... ok
    test working_memory::checkpoint_tests::checkpoint_round_trip_is_byte_identical ... ok
    test result: ok. 5 passed; 0 failed; 0 ignored
    ```

### Checkpoint integrity

- Schema columns (`schema.sql:6-16`) match T34's documented layout: `(checkpoint_id, workspace_id, session_id, task_id, checkpoint_name, created_at, state_version, state_json, state_hash)` plus index on `(workspace_id, session_id, task_id, created_at DESC)` and `(state_hash)`.
- Round-trip byte-identical via canonical-JSON SHA-256 confirmed by `checkpoint_round_trip_is_byte_identical` (`checkpoint_tests.rs:17-26`) and reinforced by `save_then_load_checkpoint_returns_byte_identical_state` (`state_tests.rs:48-61`).
- Version mismatch rejected — `loading_newer_state_version_is_rejected_clearly` (`checkpoint_tests.rs:52-69`) and `loading_checkpoint_with_unknown_state_version_is_rejected` (`state_tests.rs:83-111`). Both assert the error string contains `"Unknown working memory state_version"`.

### MCP surface compliance (spec §9)

- Tool registration appears in `tools/list` — `tools_list_registers_inspect_working_memory_schema` (`working_memory_tool_tests.rs:38-66`) asserts the required parameters and the `mode` enum values.
- Response supports compact and diagnostic modes — `InspectWorkingMemoryCompactResponse` (`working_memory_tool.rs:22-29`) and `InspectWorkingMemoryDiagnosticResponse` (`working_memory_tool.rs:31-38`).
- `expansion_handle` is included in every response and round-trips — `compact_mode_returns_summary_and_snapshot_handle` (`working_memory_tool_tests.rs:69-109`) asserts the handle resolves back to the stored snapshot.
- Budget controls — the working-memory state respects shaper pins and compresses through the Phase 4 shaper; the MCP tool is read-only so no separate budget control is added at this layer (correct per T36 scope).

### Open observations (not blockers)

These show Phase 5 is wired through to production for inspection but the *mutation* pipeline does not yet have a production consumer. That is intentional per phase scoping (operations are a library; the consumer is Phase 8 Workflow Engine V2), but two artifacts of that gap deserve a note:

- `event_hooks::is_backpressure_error` (`event_hooks.rs:153-167`) is unused in production wiring. Tests exercise it through their `EventHookObserver` (`working_memory_tool_tests.rs:445-501`) but no production caller subscribes to mutation events yet. When Phase 8 wires the production observer, the backpressure helper must be the wrap that converts `EventWriteError` into a logged drop per T36's hot-path discipline. Until then, the helper is at risk of bit-rotting.
- The MCP handler in `mcp.rs:3862-3924` reads `working_memory_states` from a cache that has no producer in production code — only the `#[cfg(test)]` helper `remember_working_memory_state_for_test` populates it. In production today, every call to `inspect_working_memory` therefore hits the checkpoint-fallback path. That is correct for Phase 5 scope (no operation tool is exposed yet), but it means a Phase 8 task must wire the operations crate into the live MCP session so the cache is populated before retrieval / expand / compress are user-callable.

## Verdict

**Pass with follow-ups.** Phase 5 deliverables are spec-faithful, the eleven state fields are present, the eight operations are implemented with auditable excluded-context tracking, the MCP tool exposes both compact and diagnostic modes with `include_excluded` and an `expansion_handle`, event hooks emit `MemoryRetrieved` / `MemoryExpanded` payloads with included / excluded diffs and reasons, and checkpoints round-trip byte-identically with version rejection. All twelve verification commands pass. Phase 6 (Consolidation Engine) is unblocked.

The follow-ups below are coding-standard violations that the build harness should track but that do not invalidate Phase 5's contract.

### Follow-ups (attach to specific subsequent tasks)

1. **T35 follow-up — split `operations.rs` into per-operation modules.**
   File is 848 lines (over the 800-line cap explicitly named in T35's coding-standard checks). T35 already documents the resolution: "split per-operation modules under `operations/` along operation boundaries." Move each of `retrieve`, `summarize`, `filter`, `pin`, `evict`, `expand`, `compress`, `checkpoint` plus the `PhaseFourRetriever` glue into its own file under `working_memory/operations/`. While splitting, shorten `PhaseFourRetriever::run_pipeline` (60 lines including signature) by extracting the candidate-ranking and shaping steps into helpers.

2. **T35 / T37 follow-up — deduplicate test fixtures with `tests_common.rs`.**
   `operations_tests.rs` re-declares `FakeRetriever`, `Harness`, `Observer`, `sample_state`, `sample_bundle_result`, `sample_bundle`, `sample_identity`, `file_identity`, and `long_bundle_result` already present in `tests_common.rs`. Single-source-of-truth was an explicit requirement in both T35 and T37. Migrate `operations_tests.rs` to import from `tests_common.rs` (rename the in-tests-common `RecordingObserver` if needed, or rename the in-`operations_tests` one consistently).

3. **T37 follow-up — bring two tests under the 30-line cap.**
   - `working_memory/operations_tests.rs::pin_is_idempotent_and_compress_preserves_pinned_identity` (41 lines) — extract the three-`pin` arrangement into a fixture helper.
   - `daemon/rpc/working_memory_tool_tests.rs::retrieve_and_expand_emit_event_diffs_through_single_mutation_observer` (76 lines) — split into one test per emitted event kind, with a shared `WorkingMemoryHarness::new_with_events()` builder.

4. **T36 follow-up — bring `emit_memory_expanded` under the 50-line cap.**
   `event_hooks.rs:99-151` is 53 lines. Extract the shared `included`/`excluded` payload setup (common with `emit_memory_retrieved`) into a helper that returns a `(Vec<IncludedContextDelta>, Vec<ExcludedContextDelta>)` plus the envelope; the two `emit_*` functions become assemblers around it.

5. **Phase 8 (T-future, when Workflow Engine V2 wires production consumers) — exercise `is_backpressure_error`.**
   The hot-path-discipline contract in T36 ("backpressure → logged drop, not failed operation") is enforced by `is_backpressure_error`, but no production caller wraps the writer yet. When Phase 8 introduces the production mutation observer, it must call `is_backpressure_error` and `warn!` on hit, and a regression test must exercise the path (simulate `SQLITE_BUSY` / `SQLITE_LOCKED` against the event store and assert the operation succeeds with a single `warn!`).
