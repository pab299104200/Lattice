# Lattice Project Instructions

## Lattice Context Engine

Lattice provides a dependency graph and context engine for this codebase.

- Prefer a Lattice workflow tool before broad manual exploration in unfamiliar areas.
- If you would otherwise open 3 or more unfamiliar files, start with `context`, `prepare_change`, or `diagnose` instead of broad file reads.
- Use the tool that matches the task shape:
  - `diagnose` for failing tests, compiler errors, stack traces, or runtime failures
  - `prepare_change` for implementation work; use `mode: "plan_edit"` for patch spans or `mode: "trace"` for scenario debugging
  - `context` for unfamiliar subsystems, docs, repo rules, file skeletons, working sets, and expanding prior handles
  - `impact` before multi-file or non-obvious changes
  - `search` for symbols, call paths, backlinks, and outgoing links
  - `recall` / `remember` for durable task memory and workflow outcomes
  - `status` for indexing, stale docs, stale memory, and conflict health
- Use `context` with `mode: "expand"` when a tool returns a `context_handle` or `suggested_expand` instead of restarting discovery.
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

- `daemon/` — Rust daemon and core graph/index/query implementation
- `daemon/crates/lattice-core/` — parser, graph, query engine, indexer, storage
- `daemon/crates/lattice-daemon/` — binary entry point and RPC server

## Build And Test

- Daemon: `cd daemon && cargo build --release`
- Tests: `cd daemon && cargo test --workspace`
