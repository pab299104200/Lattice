# Agent-usefulness improvement plan

Authority: follows the [operational resolution plan](2026-09-30-operational-resolution.md) (R1/R3a/R3b landed and verified). Evidence: [product/architecture evaluation](../reports/2026-09-30-product-architecture-evaluation.md) and [operational-failure diagnosis](../reports/2026-09-30-operational-failure-diagnosis.md). Beacon acceptance is explicitly out of scope for this plan.

Status: all seven items landed (A1, A2, A3, Q1, D1 fully; Q2 and R2 as flag-gated Phase 1 shipments, default OFF). `cargo test --workspace -- --test-threads=4` green; fmt clean. Phase 2/3 of Q2 (golden-fixture precision comparison, flipping defaults) and Phase 3 of R2 (macOS default, live multi-batch RSS measurement) remain as future work, documented in the [Q2/R2 design](2026-09-30-q2-r2-design.md) ledger.

## Governing principle (unchanged from the memory fix)

Prove each change with a failing (RED) test before writing the fix; sequence availability before quality (an agent cannot benefit from ranking it never receives); verify with `cargo test --workspace`, build, and fmt after each item; never accept an item on reasoning alone. Toolchain: cargo 1.99 under `~/.cargo` (run `. "$HOME/.cargo/env"` first).

## Why this order

An agent's experience of Lattice is governed by two questions: *is it ready when I call it* and *do I trust what it returns*. Availability gates everything, so the warm-load and cold-index items come first; ranking and edge-fidelity (which only matter once results are being served) come after; the determinism and freshness-contract items are trust polish on top.

## Task ledger

| ID | Lever | What it fixes | Verify-first test | Status |
|---|---|---|---|---|
| A1 | Availability | Warm-load large repos instead of silently cold-restarting (R4) | persisted graph above the limit → engine starts non-empty | **DONE — RED→GREEN verified** |
| A2 | Availability | Retry a failed cold index with bounded backoff (R5) | forced one-shot index failure → a retry is scheduled and later publishes | **DONE — policy test verified** |
| A3 | Trust | Deterministic ordering: stable secondary sort on ANN and engine tie-breaks | equal-score/duplicate-name inputs → identical order across runs | **DONE — RED→GREEN verified** |
| Q1 | Quality | `impact` reports per-edge confidence (name-approximate vs resolved) | a name-collision fixture → `impact` marks the uncertain edge | **DONE — verified** |
| D1 | Trust | Freshness contract: a cheap "is your view of these paths current?" affordance | after an edit, the contract reports stale until reindex | **DONE — proven live end-to-end** |
| Q2 | Quality | Converge the agent tools onto the `retrieval_v1` signal engine | the agent path invokes the signal ranker; diagnostics present | **DONE — Phase 1 shipped, flag-gated OFF, verified live** ([Q2/R2 design](2026-09-30-q2-r2-design.md)) |
| R2 | Footprint | Move the ONNX arena out of the daemon process (from the memory plan) | daemon RSS no longer grows with embedding batch size | **DONE — Phase 1 + recycle shipped, flag-gated OFF, 6 hermetic tests** ([Q2/R2 design](2026-09-30-q2-r2-design.md)) |

### Shipped and verified 2026-09-30 (A1, A2, A3, Q1, D1)

All changes verified with cargo 1.99: full `cargo test --workspace` green (~2371 tests), `cargo fmt --check` clean, no new build warnings. Files touched: `main.rs` (A1 `select_warm_graph`, A2 `next_cold_index_retry_delay` + retry loop), `storage/usearch_index.rs` + `query/engine.rs` (A3 stable tie-breaks), `graph/model.rs` + `graph/mod.rs` + `rpc/mcp.rs::tool_get_dependents` (Q1 `EdgeConfidence`), `rpc/mcp.rs::tool_path_freshness` + `cli.rs::parse_status` (D1 freshness scope). Each has a RED-first test; A1 and A3 were proven RED→GREEN by temporary revert; D1 was additionally proven on a live daemon (edit a file → `stale` → reconciled to `current`).

