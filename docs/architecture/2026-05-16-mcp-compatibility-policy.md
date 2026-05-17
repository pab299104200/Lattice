# MCP Compatibility Policy

## Summary

This policy defines the Phase 0 compatibility contract required by [docs/plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-0-fork-foundation](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-0-fork-foundation). It classifies the current MCP surface implemented in `daemon/crates/lattice-daemon/src/rpc/mcp.rs` and binds later redesigns to the stability rules in [## MCP Surface](../plans/2026-05-16-cognitive-workspace-fork-plan.md#mcp-surface) and [## MCP Tool Contract Principles](../plans/2026-05-16-cognitive-workspace-fork-plan.md#mcp-tool-contract-principles).

This policy applies to the in-place successor architecture described in [docs/architecture/2026-05-16-cognitive-workspace-architecture.md](./2026-05-16-cognitive-workspace-architecture.md).

## Status classes

| Status | Meaning |
|---|---|
| `stable` | No breaking request or response change is planned. Existing fields remain valid; only behavior fixes are allowed. |
| `additive` | The tool keeps its request and response contract, but may gain optional response fields, richer metadata, or stable identity fields. |
| `redesigned-with-shim` | A successor contract is planned. The legacy tool name must keep working through a compatibility adapter for at least one full phase cycle. |
| `deprecated-with-deadline` | The name is already a legacy alias. It remains callable only through the documented shim window and should be removed after the deadline protocol is satisfied. |

## Canonical tool inventory

The current canonical `tools/list` surface contains 37 tools. Their successor classification is:

| Tool | Status | Successor expectation |
|---|---|---|
| `get_context_capsule` | `additive` | Core workflow remains; Phase 8 may add event and memory reasons, stable identity payloads, and richer diagnostics. |
| `prepare_change` | `additive` | Core workflow remains; later phases add event-backed retrieval, memory evidence, and working-memory details. |
| `plan_edit` | `additive` | Core workflow remains; later phases add shared identity, event, and memory context. |
| `trace_scenario` | `additive` | Core workflow remains; later phases add event episodes and verification signals. |
| `find_relevant_tests` | `additive` | Stable name; later phases add graph/event-backed confidence and identity fields. |
| `impact_from_diff` | `additive` | Stable name; later phases add identity-rich impact edges and event-derived co-change signals. |
| `get_working_set_context` | `additive` | Stable name; later phases may enrich output with explicit working-memory checkpoint metadata. |
| `summarize_subsystem` | `additive` | Stable name; later phases add memory and event explanations. |
| `get_repo_playbook` | `additive` | Stable name; later phases add consolidated procedures and evidence provenance. |
| `get_docs_capsule` | `additive` | Stable name; later phases add doc identity, stale-against signals, and event evidence. |
| `get_backlinks` | `additive` | Stable name; later phases add typed identity outputs and verification hints. |
| `get_outgoing_links` | `additive` | Stable name; later phases add typed identity outputs and verification hints. |
| `find_stale_docs` | `additive` | Stable name; later phases add verification jobs and stale evidence payloads. |
| `diagnose_failure` | `additive` | Stable name; later phases add event episodes, failure-pattern memory, and stronger diagnostics. |
| `record_workflow_outcome` | `additive` | Stable name; later phases add explicit event ids, memory ids, and proposal metadata. |
| `expand_context` | `additive` | Stable name; later phases add cross-substrate expansion targets over unified identities. |
| `get_symbol` | `additive` | Stable name; later phases add typed ids and richer evidence links. |
| `get_dependents` | `additive` | Stable name; later phases add typed ids and graph provenance. |
| `get_dependencies` | `additive` | Stable name; later phases add typed ids and graph provenance. |
| `get_impact_graph` | `additive` | Stable name; later phases add event-derived and verification-derived edges. |
| `search_symbols` | `additive` | Stable name; later phases add typed ids and ranking explanations. |
| `get_skeleton` | `additive` | Stable name; later phases add typed ids and richer structural metadata. |
| `save_observation` | `redesigned-with-shim` | Legacy observation-centric write path evolves toward `save_memory` over the broader memory class model. |
| `get_session_context` | `redesigned-with-shim` | Session-only memory recall evolves toward task-scoped and working-memory-aware retrieval such as `get_task_memory`. |
| `search_memory` | `additive` | Stable name; remains a required workflow tool but gains typed memory, evidence, and event-aware ranking fields. |
| `search_logic_flow` | `additive` | Stable name; later phases add typed ids and explainable path reasons. |
| `submit_lsp_edges` | `stable` | Internal graph enrichment contract stays narrow; no redesign is planned in the cognitive workspace phases. |
| `workspace_setup` | `stable` | Workspace convention summary remains stable; only content freshness changes are expected. |
| `index_status` | `stable` | Operational status tool remains stable; output may gain optional fields only if needed. |
| `get_session_metrics` | `additive` | Stable name; later phases add event-log and memory-usefulness metrics. |
| `get_project_rules` | `stable` | Project-rule summary remains stable; only content freshness changes are expected. |
| `list_observations` | `redesigned-with-shim` | Observation-centric listing evolves toward typed memory inbox and review surfaces. |
| `list_stale_memories` | `additive` | Stable name; later phases add verification-job, evidence, and conflict metadata. |
| `promote_observation` | `redesigned-with-shim` | Promotion stays conceptually valid but will shift to broader typed-memory promotion semantics. |
| `refresh_memory` | `additive` | Stable name; later phases add explicit verification, evidence, and event provenance fields. |
| `delete_observation` | `redesigned-with-shim` | Observation-specific delete path evolves toward generalized memory lifecycle operations. |
| `update_observation` | `redesigned-with-shim` | Observation-specific update path evolves toward generalized memory lifecycle operations. |

The rationale for treating most workflow tools as `additive` is simple: [## MCP Surface](../plans/2026-05-16-cognitive-workspace-fork-plan.md#mcp-surface) keeps the current workflow vocabulary as the successor surface. The architecture changes the backing substrates, not the top-level user intent of those tools.

## Legacy aliases and deadlines

The current daemon accepts five legacy aliases that do not appear in `tools/list` but are part of the callable surface. They are already compatibility shims and therefore classified separately from canonical names.

| Legacy alias | Canonical tool | Status | Deadline |
|---|---|---|---|
| `query_context` | `get_context_capsule` | `deprecated-with-deadline` | Remove no earlier than one full phase cycle after all first-party fixtures and cached seeds move to `get_context_capsule`. |
| `blast_radius` | `get_impact_graph` | `deprecated-with-deadline` | Remove no earlier than one full phase cycle after all first-party clients move to `get_impact_graph`. |
| `get_file_context` | `get_skeleton` | `deprecated-with-deadline` | Remove no earlier than one full phase cycle after all first-party clients move to `get_skeleton`. |
| `store_memory` | `save_observation` | `deprecated-with-deadline` | Remove no earlier than one full phase cycle after the `save_observation` shim or its successor is in place for all maintained clients. |
| `recall_memories` | `search_memory` | `deprecated-with-deadline` | Remove no earlier than one full phase cycle after all first-party clients move to `search_memory`. |

No new alias may be added casually. Every alias is compatibility debt and must name its canonical target, phase-cycle deadline, and removal test coverage at creation time.

## Backward compatibility

Every existing assistant client must continue to function for one full phase cycle after a redesigned tool lands. This is the binding rule for every `redesigned-with-shim` and `deprecated-with-deadline` entry.

The rule has four parts:

1. The legacy name must continue to accept the legacy request shape.
2. The legacy response must remain parseable by existing clients, even if the backend is now implemented through a successor tool or substrate.
3. New identity, event, ranking, or memory fields may be added only as optional additive output unless the tool is explicitly in shim mode.
4. Removal requires an explicit phase boundary, updated docs, updated fixtures, and a passing compatibility test that proves both the legacy shim period and the post-removal canonical path were intentionally handled.

### Shim removal protocol

Remove a shim only when all of the following are true:

1. The canonical replacement has shipped and remained stable for one full numbered phase cycle.
2. `tools/list` no longer advertises the legacy name, and all first-party prompts, fixtures, docs, and cached examples have been migrated.
3. Compatibility tests prove that maintained clients worked during the shim window.
4. The removal is documented in the architecture and compatibility docs in the same change.

If any maintained client still depends on the old name or response shape, the shim stays. Convenience is not a valid reason to break a client early.

## Contract details for additive evolution

Per [## MCP Tool Contract Principles](../plans/2026-05-16-cognitive-workspace-fork-plan.md#mcp-tool-contract-principles), additive evolution may introduce optional fields such as:

- stable ids for files, symbols, docs, sections, events, memories, and handles
- inclusion reasons
- evidence strength
- verification and freshness state
- contradiction and supersession metadata
- event episode summaries
- ranking diagnostics in explicit diagnostic modes

Additive evolution may not:

- rename required top-level fields in place
- remove existing render modes
- make compact responses materially larger without a documented contract change
- replace an existing request parameter with a different required parameter under the same tool name

### `get_context_capsule` additive identity fields

Per [docs/plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-1-unified-identity-model](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-1-unified-identity-model) and this document's [## Canonical tool inventory](#canonical-tool-inventory), `get_context_capsule` remains `additive` in Phase 1.

The Phase 1 additive fields for this tool are:

- `context_handle_identity` — typed `ContextHandle` identity payload added alongside the existing `context_handle` string.
- `pivots[*].file_identity` and `pivots[*].symbol_identity` — typed identity payloads added alongside the existing `file` and `symbol` fields.
- `context[*].file_identity` and `context[*].symbol_identity` — typed identity payloads added alongside the existing `file` and `symbol` fields.
- `stats.seed_symbol_identities` — resolver outcomes for legacy `seed_symbols`, including structured ambiguity diagnostics when a legacy name maps to multiple symbols.

The legacy fields remain populated for one full phase cycle. Existing clients may continue to read `context_handle`, `file`, `symbol`, and `seed_symbols`; new clients may prefer the additive identity payloads.

## Cross-reference to successor architecture

This policy depends on the shared substrate boundaries in:

- [docs/architecture/2026-05-16-cognitive-workspace-architecture.md#Workspace-graph](./2026-05-16-cognitive-workspace-architecture.md#workspace-graph)
- [docs/architecture/2026-05-16-cognitive-workspace-architecture.md#Event-log](./2026-05-16-cognitive-workspace-architecture.md#event-log)
- [docs/architecture/2026-05-16-cognitive-workspace-architecture.md#Memory-graph](./2026-05-16-cognitive-workspace-architecture.md#memory-graph)
- [docs/architecture/2026-05-16-cognitive-workspace-architecture.md#Shared-substrate-primitives](./2026-05-16-cognitive-workspace-architecture.md#shared-substrate-primitives)
