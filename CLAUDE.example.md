# Lattice Project Instructions

## Lattice Context Engine

Lattice provides a dependency graph and context engine for this codebase.

- Prefer a Lattice workflow tool before broad manual exploration in unfamiliar areas.
- If you would otherwise open 3 or more unfamiliar files, call `get_context_capsule`, `prepare_change`, or `summarize_subsystem` first.
- If `get_context_capsule` or a workflow tool returns a `context_handle` or `suggested_expand`, prefer `expand_context` before starting a fresh broad search.
- If you have raw failure text, pass it to `diagnose_failure` before grep-driven triage.
- Default to `prepare_change` for implementation tasks once the likely change area is known.
- Default to `get_context_capsule` for understanding unfamiliar subsystems, then use its handle with `expand_context` instead of restarting discovery.
- Start with `diagnose_failure` when the task begins from a failing test, compiler error, stack trace, or runtime failure.
- Use `get_docs_capsule` when the answer is likely in Markdown docs, ADRs, runbooks, or scorecards.
- Use `get_backlinks`, `get_outgoing_links`, and `find_stale_docs` for doc-to-code navigation and documentation review.
- Use `impact_from_diff` and `find_relevant_tests` when reviewing local edits or selecting test coverage.
- Use memory tools when long-running sessions, repeated workflows, or prior observations are likely to help.

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
