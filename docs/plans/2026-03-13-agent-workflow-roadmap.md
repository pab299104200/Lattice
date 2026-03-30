# Lattice Agent Workflow Roadmap

**Date:** 2026-03-13
**Status:** In Progress
**Audience:** Lattice maintainers building for coding assistants

## Goal

Shift Lattice from a strong retrieval engine into a high-leverage workflow engine for coding assistants.

Primary success metrics:

- Reduce tool round-trips per task
- Reduce repeated context retrieval
- Improve targeted-edit token savings
- Improve durable, freshness-aware memory
- Preserve the existing precision-first retrieval behavior

## Baseline

Current strengths:

- `get_context_capsule` and `get_skeleton` save large amounts of context on discovery work
- Graph traversal and symbol-level retrieval are already strong
- Persistent memory and stale-memory marking exist

Current gaps:

- Targeted edits still require too many chained lookups
- Diff review and test selection are mostly manual
- Memory is persistent, but not yet scoped or freshness-aware enough for long-lived assistant use
- The VS Code extension exposes only a small fraction of the backend value

## Working Principles

1. Favor compound task-oriented tools over more low-level primitives.
2. Return structured compact responses first, with optional expansion.
3. Reuse existing graph, query, diff, and memory components instead of creating parallel systems.
4. Make every durable memory traceable to symbols, files, and freshness signals.
5. Optimize for assistant efficiency, not human-facing dashboard breadth.

## Roadmap

### Phase 0: Benchmarks And Contracts

Status: `[~]` in progress

- `[x]` Add agent-workflow benchmarks for prepare-change, diff review, test selection, and memory recall.
- `[~]` Track median tool calls per task, returned tokens per task, edit-file hit rate, and stale-memory precision.
- `[~]` Define stable response contracts for `TaskBundle`, `TestSelectionReport`, and `DiffImpactReport`.

### Phase 1: Highest-ROI Agent Tools

Status: `[~]` in progress

- `[x]` Add `prepare_change(query, entry_files?, entry_symbols?, mode?)`
- `[x]` Add `find_relevant_tests(files?, symbols?, diff?)`
- `[x]` Add `impact_from_diff(base?, head?, diff?)`
- `[x]` Add MCP schemas and compact/default output modes
- `[x]` Add extension commands for the new tools after daemon behavior stabilizes

Expected outcome:

- Collapse common “where do I edit / what else is affected / what do I test” workflows into one or two tool calls
- Raise targeted-edit savings meaningfully above the current baseline

### Phase 2: Memory 2.0

Status: `[~]` in progress

- `[~]` Extend memory with `scope`, `workspace_id`, `branch`, `linked_files`, `linked_commits`, and `refresh_key`
- `[x]` Add `promote_observation`, `list_stale_memories`, and `refresh_memory`
- `[x]` Mark memories stale by linked file/symbol changes, not just broad keyword matches
- `[x]` Support promotion from short-lived session notes to durable repo knowledge

Expected outcome:

- Better persistent memory with fewer stale recalls

### Phase 3: Working Set And Failure Triage

Status: `[~]` in progress

- `[x]` Add `get_working_set_context(scope?)`
- `[x]` Add `diagnose_failure(input, kind?)`
- `[x]` Add `expand_context(handle, focus, max_tokens?)`
- `[x]` Add daemon-side context handle caching for delta expansion

Expected outcome:

- Fewer repeated context fetches and better bug-fixing assistance

### Phase 4: Compression And Semantic Fallback

Status: `[~]` in progress

- `[~]` Add durable summaries for symbols, files, and subsystems
- `[ ]` Use embeddings as fallback for low-confidence queries instead of the default path
- `[x]` Add `summarize_subsystem` and `get_repo_playbook`
- `[~]` Distill accepted changes into reusable patterns and anti-patterns

Expected outcome:

- Smaller default payloads and better reuse of prior understanding

## Implementation Order

1. Land core workflow structs and heuristics in `daemon/crates/lattice-core/src/intelligence/`
2. Expose them through `daemon/crates/lattice-daemon/src/rpc/mcp.rs`
3. Add core tests before expanding UI
4. Add diff-aware workflows next
5. Upgrade memory scope and freshness
6. Add VS Code commands only after daemon outputs are stable

## UI And Metrics Guidance

- No additional human-facing UI is required to hit the primary goals. The meaningful gains come from daemon behavior, response shaping, memory quality, and lower assistant round-trips.
- If anything is surfaced in the UI, keep it operator/debug-focused rather than product-facing.

Recommended debug metrics to surface if needed:

- Approximate returned tokens per workflow call
- Estimated tokens saved versus a simple full-response baseline
- Delivery mode mix: `tiny` / `compact` / `full`
- Dense wire usage rate
- Single-anchor usage rate
- Compact-to-expand conversion rate
- Follow-up avoidance rate
- Context-handle reuse rate
- Outcome-memory reuse rate

