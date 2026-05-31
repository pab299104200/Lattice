# Lattice

Local AI context engine for MCP-enabled coding assistants.

Lattice indexes your codebase and repo Markdown into a dependency graph, then serves ranked context, workflow bundles, docs navigation, compact summaries, and persistent memory to assistants like Codex and Claude Code. Instead of sending whole files or relying on broad search, it returns the files, symbols, docs sections, tests, and prior decisions that are most likely to matter.

## Why Lattice

- `get_skeleton` and `get_context_capsule` cut discovery cost before an assistant starts reading source
- Workflow tools like `prepare_change`, `plan_edit`, `trace_scenario`, `impact_from_diff`, and `diagnose_failure` collapse multi-step coding tasks into one or two calls
- Markdown docs, runbooks, and decisions are first-class graph nodes with backlinks, outgoing links, and code mentions
- `find_stale_docs` helps catch docs that likely drifted after code or runbook changes
- `expand_context` reuses a prior handle from `get_context_capsule` or a workflow tool and returns only the next delta
- workflow bundles now include per-pivot and per-memory retrieval relevance summaries plus item-level relevance detail handles for `expand_context`
- `plan_edit` adds a patch-oriented planning bundle with likely edit files, candidate edit spans, affected callers and dependencies, relevant docs, and recommended tests
- `trace_scenario` turns a behavior description into likely versus plausible entrypoints, execution-path segments, guards, side effects, failure branches, and related tests/docs, while keeping confidence separate from coverage
- Memory is persistent, scoped, refreshable, and stale-aware
- Compact workflow shaping now defaults to small assistant-friendly responses instead of large generic payloads

## Current Measured Results

Current guidance from internal evaluation:

- Broad codebase exploration: about `60-80%` fewer tokens
- Deep discovery tasks: about `35-55%` fewer tokens
- Mixed coding workloads: about `40%` fewer tokens
- Current synthetic workflow benchmark: about `1305B` average payload, about `327` estimated tokens, `100%` top-3 hit, `100%` target hit, about `2.6` calls saved, and `100%` stale-memory precision
- Current product working-set eval across Portal, Meridian, Keystone, and RMM: raw capsule top-5 hit `8/8`, prepared working-set top-5 hit `8/8`, and `rg` top-5 baseline hit `3/8` on the same cases

That synthetic workflow benchmark reflects the current compact workflow stack, including:

- auto `tiny` / `compact` / `full` shaping
- high-confidence single-anchor responses
- optional dense wire format
- restart-persistent `context_handle` reuse

Richer semantic indexing now broadens those retrieval paths:

- vector sync embeds symbol bodies with compact summaries, comments/docstrings, error strings, config keys, and route anchors instead of only `name + signature`
- the first multi-granularity slice adds per-file summary vectors alongside symbol vectors, and the query engine can retrieve both scopes before reranking
- semantic candidates are re-ranked with graph proximity, identifier overlap, and query-intent signals before final delivery
- vector sync, watcher sync, and USearch flushes now emit structured rollout logs for payload size, throughput, and storage growth
- assistant-style benchmark scorecards now cover natural-language and identifier-heavy prompts in the query and intelligence benchmark suites

Compact follow-up targets now prefer stable handles when the graph node identity is known:

- stable symbol follow-up targets use `symbol_id:{...}` and are derived from the graph identity already carried by `SymbolId` (`file`, `name`, and `byte_offset`)
- stable file follow-up targets use `file_id:...`
- `expand_context` resolves `symbol_id:` and `file_id:` first, then falls back to legacy `symbol:` and `file:` targets for compatibility
- compact follow-up suggestions and cached seeds preserve stable handles when available in `get_context_capsule`, `prepare_change`, `impact_from_diff`, `get_working_set_context`, `summarize_subsystem`, `get_repo_playbook`, and `diagnose_failure`
- `plan_edit` also preserves stable handles where the graph node is known and prefers the top candidate edit span for `suggested_expand`

## How You Use Lattice

When you run Lattice through an MCP client, you get:

