# Persistent Daemon Shard Architecture

## Status

This note defines the successor architecture for the long-lived Lattice daemon after the current multi-root runtime model proved unsafe under real combined-workspace usage.

The current daemon shape materially differs from the old per-repo model:

- one persistent process stays resident across assistant sessions
- multiple MCP clients can connect concurrently
- one client can request a combined workspace set across several repo roots
- the daemon must reuse state over time instead of treating every session as a fresh cold-start

The old per-repo architecture does not scale to that model by simply extending lifetime or combining roots. Under the current implementation, combined workspaces can accumulate enough resident state to trigger kernel OOM kill.

## Problem Statement

The current daemon still behaves like a self-contained per-workspace runtime:

- one runtime owns graph state, parsed-file cache, watcher tasks, memory maintenance, vector sync, and compaction
- multi-root requests are materialized as one large namespaced graph
- graph nodes retain full bodies in memory
- query, indexing, retrieval, and persistence layers remain tightly coupled around that resident graph

That shape creates several failure modes in a persistent daemon:

1. overlapping workspace requests duplicate ownership
2. combined workspace sets force giant resident graphs
3. hot graph bodies turn long-lived reuse into unbounded RSS growth
4. every new daemon responsibility increases the cost of a single runtime object
5. failures become process-wide instead of repo-local

The key mistake is architectural: a persistent multi-repo daemon is not just a longer-lived per-repo runtime.

## Design Goals

The successor architecture must:

- keep one daemon process per user environment
- support multiple connected MCP clients concurrently
- support combined workspace queries without materializing a giant merged runtime as the primary state model
- bound resident memory with explicit ownership and eviction
- isolate per-root indexing and refresh work
- preserve dynamic MCP tool exposure through the proxy/daemon split
- keep request-time freshness validation for graph-backed tools
- degrade by returning bounded placeholders instead of serving stale or partial state

## Core Model

### 1. Per-root shards are the primary ownership unit

Each canonical workspace root becomes a shard with its own durable state:

- structural graph store
- file index / fingerprint manifest
- identity and section lookup data
- parsed metadata needed for docs and identity
- vector index state
- watcher / refresh ownership
- repo-state epoch tracking

A shard is the unit that loads, refreshes, evicts, and publishes.

The daemon does not treat a combined workspace request as a new graph-owning runtime.

### 2. Combined workspaces are logical views, not materialized mega-runtimes

When a client requests:

- `rmm`
- `portal`
- `rmm + portal + shared + lattice + ...`

the daemon composes a session view over a selected shard set.

That view owns:

- the root list
- scope policy
- ranking / merge policy
- context-handle namespace
- session metrics and working-memory scope

That view does not own a second indexing runtime or a second fully materialized graph by default.

### 3. Structural graph and source body retrieval are separate concerns

The primary graph should retain only compact structural data:

- stable id
- file path
- name
- kind
- signature
- byte/line span
- visibility
- dependency edges

Full symbol bodies should not be resident on every graph node by default.

Body/snippet access should come from:

- source file span reads
- compact snippet caches
- vector-text payload caches
- targeted materialization for ranking or answer packaging

This is the main memory boundary. The graph is for structure. Rich source text is an on-demand layer.

## Runtime Components

### Shard Registry

Global daemon map keyed by canonical root:

- shard lifecycle state
- load status
- active lease count
- last-used time
- memory budget metadata

The registry is responsible for:

- single-flight shard load
- eviction coordination
- backpressure when memory budget is exceeded
- process-wide admission control for graph rebuilds

### Shard Runtime

Per-root runtime owns:

- graph snapshot pointer
- repo epoch / freshness state
- watcher
- incremental index pipeline
- shard-local vector and snippet state
- memory store handle for that root

It must be possible to evict a shard without affecting unrelated roots.

### Session View

Per-client or per-workspace-set logical state:

- selected shard list
- current scope filter
- context-handle cache
- session metrics
- working memory / consolidation state

The session view composes retrieval across shards. It does not rebuild shard state.

## Query Model

### Graph-backed workflow tools

For tools such as:

- `get_context_capsule`
- `prepare_change`
- `plan_edit`
- `trace_scenario`
- `impact_from_diff`
- `find_relevant_tests`

the daemon should:

1. validate freshness per selected shard
2. exclude or suppress shards that are invalid or mid-refresh
3. query each fresh shard independently
4. merge candidates in a bounded ranking layer
5. return a combined answer plus explicit indexing/branch-switch/workspace-change reasons when one or more shards are not fresh

The merge layer is the place where combined workspaces become visible. It should not require a giant unified graph object.

### Query isolation and backpressure

Graph publication and graph querying have different concurrency requirements. Indexers publish a new immutable `Arc<CodeGraph>` snapshot under the short-lived engine lock. Context, preparation, plan, and subsystem workflows clone that query-engine snapshot, release the lock, and perform traversal and bundle construction on the blocking CPU pool. Query history is shared across snapshots so adaptive ranking behavior remains consistent without making the live graph lock a query-lifetime lock.

