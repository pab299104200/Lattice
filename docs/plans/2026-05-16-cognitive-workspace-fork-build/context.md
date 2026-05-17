# Build Context — Cognitive Workspace Fork

**Spec:** [`../2026-05-16-cognitive-workspace-fork-plan.md`](../2026-05-16-cognitive-workspace-fork-plan.md)
**Build directory:** `docs/plans/2026-05-16-cognitive-workspace-fork-build/`
**Date assembled:** 2026-05-16

This file briefs the build harness on what the existing Lattice platform already provides, what the spec demands that it does not yet provide, and the engineering invariants every task must honor.

---

## 1. What the spec actually demands

The spec describes a **workspace cognition substrate** with three first-class sources of truth — **workspace graph**, **event log**, **memory graph** — composed under one daemon with explicit identity, retrieval, working memory, consolidation, verification, MCP surface, and human-review layers.

The implementation plan in the spec is structured as **eleven sequential phases** (Phase 0 → Phase 11). The implementation slice ordering (event log → memory graph → retrieval → consolidation → verification) is binding: identity must precede the event log, the event log must precede consolidation, and verification must precede retrieval taking memory as trusted. The build harness honors that sequence through `depends_on` edges in `tasks.json`.

**Non-negotiables called out by the spec (Section "Non-Negotiable Product Properties"):**

- Local-first by default; no silent broad workspace reads bypassing ignore rules.
- Deterministic identities for every file, symbol, doc section, event, and memory.
- Every durable memory has evidence; every retrieved memory has an inclusion reason.
- Every stale or contradicted memory is surfaced — never hidden behind recency.
- Every workflow bundle is compact by default; expansion is deliberate via stable handles.
- Every consolidation pass is recoverable, replayable, observable, and reversible.
- No unbounded payload growth, graph traversal, or event-log scan on hot paths.
- Hot-path budgets: identity resolution ≤2ms P99; event write ≤5ms P99 on `prepare_change` / `get_context_capsule` (spec §Phase 1, §Phase 2 DoD).
- LLM-driven consolidation jobs must produce proposals (never silent writes), record model + prompt hash + response hash, run only in background or manual-review modes, and operate under bounded queue depth with documented cost budgets (spec §Phase 6, "LLM-driven consolidation").
- Event log compaction snapshots are daemon-managed background operations, not manual; snapshot format is versioned and independently readable (spec §Storage Design).

---

## 2. Carry-forward inventory (what Lattice already gives us)

### Daemon (Rust)

- **Crates:** `daemon/crates/lattice-core/`, `daemon/crates/lattice-daemon/`.
- **Graph:** `petgraph::DiGraph<GraphNode, EdgeKind>` in `crates/lattice-core/src/graph/`; nine edge kinds today (`Calls`, `Imports`, `Implements`, `Extends`, `TypeRef`, `Contains`, `LinksTo`, `Mentions`, `CoChanges`). Will be extended to the families listed in spec §Workspace Graph.
- **Parsers:** tree-sitter for TypeScript, Python, Rust, Go, Java, Markdown under `crates/lattice-core/src/parser/`.
- **Indexer:** incremental file → graph indexer with cached `ParsedFile` map and one-shot graph rebuild.
- **Storage:** SQLite under `.lattice/` with `graph.db` (nodes, edges, file_index, parsed_files) and `memories.db` (rich 80+ column `memories` table with `memories_fts`).
- **Embeddings:** ONNX runtime, 384-dim L2-normalized vectors loaded from `.lattice/models/`; vector store backed by USearch at `.lattice/vector_index/`.
- **Intelligence:** workflow tools `prepare_change`, `plan_edit`, `get_context_capsule`, `expand_context`, `get_working_set_context`, `trace_scenario`, `summarize_subsystem`, `impact_from_diff`, `diagnose_failure`, `record_workflow_outcome`.
- **Structured memory:** verification status (`unverified` → `in_review` → `verified` / `stale` / `contradicted` / `superseded`), freshness policy (`session_scoped` | `branch_scoped` | `repo_scoped`), assertion types, provenance/evidence JSON blobs — documented in `docs/architecture/2026-04-11-structured-memory.md`.
- **Stable handles:** `SymbolId { file, name, byte_offset }` already used by `expand_context` — documented in `docs/architecture/2026-04-11-stable-follow-up-handles.md`.

### MCP / RPC

- JSON-RPC 2.0 over stdio (`crates/lattice-daemon/src/rpc/server.rs`).
- 43 registered tools dispatched through `McpHandler::handle_tools_call()` (`rpc/mcp.rs`).
- Context-handle cache (`rpc/context_cache.rs`) and per-session telemetry (`rpc/session_metrics.rs`).