- workflow tools like `prepare_change`, `plan_edit`, `trace_scenario`, `impact_from_diff`, `diagnose_failure`, and `expand_context`
- docs tools like `get_docs_capsule`, `get_backlinks`, `get_outgoing_links`, and `find_stale_docs`
- graph-backed code retrieval, project rules, test discovery, and workspace setup guidance
- persistent memory and workflow outcome reuse across sessions
- persisted ANN semantic search under `.lattice/` with automatic SQLite exact-search fallback, plus scoped symbol and file-summary vector retrieval
- SQLite FTS5-backed memory keyword search with automatic backfill for existing memory databases
- graceful stdio shutdown: the daemon now aborts in-flight requests on client cancellation or disconnect so abandoned sub-agent calls do not linger

## Local Setup

For local development or GitHub installs:

```bash
git clone https://github.com/pab299104200/Lattice.git
cd Lattice/daemon
cargo build --release
```

Then point your MCP client or coding CLI at the built binary:

```text
daemon/target/release/lattice
```

For MCP clients, `lattice --stdio --workspace <path>` is a lightweight proxy. The proxy keeps client stdio dedicated to MCP, connects to the long-lived local Lattice daemon, and starts that daemon if it is not already running. The daemon is one process per user environment and can host multiple workspace shards at the same time; each proxy connection is bound to the workspace path supplied by that MCP client.

The proxy forwards JSON-RPC to the daemon instead of implementing tool schemas locally, so `tools/list`, `tools/call`, and future MCP capabilities are exposed dynamically by the daemon. The internal proxy listener defaults to `127.0.0.1:47659`; set `LATTICE_DAEMON_ADDR` for a different loopback address. If you need the proxy to respawn the daemon from an explicit binary path instead of its own invocation path, set `LATTICE_DAEMON_EXE=/absolute/path/to/lattice`.

Multi-root proxy requests are represented as logical views over canonical per-root shards instead of as graph-owning combined runtimes. The daemon preserves the existing MCP method and tool schemas, warms the primary requested shard before accepting tool traffic, and prewarms the remaining view shards sequentially in the background so clients do not pay a seven-repo startup latency spike. Graph-backed workflow and dependency-analysis tools fan out across selected shards and return a bounded merged payload with per-shard summaries, source-workspace annotations, `failed_shards`, `incomplete_shards`, and context-handle routing back to the shard that created the handle. The successor direction is documented in [2026-05-20-persistent-daemon-shard-architecture.md](docs/architecture/2026-05-20-persistent-daemon-shard-architecture.md): per-root shard ownership, session-level composed views, and compact structural graphs instead of permanently materialized multi-root mega-runtimes.

The long-lived daemon bounds shard residency instead of keeping every graph forever. Loaded workspace shards are capped by `LATTICE_MAX_LOADED_SHARDS` (default `8`; falls back to the legacy `LATTICE_MAX_LOADED_WORKSPACES` value when set) and idle shards are evicted after `LATTICE_WORKSPACE_IDLE_TTL_SECS` (default `1800`). Multi-root view prewarming is enabled by default and can be disabled with `LATTICE_PREWARM_VIEW_SHARDS=0`; it is bounded by the same loaded-shard cap and stops at the first shard-load failure. Eviction stops that shard's indexing, watcher, memory-maintenance, and compaction tasks before dropping its graph and index handles. Full-graph semantic vector sync is disabled by default because it can be CPU-expensive on large repos; set `LATTICE_ENABLE_BACKGROUND_VECTOR_SYNC=1` to run it during background indexing.

`index_status` includes warm-load diagnostics for each shard entry in a logical view: `warm_load_skipped`, `warm_load_skip_reason`, `persisted_files`, `limit`, `env_var`, `persisted_bytes`, `byte_limit`, `byte_env_var`, and `effective_files`. This distinguishes a first-run cache miss from a persisted graph that was intentionally skipped because it exceeded `LATTICE_MAX_WARM_GRAPH_FILES` or `LATTICE_MAX_WARM_GRAPH_BYTES`.

Within a loaded workspace, the query engine and indexer share immutable graph snapshots instead of retaining separate graph copies. Reindexing and watcher updates publish a new shared snapshot only when the graph changes, while cached parsed files remain in the indexer for incremental rebuilds.

