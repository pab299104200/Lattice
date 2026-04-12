# Stable Follow-Up Handles

## Summary

Phase 1 of the assistant usefulness roadmap now uses stable follow-up expansion where the graph identity is known. Symbol-name targets are still supported, but they are no longer the preferred contract for assistant-facing follow-up when a node can be identified exactly.

The graph model already has a durable identity basis: `SymbolId` is keyed by `file`, `name`, and `byte_offset`. The Phase 1 contract is to reuse that durable identity where possible and to carry a stable handle through workflow output, suggested follow-up targets, cached seeds, and `expand_context` resolution.

## Why This Exists

Current assistant-facing follow-up targets often look like `symbol:<name>`. That is usable when names are unique, but it breaks down in real codebases:

- two files can export the same helper name
- a subsystem can contain the same function name in a primary file and a support file
- a query can surface both a pivot and a supporting context node with the same symbol label

In those cases, a name-only target forces `expand_context` to guess. That guess is acceptable as a fallback, but it is not a durable contract for assistant-driven code work.

## Durable Identity Basis

The repository already has the stable identity needed for Phase 1:

- `daemon/crates/lattice-core/src/symbols.rs` defines `SymbolId { file, name, byte_offset }`
- `daemon/crates/lattice-core/src/graph/model.rs` stores that identifier on every `GraphNode`

For this phase, the handle should be derived from that identity instead of only from the symbol label. The exact encoding is an implementation detail, but the handle must be deterministic for the same graph node and must distinguish duplicate names across files and offsets.

## Contract

Phase 1 follows this resolution order:

1. Resolve an exact stable handle if one is present.
2. If no exact handle is available, resolve a file-qualified or symbol-qualified target using the current backward-compatible behavior.
3. Preserve fuzzy name matching only as a fallback path for older clients and older cached payloads.

That means:

- exact handle lookup is the preferred path
- the old `symbol:<name>` shape must continue to work during migration
- `expand_context` should return the intended node when a stable handle is available, even if a duplicate-name node exists elsewhere in the graph

## Assistant-Facing Surfaces

The implemented assistant-facing surfaces that emit stable handles or preserve them in cached seeds are:

- `get_context_capsule`
- `prepare_change`
- `impact_from_diff`
- `get_working_set_context`
- `summarize_subsystem`
- `get_repo_playbook`
- `diagnose_failure`
- helper paths that currently synthesize `suggested_expand`

The Phase 1 goal is not to remove the existing `file:` and `symbol:` surfaces immediately. It is to make them carry stable identity where available, and to make `expand_context` prefer that identity first.

## Current Implementation Touchpoints

The stable-handle implementation now lives in these areas:

- `daemon/crates/lattice-core/src/intelligence/agent.rs`
- `daemon/crates/lattice-daemon/src/rpc/mcp.rs`
- `daemon/crates/lattice-core/src/symbols.rs`

Those paths now emit stable handles where the graph node identity is known and keep legacy forms available as fallback.

## Migration Notes

This change stays backward-compatible for cached handles and older clients:

- keep accepting existing handle payloads
- keep accepting legacy `symbol:<name>` follow-up targets
- prefer stable handles in compact follow-up suggestions and cached seeds when the node can be resolved exactly

The important boundary is behavioral, not cosmetic: after this phase, follow-up expansion should be deterministic enough that assistants can use it for code actions without guessing which duplicate symbol was intended.

## Verification Status

Manual consistency pass only.

Verified against the current roadmap and code that:

- the roadmap explicitly calls for stable follow-up handles in Phase 1
- `SymbolId` already contains the durable identity fields needed for handle generation
- compact workflow suggestions now emit stable `symbol_id:` / `file_id:` targets when the node is known
- `expand_context` resolves exact `symbol_id:` and `file_id:` targets before falling back to legacy `file:` / `symbol:` / `test:` / `memory:` handling

No automated docs test was applicable for this note.
