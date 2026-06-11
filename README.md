# Lattice

Local AI context, workflow, and memory engine for MCP-enabled coding assistants.

Lattice indexes your codebase and repo Markdown into a dependency graph, then serves ranked context, workflow bundles, docs navigation, compact summaries, and persistent memory to assistants like Codex and Claude Code. Instead of sending whole files or relying on broad search, it returns the files, symbols, docs sections, tests, prior decisions, and current-checkout memory diagnostics that are most likely to matter.

Memory is treated as recall, not proof. Lattice stores durable observations and workflow outcomes, but every retrieved memory carries trust diagnostics: verification state, evidence links, Git checkout comparison, high-risk recheck flags, suggested verification commands, and linked-artifact conflict warnings. That lets assistants reuse prior work without silently trusting stale, unverified, cross-checkout, or docs-drifted claims.

## Why Lattice

- `context` cuts discovery cost before an assistant starts reading source, docs, rules, or file skeletons
- `prepare_change` collapses implementation prep, patch planning, and scenario tracing into one verb
- `impact` computes dependency blast radius and relevant tests before multi-file changes
- `diagnose` maps compiler, test, and runtime failures to likely culprit code
- `search` uses graph identity for symbols, call paths, backlinks, and outgoing links
- `remember` and `recall` provide cross-session memory with trust diagnostics
- `status` makes indexing, stale docs, stale memory, and conflict health visible
- Memory is persistent, scoped, refreshable, stale-aware, and evidence-linked
- Memory trust diagnostics make unverified, stale, high-risk, different-checkout, or docs-conflicted claims explicit before an assistant relies on them
- Compact workflow shaping now defaults to small assistant-friendly responses instead of large generic payloads

## Current Measured Results

Current guidance from local evaluation:

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
- `context` with `mode=expand` resolves `symbol_id:` and `file_id:` follow-up targets first
- compact follow-up suggestions and cached seeds preserve stable handles behind the 8 public verbs
- `context` with `mode=expand` reuses a prior `context_handle` and returns only the next delta

## How You Use Lattice

When you run Lattice through an MCP client, you get:

- 8 MCP verbs: `context`, `prepare_change`, `impact`, `diagnose`, `search`, `remember`, `recall`, and `status`
- docs, graph, workflow, memory, and operator checks routed through those verbs
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

Multi-root proxy requests are represented as logical views over canonical per-root shards instead of as graph-owning combined runtimes. The daemon preserves the existing MCP method and tool schemas, warms the primary requested shard before accepting tool traffic, and prewarms the remaining view shards sequentially in the background so clients do not pay a seven-repo startup latency spike. Graph-backed workflow and dependency-analysis tools fan out across selected shards and return a bounded merged payload with per-shard summaries, source-workspace annotations, `failed_shards`, `incomplete_shards`, and context-handle routing back to the shard that created the handle. The architecture favors per-root shard ownership, session-level composed views, and compact structural graphs instead of permanently materialized multi-root mega-runtimes.

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

If you are not sure which tool to call, choose one of these public verbs:

1. `diagnose`
   Use for failing tests, stack traces, compiler errors, or runtime failures.
2. `prepare_change`
   Use for fix/add/refactor tasks. Set `mode=plan_edit` for patch spans and `mode=trace` for behavior-level debugging.
3. `context`
   Use for unfamiliar subsystems, docs questions, repo rules, file skeletons, working-set compression, or handle expansion.
4. `impact`
   Use before multi-file or non-obvious changes to compute dependents, dependencies, diff impact, and relevant tests.
5. `recall`
   Use when prior task memory or durable memory could change the plan.

Use `search` for symbols, call paths, backlinks, and outgoing links. Use `remember` only for reusable memory or workflow outcomes. Use `status` when indexing, stale docs, stale memory, or memory conflicts may explain incomplete results.

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

`tools/list` advertises exactly these 8 agent-facing tools:

