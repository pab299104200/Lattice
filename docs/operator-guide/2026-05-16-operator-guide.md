# Operator Guide

This guide covers daemon setup and daily operation for the cognitive workspace successor. It is driven by [## Documentation Requirements](../plans/2026-05-16-cognitive-workspace-fork-plan.md#documentation-requirements), [## Phase 11: Hardening](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-11-hardening), and the architecture front door at [Successor Architecture Overview](../architecture/2026-05-16-successor-architecture-overview.md#overview).

## Installation

Build the daemon from the repository root:

```bash
cd daemon && cargo build --release
```

After building, use the canonical restart procedure in [Operator Runbook](./2026-05-16-runbook.md#deploy-sequence) and point MCP clients at `daemon/target/release/lattice --stdio --workspace <workspace>`.

## Initial setup

1. Configure an MCP client to launch `daemon/target/release/lattice --stdio --workspace <workspace>`.
2. Confirm the lightweight proxy starts or reuses the local daemon and that `index_status` reports the intended workspace.
3. Run `get_repo_playbook` for a compact architecture and convention summary.
4. Run `get_context_capsule` or `prepare_change` for the first real task instead of opening broad source sets manually.
5. Use `get_memory_metrics`, `get_event_trace`, `list_stale_memories`, `list_memory_conflicts`, and `verify_explain_memory` for operator review.

The MCP contract and tool list are in [MCP Tool Reference](../architecture/2026-05-16-mcp-tool-reference.md#final-tool-list). Tool compatibility rules are in [MCP Compatibility Policy](../architecture/2026-05-16-mcp-compatibility-policy.md#backward-compatibility).

## Daily operations

Use high-level workflow tools for assistant tasks:

- `get_context_capsule` for unfamiliar areas.
- `prepare_change` for implementation work once the area is known.
- `diagnose_failure` for stack traces, compiler diagnostics, and failing tests.
- `find_relevant_tests` and `impact_from_diff` before verification and review.
- `record_workflow_outcome` after successful or failed work worth preserving.

Operational rules:

- Keep MCP clients on `--stdio`; `--daemon` is the internal long-lived process started by proxies.
- Use `LATTICE_DAEMON_ADDR` only when the default loopback listener `127.0.0.1:47659` conflicts with another local service.
- Bound resident daemon memory with `LATTICE_MAX_LOADED_WORKSPACES` and `LATTICE_WORKSPACE_IDLE_TTL_SECS`; defaults are `8` loaded runtimes and `1800` seconds idle TTL.
- Leave `LATTICE_ENABLE_BACKGROUND_VECTOR_SYNC` unset unless the operator explicitly wants full-graph semantic embedding sync during background indexing.
- Prefer compact responses first; use diagnostic modes to investigate misses.
- Use `expand_context` for focused follow-up from returned handles.
- Treat stale, contradicted, superseded, expired, and invalidated memory labels as trust boundaries.
- Review high-scope memory proposals before applying them.
- Check `get_event_trace` when a workflow outcome, proposal, or retrieval result is difficult to explain.

## Review Surfaces

Operator review happens through MCP tools. Use `get_memory_metrics` for signal health, `get_event_trace` for workflow provenance, `list_stale_memories` and `list_memory_conflicts` for memory quality queues, and `verify_explain_memory` before trusting high-scope or recently changed memory.

## Metrics dashboard

Metrics are available through `get_memory_metrics` and the Phase 9 reports described in [Metrics Report Architecture](../architecture/2026-05-16-metrics-report.md#architecture). Use the dashboard or report output to watch:

- tool calls per successful task
- irrelevant files opened per task
- relevant anchor recall
- memory inclusion precision
- memory later-used rate
- stale memory surfaced rate
- contradiction missed rate
- tests recommended versus tests needed
- workflow success after first plan

Benchmark operation is covered in [Benchmark Evaluation Guide](./2026-05-16-benchmark-evaluation-guide.md#overview).

## Troubleshooting

Use this order for common failures:

1. Daemon not responding: confirm the MCP client command points at `daemon/target/release/lattice --stdio --workspace <workspace>`, stop stale `lattice` processes, restart the client, then run `index_status`.
2. High daemon memory: lower `LATTICE_WORKSPACE_IDLE_TTL_SECS`, lower `LATTICE_MAX_LOADED_WORKSPACES`, close idle MCP clients, and confirm logs show idle workspace eviction.
3. High daemon CPU: check whether initial indexing is still running; keep `LATTICE_ENABLE_BACKGROUND_VECTOR_SYNC` unset unless full semantic sync is intentionally scheduled.
4. Missing context: run `get_context_capsule` in diagnostic or focused mode and inspect excluded candidates.
5. Stale or wrong memory: run `list_stale_memories` or `verify_explain_memory` and inspect the returned evidence.
6. Proposal confusion: inspect memory-evolution results and open the event trace for the proposal source session.
7. Retrieval miss: use diagnostic render modes and compare anchor resolution, candidate sources, ranking signals, and token-cost choices.
8. Scope concern: check workspace id, branch, memory scope, and event trace filters before trusting a result.

Recovery playbooks for corruption, partial indexing, and rollback are owned by the Phase 11 runbook pair, not this guide.