Graph-backed workflow tools also validate lightweight repo and workspace state at request time. If the current workspace branch, detached `HEAD`, Git index, or other substantial workspace state no longer matches the graph snapshot that was last published, those tools return the existing bounded indexing-style response with `"reason": "branch_switch"` or `"reason": "workspace_change"` instead of serving stale graph results. The daemon stamps context handles with a repo epoch and rejects handles created before a later workspace epoch publishes.

Working-memory state is checkpointed automatically. `get_task_memory` writes a bounded checkpoint when active task state changes, the daemon checkpoints active tasks periodically while runtimes stay loaded, and runtime shutdown triggers a final checkpoint plus session consolidation submission so abrupt session endings are less likely to strand working-memory state.

Process lifecycle events for the proxy and the long-lived daemon are appended to `~/.lattice/logs/lifecycle.jsonl` by default. Set `LATTICE_LIFECYCLE_LOG_DIR` to move that log directory.

At runtime Lattice keeps assistant state under the workspace-local `.lattice/` directory:

- `graph.db` stores the persisted graph snapshot, file fingerprint manifest, and cached parsed files used for incremental startup indexing
- `memories.db` stores memory rows, plus an FTS5 keyword index that is rebuilt automatically on open
- `vectors.db` stores semantic vectors as the durable source of truth and exact-search fallback
- `vectors.usearch` stores the persisted ANN index used on the semantic-search hot path

On startup, Lattice warm-loads the persisted graph immediately, computes current file fingerprints in the background, and reparses only new, changed, deleted, or parser/schema-version-stale files. The first run after this cache format is introduced populates cached parsed files; later restarts reuse unchanged parsed files and update the loaded graph from deltas.

If `memories.db` cannot be opened cleanly, the daemon first quarantines `memories.db`, `memories.db-wal`, and `memories.db-shm` under `.lattice/recovered-memory/`, rebuilds a fresh persistent store, and only falls back to in-memory session memory if that recovery path also fails.

If the USearch index cannot be opened or synchronized, the daemon falls back to exact SQLite vector search without changing MCP response shapes.

## Which Tool First?

If you are not sure which tool to call, choose one of these five first-call tools:

1. `diagnose_failure`
   Use for failing tests, stack traces, compiler errors, or runtime failures.
2. `trace_scenario`
   Use for behavior-level debugging when you have a scenario description and want likely versus plausible entrypoints, execution-path segments, guards, side effects, failure branches, and follow-up targets before editing.
3. `prepare_change`
   Use for fix/add/refactor tasks once the likely change area is known.
4. `plan_edit`
   Use when you want a patch plan with edit spans, affected callers and dependencies, docs guidance, and tests in one bundle.
5. `get_context_capsule`
   Use for unfamiliar subsystems, broad architecture questions, or "how does X work?" It is a bounded first-pass working-set finder, not a source dump.

Then use `expand_context` when one of those results returns a `context_handle` or `suggested_expand`.

Use these helpers only when they match the situation more closely:

- `get_working_set_context` is best when the assistant already has a few open files and wants them compressed into one bundle
- `get_repo_playbook` is best for quickly refreshing repo-wide conventions and architecture patterns
- `summarize_subsystem` is best when you explicitly want a summary-first map instead of ranked pivots
- `get_skeleton` is best before opening a large file when structure matters more than retrieval
- `impact_from_diff` is best when you already have a diff or local edits and want downstream impact plus tests
- `find_relevant_tests` is best when test selection is the main question
- `get_docs_capsule` is best when the answer is more likely to be in Markdown docs, runbooks, or design notes
- `get_backlinks` and `get_outgoing_links` are best for walking the local docs graph around a known symbol, file, document, or section
- `find_stale_docs` is best after a diff or active edit when you want to see what docs may now be out of date
- memory hygiene tools matter most in long-running or repeated assistant sessions where observations and outcomes are actually being written

Practical rule:

- `diagnose_failure` decides where the failure is coming from
- `prepare_change` decides what to edit
- `plan_edit` decides how to patch it
- `get_context_capsule` decides how the code works
- `get_skeleton` decides whether a file is worth opening

## MCP Server Setup