| Tool | Routes To |
|---|---|
| `context` | code/docs context, subsystem summaries, file skeletons, repo rules, working sets, and handle expansion |
| `prepare_change` | change prep, patch planning, and scenario tracing |
| `impact` | impact graph, dependents, dependencies, diff impact, and relevant-test selection |
| `diagnose` | failure diagnosis from compiler, test, stack trace, or runtime output |
| `search` | symbol search, symbol detail, call-flow search, backlinks, and outgoing links |
| `remember` | quick memory, durable memory, and workflow outcome capture |
| `recall` | memory search, task memory retrieval, and memory verification/explanation |
| `status` | index status, stale docs, stale memories, and memory conflicts |

Lower-level tool names remain daemon-internal for first-party CLI/runtime use and are not accepted through public MCP `tools/call`. The removed aliases `query_context`, `blast_radius`, `get_file_context`, and `recall_memories`, plus the removed shims `verify_memory`, `explain_memory`, and `apply_memory_evolution`, return the standard unknown-tool error.

### Docs, Decisions, And Runbooks

Docs graph capabilities are reached through the public verbs:

- `context` with `mode=docs` returns the most relevant Markdown documents and sections for a natural-language query, plus related code symbols mentioned from those docs.
- `search` with `kind=links` returns inbound or outgoing Markdown references for a symbol, file, document, or section.
- `status` with `scope=docs` flags docs and sections that likely need review because they mention changed symbols, changed files, or changed docs.

### Memory, Outcomes, And Long-Running Context

Memory payloads now include additive structured assertion metadata alongside the legacy fields (`id`, `content`, `type`, `scope`, links, stale flags, and related fields). Workflow bundles and memory tools may expose:

- `assertion_type`, `verification_status`, and `confidence_reason`
- `supersedes_memory_id`, `superseded_by_memory_id`, `contradicts_memory_ids`, and `contradicted_by_memory_ids`
- `freshness_policy` and `freshness_policy_detail`
- `provenance` and `evidence`
- `evidence_links`
- `trust_status`, `trust_reason`, and `checkout_state`
- `recheck_commands`
- `risk_domains`, `requires_reverification`, and `reverification_reason`
- `artifact_conflicts`

At a high level, assistants should trust verified workflow outcomes with evidence and matching checkout state ahead of weaker stale, superseded, contradicted, unverified, or in-review recall. The legacy fields still exist; these structured fields are additive and help explain why one memory is preferred over another. New durable memories record the current Git `HEAD` ref/OID in provenance when the workspace is a Git checkout, and memory responses compare the recorded state with the current checkout. `evidence_links` lift tests, docs, files, symbols, commits, and recorded evidence into actionable references with optional recheck commands. Recheck commands are suggestions for current-code verification; they are not proof until the caller runs them and inspects the result. Security, tenancy, migration, deploy, dependency, and test-suite memories are tagged as high risk and may require re-verification even when they are relevant.

Memory trust diagnostics are deliberately response-level and current-checkout-aware:

- `trust_status` is the caller-facing tier: `trusted`, `advisory`, or `stale`.
- `checkout_state` compares the memory's recorded Git state with the active workspace checkout.
- `requires_reverification` is raised for high-risk domains until the memory is verified, evidence-backed, tied to the current checkout, and has a persisted verification timestamp.
- `evidence_links` make the supporting artifacts inspectable without digging through raw structured metadata.
- `artifact_conflicts` flag linked docs or artifacts that contain conflicting resolved/blocked style status claims for the same structured ID.
- Workflow bundles propagate memory trust fields, evidence links, recheck commands, and artifact-conflict risks so prior memory is treated as a hypothesis until current code, docs, and tests confirm it.

- `recall` with `mode=task` reads task-scoped working memory plus relevant durable memory. The daemon seeds missing task state from the task statement or hint, records automatic checkpoints, and returns trust diagnostics so unverified, evidence-free, high-risk, different-HEAD, or docs-drifted claims are not mistaken for proof.
- `recall` with `mode=search` searches stored memory across sessions within the active workspace. In multi-root logical views, the daemon fans out to every shard and merges results by exact-match score plus cross-shard context coverage so a wrong primary shard or same-ID collision cannot hide the correct workspace memory.
- `recall` with `mode=verify` verifies or explains a memory before callers rely on it.
- `remember` with `kind=quick` captures lightweight memory using active task state, focus paths, and recent failure context. In a multi-root logical view, path-bearing memory arguments route the write to the uniquely matching shard; ambiguous relative paths fail instead of falling back to the primary shard.
- `remember` with `kind=durable` creates durable memory with explicit evidence, validity conditions, and invalidation triggers.
- `remember` with `kind=outcome` persists successful workflow outcomes so future sessions can reuse real solutions. Verified workflow outcomes are treated as stronger recall when later bundles summarize durable memory.
- `status` with `scope=memory` or `scope=conflicts` finds stale memories and contradiction/supersession conflicts.

