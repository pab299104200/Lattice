# Scenario Tracing

Phase 4 adds a scenario-focused debugging workflow around `trace_scenario`. The goal is to take a behavior description and return the most likely execution paths first, without pretending that every plausible branch has been proven exhaustively.

## Why This Exists

`diagnose_failure` is still the right first call when you already have raw failure evidence such as a stack trace, compiler error, or failing test output. `trace_scenario` fills a different gap: the assistant has a behavior description and needs to reason about what code paths, guards, side effects, and failure branches are most relevant before it starts editing.

## Contract

The workflow is exposed as an MCP tool with:

- required `scenario`
- optional `entry_files` and `entry_symbols` to bias the search toward a known subsystem
- the usual workflow shaping controls: `mode`, `budget`, `max_tokens`, `wire_format`, and `render`

The bundle separates:

- likely entrypoints from plausible alternatives
- the primary execution path from other plausible paths
- guard signals from side-effect signals
- failure branches from successful-path coverage

That separation matters. `likely` means the strongest evidence in the current graph and text signals, while `plausible` means the path is viable but lower confidence or weaker coverage. The bundle should be read as a prioritization aid, not as proof of full workspace coverage.

## Follow-Up Behavior

Compact responses can seed a `context_handle` for the trace result and expose a `suggested_expand` target focused on the most likely path. That keeps follow-up work anchored to the highest-signal branch first, while still allowing `expand_context` to widen the result when the assistant needs more depth.

If the caller already knows the likely subsystem, the optional entry-file or entry-symbol biasing should be used to improve ranking instead of forcing a broad workspace search.

## Verification Scope

This note matches the landed core and daemon changes for `trace_scenario` and the focused regression coverage added with them. It does not claim full workspace validation or exhaustive behavioral proof.
