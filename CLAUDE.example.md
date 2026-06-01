# Lattice Project Instructions

## Lattice Context Engine

Lattice provides a dependency graph and context engine for this codebase.

- Prefer a Lattice workflow tool before broad manual exploration in unfamiliar areas.
- If you would otherwise open 3 or more unfamiliar files, start with `diagnose_failure`, `trace_scenario`, `prepare_change`, `plan_edit`, or `get_context_capsule` instead of broad file reads.
- Use the tool that matches the task shape:
  - `diagnose_failure` for failing tests, compiler errors, stack traces, or runtime failures
  - `trace_scenario` for behavior-level debugging when you have a scenario but not the exact symbol
  - `prepare_change` when the likely change area is known and you need edit files, symbols, tests, and risks
  - `plan_edit` when you need a patch-oriented plan with candidate edit spans, affected callers/dependencies, docs, and recommended tests
  - `get_context_capsule` for unfamiliar subsystems or architecture questions
- Use `expand_context` when one of those tools returns a `context_handle` or `suggested_expand` instead of restarting discovery.
- Prefer stable follow-up targets when provided. `expand_context` resolves exact `symbol_id:` and `file_id:` handles first, then legacy `symbol:` and `file:` targets for compatibility.
- Use `summarize_subsystem`, `get_repo_playbook`, or `get_skeleton` when you explicitly want a summary-first map, repo-wide conventions, or file structure before opening source.
- Use `get_docs_capsule` for doc-first questions and `get_backlinks`, `get_outgoing_links`, and `find_stale_docs` for docs-graph navigation and drift checks.
- Use `impact_from_diff` and `find_relevant_tests` when reviewing local edits, checking blast radius, or deciding what to run.
- Prefer durable, verified memory over weaker freeform recollection. Reuse or record repo-/branch-scoped workflow outcomes when they are verified, and treat structured memory fields like verification, provenance, evidence, freshness, and contradiction status as signals for trust.
- Treat retrieved memory as recall, not proof. A memory is only dependable after current code, docs, and tests confirm it.
- Prefer memories with `trust_status: "trusted"`, matching `checkout_state`, concrete `evidence_links`, and useful `recheck_commands`.
- Treat `advisory`, `stale`, unverified, different-checkout, evidence-free, high-risk `requires_reverification`, or `artifact_conflicts` memory as a hypothesis until you inspect the linked evidence and rerun the suggested checks.
- When saving memory, separate hypotheses from verified outcomes and attach evidence links, linked files/docs/tests, validity conditions, invalidation triggers, and the verification command that proved the claim.

For targeted edits to already-known files, direct Read/Grep/Edit is still fine.
Lattice adds the most value when you do not already know where to look.

## Markdown Heading References

Use exact Markdown file + heading references when documented behavior matters.

- In final summaries, review comments, and saved observations, cite the exact doc path and heading when behavior comes from docs.
- Prefer updating the relevant Markdown section and referencing that heading instead of duplicating the same explanation in code comments.
- Only add inline code comments that reference a Markdown heading when the behavior is non-obvious and the Markdown section is the authoritative explanation.

## Project Structure

- `extension/` — VS Code extension
- `daemon/` — Rust daemon and core graph/index/query implementation
- `daemon/crates/lattice-core/` — parser, graph, query engine, indexer, storage
- `daemon/crates/lattice-daemon/` — binary entry point and RPC server

## Build And Test

- Daemon: `cd daemon && cargo build --release`
- Extension: `cd extension && npm install && npm run compile`
- Tests: `cd daemon && cargo test --workspace`