Add Lattice to your project's `.mcp.json` for Claude Code, Codex CLI, or any other MCP client:

```json
{
  "mcpServers": {
    "lattice": {
      "type": "stdio",
      "command": "/path/to/lattice",
      "args": ["--stdio", "--workspace", "/path/to/your/project"]
    }
  }
}
```

The configured command should stay `lattice --stdio --workspace ...`. Do not point MCP clients at `lattice --daemon`; that mode is the long-lived internal server that proxies start or reuse automatically.

## MCP Tools

### Summary-First Discovery And Planning

- `get_skeleton`
  Fast file map: symbols, kinds, and structure before loading source.
- `get_context_capsule`
  Broad discovery tool for unfamiliar subsystems or architectural questions. Returns bounded ranked pivots plus a reusable `context_handle` and `suggested_expand`; compact first-pass responses strip pivot source bodies and expect `expand_context` for the next delta.
  When the graph node is known, `suggested_expand` uses stable `symbol_id:` or `file_id:` follow-up targets.
  For implementation-oriented queries, it favors source files over Markdown docs; use `get_docs_capsule` for doc-first questions.
- `summarize_subsystem`
  Summary-first subsystem map: key files, key symbols, tests, and memories in a compact bundle.
  For code-oriented queries, Markdown/meta file hints do not outrank real code anchors; use `get_docs_capsule` for doc-first questions.
- `get_repo_playbook`
  Repo-wide architecture and convention summary for fast session startup.
- `prepare_change`
  Change-oriented bundle: likely edit files, symbols, tests, memory, and risks. Bundled memory items keep the same legacy-plus-structured payload shape described below. Compact responses prefer stable follow-up handles when symbol identity is available.
- `plan_edit`
  Patch-oriented planning bundle: likely edit files, candidate edit spans, affected callers and dependencies, relevant docs, and recommended tests. Compact responses prefer the top candidate edit span handle when one is available.
- `trace_scenario`
  Scenario-focused debugging bundle: given a behavior description, it surfaces likely entrypoints, plausible alternatives, execution-path segments, guards, side effects, failure branches, relevant docs/tests, and confidence-separated signals. Compact responses seed a `context_handle` and `suggested_expand` toward the most likely path focus.
- `impact_from_diff`
  Diff review bundle: changed symbols, affected code, review checklist, and tests. Compact responses prefer stable follow-up handles when symbol identity is available.
- `diagnose_failure`
  Failure triage bundle: likely culprit symbols, tests, likely causes, and next steps. Compact responses prefer stable follow-up handles when symbol identity is available.
- `expand_context`
  Focused delta expansion from a prior `context_handle`, including one returned by `get_context_capsule`.
  Accepts `symbol_id:`, `file_id:`, `symbol:`, `file:`, `test:`, and `memory:` focuses, with stable handles taking precedence.

### Tests, Working Set, And Refactoring

- `find_relevant_tests`
  Rank tests from files, symbols, or diff text.
- `get_working_set_context`
  Compress active files, nearby symbols, tests, and memory into one bundle. Returned memory items preserve the same additive structured fields when present.
- `get_symbol`
  Full symbol details: source, signature, dependencies, and dependents.
- `get_dependencies`
  Outbound dependencies for a symbol.
- `get_dependents`
  Inbound dependents for a symbol.
- `get_impact_graph`
  Transitive blast radius before refactoring.
- `search_symbols`
  Symbol lookup by name pattern.
- `search_logic_flow`
  Call-chain tracing between two functions or symbols.
- `submit_lsp_edges`
  Add high-confidence LSP edges to enrich the graph.

### Docs, Decisions, And Runbooks

- `get_docs_capsule`
  Return the most relevant Markdown documents and sections for a natural-language query, plus related code symbols mentioned from those docs.
- `get_backlinks`
  Return inbound Markdown references to a symbol, file, document, or section.
- `get_outgoing_links`
  Return outgoing Markdown links and code mentions from a document, section, or file target.
- `find_stale_docs`
  Flag docs and sections that likely need review because they mention changed symbols, changed files, or changed docs.

### Memory, Outcomes, And Long-Running Context

