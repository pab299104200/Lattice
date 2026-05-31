# Event Log Capture And Compaction

This note records the Phase 2 MCP event-capture contract. The compaction snapshot
format and scheduler are owned by T16; this section is intentionally limited to
the event sequences emitted by T15 so later compaction and retrieval work can
depend on stable history.

## MCP Capture Sequence

Every `tools/call` dispatched through `McpHandler::handle_tools_call` emits:

1. `AssistantTaskStarted` once for the session task.
2. `ToolCalled` before dispatch, with the tool name and compact input summary.
3. `ToolResult` after dispatch, including structured success or error status.
4. `WorkflowSucceeded` or `WorkflowFailed` for the terminal workflow outcome.

Tool errors still return through JSON-RPC unchanged. Capture write errors are
logged with `tracing::warn!` and do not mask the tool result.

## Tool Event Matrix

All currently dispatched MCP tools emit the base sequence above, including:

- Workflow bundles: `get_context_capsule`, `prepare_change`, `plan_edit`,
  `trace_scenario`, `impact_from_diff`, `get_working_set_context`,
  `summarize_subsystem`, `get_repo_playbook`, `get_docs_capsule`,
  `diagnose_failure`, `expand_context`.
- Discovery and graph tools: `find_relevant_tests`, `get_backlinks`,
  `get_outgoing_links`, `find_stale_docs`, `get_symbol`, `get_dependents`,
  `get_dependencies`, `get_impact_graph`, `search_symbols`, `get_skeleton`,
  `search_logic_flow`, `submit_lsp_edges`, `workspace_setup`, `index_status`,
  `get_project_rules`.
- Memory tools: `record_workflow_outcome`, `get_task_memory`,
  `search_memory`, `save_quick_memory`, `save_memory`,
  `propose_memory_evolution`, `list_stale_memories`,
  `list_memory_conflicts`, `verify_explain_memory`.
- Session surface: `get_session_metrics`.

Tools that return a stable context handle additionally emit
`ContextBundleReturned`. Tools that materialize memory ids in the returned
payload emit `MemoryRetrieved`. `plan_edit` emits `PlanCreated`.

## Durability And Hot Path

`ToolCalled`, `ToolResult`, and context bundle events use the batched writer
policy to avoid adding synchronous fsync latency to hot-path tools. Terminal
workflow and memory lifecycle events use synchronous flushes so downstream
history consumers can rely on the outcome before proceeding.

## Workspace Scope

`EventCapture::new` rejects writer/session workspace mismatches. Events are
written with the session id assigned at daemon startup and the workspace id of
the active root, so later readers can scope history by workspace and session
without relying on transient tool names or process-local handles.

## Phase 2 Budget Evidence

T17 adds ignored measurement-mode tests in
`daemon/crates/lattice-core/src/events/budget_tests.rs` and writes the captured
artifacts to `daemon/target/event_budget/`.

### Measurement environment

- CPU: Intel(R) Core(TM) i7-10610U CPU @ 1.80GHz
- Store mode: in-memory SQLite
- Writer mode: append-only `EventWriter::append` with
  `FlushPolicy::Batched { interval_ms: 250 }`
- SQLite semantics under this harness: `journal_mode=MEMORY`,
  `synchronous=NORMAL`
- Concurrency control: the ignored budget tests serialize themselves with a
  global mutex so each case measures the writer hot path instead of benchmark
  self-contention

### Captured P99 results

- `prepare_change`: payload 1514 bytes, 10_000 measured iterations,
  P50 664 us, P95 801 us, P99 3710 us
- `get_context_capsule`: payload 383 bytes, 10_000 measured iterations,
  P50 289 us, P95 353 us, P99 3314 us
- `prepare_change` spill path: payload 586 bytes with inline ceiling 256 bytes,
  2_000 measured iterations, P50 361 us, P95 3380 us, P99 3427 us

### Notes on the passing configuration

- The writer now hashes the already-serialized payload bytes instead of
  canonicalizing the same typed payload twice on every append.
- Spilled payload writes reuse a per-writer row-id cache keyed by payload hash so
  repeated large payloads do not re-hit the side-table lookup path after warmup.
- The reader replay path now orders streamed reads by append-only `event_id`,
  which aligns the public replay contract with the storage substrate’s total
  order.
