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
2. Confirm the lightweight proxy starts or reuses the local daemon and that `status` with `scope=index` reports the intended workspace.
3. Run `context` with `mode=repo` for a compact architecture and convention summary.
4. Run `context` or `prepare_change` for the first real task instead of opening broad source sets manually.
5. Use `status`, `recall`, and `remember` for operator review until the phase-5 `lattice metrics` CLI lands.

The MCP contract and tool list are in [MCP Tool Reference](../architecture/2026-06-11-mcp-tool-reference.md#public-mcp-tools).

## Daily operations

Use high-level workflow tools for assistant tasks:

- `context` for unfamiliar areas, docs, rules, skeletons, working sets, and handle expansion.
- `prepare_change` for implementation work once the area is known.
- `diagnose` for stack traces, compiler diagnostics, and failing tests.
- `impact` before verification and review.
- `remember` after successful or failed work worth preserving.

Operational rules:

- Keep MCP clients on `--stdio`; `--daemon` is the internal long-lived process started by proxies.
- Use `LATTICE_DAEMON_ADDR` only when the default loopback listener `127.0.0.1:47659` conflicts with another local service.
- Bound resident daemon memory with `LATTICE_MAX_LOADED_WORKSPACES` and `LATTICE_WORKSPACE_IDLE_TTL_SECS`; defaults are `8` loaded runtimes and `1800` seconds idle TTL.
- Leave `LATTICE_ENABLE_BACKGROUND_VECTOR_SYNC` unset unless the operator explicitly wants full-graph semantic embedding sync during background indexing.
- Prefer compact responses first; use diagnostic modes to investigate misses.
- Use `context` with `mode=expand` for focused follow-up from returned handles.
- Treat stale, contradicted, superseded, expired, and invalidated memory labels as trust boundaries.
- Review high-scope memory proposals before applying them.
- Check `status` and, after phase 5, `lattice metrics` when workflow usage or retrieval behavior is difficult to explain.

## Review Surfaces

Operator review happens through MCP tools and CLI diagnostics. Use `status` for index/docs/memory health, `recall` for memory retrieval and verification, and `lattice doctor` for daemon/config checks.

## Metrics dashboard

Metrics are exposed by the phase-5 `lattice metrics` CLI and the Phase 9 reports described in [Metrics Report Architecture](../architecture/2026-05-16-metrics-report.md#architecture). Use the dashboard or report output to watch:

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

1. Daemon not responding: confirm the MCP client command points at `daemon/target/release/lattice --stdio --workspace <workspace>`, stop stale `lattice` processes, restart the client, then run `lattice doctor`.
2. High daemon memory: lower `LATTICE_WORKSPACE_IDLE_TTL_SECS`, lower `LATTICE_MAX_LOADED_WORKSPACES`, close idle MCP clients, and confirm logs show idle workspace eviction.
3. High daemon CPU: check whether initial indexing is still running; keep `LATTICE_ENABLE_BACKGROUND_VECTOR_SYNC` unset unless full semantic sync is intentionally scheduled.
4. Missing context: run `context` with focused or diagnostic options and inspect excluded candidates.
5. Stale or wrong memory: run `status` with `scope=memory` or `recall` with `mode=verify` and inspect the returned evidence.
6. Proposal confusion: inspect memory-evolution results through `remember`/`recall` and the daemon-internal event trace if needed.
7. Retrieval miss: use diagnostic render modes and compare anchor resolution, candidate sources, ranking signals, and token-cost choices.
8. Scope concern: check workspace id, branch, memory scope, and event trace filters before trusting a result.

Recovery playbooks for corruption, partial indexing, and rollback are owned by the Phase 11 runbook pair, not this guide.