### Extension (TypeScript)

- `extension/src/`: `extension.ts` (activation + 14 commands), `daemon.ts` (RPC client), `sidebar.ts` (webview with delivery-mix + handle-reuse meters), `docsWorkbench.ts` (docs graph), `codelens.ts`, `hover.ts`, `statusbar.ts`.
- Compile via `cd extension && npm run compile`; lint via `npm run lint`; tests via `npm test` (`node ./out/test/runTest.js`).
- Activity bar contribution (`viewsContainers.activitybar.lattice`) is the slot the new review UI will extend.

### Tests

- `cd daemon && cargo test --workspace` runs all crates.
- In-tree `tests.rs` next to each module is the convention (e.g. `storage/tests.rs`, `memory/tests.rs`, `intelligence/agent_tests.rs`).
- Benchmark fixtures live under `crates/lattice-core/src/intelligence/benchmark_tests.rs` and `crates/lattice-core/src/query/benchmark_tests.rs`.

### Architecture docs already present

- `docs/architecture/2026-04-11-structured-memory.md`
- `docs/architecture/2026-04-11-stable-follow-up-handles.md`
- `docs/architecture/2026-04-11-patch-oriented-planning.md`
- `docs/architecture/2026-04-11-scenario-tracing.md`
- `docs/architecture/2026-04-11-storage-and-search-backends.md`

Reference these from new design notes; do not duplicate their content.

---

## 3. Gap inventory (what the spec needs that does not yet exist)

| Spec section | Status in current codebase | Build action |
|---|---|---|
| **Phase 0** — fork-or-extend decision + baseline benchmarks | Not done | Decision doc, baseline benchmark capture |
| **Phase 1** — unified identity for files, docs, sections, events, memories, handles | Partial: only `SymbolId` is stable today | Generalize identity, add resolver, ambiguity diagnostics |
| **Phase 2** — append-only event log + 20 event kinds + compaction | Absent | New `events` + `event_payloads` tables, writer/reader API, MCP capture, compaction snapshots |
| **Phase 3** — memory graph with first-class links, evidence, accesses, scores; `CounterMemory` class | Memory is single-table today | New `memory_links`, `memory_evidence`, `memory_accesses`, `memory_scores`; class taxonomy expansion; migration from existing rows |
| **Phase 4** — Retrieval V1 with intent classifier, anchor resolver, hybrid candidates, explainable scoring | Heuristic intent + keyword/semantic in `query/engine.rs`; no diagnostic reasons | New retrieval pipeline returning inclusion reasons + expansion handles |
| **Phase 5** — explicit working memory + checkpoints | Implicit prompt assembly today | New `working_memory_checkpoints` table + operations + MCP surface |
| **Phase 6** — consolidation engine (deterministic + LLM-driven proposals, manual queue, replay-safe) | Absent | New `consolidation_jobs` table, proposal-only writes, LLM provenance |
| **Phase 7** — verification engine (existence, span, scope, expiry, incremental) | `is_stale` flag exists; no incremental verifier | New verifier + `verification_jobs`; scope-leak prevention; stale-label discipline |
| **Phase 8** — Workflow Engine V2 + 10 new memory tools (audited for surface size) | Old tools exist; new memory tools absent | Redesign existing tools + add audited memory toolset |
| **Phase 9** — metrics + golden benchmark suite | Session metrics exist; no golden tasks | New benchmark fixture repo + CLI report |
| **Phase 10** — Human Review UI (memory inbox, queues, event/retrieval views) | Absent (sidebar shows stats only) | New webviews + JSON-RPC plumbing |
| **Phase 11** — hardening + full documentation suite | Partial | Large-repo perf, concurrency, recovery, corruption, migration tests; complete doc suite |

---

## 4. Fork-or-extend decision (spec §Phase 0 gate)

The spec's gate conditions for forking are: breaking `memories` table changes incompatible with migration, identity primary-key changes invalidating indexes, event log requiring incompatible SQLite/WAL changes, or two-or-more of the above.

Current evidence suggests **extend on a long-lived branch** is feasible:

- The existing `memories` schema already accommodates most of the new fields; new tables (`memory_links`, `memory_evidence`, `memory_accesses`, `memory_scores`) can be added alongside.
- `SymbolId` already gives stable identity; generalizing to other node families is additive.
- The event log can be a new SQLite file (`.lattice/events.db`) without disturbing `graph.db` or `memories.db`.

