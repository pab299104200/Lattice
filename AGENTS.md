# Lattice Project Instructions

## Purpose

This file is the Codex-specific instruction surface for durable repo-wide rules.
Do not put Claude-specific behavior here.

Use this file for behavior that should apply across the Lattice repo, including workflow rules, implementation constraints, documentation expectations, testing requirements, and review standards.

## Lattice Context Engine — Available Tools

Lattice provides a dependency graph and context engine for this codebase.
Prefer a Lattice workflow tool before broad manual exploration in unfamiliar areas.
If you would otherwise open 3 or more unfamiliar files, call `get_context_capsule`, `prepare_change`, or `summarize_subsystem` first.
If `get_context_capsule` or a workflow tool returns a `context_handle` or `suggested_expand`, prefer `expand_context` before starting a fresh broad search.
If the task starts from a failing test, stack trace, compiler error, or runtime failure, start with `diagnose_failure` and use `prepare_change` after it narrows the likely culprit.
If you're unsure which tool to use, default to `prepare_change` for implementation tasks and `get_context_capsule` for understanding tasks.

Use these tools when they're the best fit:

- `prepare_change` — first choice for fix, add, or refactor tasks once you know the change area and want likely edit files, tests, risks, and nearby memory in one result
- `get_context_capsule` — first choice for unfamiliar subsystems or broad questions; use `mode: "focused"` for targeted lookups, and use its returned handle with `expand_context`
- `summarize_subsystem` — use for a summary-first subsystem map before loading full source
- `get_skeleton` — use before opening a large file when you want structure without full source
- `impact_from_diff` — use when reviewing a diff or local change and you want downstream impact plus tests
- `find_relevant_tests` — use when deciding what tests to run for a file, symbol, or diff
- `diagnose_failure` — first choice when a fix starts from a stack trace, compiler error, or failing test and you need likely culprit symbols before planning the change
- `expand_context` — use when a prior workflow call returned a handle and you only want the next delta, not a full bundle again
- `get_working_set_context` — use only when you already have a few open files and want a compressed working-set bundle instead of reading them one by one; not as a first discovery call
- `get_impact_graph` — use before refactoring to understand blast radius
- `search_symbols` — use when looking for a symbol by name across the project
- `search_logic_flow` — use to trace call chains between functions
- `save_observation` / `get_session_context` / `search_memory` — use to persist and recall insights across sessions
- `list_observations` / `list_stale_memories` / `promote_observation` / `refresh_memory` / `update_observation` / `delete_observation` — use to maintain durable memory quality
- `record_workflow_outcome` — use to store successful outcomes so later sessions can reuse them

For targeted edits to known files, direct read, grep, and edit operations are fine.
Lattice adds the most value when you do not already know where to look.

## Core Rules

- Treat this file as the durable instruction surface for repo-wide Codex behavior.
- Keep instructions here scoped to behavior, not task-specific notes.
- Build every daemon, extension, MCP surface, indexing path, ranking flow, memory path, and docs workflow the correct way, not the easy way.
- Do not optimize for demo shortcuts, toy repos, or temporary operator conveniences. Optimize for trustworthy behavior, durable contracts, and real assistant usage.
- Solve problems once at the correct layer instead of shipping partial fixes, policy exceptions, or temporary workarounds.
- Treat the quality bar as production-grade on the first implementation pass, including workflow completeness, failure handling, documentation, and verification.
- Lattice is an assistant-facing dependency graph, MCP server, and VS Code extension. When internal convenience conflicts with workspace safety, protocol correctness, backward compatibility, or assistant ergonomics, choose correctness and tighten the contract.

## Communication Style

- Be direct, factual, and concise.
- Avoid empty praise, fake agreement, or performative encouragement.
- Say clearly when an idea is flawed, incomplete, risky, or not worth doing.
- Focus on practical tradeoffs, failure modes, and what will actually work.
- Keep the tone calm and respectful. Clarity matters more than theatrics.
- Never praise the user for their work.
- Never lie to the user.

## Technical Approach