The daemon admits at most two concurrent CPU query jobs per handler. If both slots are occupied, another graph workflow returns a bounded partial response with `reason: query_capacity` instead of queueing unbounded work. A client-side or server-side timeout may stop waiting for a blocking task, but the task retains its permit until it actually exits. This prevents timed-out work from multiplying in the background. Status and other latency-sensitive administrative paths do not acquire a query permit and remain available while a query runs.

### Exact and administrative tools

Tools that do not require a cross-shard structural graph can operate shard-locally or directly against durable stores:

- memory queries
- session metrics
- stale-memory review
- docs link navigation when the target root is known
- status and setup surfaces

These should not pay the cost of loading or merging more graph state than needed.

## Freshness Model

The existing repo/workspace epoch idea stays, but at shard scope:

- each shard tracks `observed_repo_state`
- each shard tracks `published_repo_state`
- each shard advances a monotonic `repo_epoch`

Session views track the shard epochs they were built against.

Context handles, cached bundles, and follow-up expansions must reject stale shard epochs explicitly.

## Memory Budget Model

The daemon must stop relying on “loaded until OOM” behavior.

Required budgets:

- total daemon memory target
- per-shard resident target
- per-shard snippet/vector cache target
- max concurrently warm shards
- max concurrent full-graph index jobs

Eviction priority should prefer:

1. session-local derived caches
2. snippet and vector payload caches
3. cold shard runtimes

The structural shard graph should be compact enough that a warm shard is affordable; if not, the graph model itself is wrong.

### As built: capacity follows connected agents (2026-09-19)

A fixed "max concurrently warm shards" count starved whichever workspace connected after it was
reached: Relay went unindexed for most of a day behind three slots held by other open sessions.
Pete's ruling replaced the count: "the shard index should just be scaling with the number of
agents connected." The daemon now:

- gives every workspace with a connected agent a shard, one per checkout however many agents
  share it;
- limits itself by its measured memory footprint against `memory_budget_mb` (default a third of
  physical memory), not by a count; `max_loaded_shards` survives only as an optional operator
  ceiling, unset by default and reported whenever it defers anyone;
- under pressure unloads, least recently used first, shards with no connection, then shards whose
  agents have been silent for 10 minutes, and never a shard that is loading, indexing or serving;
- resolves each session's shard per request, so an unloaded shard reloads transparently on its
  next request and a deferred workspace heals without reconnecting.

The rules, settings and signals are in [shard capacity](../shard-capacity.md).

## Migration Plan

### Phase 1: Introduce shard registry without changing tool contracts

Status: implemented for persistent-daemon runtime ownership.

- add canonical per-root shard objects
- route daemon loading and watcher ownership to shards
- keep existing MCP tool schemas unchanged
- keep combined-workspace requests as logical shard sets

Implemented behavior:

- the global daemon registry is keyed by canonical root shard, not by the full requested workspace set
- overlapping requests such as `rmm` followed by `rmm + portal` reuse the `rmm` shard and load `portal` as a separate bounded shard when a request needs it instead of creating a combined graph-owning runtime
- shard load is single-flight per root
- idle eviction operates on shards, and capacity pressure evicts the least-recently-used inactive non-indexing shard before another shard loads
- `LATTICE_MAX_LOADED_SHARDS` caps warm shards and falls back to the legacy `LATTICE_MAX_LOADED_WORKSPACES` value for compatibility
- `LATTICE_PREWARM_VIEW_SHARDS` controls best-effort background prewarm of non-primary logical-view roots and defaults off
- startup, watcher, refresh, and explicit reindex work share `LATTICE_MAX_CONCURRENT_INDEX_JOBS` admission control, which defaults to one full-graph job per daemon
- file-watcher batches apply all upserts and removals with one graph rebuild instead of rebuilding once per changed file
- secondary shards use request-scoped leases and remain eligible for capacity eviction between fan-out requests
- current MCP methods and tool schemas are unchanged

Deliberate Phase 1 limitation:

- combined workspace views delegate current MCP tool execution to the primary requested shard
- non-primary view roots may be warm as individual shards, but current tools cannot query or merge them correctly until Phase 2
- no combined request creates a materialized multi-root graph-owning runtime
- cross-shard workflow retrieval and ranked merge are explicitly Phase 2 work

This means Phase 1 fixes unsafe ownership and duplicate runtime loading without pretending the daemon already has complete cross-shard query semantics. Background prewarm is a latency hedge for MCP clients, not a semantic merge layer: the daemon may keep the configured roots hot as evictable shards, but it must not rebuild the old multi-root mega-runtime.

### Phase 2: Move workflow query composition to shard merge

Phase 2 implemented behavior:

- logical multi-root sessions use a view request handler instead of binding every tool call directly to the primary shard
- `index_status` fans out across the selected shard set and returns aggregate node, edge, and file counts plus per-shard status entries; logical-view responses label primary-shard metadata separately from query scope and report per-call request workspace as unavailable when the stdio client does not provide caller CWD
- each shard-local `index_status` entry reports warm-load diagnostics: graph storage state (`healthy`, `rebuilt_corrupt`, `busy`, or `unhealthy`), whether persisted graph warm-load was skipped, skip reason, persisted file count, persisted DB/WAL/SHM bytes, active file and byte limits, controlling env vars, and the current effective indexed file count
- status uses lock-free graph snapshots and a non-blocking graph-store probe, so snapshot publication produces a typed `busy` diagnostic instead of delaying status
- index status includes process-wide active, queued, completed, capacity, and state fields for the bounded index-work scheduler
- tool calls with explicit absolute file/path arguments are routed to the matching shard, so warmed non-primary shards can answer targeted file and symbol requests without changing MCP schemas
- relative file/path arguments route to a non-primary shard only when that path exists under exactly one configured root; ambiguous relative paths fall back to primary handling and include routing diagnostics instead of silently inferring ownership
- explicit absolute file/path arguments outside every configured shard are not routed by guesswork; the logical view falls back to primary-shard handling and the primary tool enforces its normal workspace-boundary behavior
- broad graph-backed workflow tools fan out across the selected shard set and merge bounded ranked pivots, relevant context, memory highlights, event episodes, risks, and verification commands
- merged workflow payloads preserve the existing MCP tool response envelope and render modes while adding logical-view metadata, per-shard summaries, source-workspace annotations, and partial-failure markers
- shard fan-out classifies reachable indexing placeholders as `incomplete_shards` with `retry: "wait_and_retry"` instead of treating them as successful results; hard errors are reported separately in `failed_shards`
- cross-shard `search_memory` merges aggregate and per-shard exact-term counts so structured-ID misses can be diagnosed as absent from a shard, absent from the whole durable corpus, or present but lower ranked
- cross-shard memory ranking treats structured IDs as required anchors but uses repo/product-specific query context to break same-ID collisions across repositories
- dependency-style graph routes behind `impact` and `search` are authoritative logical-view fan-out paths rather than primary-shard-only lookups
- context handles discovered from non-primary shard responses are recorded by the logical view, so `expand_context` follow-ups route back to the shard that created the handle

Remaining work:

- add semantic score normalization across heterogeneous shard result sets instead of relying only on existing per-shard scores
- add view-level result budgeting so merged responses remain compact under large shard sets

### Derived graph integrity and daemon startup

Each shard treats `graph.db` as reconstructable cache state. File-backed open validates SQLite integrity before enabling WAL or loading graph rows. Confirmed corruption removes only the graph database and its WAL/SHM sidecars, recreates the schema, records `rebuilt_corrupt` in shard status, and lets normal workspace indexing repopulate it. Non-corruption storage failures fail shard construction; they do not create a misleading ready, memory-only graph.

Stdio proxies coordinate auto-start with a cross-process advisory lock keyed by loopback daemon address. A waiting proxy rechecks the listener after acquiring the lock and spawns only if no daemon is available, preventing simultaneous hook/MCP processes from creating competing daemon children. The lock is held through listener readiness and released when the connected stream is returned.
- remove giant unified multi-root graph as the default query surface

Required design details before implementation:

- define per-tool shard eligibility, especially for memory-only/admin tools versus graph-backed workflow tools
- preserve context-handle identity across daemon restarts by stamping handles with shard identity and all contributing shard epochs
- expose richer partial-shard freshness reasons in existing response envelopes without changing tool schemas
- prevent fan-out from becoming an unbounded N-shard query for broad combined workspace sets
- make `workspace_setup` and `index_status` report logical-view state while still showing shard-local readiness

### Phase 3: Remove resident full-body graph ownership

- change graph node payload to compact structural metadata
- move body/snippet access to span reads and snippet caches
- keep embeddings/vector payloads decoupled from permanent graph residency

### Phase 4: Add hard memory budgets and shard eviction

- daemon-wide memory budget
- shard-local cache budget
- eviction/backpressure policy
- status surfaces for resident shard count, evictions, and budget pressure
- add per-workspace configuration for warm-load and parsed-cache thresholds instead of relying only on global environment limits
- persisted graph warm-load thresholds should become per-workspace policy, with global env defaults such as `LATTICE_MAX_WARM_GRAPH_FILES` only as fallback

## Immediate Implementation Rules

Until the shard model lands:

- do not add more broad runtime responsibilities to the current combined-runtime object
- do not add more permanent large graph/body copies
- do not introduce new combined-workspace caches that materialize full merged graphs
- treat current large multi-root daemon behavior as unstable for memory-sensitive workloads

## Why This Is The Correct Direction

The current failures are not just “performance issues.” They are ownership issues.

A persistent daemon serving many repos needs:

- shard ownership
- logical composition
- explicit budgets
- structural-first graph state

Without those, every improvement is just moving the OOM threshold around.