**The fork-or-extend decision is itself the first task (T01).** It is binding for every subsequent task: `depends_on` chains assume the decision documents which repo layout, crate boundaries, and migration policy all subsequent work targets. The harness does not assume the answer.

---

## 5. Shared standards every task must satisfy

| Standard | Location | Where it applies |
|---|---|---|
| Cadres coding standard | `/home/pete/cadres/shared/templates/coding.md` | All code tasks: hard limits (file ≤800 lines, function ≤50 lines, nesting ≤3, cyclomatic ≤10), single source of truth, error handling, naming, no broken windows, schema parity, tests as documentation, observability. |
| UI specification | `/home/pete/cadres/shared/templates/ui-specification.md` | All frontend tasks (Phase 10 review UI): every interactive surface follows the spec; no `window.confirm/prompt/alert`, modals via `<dialog>`, i18n via `i18n.t(...)`. |
| Definition of done | `/home/pete/cadres/shared/templates/definition-of-done-checklist.md` | The final readiness review task asserts every applicable checklist item is satisfied or explicitly waived. |
| Lattice project rules | `/home/pete/cadres/lattice/CLAUDE.md`, `/home/pete/cadres/lattice/AGENTS.md` | Build/deploy procedure (`pkill -f lattice && cp daemon/target/release/lattice extension/bin/ && cp daemon/target/release/lattice ~/.vscode/extensions/lattice.lattice-0.1.0/bin/`), Markdown heading references for doc-anchored behavior, backward-compatibility discipline. |
| Execution philosophy | `/home/pete/.claude/CLAUDE.md` "Execution Philosophy" | No deferrals, no workarounds, no half-finished work, enterprise-grade on day one. The fork is implemented end-to-end across all eleven phases. |

---

## 6. Verification discipline

Every task in `tasks.json` carries `verification_commands` that are real shell commands run from the repo root. Conventions:

- **Doc tasks** verify file existence and presence of required headings via `test -f` + `grep -q "## Heading"` chains.
- **Backend tasks** verify with `cd daemon && cargo build --release` and `cd daemon && cargo test --workspace -- <scope>`. New crate or module tests are pinned to the precise test path (`cargo test -p lattice-core --lib events::tests`) so failures map to a single task.
- **Schema/migration tasks** include a `cargo test` invocation that exercises the migration on a temp DB, plus a `sqlite3` schema-dump diff against a checked-in golden schema where the spec demands it.
- **Frontend tasks** verify with `cd extension && npm run compile` and `npm run lint`. Smoke tasks add `npm test`.
- **Cross-layer contract gates** include MCP schema tests (`cargo test --workspace -- mcp_schema`) and extension/daemon round-trip tests that exercise the new tool surface end-to-end.
- **Benchmark tasks** include `cargo bench` or a CLI invocation that emits a metrics report file the verifier can `test -f`.
- **Review tasks** verify by file existence of a structured findings report under `docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/RNN.md` with the required sections.

If a task's verification cannot be expressed as a real command, the task is split or rewritten until it can.

---

## 7. Sequencing rationale

The eleven spec phases collapse into the following review groupings:

1. **Foundation review** after Phase 0 — fork/extend decision, baseline benchmarks, crate boundary plan, architecture overview.
2. **Backend reviews** after Phases 1, 2, 3, 4, 5, 6, 7, 8 — one per substrate layer.
3. **Contract gates** at three boundaries: (a) Identity + Event Log + Memory cross-layer, (b) Memory + Verification + Retrieval cross-layer, (c) MCP tool surface schema regression, (d) Extension ↔ Daemon review-UI plumbing.
4. **Metrics review** after Phase 9.
5. **Frontend review** after Phase 10.
6. **Tests + Docs review** after Phase 11.
7. **Definition-of-Done gate** asserting every shared standard is satisfied.
8. **Final readiness review** asserting the system is production-grade for repeated agent use.

No phase is allowed to defer scope from a later phase. No phase ships a workaround when the real implementation is reachable in the same build.

---

## 8. Out of scope (explicitly named so it is not silently dropped)

The spec lists no items as out of scope. Therefore every section under spec §Implementation Plan, §Storage Design, §MCP Tool Contract Principles, §Documentation Requirements, §Testing Requirements, §Measurable Success Criteria, and §Risks is in scope for this build. The first-implementation-slice ordering is a *priority hint*, not a scope reduction.

The plan honors that: phases 0 through 11 are each materialized as tasks. The "First Implementation Slice" steps map to the earliest tasks in their respective phases — they are sequenced first inside their phase, but every phase ships in full before the build completes.