- Challenge assumptions instead of quietly accepting them.
- Verify by reading the code, the docs, and the tests before making claims.
- When reporting findings or explaining behavior, cite what you verified with line-aware file references when the evidence matters.
- Fix real problems in the correct layer now instead of leaving known defects behind when they are in scope.
- Be critical by default. Look for weak assumptions, incomplete logic, drift between contracts and implementation, and places where the implementation does not match the claim.
- Prefer deterministic, bounded, and explainable behavior over magic heuristics that are hard to reason about.
- Preserve backwards compatibility intentionally. If a tool schema, wire shape, or workflow contract must change, update docs, tests, and migration expectations together.
- Never add yourself as a co-author to commits, patches, or generated artifacts.
- After major updates, add the strongest repo-supported verification that fits the change: Rust unit or integration tests for daemon behavior, TypeScript compile or extension checks for extension changes, and higher-level workflow coverage when the repo has a real harness for it. Do not claim coverage you did not run.
- Update documentation in parallel with code changes when public behavior, operator workflow, setup, or architecture changes.

## Documentation Rules

- Update `README.md` whenever public MCP behavior, setup, workflow guidance, render controls, or extension behavior changes.
- Update this file when durable Codex repo rules change.
- If a change materially affects architecture, daemon-extension boundaries, ranking strategy, memory semantics, context caching, or workflow delivery behavior, add or update the right durable design note under `docs/` instead of leaving the rationale implicit.
- Do not leave docs claiming behavior that the code no longer implements. Fix the docs or fix the implementation in the same task.

## Error Handling

- Treat error handling as part of the core implementation, not later polish.
- Design failure paths at the same time as success paths.
- Cover invalid input, missing files, expired handles, workspace-boundary failures, indexing gaps, serialization failures, dependency failures, and partial-failure recovery wherever they apply.
- Make errors actionable for operators and developers. Prefer specific, truthful error messages over vague generic failures.
- Do not hide failures behind silent catches or generic internal errors when a more precise domain or contract error is the real outcome.
- Do not ship happy-path-only workflows. If the workflow matters, its failure handling matters too.
- Add or update tests that prove intended failure behavior, not just the success case.

## Definition Of Done

- A change is not done until success paths, failure paths, documentation, and verification are complete.
- Do not treat daemon-only completion or extension-only completion as done when the workflow is meant to be end-to-end.
- Do not close work with known partial states unless the deferral is explicit, documented, and intentionally accepted.
- A feature that cannot be trusted, debugged, explained, or recovered in production is not done.

## Workspace Safety And Contract Discipline

- Respect workspace boundaries, security filters, and excluded directories by default.
- Do not bypass path scoping, ignored-directory rules, or memory scoping for convenience.
- Keep MCP schemas, response formats, and workflow metadata deliberate and stable.
- Do not claim compatibility, protocol correctness, render behavior, or workflow guarantees unless the behavior is actually implemented, documented, and verified.
- Avoid hidden shortcuts that make the tool appear smarter while weakening explainability, safety, or determinism.
- If a behavior change would surprise existing clients, treat it as a contract change and handle it intentionally.

## Observability And Recovery

- Critical workflows must be diagnosable and recoverable.
- Emit logs, errors, and workflow outputs with enough context to understand ranking misses, handle-expiry issues, indexing failures, memory drift, and extension-daemon mismatches.
- Think through partial failure, retry behavior, stale cache behavior, replay safety, and operator recovery before calling the workflow done.
- If a production failure would be hard to trace, explain, or recover from, the implementation is not finished.

## Performance And Scale

- Design for large repos, realistic token budgets, and repeated assistant usage from day 1.
- Reject N+1 traversal patterns, unnecessary full-repo rescans, unbounded payload growth, and workflows that only work because the repo is small.
- Prefer summary-first responses, compact workflow bundles, stable ranking, bounded expansion, and selective reads over brute-force file loading.
- Call out designs that degrade badly with larger repos, larger docs sets, longer sessions, or heavier memory reuse, and fix them before they harden into debt.

## Project Structure

- `extension/` — VS Code extension (TypeScript), communicates with the daemon over JSON-RPC stdio
- `daemon/` — Rust backend with tree-sitter parsing, petgraph dependency graph, SQLite storage, workflow logic, and MCP handling
- `daemon/crates/lattice-core/` — core library for parser, graph, query engine, indexer, storage, memory, and intelligence flows
- `daemon/crates/lattice-daemon/` — binary entry point, RPC server, MCP handler, and session metrics
- `docs/` — durable design and planning notes

## Build

- Daemon: `cd daemon && cargo build --release`
- Extension: `cd extension && npm install && npm run compile`
- Tests: `cd daemon && cargo test --workspace`

## Deploy

After building, update both binary locations:

```bash
pkill -f lattice && sleep 2
cp daemon/target/release/lattice extension/bin/
cp daemon/target/release/lattice ~/.vscode/extensions/lattice.lattice-0.1.0/bin/
```
