# R63 — Phase 8 Workflow Engine V2 review

This review covers the redesigned workflow tools (T56, T57), the new memory tools (T58, T59, T60), the audited tool surface (T61), and the integrated outcome recorder + composition tests (T62) against [docs/plans/2026-05-16-cognitive-workspace-fork-plan.md `## Phase 8: Workflow Engine V2`](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-8-workflow-engine-v2), [`## MCP Surface`](../../2026-05-16-cognitive-workspace-fork-plan.md#mcp-surface), and [`## MCP Tool Contract Principles`](../../2026-05-16-cognitive-workspace-fork-plan.md#mcp-tool-contract-principles). It also cross-references [docs/architecture/2026-05-16-mcp-tool-reference.md](../../../architecture/2026-05-16-mcp-tool-reference.md), [docs/architecture/2026-05-16-mcp-surface-audit.md](../../../architecture/2026-05-16-mcp-surface-audit.md), and [docs/architecture/2026-05-16-mcp-compatibility-policy.md](../../../architecture/2026-05-16-mcp-compatibility-policy.md).

## Definition of done (verbatim)

From [`## Phase 8: Workflow Engine V2`](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-8-workflow-engine-v2):

> Definition of done:
>
> - common coding tasks require fewer manual discovery calls
> - workflows produce enough context to implement and verify changes without broad file dumping

## Spec alignment

Walks every Phase 8 deliverable from spec [`## Phase 8: Workflow Engine V2`](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-8-workflow-engine-v2) and cites the implementing files.

| Deliverable | Implementation | Status |
|---|---|---|
| Redesigned `context` | `daemon/crates/lattice-daemon/src/rpc/workflow_v2/context_capsule.rs`; dispatcher at `daemon/crates/lattice-daemon/src/rpc/mcp.rs:1453`; tool definition at `mcp.rs:344–369`. | Implemented. |
| Redesigned `change planning` (`prepare_change` + `plan_edit`) | `daemon/crates/lattice-daemon/src/rpc/workflow_v2/prepare_change.rs`, `daemon/crates/lattice-daemon/src/rpc/workflow_v2/plan_edit.rs`; dispatcher `mcp.rs:1454–1455`. | Implemented. |
| Redesigned `scenario tracing` | `daemon/crates/lattice-daemon/src/rpc/workflow_v2/trace_scenario.rs`; dispatcher `mcp.rs:1456`. | Implemented. |
| Redesigned `failure diagnosis` | `daemon/crates/lattice-daemon/src/rpc/workflow_v2/diagnose_failure.rs`; dispatcher `mcp.rs:1466`. | Implemented. |
| Redesigned `docs retrieval` | `daemon/crates/lattice-daemon/src/rpc/workflow_v2/docs_capsule.rs`; dispatcher `mcp.rs:1462`. | Implemented. |
| Redesigned `test selection` | `daemon/crates/lattice-daemon/src/rpc/workflow_v2/relevant_tests.rs`; dispatcher `mcp.rs:1457`. | Implemented. |
| Redesigned `impact analysis` | `daemon/crates/lattice-daemon/src/rpc/workflow_v2/impact_from_diff.rs`; dispatcher `mcp.rs:1458`. | Implemented. |
| Redesigned `playbook generation` | `daemon/crates/lattice-daemon/src/rpc/mcp.rs:1461` (`get_repo_playbook`); also surfaced via `summarize_subsystem` at `mcp.rs:1460`. | Implemented (playbook is delivered by the existing tool; no separate `workflow_v2` module is needed because the playbook bundle composes graph and memory through shared helpers in `workflow_v2/mod.rs`). |
| Workflow composition over graph, event, memory, and working-memory substrates | Shared `WorkflowBundle` and `emit_standard_events` in `daemon/crates/lattice-daemon/src/rpc/workflow_v2/mod.rs:124–336`; composition coverage in `daemon/crates/lattice-daemon/src/rpc/workflow_v2/composition_tests.rs`. | Implemented. |
| One-call edit-planning bundles with code, docs, tests, memory, risks, and verification commands | `WorkflowBundle.verification_commands` (`workflow_v2/mod.rs:497–523`), `bundle_from_task` (`workflow_v2/mod.rs:525–582`), and `plan_edit::run` (`workflow_v2/plan_edit.rs:13–51`). Bundles include `memory_highlights`, `risks`, `event_episodes`, `verification_commands`, `stable_handles`, `workflow_record`, and `structured_payload`. | Implemented. |
| Workflow outcome recording integrated by default | `daemon/crates/lattice-daemon/src/rpc/workflow_v2/outcome_capture.rs:58–143` plus `should_record` whitelist at `outcome_capture.rs:63–87` covering every Phase 8 workflow tool. Integrated through `record_tool_metrics` in `mcp.rs`. | Implemented. |
| Phase 8 memory tools — `get_task_memory`, `save_memory`, `propose_memory_evolution` (+ apply shim), `verify_explain_memory` (+ verify/explain shims), `list_memory_conflicts`, `consolidate_session`, `get_memory_metrics`, `get_event_trace` | `daemon/crates/lattice-daemon/src/rpc/memory_v2/{get_task_memory,save_memory,propose_memory_evolution,verify_explain_memory,list_memory_conflicts,consolidate_session,get_memory_metrics,get_event_trace}.rs`; dispatcher `mcp.rs:1491–1520`; tool definitions added in `mcp.rs:1427–1437`. | Implemented at the surface level; **multiple runtime defects in the `save_memory` and `propose_memory_evolution` paths — see `## Findings`**. |

### Definition-of-done assessment

- `common coding tasks require fewer manual discovery calls` — workflow tools `prepare_change`, `plan_edit`, `diagnose_failure`, `trace_scenario`, `impact_from_diff` return a complete `WorkflowBundle` with ranked pivots, supporting context, memory highlights, risks, recommended verification commands, and a stable expansion handle (`workflow_v2/mod.rs:124–152`). The bundle eliminates the need to call `search_symbols`, `get_skeleton`, `find_relevant_tests`, and `search_memory` separately for common fix/add/refactor work, and a follow-up `expand_context` reuses the stored handle.
- `workflows produce enough context to implement and verify changes without broad file dumping` — workflow bundles surface concrete edit spans (`plan_edit::ordered_steps`), tests with file paths (`find_relevant_tests`), and verification commands inferred from the affected build targets (`workflow_v2/mod.rs:497–523`). The recorder ensures the workflow event sequence `AssistantTaskStarted → ToolCalled → ContextBundleReturned → MemoryRetrieved → PlanCreated` is recorded for every workflow call (`workflow_v2/mod.rs:330–336`).

The runtime defects identified under `## Findings` mean the DoD is **not satisfied for memory_v2 tools** today: `save_memory`, `get_task_memory`, and the `propose_memory_evolution` lifecycle return errors for plausible inputs that the published reference accepts.

## Coding-standard alignment

Per-file pass over `daemon/crates/lattice-daemon/src/rpc/workflow_v2/` and `daemon/crates/lattice-daemon/src/rpc/memory_v2/` against the Cadres coding standard (`## Hard limits`, `## Single source of truth`, `## No broken windows`, `## Schema / model parity`, `## Tests as documentation`, `## Observability`).

### workflow_v2/

| File | Lines | Largest fn | Findings |
|---|---|---|---|
| `mod.rs` | 643 | `bundle_from_task` (58 lines incl. struct-literal lines) | Schema parity: every public type derives `Serialize`/`Deserialize`. No suppressions. Within file/function limits. |
| `prepare_change.rs` | 19 | `run` (10) | Within limits. |
| `plan_edit.rs` | 99 | `run` (39) | Within limits. |
| `trace_scenario.rs` | 74 | `run` (32) | Within limits. |
| `diagnose_failure.rs` | 82 | `run` (28) | Within limits. |
| `context_capsule.rs` | 109 | `compose` (75) | **Function length violation:** `compose` is ~75 lines counting nested literal construction (`workflow_v2/context_capsule.rs:10–84`). Refactor into local helpers (`build_pivots`, `build_context`, `build_handles`) to drop under 50. |
| `docs_capsule.rs` | 70 | (struct constants only) | Within limits. |
| `relevant_tests.rs` | 80 | (struct constants only) | Within limits. |
| `impact_from_diff.rs` | 139 | `compose` (94) | **Function length violation:** `compose` is ~94 lines (`workflow_v2/impact_from_diff.rs:10–103`). Extract `affected_docs`, `impact_pivots`, `risk_notes` helpers. |
| `outcome_capture.rs` | 507 | `prepare` (~52) | One function (`prepare`) sits right at the 50-line threshold (`outcome_capture.rs:145–196`); split parsing into helpers to stay comfortably under. Observability span at `outcome_capture.rs:101–113` is good. |
| `composition_tests.rs` (test) | 823 | many | **File length violation (823 > 800).** **Six test functions exceed the 30-line test ceiling** at lines 31 (75), 107 (55), 163 (45), 209 (59), 269 (61), 331 (122). Extract shared fixture builders or split per scenario file. |
| `edit_workflows_tests.rs` (test) | 301 | `workflow_events_are_emitted_in_order_under_budget` and `prepare_change_returns_complete_workflow_bundle` exceed 30 lines | **Test length violations.** Largest function spans ~121 lines (`edit_workflows_tests.rs:88–208`). |
| `discovery_workflows_tests.rs` (test) | 278 | `stale_memories_are_labeled_when_present` and others | **Test length violations.** A 119-line test at `discovery_workflows_tests.rs:69–187`. |

Other workflow_v2 coding-standard checks:

- No `TODO`/`FIXME`/`XXX`, no commented-out code, no `#[allow(...)]` suppressions in any workflow_v2 file (verified with `grep -nE "TODO|FIXME|XXX|allow\("`).
- Public types in `workflow_v2/mod.rs` (`WorkflowBundle`, `Pivot`, `ContextItem`, `MemoryHighlight`, `EventEpisode`, `RiskNote`, `ExpansionHint`, `StableIdentity`, `RenderChoice`, `WorkflowRecord`, `WorkflowRequest`, `VecEventSink`) all derive `Serialize` and `Deserialize`; schema-parity rule from `## Schema / model parity` satisfied.
- Observability: `outcome_capture.rs:101–113` emits a `tracing::info!` event per recorded outcome with tool, task, session, outcome, excluded count, and latency_ms. `emit_standard_events` (`workflow_v2/mod.rs:330–336`) emits the spec-required workflow event sequence into the sink.
- Hot-path budget: `workflow_v2/edit_workflows_tests.rs:50–61` enforces `start.elapsed().as_millis() < 5` on the prepare/plan/trace/diagnose path, satisfying the T56 budget requirement.
- Recorder budget: `workflow_v2/composition_tests.rs:331–452` enforces p99 ≤ 5,000 µs on the outcome recorder, satisfying the T62 budget requirement.

### memory_v2/

| File | Lines | Largest fn | Findings |
|---|---|---|---|
| `mod.rs` | 263 | `evidence_strength` (16) | Within limits. Schema-parity OK (all public types derive serde). |
| `get_task_memory.rs` | 311 | `rank_memories` (100) | **Function length violation:** `rank_memories` is 100 lines (`get_task_memory.rs:196–295`). Also `build_response` at `get_task_memory.rs:89–149` is ~60 lines. Refactor into per-signal scoring helpers (`score_active_files`, `score_active_symbols`, `score_task_terms`, `score_verification_status`). |
| `save_memory.rs` | 298 | `execute` (53) | **Function length violation:** `execute` is ~53 lines (`save_memory.rs:105–157`). Borderline; extract `validate` and `record_event` blocks. |
| `propose_memory_evolution.rs` | 263 | `build_proposal` (33) | Within function-length limits. |
| `verify_explain_memory.rs` | **1006** | `verify_and_cache` (37), `build_checks` (29), `push_symbol_checks` (47), `push_file_checks` (41), `push_doc_checks` (42), `push_test_checks` (35), `push_span_checks` (37) | **File length violation (1006 > 800).** Split into `verify_explain_memory/{mod.rs, request.rs, render.rs, checks.rs, phase7_bridge.rs}` along the natural boundaries that already exist in the file. All functions stay under 50. |
| `list_memory_conflicts.rs` | 601 | `build_response` (75), `collect_contradictions` (68), `collect_supersession_chain` (56), `assemble_page` (52) | **Multiple function length violations.** Split into `list_memory_conflicts/{request.rs, render.rs, contradictions.rs, supersession.rs}`. |
| `consolidate_session.rs` | 353 | `materialize_proposals` (58) | **Function length violation** at `consolidate_session.rs:230–289`. Extract `materialize_observation`, `materialize_workflow_outcome`. |
| `get_memory_metrics.rs` | 211 | `compute_signals` (38) | Within limits. |
| `get_event_trace.rs` | 204 | `execute` (49) | Within limits (right at the threshold). |
| `memory_tools_tests.rs` (test) | 498 | ~70-line tests | **Test length violations** across multiple tests; same factor-into-fixtures fix as the workflow_v2 tests. |
| `verify_explain_tests.rs` (test) | 610 | ~70-line tests | **Test length violations.** |
| `admin_tools_tests.rs` (test) | 451 | several large fixtures | **Test length violations** in `consolidate_session_emits_proposals_without_direct_writes_and_proposals_are_actionable` and `get_event_trace_enforces_workspace_boundary_and_has_stable_pagination`. |

Other memory_v2 coding-standard checks:

- No `TODO`/`FIXME`/`XXX`, no commented-out code, no `#[allow(...)]` suppressions in any memory_v2 file.
- All public types (`TaskMemoryBundle`, `MemoryRecord`, `EvolutionAction`, `EvolutionProposal`, etc.) derive serde. Schema parity with `MemoryStructuredFields` is preserved by deferring to the canonical `lattice-core::memory::model` types.
- Observability: `consolidate_session_v2`, `verify_explain_memory`, `list_memory_conflicts` emit `tracing::info!` spans with tool, scope, status, and result counts (`mcp.rs:4597–4604`, `mcp.rs:4674–4680`).
- The dispatcher span at `mcp.rs:1448` (`tracing::info_span!("tool", name = tool_name)`) wraps every workflow call so generic event capture and metric recording happen under a named span.

### Build status

```
cd daemon && cargo build --release
    Finished `release` profile [optimized] target(s) in 1m 58s
```

Output: success. Existing dead-code warnings remain in `daemon/crates/lattice-daemon/src/rpc/identity_payload.rs:13–229` (10 entries) and `daemon/crates/lattice-daemon/src/rpc/memory_v2/get_memory_metrics.rs:75` (`MetricSignal::as_str`) and `daemon/crates/lattice-daemon/src/rpc/memory_v2/get_task_memory.rs:24` (`RankedMemory` struct) and `daemon/crates/lattice-daemon/src/rpc/memory_v2/get_task_memory.rs:168` (`token_budget_limit`). Per Cadres standard `## No broken windows`, those unused symbols should either be wired up or deleted as part of the reopen of the failing tasks below — they indicate incomplete integration of new helpers added during T58/T60.

### Test status

```
cargo test -p lattice-daemon --lib "rpc::workflow_v2"
test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 77 filtered out
```

```
cargo test -p lattice-daemon --lib "rpc::memory_v2"
test result: FAILED. 13 passed; 4 failed; 0 ignored; 0 measured; 78 filtered out
```

The four failures (full output captured below in `## Findings`):

- `rpc::memory_v2::memory_tools_tests::save_memory_defaults_unverified_persists_fields_emits_event_and_queues_verification`
- `rpc::memory_v2::memory_tools_tests::propose_apply_and_reject_memory_evolution_are_auditable`
- `rpc::memory_v2::memory_tools_tests::apply_memory_evolution_shim_forwards_with_deprecation_warning`
- `rpc::memory_v2::memory_tools_tests::get_task_memory_surfaces_inclusion_reason_and_verification_status`

## Tool-surface size

The Phase 8 advertised tool surface assembled in `daemon/crates/lattice-daemon/src/rpc/mcp.rs::handle_tools_list` contains the following 49 tools. The count and the names match [docs/architecture/2026-05-16-mcp-tool-reference.md `## Final tool list`](../../../architecture/2026-05-16-mcp-tool-reference.md#final-tool-list) exactly.

### Advertised tools (from `mcp.rs:280–1438`)

1. `get_context_capsule`
2. `prepare_change`
3. `plan_edit`
4. `trace_scenario`
5. `find_relevant_tests`
6. `impact_from_diff`
7. `get_working_set_context`
8. `summarize_subsystem`
9. `get_repo_playbook`
10. `get_docs_capsule`
11. `get_backlinks`
12. `get_outgoing_links`
13. `find_stale_docs`
14. `diagnose_failure`
15. `record_workflow_outcome`
16. `expand_context`
17. `get_symbol`
18. `get_dependents`
19. `get_dependencies`
20. `get_impact_graph`
21. `search_symbols`
22. `get_skeleton`
23. `save_observation`
24. `get_session_context`
25. `search_memory`
26. `search_logic_flow`
27. `submit_lsp_edges`
28. `workspace_setup`
29. `index_status`
30. `get_session_metrics`
31. `get_project_rules`
32. `inspect_working_memory` (via `working_memory_tool::tool_definition()` at `mcp.rs:1258`)
33. `list_observations`
34. `list_stale_memories`
35. `promote_observation`
36. `refresh_memory`
37. `delete_observation`
38. `update_observation`
39. `consolidate_session` (memory_v2)
40. `get_memory_metrics` (memory_v2)
41. `get_event_trace` (memory_v2)
42. `get_task_memory` (memory_v2)
43. `save_memory` (memory_v2)
44. `propose_memory_evolution` (memory_v2)
45. `apply_memory_evolution` (memory_v2, deprecated shim)
46. `verify_explain_memory` (memory_v2)
47. `verify_memory` (memory_v2, deprecated shim)
48. `explain_memory` (memory_v2, deprecated shim)
49. `list_memory_conflicts` (memory_v2)

### Callable-only legacy aliases (from `mcp.rs::handle_tools_call`)

These match the five aliases recorded under `## Callable deprecated aliases` in the reference and are governed by `2026-05-16-mcp-compatibility-policy.md`:

- `query_context` → `get_context_capsule` (`mcp.rs:1453`)
- `blast_radius` → `get_impact_graph` (`mcp.rs:1472`)
- `get_file_context` → `get_skeleton` (`mcp.rs:1474`)
- `store_memory` → `save_observation` (`mcp.rs:1475`)
- `recall_memories` → `search_memory` (`mcp.rs:1477`)

### Collapse decisions from T61 — implementation status

| Audit decision | Implementation | Status |
|---|---|---|
| Collapse `propose_memory_evolution` + `apply_memory_evolution` into `propose_memory_evolution(action=…)` | Canonical tool at `memory_v2/propose_memory_evolution.rs:56–82` accepts `action: "propose"|"apply"|"reject"`. Deprecation shim at `memory_v2/propose_memory_evolution.rs:84–98` advertises `apply_memory_evolution`; the dispatcher (`mcp.rs:1499–1500`) routes the shim through `tool_apply_memory_evolution_v2` (`mcp.rs:4364–4382`) which parses the shim args, forwards to the canonical handler, and attaches `deprecation_warning`. | Implemented as documented. |
| Collapse `verify_memory` + `explain_memory` into `verify_explain_memory(mode=…)` | Canonical tool at `memory_v2/verify_explain_memory.rs:172–209`; deprecation shims at `verify_explain_memory.rs:211–242`. Dispatcher routes `verify_memory` and `explain_memory` through `tool_verify_memory` and `tool_explain_memory` (`mcp.rs:4635–4665`) which force the appropriate `mode` and attach `deprecation_warning`. | Implemented as documented. |

### Reference vs implementation reconciliation

- Every tool advertised in `mcp.rs::handle_tools_list` appears as a row in `2026-05-16-mcp-tool-reference.md ## Final tool list`.
- Every tool listed in the reference table is callable via `mcp.rs::handle_tools_call`.
- The five callable-only aliases listed in the reference under `## Callable deprecated aliases` match the dispatcher table.

No reference/code drift was detected. The audit collapse decisions are honored. The shim policy in `2026-05-16-mcp-compatibility-policy.md` is followed: `apply_memory_evolution`, `verify_memory`, and `explain_memory` remain advertised with `deprecation_warning` attached, and their removal deadline is governed by `## Shim removal protocol`.

## Findings

### F-1 — `save_memory` rejects spec-conformant `memory_class` and `assertion_type` values (T58 defect)

- File: `daemon/crates/lattice-daemon/src/rpc/memory_v2/save_memory.rs`
- Spec violation: [`## MCP Tool Contract Principles`](../../2026-05-16-cognitive-workspace-fork-plan.md#mcp-tool-contract-principles) requires that memory-bearing responses identify *scope*, *evidence strength*, *verification status*, and *contradiction state*; the `save_memory` schema in `mcp.rs:1431` and the reference at `2026-05-16-mcp-tool-reference.md:55` advertise the snake_case wire form. The current implementation deserializes `MemoryClass` and `MemoryAssertionType` directly through `lattice-core::memory::model`, which serializes with CamelCase variant names.
- Reproduction: `cargo test -p lattice-daemon --lib rpc::memory_v2::memory_tools_tests::save_memory_defaults_unverified_persists_fields_emits_event_and_queues_verification` returns the runtime error: `Invalid save_memory arguments: unknown variant 'constraint', expected one of 'Observation', 'Decision', 'Exploration', 'Pattern', 'AntiPattern', 'WorkflowOutcome', 'Constraint', 'Hypothesis', 'Procedure', 'Outcome', 'Preference', 'Question', 'Counter'`.
- Required fix: add `#[serde(rename_all = "snake_case")]` (or a dedicated wire enum mapped to `MemoryClass`/`MemoryAssertionType`) so the public surface matches the documented snake_case wire form. The fix must also be added to `propose_memory_evolution`'s evolution proposal accept path and to any other tool that takes `MemoryClass`/`MemoryAssertionType` over the wire.
- Triggers reopen of: **T58**.

### F-2 — `propose_memory_evolution(action="propose")` returns a FOREIGN KEY violation when persisting the proposal (T58 defect)

- File: `daemon/crates/lattice-daemon/src/rpc/memory_v2/propose_memory_evolution.rs:165–197`; the proposal persistence goes through `lattice_core::consolidation::ConsolidationProposal` (called from `mcp.rs::tool_propose_memory_evolution_v2`).
- Spec violation: [`## Phase 8: Workflow Engine V2`](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-8-workflow-engine-v2) requires workflow outcome recording integrated by default; the proposal lifecycle is the canonical Phase 8 memory-evolution path and must succeed for plausible inputs. The reference (`2026-05-16-mcp-tool-reference.md:56`) describes the proposal lifecycle as canonical.
- Reproduction: `cargo test -p lattice-daemon --lib rpc::memory_v2::memory_tools_tests::propose_apply_and_reject_memory_evolution_are_auditable` returns `Failed to persist proposal: Storage error: Failed to insert proposal: FOREIGN KEY constraint failed`.
- Likely root cause: `ConsolidationProposal::store` references a parent row (likely a `consolidation_jobs` row) that is not created by `build_proposal` at `propose_memory_evolution.rs:182–196` (the synthetic `job_id` is formatted but the corresponding row is never inserted). The proposal-persist path must create or reuse a real job/parent row.
- Required fix: bind the proposal to an actual job parent record (or relax the FK with a dedicated migration), and add a regression test for the propose → apply → reject chain.
- Triggers reopen of: **T58** (and reopens `propose_memory_evolution` + `apply_memory_evolution` shim verification under that task).

### F-3 — `apply_memory_evolution` shim is blocked by F-2 (T58 defect)

- File: `daemon/crates/lattice-daemon/src/rpc/memory_v2/propose_memory_evolution.rs:84–98` (shim definition); dispatch at `mcp.rs:4364–4382`.
- Spec violation: same as F-2; in addition the shim deprecation contract from `2026-05-16-mcp-surface-audit.md ## Audit findings` requires the shim to forward correctly.
- Reproduction: `cargo test -p lattice-daemon --lib rpc::memory_v2::memory_tools_tests::apply_memory_evolution_shim_forwards_with_deprecation_warning` returns the same FK error as F-2; the shim cannot be exercised end-to-end until F-2 is fixed.
- Required fix: depends on F-2.
- Triggers reopen of: **T58**.

### F-4 — `get_task_memory` returns zero memories for a populated task scope (T58 defect)

- File: `daemon/crates/lattice-daemon/src/rpc/memory_v2/get_task_memory.rs:196–295`.
- Spec violation: [`## MCP Surface`](../../2026-05-16-cognitive-workspace-fork-plan.md#mcp-surface) and the reference at `2026-05-16-mcp-tool-reference.md:54` require `get_task_memory` to return the working memory plus relevant durable memory for the current task.
- Reproduction: `cargo test -p lattice-daemon --lib rpc::memory_v2::memory_tools_tests::get_task_memory_surfaces_inclusion_reason_and_verification_status` fails with `expected at least one surfaced memory`.
- Likely root cause: a combination of the F-1 serde issue (the structured field updater takes typed `MemoryClass::Constraint` but the rank/filter path may filter by serialized form) and a scope/branch filter mismatch in `rank_memories`. The dead-code warnings on `RankedMemory` and `token_budget_limit` (cargo build output) suggest the helpers were defined but never wired into the response path.
- Required fix: wire `RankedMemory`/`token_budget_limit` into `build_response` or remove them; add a regression test that asserts the surfaced count is non-zero given a seeded matching memory.
- Triggers reopen of: **T58**.

### F-5 — Multiple coding-standard hard-limit violations

- File length: `daemon/crates/lattice-daemon/src/rpc/memory_v2/verify_explain_memory.rs` (1006 lines) and `daemon/crates/lattice-daemon/src/rpc/workflow_v2/composition_tests.rs` (823 lines) both exceed the 800-line file ceiling from Cadres `## Hard limits`.
- Function length: `get_task_memory::rank_memories` (100 lines), `list_memory_conflicts::build_response` (75), `consolidate_session::materialize_proposals` (58), `save_memory::execute` (53), `impact_from_diff::compose` (94), `context_capsule::compose` (75) all exceed the 50-line function ceiling.
- Test length: ten test functions across `composition_tests.rs`, `edit_workflows_tests.rs`, `discovery_workflows_tests.rs`, `memory_tools_tests.rs`, `verify_explain_tests.rs`, `admin_tools_tests.rs` exceed the 30-line test ceiling. The largest is `every_outer_call_emits_exactly_one_outcome_event_and_excluded_candidates_are_recorded` at 122 lines.
- Required fix: extract per-section helpers and shared fixture builders. Test fixtures already exist in `composition_tests.rs:453–820`; new tests should use them instead of inlining setup.
- Triggers reopen of: refactor under **T56/T57/T58/T59/T60/T62** as the relevant authors land their fixes.

### F-6 — Dead-code warnings in newly added memory_v2 helpers

- File: `daemon/crates/lattice-daemon/src/rpc/memory_v2/get_task_memory.rs:24` (`RankedMemory` struct never constructed) and `get_task_memory.rs:168` (`token_budget_limit` never called); `memory_v2/get_memory_metrics.rs:75` (`MetricSignal::as_str` never used).
- Spec violation: Cadres `## No broken windows` — "no dead imports, dead symbols, dead routes, dead feature flags. Remove as part of the change that made them dead."
- Required fix: either wire these symbols into the relevant response paths or delete them. The presence of `RankedMemory`/`token_budget_limit` strongly suggests incomplete integration that is the proximate cause of F-4.
- Triggers reopen of: **T58** and **T60**.

### F-7 — Shim policy is honored; deadline tracking is correctly cross-referenced

- Files: `2026-05-16-mcp-surface-audit.md` cites `2026-05-16-mcp-compatibility-policy.md ## Shim removal protocol`; the tool reference at line 99 lists the three Phase 8 advertised shims (`apply_memory_evolution`, `verify_memory`, `explain_memory`) and the five callable-only legacy aliases.
- Status: pass. No fix required.

### F-8 — Hot-path budget assertions present and passing

- `workflow_v2/edit_workflows_tests.rs:50–61` asserts `start.elapsed().as_millis() < 5` over the prepare/plan/trace/diagnose edit-bundle path.
- `workflow_v2/composition_tests.rs:331–452` asserts p99 ≤ 5,000 µs for the outcome recorder over 64 iterations.
- Both pass under `cargo test --release` and `cargo test --lib`. T56 and T62 budget requirements satisfied.

### F-9 — Pre-existing release-build warnings should be cleaned

- Files: `daemon/crates/lattice-daemon/src/rpc/identity_payload.rs:13–229` contains a constant and nine helper functions all flagged unused at release. They predate Phase 8 but are part of the same MCP surface and contribute broken-window risk per `## No broken windows`.
- Recommendation: delete or wire them in a follow-up change. Not blocking for R63 but called out for visibility.

## Verdict

**fail**

Phase 8 advertises ten new memory tools and a redesigned workflow surface, but four of seventeen `rpc::memory_v2` tests fail under `cargo test -p lattice-daemon --lib "rpc::memory_v2"`. Three of them (`save_memory_defaults_unverified_persists_fields_emits_event_and_queues_verification`, `propose_apply_and_reject_memory_evolution_are_auditable`, `apply_memory_evolution_shim_forwards_with_deprecation_warning`) are direct contract failures of the documented schema and lifecycle in `2026-05-16-mcp-tool-reference.md`. The fourth (`get_task_memory_surfaces_inclusion_reason_and_verification_status`) means the canonical Phase 8 retrieval tool returns zero results for a plausible populated task scope. The corresponding T58 result file already self-classified as `partial`, which this review confirms and escalates: every Phase 8 surface row in the reference doc must round-trip end-to-end before Phase 9 metrics work can build on it.

Tasks to reopen:

- **T58** — required to fix F-1 (serde rename for `MemoryClass`/`MemoryAssertionType` over the wire), F-2 (FK constraint failure in proposal persistence), F-3 (transitively unblocks the apply shim), F-4 (wire `RankedMemory`/`token_budget_limit` or remove them and make `get_task_memory` return the expected memories for a populated scope), and F-6 (dead-code cleanup).
- **T56/T57/T62** — owners must fold the F-5 file-length and function-length refactors into their respective modules in the same change as the F-1..F-4 fixes (composition tests live with T62; discovery/edit tests live with T56/T57). A bundled refactor PR is appropriate per the Cadres `## Consistency over cleverness` and `## No broken windows` guidance.
- **T59 / T60** — confirm via re-run of `cargo test -p lattice-daemon --lib "rpc::memory_v2"` that the seven `verify_explain_tests` and four `admin_tools_tests` still pass after the T58 fixes, and shrink `verify_explain_memory.rs` (F-5) below the 800-line file ceiling.

After the reopens land, R63 should be re-run end-to-end and the `## Verdict` flipped to `pass` once `cargo test -p lattice-daemon --lib "rpc::memory_v2"` reports `0 failed` and no Cadres `## Hard limits` violations remain in workflow_v2 or memory_v2.