Notes:

- Returned-token metrics are worth showing because they map directly to assistant cost and latency.
- "Tokens saved" is useful, but it should stay explicitly estimated, since it depends on the baseline and on model/provider tokenization details.

## Next Sprint

Status: `[x]` implemented in the daemon

Sprint goal:

- Make compact agent workflows smarter by default, more self-reinforcing over time, and easier to trust without increasing context load.

Sprint backlog:

1. `[x]` Auto-select compact vs wider responses from confidence and signal quality instead of relying only on an explicit mode flag.
2. `[x]` Auto-distill successful edit and test outcomes into durable repo-scope and branch-scope patterns.
3. `[x]` Return a suggested `expand_context` focus target when a compact workflow result is useful but incomplete.
4. `[x]` Extend session metrics to track follow-up avoidance, compact-to-expand conversion, and outcome-linked memory reuse.
5. `[x]` Use semantic fallback selectively only when summary/playbook confidence is weak or conflicting.

Sprint success metrics:

- Lower average compact workflow payload below the current ~`2980B` / ~`745` estimated tokens without dropping hit rates.
- Improve context-handle reuse after compact responses and across restarts.
- Increase the share of useful durable memory that comes from successful coding outcomes instead of manual observation writes.
- Keep `top3_hit_rate` and `target_hit_rate` at `100%` on the current synthetic harness.

Rationale:

- Raw compact-mode payload is better now, but the next efficiency jump comes from smarter defaults and fewer unnecessary follow-up calls.
- Restart-persistent handles are in place, so the next leverage point is making successful sessions feed future sessions automatically.
- The remaining product lift is agent behavior quality, not human-facing UI breadth.

Sprint outcome:

- Adaptive workflow metadata, durable outcome capture, suggested expand targets, and richer session metrics are now live in the daemon.
- Selective semantic fallback is wired into `prepare_change` and `summarize_subsystem`, where fuzzy query rescue is most valuable.
- The current synthetic benchmark still holds `100%` top-3 and target hit rates, but payload landed at about `3120B` / `781` estimated tokens, so the sprint improved behavior and continuity more than raw compact payload size.

## Progress Log