### Observability And Workspace Understanding

- `status` with `scope=index` returns current indexing progress, graph stats, and watcher health. Logical-view responses distinguish `primary_workspace` from query scope and include `workspace_field_meaning`.
- `status` with `scope=docs`, `scope=memory`, or `scope=conflicts` exposes drift and memory quality queues.
- `context` with `mode=rules` returns auto-detected repo conventions and recurring patterns.

## Workflow Response Controls

The public verbs support assistant-oriented response shaping where documented:

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
- `context`, `prepare_change`, `impact`, and `diagnose` responses can include a `context_handle` and a `suggested_expand` target
- compact `prepare_change` results keep bounded ranking evidence and state when the bundle is useful as a working-set finder versus when `rg` is the better literal-search tool
- `context` expansion handles persist across daemon restarts
- later metrics surfaces expose how often tiny/dense/single-anchor paths are actually being used

## Docs Graph Workflow

Use `context` and `search` when you want to navigate repo docs the way people use Obsidian-style vault graphs, but grounded in code and assistant workflows:

- `context` with `mode=docs` for a natural-language docs query like "how does auth login work?"
- `search` with `kind=links` and `direction=backlinks` to see which docs mention a symbol, file, document, or section
- `search` with `kind=links` and `direction=outgoing` to inspect what a document points to
- `status` with `scope=docs` after a staged diff, working tree change, or active edit

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
   the 8 public verbs build compact assistant-facing payloads on top of the graph.
5. **Remembers across sessions**
   SQLite-backed memories, playbooks, outcomes, stale-memory tracking, and stale-doc workflows preserve useful context without blindly replaying old notes.

The workflow layer is built on top of the core discovery primitives, but public MCP callers use the 8 consolidated verbs.

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
If you would otherwise open 3 or more unfamiliar files, call `context`, `prepare_change`, or `diagnose` first.
If a response returns a `context_handle` or `suggested_expand`, prefer `context` with `mode=expand` before starting a fresh broad search.
If you have raw failure text, pass it to `diagnose` before grep-driven triage.
If you're unsure which tool to use, default to `prepare_change` for implementation tasks and `context` for understanding tasks.
If the task starts from a failing test, stack trace, or compiler error, start with `diagnose` and use `prepare_change` after it narrows the likely culprit.

Use these tools when they're the best fit:

- `context` — first choice for unfamiliar subsystems, docs, repo rules, file skeletons, working sets, or handle expansion
- `prepare_change` — first choice for fix/add/refactor work; use `mode=plan_edit` for patch spans or `mode=trace` for scenario debugging
- `impact` — check blast radius, dependencies, dependents, diffs, and relevant tests
- `diagnose` — first choice when a fix starts from a failing test or error
- `search` — use for symbols, call paths, backlinks, and outgoing links
- `remember` — save quick memory, durable memory, or workflow outcomes
- `recall` — retrieve task memory, search durable memory, or verify/explain a memory
- `status` — inspect indexing health, stale docs, stale memories, or memory conflicts

Memory is recall, not proof. Treat retrieved memory as a hypothesis until current code, docs, and tests confirm it. Prefer memories with `trust_status: "trusted"`, matching `checkout_state`, concrete `evidence_links`, and useful `recheck_commands`. Do not rely on memories that are `advisory`, `stale`, unverified, from a different checkout, missing evidence, high-risk with `requires_reverification`, or carrying `artifact_conflicts` until you inspect the linked evidence and rerun the suggested checks. When saving memory, separate hypotheses from verified outcomes and attach evidence links, linked files/docs/tests, validity conditions, invalidation triggers, and the verification command that proved the claim.

For targeted edits to known files, direct Read/Grep/Edit are still fine. If the question is exact literal search, use `rg`; Lattice is intended to find the working set.
Lattice adds the most value when you do not already know where to look.
```

## License

MIT
