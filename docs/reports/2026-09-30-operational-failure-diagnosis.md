# Lattice operational-failure diagnosis

Date: 2026-09-30. This is the investigation that matters: aimed at the *actual* reported symptoms — "normally not ready / indexing / failed," "uses too much memory," "operationally gives bad data back," and "Claude refuses to use it, Codex uses it religiously" — rather than at code quality or ranking sophistication. The earlier audits (code-standard, product-architecture) looked at the interesting parts; this one looks at the broken parts. All findings are from direct reading of the daemon source; the daemon could not be run here (no toolchain/binary), so questions needing runtime measurement are listed at the end.

Why this framing: the tool has been through four repair attempts that didn't stick. That pattern usually means each attempt fixed one visible symptom while the real cause — which produces several symptoms at once — went untouched. The evidence below says the symptoms interlock, which is the most likely reason point-fixes keep failing.

## The symptoms, mapped to mechanism

### Symptom 1 — "normally not ready / indexing / failed"

What "not ready" actually is (verified): it is **not** an error. A tool call on a not-ready workspace returns a *successful* but empty bounded result (`indexing_workflow_response`, `rpc/mcp.rs:~9440`) with `indexing:true`, `partial:true`, empty `primary_files/symbols/tests`, and `suggested_expand.focus:"index_status"`. The agent is being told "retry later."

A workspace serves that bounded placeholder only when **both** `is_indexing()` is true **and** the published graph is empty (`query_engine_snapshot_for_workflow`, `mcp.rs:~1390`). The decisive fact is the warm/cold split:

- **Warm start** — a persisted `graph.db` exists and is loaded at boot (`open_graph_store_with_warm_graph`, `main.rs:~976`), so the engine has a non-empty graph and answers *immediately*, even while re-indexing. Good path.
- **Cold start** — no persisted graph, so the engine starts empty and every tool call returns the bounded "indexing" placeholder until the first startup index publishes (`update_graph_arc`, `main.rs:611`).

Here is the trap: warm-load is **skipped** when the persisted graph exceeds `LATTICE_WARM_GRAPH_FILE_LIMIT` or the byte limit (`should_warm_load_graph`, `main.rs:~1043`). For a large repository, that means **every daemon start is effectively a cold start** — a full not-ready window while it re-indexes from scratch — even though a perfectly good graph is sitting on disk. Combine that with the daemon being started on demand and restarted whenever the binary/ONNX environment changes, and a big repo spends a lot of its life "not ready."

Does it get permanently wedged? Mostly no. Every indexing-failure branch flips `indexing=false` and marks ready, keeping whatever graph exists — but there is **no retry/backoff**: a failed cold index just gives up and reports ready with an *empty* graph until some later file event or branch change triggers another attempt. The one true permanent wedge is the **process-global embedding circuit breaker** (`embeddings/engine.rs:~37-115`): if ONNX init fails or its mutex is poisoned once, semantic retrieval is `Disabled` for the entire process and only recovers on a daemon restart. That degrades data quality (lexical-only) but not overall readiness.

### Symptom 2 — "uses too much memory"

Per loaded workspace shard, RAM holds: the in-memory `CodeGraph` (petgraph), the USearch ANN index + its SQLite, parsed caches, and a stack of per-shard background runtimes (git-intelligence, health-facts, compaction, session-digest consolidation, event store/writer). Process-global on top: one ONNX embedding engine (~265 MiB) and ORT's CPU arena.

Two mechanisms make footprint **ratchet up and not come back down** — this is the core of the memory complaint:

1. **ORT CPU arena never returns memory** (documented in `embeddings/engine.rs:16-33`): it grows to the largest run it has ever seen and keeps it for the life of the process. The measured table shows one 2,000-text run leaving **7 GiB** resident permanently. The mitigation (`EMBEDDING_RUN_BATCH = 16`) caps the high-water mark but the arena still only grows.
2. **The OS allocator keeps freed pages.** The code itself documents this (`log_unload_footprint`, `socket_server.rs:~251`): on macOS, unloading a shard was measured to release *nothing* back to the OS. So after an eviction, the daemon's real RSS — which is what the budget is measured against (`process_memory_footprint_bytes`, `daemon_settings.rs:~357`) — does **not** drop.

Now the budget logic (default = physical RAM / 3, floor 2 GiB; default `max_loaded_shards = None`): admission refuses or evicts when `footprint + 512 MiB headroom > budget`, and a once-a-minute sweep (`relieve_memory_pressure`) tries to unload one idle shard when over budget. But because of (1) and (2), footprint stays high even after unloading, so the sweep "sees no benefit" (its own comment anticipates this) and the daemon sits over budget, which makes admission **defer new workspaces** (`ShardDeferred`). That is the bridge to Symptom 1: a memory ratchet turns into "not ready" for additional worktrees.

There is also a genuine **thrash window**: eviction has no hysteresis and no minimum-residency time (confirmed absent, not just unlocated). A shard can load for an active agent, push over budget, cause an idle peer to be unloaded, and that peer's agent then reloads it and evicts the first — a load→evict→reload ping-pong for agents pacing around the 10-minute idle line. When agents are *continuously* busy over budget, it defers rather than thrashing (no OOM, but no service either).

### Symptom 3 — "operationally gives bad data back"

Two independent causes, both already established:

- **When not-ready/cold**, the bounded placeholder returns *empty* results by design. If the agent calls during the cold window, it gets nothing useful — which reads as "bad data."
- **When ready**, the agent-facing `context`/`prepare_change`/`impact` tools are served by the heuristic `QueryEngine` over a **name-approximate graph** (both findings verified in the product-architecture evaluation: the principled `retrieval_v1` ranker is not on the agent path, and cross-symbol edges are resolved by bare name with a first-match fallback). So even on the good path, quality is capped by name-based edges and a magic-constant ranker, and `impact` especially can mislead.
- **Embeddings disabled** (the circuit-breaker wedge, or no ONNX runtime installed) drops the system to lexical-only, which the docs admit is second-tier for natural-language questions.

### Symptom 4 — "Claude refuses, Codex uses it religiously"

This is a *downstream symptom of 1–3, not an independent problem.* Verified:

- Both clients receive **identical** managed instruction content (`AGENTS.md` and `CLAUDE.md` are both `Kind::Instructions`). The only mechanical difference is cosmetic: the `SessionStart` notice is plain text for Codex and a JSON `hookSpecificOutput.additionalContext` envelope for Claude (`session_start_notice`, `cli.rs:~1145`).
- The hook adapter is explicitly designed so **nothing can block a tool** — every failure allows the call and emits at most one notice (`hook_adapter.rs`). So Lattice is not mechanically refusing Claude.

The most likely explanation: Codex follows its `AGENTS.md` instruction to call Lattice literally and persistently. Claude makes its own cost/benefit judgment, and if its early calls return not-ready placeholders or thin data (Symptoms 1–3), it concludes the tool isn't worth the turn and falls back to its own search. **Fix readiness and data quality and Claude's behavior should follow; tuning the Claude instructions alone will not, which may be exactly why a prior attempt failed.**

## The interlock (why point-fixes keep failing)

```
large repo ──► warm-load skipped ──► cold start on every daemon restart ──► long "not ready"
                                                                   │
ORT arena + allocator page retention ──► RSS ratchets up, never falls ──► over budget
                                                                   │
over budget ──► new worktrees DEFERRED ──────────────────────────┴──► more "not ready"
                                                                   │
not-ready / name-approximate / lexical-only ──► thin or empty data ──► Claude stops calling
```

Each previous repair likely addressed one box (raise the memory budget; tweak ranking; adjust instructions) while the arrows kept the failure alive. The two load-bearing boxes are **(a) warm-load being skipped on large repos** (turns restarts into cold starts) and **(b) the footprint ratchet** (turns a transient spike into permanent over-budget). Those two, plus the retrieval-placement issue from the product audit, are where effort should go — in that order.

## Highest-leverage fixes (hypotheses to validate against the runtime answers below)

1. **Make warm-load succeed on large repos.** Right now exceeding the file/byte limit silently downgrades to a cold start on every restart. Options: raise/remove the limit with a documented memory cost, stream the graph load instead of gating on total size, or load a bounded working subset warm and finish the rest in the background while *serving the warm subset*. This directly attacks the dominant "not ready" symptom.
2. **Stop the footprint ratchet, or budget around it.** The ORT arena and allocator retention mean RSS is a poor eviction signal. Either (a) run embeddings in a separate child process that can be recycled to actually reclaim the arena, (b) measure a per-shard logical estimate for eviction decisions instead of process RSS, or (c) add hysteresis + minimum residency so eviction stops chasing a number that won't drop. This attacks "too much memory" and the deferral cascade.
3. **Retry a failed cold index with backoff** instead of giving up and reporting ready with an empty graph. A one-shot failure currently leaves the workspace empty-but-"ready" until an unrelated event retriggers it.
4. **Converge the agent tools onto the principled retrieval and raise edge fidelity** (from the product audit) so that when the tool *is* ready, the data earns Claude's trust.

None of these is worth implementing until the runtime questions below confirm which box dominates *your* usage — that is the discipline the four prior attempts appear to have skipped.

## Open questions — only runtime data can answer these

These determine which fix is first. The daemon could not be run in this environment, so I need observations from your actual setup:

1. **`lattice status --scope index --json` on your real repo**, both right after a daemon start and once settled. Specifically: `warm_load_skipped`, `warm_load_skip_reason`, `persisted_files`, `graph_storage_state`, and `status`. If `warm_load_skipped` is true, Fix #1 is the dominant issue and explains most "not ready."
2. **Memory shape over time.** Is one `lattice --daemon` process's RSS high from the start (working-set-shaped) or does it climb over hours and never fall (ratchet-shaped)? `ps -o rss= -p <pid>` sampled over a session would settle it. Ratchet-shaped points at Fix #2.
3. **Repo size and worktree count.** Rough file count of the repo, and how many worktrees/branches have agents connected to one daemon at once. This tells us whether you're hitting the warm-load size limit and/or the per-shard memory multiplier.
4. **Is `semantic_retrieval` in status reporting `lexical_fallback` with a reason?** If so, the ONNX runtime isn't loaded or the circuit breaker tripped — a large, independent chunk of "bad data."
5. **When Claude declines, does it call `context`/`prepare_change` once and stop, or never call at all?** One transcript excerpt would confirm the cost/benefit hypothesis for Symptom 4.
6. **`memory_budget_mb` and `max_loaded_shards`** — set, or default? And the machine's physical RAM. Confirms the budget the ratchet is fighting.

Answer 1, 2, and 3 and the dominant root cause should be unambiguous — and we avoid making this a fifth attempt that fixes the wrong box.