### Why Q2 and R2 were scoped, not rushed
Both are architectural changes to the highest-blast-radius surfaces (Q2 changes what every agent receives from `context`/`prepare_change`; R2 adds a process boundary). The four prior repair attempts failed by rushing high-risk changes; the discipline here is flag-gated, fixture/test-proven, incremental rollout. The design doc maps the exact integration points, risks, and phased test strategy so the next session executes rather than re-investigates. Neither blocks the five shipped improvements.

Scope discipline (point 7 of the review): this plan hardens the tools agents already call. It adds no new MCP tools. R2 remains last because it is the largest and riskiest, and the lockout it contributed to is already fixed.

## Per-item detail

### A1 — Warm-load large repos (availability, do first)
`should_warm_load_graph` (`main.rs`) returns false above `LATTICE_MAX_WARM_GRAPH_FILES` (50,000) or `LATTICE_MAX_WARM_GRAPH_BYTES` (1 GiB), so a big repo cold-restarts on every daemon start — a full "not ready" window with a good graph sitting on disk. Approach, cheapest acceptable first: raise the limits with a documented memory cost now that admission is governed (R3a/R3b), and/or serve the warm graph immediately while reconciling in the background. RED test: a persisted graph above the current limit must yield a non-empty engine and a `context` call that does not return the `indexing` placeholder.

### A2 — Retry failed cold index (availability)
A startup-index failure currently flips `indexing=false`, marks ready with an *empty* graph, and waits for an unrelated event. Add a bounded, cancellable exponential-backoff retry; keep "serve the previous graph" when one exists. RED test: force a one-shot failure, assert a retry is scheduled and a later attempt publishes a non-empty graph.

### A3 — Deterministic ordering (trust, cheap)
`usearch_index.rs` sorts ANN hits by score with no secondary key; `engine.rs` tie-breaks by name, which collides for overloads/re-exports. Add a stable, unique secondary key (identity string / byte offset) at both sites. RED test: inputs with equal scores and duplicate names produce identical ordering across repeated runs.

### Q1 — Honest `impact` confidence (quality)
Graph edges are name-based with a first-match fallback and no resolution for non-relative imports, so `impact` over-reports. Rather than (yet) rebuilding resolution, first make `impact` label each dependent/dependency edge with a confidence (resolved-by-import vs matched-by-name-only), so an agent can tell certain from guessed. RED test: a fixture with two same-named symbols in different files asserts the cross-file name-only edge is marked lower confidence.

### Q2 — Converge agent tools onto `retrieval_v1` (quality, biggest)
`context`/`prepare_change`/`impact`/`diagnose` run through the heuristic `QueryEngine`; the principled 13-signal ranker serves only working memory (proven by call-site search + handler trace). Route the agent path through `retrieval_v1` so results carry its per-signal diagnostics, or document a concrete reason code context needs a different ranker. This is the largest quality change and depends on A1–A3 being in place so it lands on a ready, deterministic base. RED/characterization test: the agent path exercises the signal ranker and surfaces ranking diagnostics.

### D1 — Freshness contract (trust)
The honest-signal metadata is passive. Add a cheap affordance: given a set of repository-relative paths, return a definitive current/stale answer against the live graph epoch, so an agent can branch on it rather than guess. RED test: edit a file, assert the contract reports those paths stale until the reindex publishes, then current.

### R2 — ONNX arena out of process (footprint, last)
Run the embedding engine in a recyclable child process so its unreclaimable arena stops inflating the daemon's own footprint; keep the in-process path behind a fallback flag. Lower absolute memory; no longer the lockout cause (R3a fixed that). Largest and riskiest, so last.

## Done means done (per item)
- [ ] A RED test reproduces the gap before the fix.
- [ ] `cargo test --workspace` green afterward with the former RED test GREEN; no regressions.
- [ ] `cargo build` clean (no new warnings from added symbols); `cargo fmt --check` clean.
- [ ] This ledger updated with verifying evidence.

When all non-deferred items are complete and verified, move this plan to `docs/plans/archive/`.