- `2026-03-13`: Roadmap created and committed to active implementation flow.
- `2026-03-13`: Starting Phase 1 backend scaffolding for `prepare_change` and test selection.
- `2026-03-13`: Added `prepare_change` and `find_relevant_tests` workflow logic under `lattice-core/src/intelligence/`.
- `2026-03-13`: Exposed the new workflow tools through MCP in `lattice-daemon/src/rpc/mcp.rs`.
- `2026-03-13`: Added passing Rust tests covering task-bundle ranking and diff-driven test selection.
- `2026-03-13`: Added `impact_from_diff` with unified diff parsing, changed-symbol mapping, downstream impact reporting, and test suggestions.
- `2026-03-13`: Extended memory records with scope, linked-file metadata, workspace/branch fields, and refresh keys in the core store.
- `2026-03-13`: Added MCP support for scoped memory writes plus `promote_observation` and `list_stale_memories`.
- `2026-03-13`: Hooked stale-memory detection to file changes as well as symbol changes, including comment-only file edits.
- `2026-03-13`: Verified the current Phase 1 + Memory 2.0 foundation with `cargo test --workspace`.
- `2026-03-13`: Added `get_working_set_context` to bundle active files, nearby symbols, relevant tests, and recalled memories into one compact MCP result.
- `2026-03-13`: Added passing Rust coverage for the working-set workflow and re-verified the full daemon workspace.
- `2026-03-13`: Added `diagnose_failure` to turn compiler errors, failing tests, and stack traces into suspect symbols, related code, and suggested tests.
- `2026-03-13`: Re-verified the expanded workflow surface with `cargo test --workspace` after adding failure triage.
- `2026-03-13`: Hardened SQLite memory recall for new sessions with connection busy timeouts, WAL checkpoint tuning, and file-backed regression coverage.
- `2026-03-13`: Queued the next backend priorities in order: `expand_context`, context-handle caching, benchmark harness, `refresh_memory`, then extension commands.
- `2026-03-13`: Added `expand_context` with daemon-side context-handle caching so follow-up calls can request focused deltas instead of rebuilding full workflow bundles.
- `2026-03-13`: Added passing Rust coverage for cached context expansion and cache eviction behavior, then re-verified the full daemon workspace.
- `2026-03-13`: Added an ignored workflow benchmark harness with an initial baseline of 100% top-3 file hit rate, 75% symbol hit rate, about 3208 bytes average payload, and about 3.0 estimated tool calls saved.
- `2026-03-13`: Added `refresh_memory` so existing memories can be refreshed in place with new content, scope, links, and evidence metadata while clearing stale state.
- `2026-03-13`: Re-verified the daemon workspace after `refresh_memory`; current baseline is `100` passing core tests and `6` passing daemon tests.
- `2026-03-13`: Added VS Code commands and sidebar actions for `prepare_change`, `impact_from_diff`, `get_working_set_context`, and `diagnose_failure`, then compiled the extension successfully.
- `2026-03-13`: Rebuilt the release daemon and refreshed the workspace extension binary so the repo stays runnable with the latest backend changes.
- `2026-03-13`: Expanded the workflow benchmark scorecard to include estimated returned tokens, memory-recall hit rate, and stale-memory precision; latest synthetic baseline is 100% top-3 hit, 80% target hit, about 2768 bytes average payload, about 692 estimated tokens, about 2.6 calls saved, and 100% stale precision.
- `2026-03-13`: Added an `expand_context` command and sidebar action that reuses the last workflow context handle for low-token follow-up drill-down.
- `2026-03-13`: Added `summarize_subsystem` and `get_repo_playbook` for summary-first compression and durable repo/subsystem playbooks.
- `2026-03-13`: Added live `get_session_metrics` tracing for tool-call counts, payload sizes, handle reuse, and automatic memory writes.
- `2026-03-13`: Added automatic playbook memory refresh/store with repo-scope and branch-scope refresh keys so durable summaries persist across sessions.
- `2026-03-13`: Folded summary-first overviews, memory highlights, and next-step hints into the primary agent workflows so `prepare_change`, `get_working_set_context`, and `diagnose_failure` return compact action-oriented bundles by default.
- `2026-03-13`: Trimmed failure-path recalled memory snippets at the RPC layer so durable playbooks help diagnosis without re-inflating token usage.
- `2026-03-13`: Persisted context handles under `.lattice/context_handles.json` so `expand_context` can survive daemon restarts, including compact playbook snapshots for failure- and task-driven workflows.
- `2026-03-13`: Slimmed compact workflow payloads by dropping duplicated raw memory blobs, shortening memory references, omitting empty sections, and trimming low-value reason/evidence tails; benchmark average payload fell from about 3351B to 2980B and estimated tokens from about 838 to 745 while keeping 100% hit rates.
- `2026-03-13`: Queued the next sprint as one agent-focused backlog: adaptive compactness, outcome-fed durable memory, suggested expand targets, better follow-up metrics, and selective semantic fallback.
- `2026-03-13`: Implemented the sprint in the daemon: auto compact-vs-full delivery metadata, outcome pattern recording and recall, suggested expand targets in workflow reports, richer session metrics, and selective semantic fallback for `prepare_change` and `summarize_subsystem`.
- `2026-03-13`: Re-verified the daemon with `cargo test --workspace` and `cargo test workflow_bench_scorecard -- --ignored --nocapture`; hit rates stayed at `100%`, average synthetic payload measured about `3120B`, and estimated tokens measured about `781`.
- `2026-03-14`: Added an ultra-compact high-confidence shaping pass for workflow outputs: shorter overviews, smaller ranked lists, leaner memory snippets, and less rationale/evidence in compact mode.
- `2026-03-14`: Added benchmark guardrails so the synthetic workflow scorecard must stay under `2000B` average payload and `500` estimated tokens while preserving `100%` hit rates on the current harness.
- `2026-03-14`: Re-verified after the ultra-compact pass; current synthetic compact averages are about `1396B` and `349` estimated tokens with `100%` top-3 hit, `100%` target hit, `2.6` calls saved, and `100%` stale precision.
- `2026-03-14`: Landed a compact-defaults sprint across the RPC layer: removed compact stats from workflow payloads, added hard response budgets, added high-confidence single-anchor shaping, added an optional dense agent wire format, made `expand_context` a more explicit second step via tighter `suggested_expand` defaults, and taught the daemon to use session metrics when deciding whether to go tiny or prune memory-heavy sections.
- `2026-03-14`: Extended session metrics to track tiny responses, dense wire usage, and single-anchor tasks so future pruning decisions can be driven by real assistant behavior instead of static assumptions.
- `2026-03-14`: Re-verified the compact-defaults sprint with `cargo test --workspace` and `cargo test workflow_bench_scorecard -- --ignored --nocapture`; the current synthetic workflow average is about `1305B` and `327` estimated tokens with `100%` top-3 hit, `100%` target hit, `2.6` calls saved, and `100%` stale precision.
- `2026-03-14`: Added an assistant-efficiency section to the existing VS Code sidebar so the installed marketplace extension can show session token usage, delivery mix, dense/single-anchor usage, follow-up avoidance, handle reuse, and outcome-memory reuse without adding new product-facing UI.
