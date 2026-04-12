# Patch-Oriented Planning

## Summary

Phase 3 adds `plan_edit`, a patch-oriented planning workflow that sits on top of the existing change-planning stack and returns a more edit-ready bundle for assistants. The goal is to move from "here are the likely files" to "here is the first patch plan," while staying compact enough to use directly in an edit loop.

## Why This Exists

`prepare_change` is useful for finding the likely area to edit, but assistants still need additional structure before they can safely patch code:

- which symbols are the best first edit anchors
- which line spans should be touched first
- which callers or dependency contracts are likely to move
- which docs and tests are likely to go stale
- which compact response is still small enough to apply without another discovery round

`plan_edit` packages that information in one bundle so the assistant can move from task statement to concrete patch plan with fewer follow-up calls.

## Bundle Shape

`PlanEditBundle` currently includes:

- `edit_files`
- `supporting_files`
- `symbols`
- `candidate_spans`
- `affected_callers`
- `affected_dependencies`
- `relevant_docs`
- `stale_doc_signals`
- `tests`
- `test_gaps`
- `matched_rules`
- `memories`
- `memory_highlights`
- `risks`
- `rationale`
- `stats` in full mode

Candidate spans carry file, symbol, line range, confidence, and stable symbol handles when available. The daemon prefers the top candidate span handle for `suggested_expand`; when one is not available, it falls back to the underlying change-plan suggestion.

## Compactness And Fallbacks

The daemon exposes both compact and full `plan_edit` delivery modes. Compact mode trims supporting material while preserving patch anchors, docs signals, and tests. Full mode keeps the wider stats bundle for inspection.

The same backward-compatible handle discipline used elsewhere in Lattice applies here as well:

- stable symbol handles are preferred when the graph node is known
- legacy `symbol:` and `file:` forms remain accepted in the surrounding workflow plumbing
- cached context handles continue to work across `expand_context`

## Current Implementation Touchpoints

The implementation now lives in:

- `daemon/crates/lattice-core/src/intelligence/agent.rs`
- `daemon/crates/lattice-core/src/intelligence/mod.rs`
- `daemon/crates/lattice-daemon/src/rpc/mcp.rs`

The daemon layer adds the MCP tool declaration, request handling, compact/full shaping, context-handle seeding, and tests for `tools/call` behavior.

## Verification Status

Manual consistency pass only for this note.

Verified against the roadmap and the landed code that:

- `plan_edit` exists as an MCP tool and daemon workflow
- the bundle includes edit files, candidate spans, caller/dependency impacts, docs guidance, and tests
- compact responses prefer the top candidate span handle when one is present
- the README and MCP docs should mention `plan_edit` as a patch-oriented planning tool

No automated docs test was applicable for this note.
