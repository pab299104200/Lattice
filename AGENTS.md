# Lattice Project Instructions

## Purpose

This file is the Codex-specific instruction surface for durable repo-wide rules.
Do not put Claude-specific behavior here.

Use this file for behavior that should apply across the Lattice repo, including workflow rules, implementation constraints, documentation expectations, testing requirements, and review standards.

## Execution Philosophy

The marginal cost of completeness is near zero with AI. Act on that.

- **Do the whole thing.** Do it right. Write real tests. Write the documentation. Do it so well the result is impressive, not merely satisfactory. The quality standard is mature enterprise-grade, every time — not MVP, not "good enough."
- **Never defer work you can do now.** If it can be done in this session, do it. Offering to "come back to this later" or "leave this for a follow-up" is a failure mode — later sessions lose context and the work either degrades or never happens.
- **Never implement a workaround when the real solution exists.** Workarounds are for humans with deadlines. You don't have deadlines, you have compute. Build the real thing.
- **Do not accumulate prerelease legacy debt.** Cadres products are prerelease unless a repo-specific release contract says otherwise, and the design is still allowed to change. When a product or architecture decision changes, replace the superseded model, route, schema, UI, docs, and tests with the new decision instead of layering a compatibility model on top. Keep historical evidence only where it is needed for audits or migrations, and isolate it from runtime authority. Do not preserve old code paths, compatibility shims, aliases, duplicate models, or "legacy" surfaces just because they once existed.
- **Stop reasoning about time like a human.** You can build in an hour what would take a person months. Complexity is not an excuse to cut scope. Size of the change is not an excuse to do half of it. If the correct solution touches 40 files, touch 40 files.

## Lattice Agent Integration

Lattice is the shared CLI/MCP context engine for Cadres repos. Use the current public verbs only: `context`, `prepare_change`, `impact`, `diagnose`, `search`, `remember`, `recall`, and `status`. Do not document or rely on removed legacy MCP tool names.

Prefer `context` for unfamiliar areas, `prepare_change` for implementation planning, `diagnose` for failures, and `impact` before non-obvious or multi-file edits. Use `search` for structural symbol/doc lookup, `recall` before relying on prior memory, `remember` for durable outcomes, and `status` for daemon/index health.

Codex hook wiring lives in the project-local `.codex/hooks.json`; Claude Code hook wiring lives in the project-local `.claude/settings.json`. The `lattice install codex` and `lattice install claude-code` commands write absolute hook-asset paths from the running installation, so instruction surfaces must not hard-code a checkout path. Hook scripts are best-effort and must exit `0` quickly when the daemon is unavailable.

For CLI fallback:

```bash
lattice context "where is this behavior implemented?"
lattice prepare_change "make the requested change"
lattice impact path/to/file.ext
lattice status
```

`metrics` is an operational CLI report, not an MCP retrieval verb. Runtime mode
is explicit: use a named command for CLI work, `lattice --stdio --workspace
<path>` for an MCP proxy, or `lattice --daemon` for the long-lived daemon. A
bare invocation must not silently select a runtime mode.

## Core Rules

- Treat this file as the durable instruction surface for repo-wide Codex behavior.
- Keep instructions here scoped to behavior, not task-specific notes.
- Build every daemon, MCP surface, indexing path, ranking flow, memory path, and docs workflow the correct way, not the easy way.
- Do not optimize for demo shortcuts, toy repos, or temporary operator conveniences. Optimize for trustworthy behavior, durable contracts, and real assistant usage.
- Solve problems once at the correct layer instead of shipping partial fixes, policy exceptions, or temporary workarounds.
- Treat the quality bar as production-grade on the first implementation pass, including workflow completeness, failure handling, documentation, and verification.
- Lattice is an assistant-facing dependency graph and MCP server. When internal convenience conflicts with workspace safety, protocol correctness, backward compatibility, or assistant ergonomics, choose correctness and tighten the contract.

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
- After major updates, add the strongest repo-supported verification that fits the change: Rust unit or integration tests for daemon behavior, documentation checks for docs changes, and higher-level workflow coverage when the repo has a real harness for it. Do not claim coverage you did not run.
- Update documentation in parallel with code changes when public behavior, operator workflow, setup, or architecture changes.

## Documentation Rules

- Update `README.md` whenever public MCP behavior, setup, workflow guidance, or render controls change.
- Update this file when durable Codex repo rules change.
- If a change materially affects architecture, MCP boundaries, ranking strategy, memory semantics, context caching, or workflow delivery behavior, add or update the right durable design note under `docs/` instead of leaving the rationale implicit.
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
- Do not treat partial daemon completion as done when the workflow is meant to be end-to-end through MCP.
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
- Emit logs, errors, and workflow outputs with enough context to understand ranking misses, handle-expiry issues, indexing failures, memory drift, and MCP client-daemon mismatches.
- Think through partial failure, retry behavior, stale cache behavior, replay safety, and operator recovery before calling the workflow done.
- If a production failure would be hard to trace, explain, or recover from, the implementation is not finished.

## Performance And Scale

- Design for large repos, realistic token budgets, and repeated assistant usage from day 1.
- Reject N+1 traversal patterns, unnecessary full-repo rescans, unbounded payload growth, and workflows that only work because the repo is small.
- Prefer summary-first responses, compact workflow bundles, stable ranking, bounded expansion, and selective reads over brute-force file loading.
- Call out designs that degrade badly with larger repos, larger docs sets, longer sessions, or heavier memory reuse, and fix them before they harden into debt.

## Project Structure

- `daemon/` — Rust backend with tree-sitter parsing, petgraph dependency graph, SQLite storage, workflow logic, and MCP handling
- `daemon/crates/lattice-core/` — core library for parser, graph, query engine, indexer, storage, memory, and intelligence flows
- `daemon/crates/lattice-daemon/` — binary entry point, RPC server, MCP handler, and session metrics
- `docs/` — durable design and planning notes

## Build

- Daemon: `cd daemon && cargo build --release`
- Tests: `cd daemon && cargo test --workspace`

## Deploy

After building, stop the long-lived daemon and active lightweight proxies so MCP clients launch the fresh binary:

```bash
pkill -f lattice && sleep 2
```

MCP clients should run `lattice --stdio --workspace <path>`. That process is a lightweight proxy; it starts or reuses the long-lived local daemon automatically.