Memory payloads now include additive structured assertion metadata alongside the legacy fields (`id`, `content`, `type`, `scope`, links, stale flags, and related fields). Workflow bundles and memory tools may expose:

- `assertion_type`, `verification_status`, and `confidence_reason`
- `supersedes_memory_id`, `superseded_by_memory_id`, `contradicts_memory_ids`, and `contradicted_by_memory_ids`
- `freshness_policy` and `freshness_policy_detail`
- `provenance` and `evidence`
- `trust_status`, `trust_reason`, and `checkout_state`

At a high level, assistants should trust verified workflow outcomes with evidence and matching checkout state ahead of weaker stale, superseded, contradicted, unverified, or in-review recall. The legacy fields still exist; these structured fields are additive and help explain why one memory is preferred over another. New durable memories record the current Git `HEAD` ref/OID in provenance when the workspace is a Git checkout, and memory responses compare the recorded state with the current checkout.

- `get_task_memory`
  Read task-scoped working memory plus relevant durable memory. The daemon seeds missing task state from the task statement or hint, records automatic checkpoints, hard-scopes recall to the active workspace unless a future cross-repo mode explicitly opts in, and requires concrete task evidence such as matched paths, files, symbols, docs, or structured remediation IDs before surfacing a memory. Returned records include advisory/trusted/stale trust diagnostics and checkout-state comparison so unverified, evidence-free, or different-HEAD claims are not mistaken for proof.
- `search_memory`
  Search stored memory across sessions within the active workspace. In multi-root logical views, the daemon fans out to every shard and merges results by exact-match score plus cross-shard context coverage so a wrong primary shard or same-ID collision cannot hide the correct workspace memory. The daemon reranks candidates by exact task evidence over memory content, refresh keys, linked files/docs/tests, and evidence; structured IDs such as remediation packet/unit IDs and code identifiers are required anchors, while repo/product-specific query terms break ties across shards. When a query contains structured IDs such as `IU-0031` or `PX-0040`, those IDs are hard anchors: memories that only match generic terms are not returned, and diagnostics report `query_exact_terms`, `matched_exact_terms`, `unmatched_exact_terms`, `durable_exact_term_counts`, `per_shard_exact_term_counts`, and `exact_term_status` so operators can distinguish “absent from durable memory” from a ranking miss. Unverified failure/blocker memories that reference local files changed after the memory was recorded include `freshness_warning`, `trust_status: "advisory"`, and `trust_reason: "freshness_warning"` fields; normal unverified or in-review advisory memories use `trust_reason` values such as `unverified` or `verification_in_review`. Memories whose workspace provenance conflicts with linked absolute file paths include `workspace_conflict` diagnostics; memories with only relative linked paths include `workspace_path_diagnostic` when ownership cannot be cross-checked from paths alone.
- `list_stale_memories`
  Find memories that likely need refresh.
- `save_quick_memory`
  Capture a lightweight memory using active task state, focus paths, and recent failure context.
- `save_memory`
  Create a durable memory with explicit evidence, validity conditions, and invalidation triggers.
- `consolidate_session`
  Trigger proposal-only session consolidation and return auditable consolidation proposal ids for later apply or reject decisions. The daemon also submits consolidation automatically on runtime shutdown after checkpointing active task state.
- `get_memory_metrics` returns canonical Phase 9 metric snapshots from `lattice_core::metrics`, with explicit `session_metrics` fallback provenance only when the canonical collector returns an honest null
  Return the current Phase 9 signal surface with per-signal provenance. When the canonical metrics module is not present yet, missing signals stay explicit `null` with a reason instead of fabricated numbers.
- `get_event_trace`
  Read a paginated task, session, or workspace event trace with compact, full, or diagnostic rendering for audit and replay workflows.
- `propose_memory_evolution`
  Propose, apply, or reject durable memory changes while preserving provenance and prior state.
- `record_workflow_outcome`
  Persist successful workflow outcomes so future sessions can reuse real solutions. Verified workflow outcomes are treated as stronger recall when later bundles summarize durable memory. Outcome recording preserves structured remediation IDs from the task, summary, inherited context handle, and linked files in the durable memory text and refresh key so later exact-ID searches can recover the outcome. Pass `dry_run: true` to compute the outcome content, identifiers, refresh key, scope, and workspace without writing durable memory; use this for live MCP verification probes that should not pollute repo memory. Refresh keys preserve structured IDs and file identity while avoiding arbitrary absolute-path fragments such as `/home` path components.

