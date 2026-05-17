# Operator Guide

This guide covers setup and daily operation for the cognitive workspace successor. It is driven by [## Documentation Requirements](../plans/2026-05-16-cognitive-workspace-fork-plan.md#documentation-requirements), [## Phase 11: Hardening](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-11-hardening), and the architecture front door at [Successor Architecture Overview](../architecture/2026-05-16-successor-architecture-overview.md#overview).

## Installation

Build the daemon and extension from the repository root:

```bash
cd daemon && cargo build --release
cd ../extension && npm install && npm run compile
```

After building, use the canonical deploy procedure in [Operator Runbook](./2026-05-16-runbook.md#deploy-sequence).

If the installed VS Code extension directory is read-only, update the repo-local `extension/bin/lattice` copy and reinstall or refresh the extension through the operator-controlled deployment path.

## Initial setup

1. Open the target workspace in VS Code.
2. Confirm the extension starts the `lattice` daemon from `extension/bin/lattice`.
3. Run `index_status` to confirm workspace indexing health.
4. Run `get_repo_playbook` for a compact architecture and convention summary.
5. Run `get_context_capsule` or `prepare_change` for the first real task instead of opening broad source sets manually.
6. Open the review panel and confirm memory inbox, event trace, retrieval explanation, indexing health, graph health, and consolidation queue views render.

The MCP contract and tool list are in [MCP Tool Reference](../architecture/2026-05-16-mcp-tool-reference.md#final-tool-list). Tool compatibility rules are in [MCP Compatibility Policy](../architecture/2026-05-16-mcp-compatibility-policy.md#backward-compatibility).

## Daily operations

Use high-level workflow tools for assistant tasks:

- `get_context_capsule` for unfamiliar areas.
- `prepare_change` for implementation work once the area is known.
- `diagnose_failure` for stack traces, compiler diagnostics, and failing tests.
- `find_relevant_tests` and `impact_from_diff` before verification and review.
- `record_workflow_outcome` after successful or failed work worth preserving.

Operational rules:

- Prefer compact responses first; use diagnostic modes to investigate misses.
- Use `expand_context` for focused follow-up from returned handles.
- Treat stale, contradicted, superseded, expired, and invalidated memory labels as trust boundaries.
- Review high-scope memory proposals before applying them.
- Check `get_event_trace` when a workflow outcome, proposal, or retrieval result is difficult to explain.

## Review surface tour

The VS Code review UI is documented in [Extension Review UI Guide](./2026-05-16-extension-review-ui-guide.md#overview). Operators use it to inspect memory inbox entries, promotion proposals, contradiction proposals, stale memory, evidence, event traces, retrieval explanations, consolidation health, indexing health, and workspace graph health.

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

1. Daemon not responding: confirm the binary exists in `extension/bin/lattice`, restart VS Code, then run `index_status`.
2. Missing context: run `get_context_capsule` in diagnostic or focused mode and inspect excluded candidates.
3. Stale or wrong memory: open the stale view or evidence inspector, then run `verify_explain_memory`.
4. Proposal confusion: inspect promotion and contradiction queues, then open the event trace for the proposal source session.
5. Retrieval miss: open retrieval explanation and compare anchor resolution, candidate sources, ranking signals, and token-cost choices.
6. Scope concern: check workspace id, branch, memory scope, and event trace filters before trusting a result.

Recovery playbooks for corruption, partial indexing, and rollback are owned by the Phase 11 runbook pair, not this guide.