### Observability And Workspace Understanding

- `index_status`
  Current indexing progress and graph stats. Logical-view responses distinguish `primary_workspace` from query scope and include `workspace_field_meaning`; `request_workspace` is `null` with `request_workspace_available: false` when the stdio client does not provide per-call caller CWD, so operators should use path-bearing tool arguments for request-specific routing.
- `get_session_metrics`
  Session-level efficiency metrics: token usage, delivery mix, follow-up avoidance, handle reuse, and outcome-memory reuse.
- `get_event_trace`
  Paginated event-log inspection with workspace-boundary enforcement and diagnostic payload hashes for replay and audit work.
- `get_project_rules`
  Auto-detected repo conventions and recurring patterns.
- `workspace_setup`
  Workspace conventions, language breakdown, and recommended setup.

## Workflow Response Controls

Workflow tools, plus `get_context_capsule`, support assistant-oriented response shaping where documented:

- `mode`
  Existing high-level mode selection (`auto`, `compact`, `full`)
- `budget`
  Output budget control: `tiny`, `compact`, or `full`
- `max_tokens`
  Approximate hard cap for returned payload size
- `wire_format`
  `standard` or `dense`
- `render`
  `hybrid` (default markdown summary + JSON payload), `markdown`, or `json`

What this means in practice:

- Lattice can auto-select `tiny`, `compact`, or `full` depending on confidence and signal quality
- first-pass discovery and planning use indexed graph, lexical, path, and memory signals before any semantic fallback
- workflow responses include an `agent_retrieval_contract` that states what the result is best for, why it is useful instead of `rg`, when to use `rg`, and the next recommended action
- workflow responses include budget metadata (`budget`, `budget_max_tokens`, `approx_tokens`, `truncated`) and apply default caps for `tiny`, `compact`, and `full`
- high-confidence results may collapse to a single anchor plus `suggested_expand`
- `get_context_capsule` and workflow responses can include a `context_handle` and a `suggested_expand` target
- compact `prepare_change` results keep bounded ranking evidence and state when the bundle is useful as a working-set finder versus when `rg` is the better literal-search tool
- `expand_context` handles persist across daemon restarts
- `get_session_metrics` exposes how often tiny/dense/single-anchor paths are actually being used

## Docs Graph Workflow

Use this flow when you want to navigate repo docs the way people use Obsidian-style vault graphs, but grounded in code and assistant workflows:

1. Open a Markdown file or place the cursor on a symbol.
2. Use `Lattice: Open Docs Graph` or the Markdown CodeLens `Lattice: Open Docs Graph`.
3. In the graph panel, follow the active editor automatically or pin the current target.
4. Click `Focus` on incoming or outgoing nodes to walk the local graph.
5. Click `Open` to jump straight to the linked file or section.

Use these shortcuts when you do not need the full panel:

- `Lattice: Get Docs Capsule` for a natural-language docs query like "how does auth login work?"
- `Lattice: Show Backlinks` to see which docs mention the current symbol or section
- `Lattice: Show Outgoing Links` to inspect what a document points to
- `Lattice: Find Stale Docs` after a staged diff, working tree change, or active edit

What Lattice indexes in the docs graph:

- Markdown files such as `README.md`, `docs/**`, runbooks, ADRs, scorecards, and repo notes
- document nodes and section nodes
- Markdown links and wiki-links
- inline code references from docs into code symbols

## How Lattice Works

1. **Indexes your codebase and Markdown**
   tree-sitter parses source into symbols and relationships, Markdown parsing extracts document sections and links, and a file watcher keeps the graph updated incrementally.
2. **Builds a unified dependency graph**
   petgraph stores calls, imports, inheritance, doc links, code mentions from docs, and related edges.
3. **Ranks context**
   keyword scoring, graph traversal, hub dampening, and query-intent heuristics identify high-signal symbols and docs.
4. **Shapes workflow and docs bundles**
   task, diff, failure, working-set, docs, and summary tools build compact assistant-facing payloads on top of the graph.
5. **Remembers across sessions**
   SQLite-backed memories, playbooks, outcomes, stale-memory tracking, and stale-doc workflows preserve useful context without blindly replaying old notes.

The workflow layer is built on top of the core discovery primitives. `get_skeleton` and `get_context_capsule` remain foundational.

## Supported Languages

Python, TypeScript, JavaScript, Rust, Go, Java, Markdown

## Query Engine Notes

The retrieval core combines keyword matching with graph scoring. Important behaviors:

- IDF-weighted keyword scoring
- graph traversal from seed hits
- hub dampening to avoid infrastructure functions dominating
- keyword coherence gating
- negative keyword signal for wrong-subsystem suppression
- word-boundary matching
- intent detection for Explore / FixBug / Refactor / AddFeature queries

The newer workflow tools layer task reasoning and response shaping on top of that retrieval core.

## Current Validation

Latest verification:

```bash
cd daemon
cargo test --workspace
cargo test workflow_bench_scorecard -- --ignored --nocapture
cargo test bench_product_working_set_vs_rg -- --ignored --nocapture
```

Current test baseline:

- `130` passing core tests
- `17` passing daemon tests

## Assistant Memory Instructions

Add this to your assistant's project memory (`CLAUDE.md`, `AGENTS.md`, Codex instructions, or equivalent):

```markdown
### Lattice Context Engine — Available Tools

Lattice provides a dependency graph and context engine for this codebase.
Prefer a Lattice workflow tool before broad manual exploration in unfamiliar areas.
If you would otherwise open 3 or more unfamiliar files, call `get_context_capsule`, `prepare_change`, or `summarize_subsystem` first.
If `get_context_capsule` or a workflow tool returns a `context_handle` or `suggested_expand`, prefer `expand_context` before starting a fresh broad search.
If you have raw failure text, pass it to `diagnose_failure` before grep-driven triage.
If you're unsure which tool to use, default to `prepare_change` for implementation tasks and `get_context_capsule` for understanding tasks.
If the task starts from a failing test, stack trace, or compiler error, start with `diagnose_failure` and use `prepare_change` after it narrows the likely culprit.

Use these tools when they're the best fit:

- `prepare_change` — first choice for "fix/add/refactor X" once you know the area to change
- `plan_edit` — first choice for patch-oriented planning when you want edit files, spans, callers, docs, and tests in one bundle
- `get_context_capsule` — first choice for unfamiliar subsystems or broad questions; it returns a bounded first-pass working set and can hand off directly to `expand_context`
- `get_docs_capsule` — first choice for "what docs or runbooks explain this?" questions
- `get_skeleton` — use before opening a large file
- `summarize_subsystem` — use for a summary-first subsystem map
- `get_repo_playbook` — use to refresh repo-wide conventions and architecture
- `impact_from_diff` — use when reviewing a diff or local change
- `find_relevant_tests` — use when deciding what tests to run
- `diagnose_failure` — first choice when a fix starts from a failing test or error
- `expand_context` — use when a prior `get_context_capsule` or workflow call returned a handle and you want the next delta
- `get_backlinks` / `get_outgoing_links` — walk the local docs graph around a known section, file, or symbol
- `find_stale_docs` — check docs after code changes or before a release
- `get_working_set_context` — only when batching several already-known open files is cheaper than reading them one by one; not as a first discovery call
- `get_impact_graph` — before refactoring to understand blast radius
- `search_symbols` — when looking for a symbol by name
- `search_logic_flow` — to trace call chains between functions
- `get_task_memory` / `search_memory` — load task working memory and retrieve durable memory
- `save_quick_memory` / `save_memory` / `propose_memory_evolution` — write or evolve durable memory
- `list_stale_memories` / `list_memory_conflicts` / `verify_explain_memory` — maintain memory quality
- `record_workflow_outcome` — store successful outcomes so later sessions can reuse them

For targeted edits to known files, direct Read/Grep/Edit are still fine. If the question is exact literal search, use `rg`; Lattice is intended to find the working set.
Lattice adds the most value when you do not already know where to look.
```

## License

MIT
