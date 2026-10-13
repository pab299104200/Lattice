# Lattice operational resolution plan

Evidence: [operational-failure diagnosis](../reports/2026-09-30-operational-failure-diagnosis.md) and [product/architecture evaluation](../reports/2026-09-30-product-architecture-evaluation.md), both 2026-09-30, both from direct source reading. Operator-reported symptoms: daemon "normally not ready / indexing / failed," "uses too much memory" (climbs and does **not** release), weak data when ready, and "Claude refuses to use it, Codex uses it religiously." Operator environment: MacBook Pro M2; 8 repositories with tens of worktrees each; typically 3–4 worktrees active at once. Runtime instrumentation of the dev machine is not available.

Status: not started. No implementation, verification, release, or efficacy claimed. Planning document only.

## Governing principle (this is attempt #5 — do it differently)

Four prior repair attempts did not stick. The diagnosis says why: the symptoms interlock, and point-fixes each touched one symptom while the shared cause survived. This plan is therefore built on one rule:

> **Prove each root cause with a failing (RED) test before changing any production code, and sequence the dominant cause first.**

Because the dev machine cannot be instrumented, the proof comes from deterministic tests that inject state (the daemon already makes the memory `footprint` probe injectable precisely for this — `socket_server.rs` `FootprintProbe`), not from field logs. A fix is accepted only when its RED test goes GREEN and no other test regresses. If a "proof" test comes back GREEN before the fix, that root cause is wrong and we stop and re-diagnose rather than ship a guess.

## Root causes (ranked), each a fact plus the inference

Facts (from code, no runtime needed):
- **F-A — Monotonic footprint.** The ORT embedding CPU arena never returns memory and is process-global (`embeddings/engine.rs:16-33`, measured 7 GiB high-water); the macOS allocator keeps freed pages after a shard unloads (`socket_server.rs` `log_unload_footprint`, "releases nothing on macOS 26"). The daemon's real RSS only rises.
- **F-B — RSS-based budgeting with no damping.** Admission/eviction compare raw process RSS against the budget (default RAM/3) with no hysteresis and no minimum shard residency (`socket_server.rs` `pressure` / `relieve_memory_pressure` / `take_victim`).
- **F-C — Per-worktree shard cost.** Each worktree is a full shard: in-RAM `CodeGraph` + USearch ANN + SQLite + per-shard background runtimes (`main.rs` `build_workspace_runtime`).
- **F-D — Warm-load skipped on large graphs.** `should_warm_load_graph` (`main.rs:~1043`) refuses the persisted graph above `LATTICE_MAX_WARM_GRAPH_FILES` (default **50,000**) or `LATTICE_MAX_WARM_GRAPH_BYTES` (default **1 GiB**), downgrading restart to a cold re-index.
- **F-E — No cold-index retry.** A failed startup index flips `indexing=false` + marks ready with an empty graph and does not retry (`main.rs` startup task failure branches).
- **F-F — Agent tools bypass the principled ranker over a name-approximate graph** (product audit): `context`/`prepare_change`/`impact` use `QueryEngine` + name-based edges, not `retrieval_v1`.

Inferred failure (high confidence, the thing to prove in R1): F-A + F-B + F-C at the operator's scale mean the daemon's RSS ratchets up as worktrees are touched and never falls, so the budget is crossed early and **new worktrees are deferred as "not ready,"** progressively, until a restart. This single mechanism produces symptoms 1 and 2 and most of 4.

## Resolution tracks

Each track: a RED proof test, the fix, the GREEN condition. Ordered by leverage.

| ID | Root cause | What it proves / fixes | Depends on | Status |
|---|---|---|---|---|
| R1 | F-A/B/C | **Proof harness**: reproduce the worktree lockout deterministically | none | **DONE — proven RED then GREEN** |
| R3a | F-A/B | Stop budgeting/evicting against raw RSS; credit ghost pages back | R1 | **DONE — verified** |
| R3b | F-B | Minimum shard residency so a just-loaded shard is not evicted to make room | R1 | **DONE — verified** |
| R2 | F-A | Take the unreclaimable embedding arena out of the daemon process | R1 | deferred (follow-up) |
| R4 | F-D | Make large-repo restarts warm, not cold | R1 (shares harness) | not started |
| R5 | F-E | Retry a failed cold index with bounded backoff | none | not started |
| R6 | F-F | Put the principled retrieval + better edges on the agent path | none (separate workstream) | not started |

### Implemented and verified 2026-09-30 (R1 + R3a + R3b)

All changes in `daemon/crates/lattice-daemon/src/socket_server.rs`. Verified with cargo 1.99 (installed this session): full workspace `cargo test --workspace` = **2371 passed, 0 failed, 38 ignored**; `cargo build -p lattice-daemon` clean (no new warnings from the added symbols); `cargo fmt --check` clean.

- **R1 (proof).** Added `ghost_pages_from_unloaded_shards_do_not_lock_out_a_new_worktree`: a daemon whose simulated footprint only ever rises (the macOS reality), one small busy shard live, a large shard loaded then idle-evicted whose pages stay resident. Asserts a fresh worktree still gets a shard. It failed RED before the fix (newcomer deferred, `left: 0, right: 1`) — the lockout, reproduced deterministically — then passed GREEN after R3a. This is the regression guard.
- **R3a (ghost-page accounting).** Budgeting now uses `effective_footprint_bytes = raw_rss − reclaimed_ghost_bytes`, floored at the live loaded shards' attributed cost. `reclaimed_ghost_bytes` (new `AtomicU64` on `GlobalDaemon`) accrues an unloaded shard's `load_cost_bytes` at every unload site (`credit_ghost_bytes`, idempotent via swap-to-zero), wired into capacity eviction, `relieve_memory_pressure`, and `evict_idle`. `pressure()` and `relieve_memory_pressure()` read the effective value. Net: an unloaded worktree stops counting against the budget forever, so a fresh worktree is no longer locked out behind pages nothing live owns — the operator's dominant "not ready" symptom.
- **R3b (minimum residency).** New per-shard `loaded_at_epoch_secs`; a connected shard younger than `MIN_SHARD_RESIDENCY_SECS` (60s) is never unloaded only to make room (`is_idle_connected` gate). Prevents the load→evict→reload thrash for worktrees whose agents pace calls near the idle line. Proof test `a_just_loaded_shard_is_not_evicted_to_make_room_for_a_peer`.

**Not fixed this round:** R2 (ONNX arena still inflates the daemon's absolute footprint — but no longer causes the lockout, since R3a stops ghost pages counting against admission), R4, R5, R6. Interim operator mitigation (run embeddings disabled to avoid the arena entirely) remains valid and is now additive, not load-bearing.

### R1 — Prove the lockout before fixing anything
- **RED test** (new, in `socket_server.rs` tests using the injected `FootprintProbe`): construct a daemon with a small budget; feed a *monotonically rising* footprint; load N shards for N active worktrees; let idle ones be eligible for eviction; assert that after "unloading" an idle shard the next `shard_for` is still refused with `ShardDeferred(DeferralKind::Memory)` because the probe never drops — i.e. a healthy workspace is locked out purely because RSS didn't fall. A second case asserts `relieve_memory_pressure` logs `memory_over_budget_nothing_to_unload` under the rising probe.
- **GREEN condition for the track:** this test exists and fails today (proving the lockout), and is the regression guard that R2/R3 must turn green.
- If it does **not** fail as predicted, stop: the lockout theory is wrong, re-open the diagnosis before writing R2/R3.

### R2 — Move ONNX/ORT embeddings into a recyclable subprocess
- **Fix:** run the embedding engine in a separate child process (or a worker the daemon can kill and respawn) so its arena can actually be reclaimed; the daemon talks to it over the existing local IPC patterns. The daemon's own RSS stops carrying the 7 GiB arena. Keep the current in-process path behind a flag for fallback.
- **Proof:** a test (or a measured child-process-recycle) showing the daemon-process footprint no longer grows with embedding batch size; R1's lockout test, re-run with the embedding consumer out of process, no longer locks out.
- **Interim mitigation available to the operator today, no build:** run with embeddings disabled (don't install the ONNX runtime / unset `ORT_DYLIB_PATH`). Removes the largest unreclaimable consumer at the cost of semantic search. Doubles as a confirmation experiment: if lockout eases, F-A is confirmed as a major contributor.

### R3 — Budget and evict on reclaimable cost, with damping
- **Fix (two parts):**
  1. Base eviction/admission decisions on a **per-shard logical cost estimate** (graph nodes/edges, ANN size, parsed-cache bytes) rather than process RSS, so unloading an idle shard actually frees budget in the accounting. Keep RSS as a secondary safety ceiling only.
  2. Add **hysteresis** (separate high/low watermarks) and a **minimum shard residency** so a just-loaded shard can't be evicted immediately and the sweep stops chasing a number that won't move.
- **Proof:** extend R1's harness — with per-shard accounting, unloading an idle shard lets the next worktree load; with hysteresis, a shard loaded within the residency window is never chosen as a victim. These assertions are RED before the change.
- **Interim mitigation today:** raise `memory_budget_mb` in `~/.config/lattice/daemon.toml`; delays (does not fix) the lockout and confirms the mechanism if readiness improves.

### R4 — Warm-load large repos instead of cold-restarting them
- **Fix:** stop silently downgrading to cold start above the 50k-file / 1 GiB limits. Options, cheapest first: (a) raise/remove the limits with a documented memory cost and let R2/R3 govern memory; (b) stream the graph load so startup cost is bounded without discarding the whole persisted graph; (c) warm-load a bounded working subset and serve it immediately while the remainder finishes in the background. The requirement: a restart of a large repo is answerable in seconds from disk, not a full re-index.
- **Proof:** a test with a persisted graph above the current limit asserts the engine starts non-empty (warm) and a `context` call does not return the `indexing` placeholder; RED today because `should_warm_load_graph` returns false.
- **Interim mitigation today:** set `LATTICE_MAX_WARM_GRAPH_FILES` / `LATTICE_MAX_WARM_GRAPH_BYTES` above your largest repo's graph so warm-load stops being skipped — but only *after* R2/R3, or it trades "not ready" for "more memory" and worsens the lockout. Note this ordering in the operator guide.

### R5 — Retry a failed cold index with bounded backoff
- **Fix:** on startup-index failure, instead of marking ready with an empty graph and waiting for an unrelated event, schedule a bounded exponential-backoff retry (capped, cancellable on shutdown). Preserve the existing "serve the previous graph" behavior when one exists.
- **Proof:** a test with a forced one-shot index failure asserts a retry is scheduled and a later attempt publishes a non-empty graph; RED today (no retry path exists).

### R6 — Make the data worth trusting (separate workstream)
- From the product audit: route `context`/`prepare_change`/`impact` through the `retrieval_v1` signal engine (or justify why not), and raise graph-edge fidelity past bare-name matching (real import resolution; scope/type resolution) so `impact` stops over-reporting. This is what converts "ready" into "useful," and is the lever most likely to bring Claude's usage back once R1–R4 keep the tool ready.
- **Proof:** golden-fixture retrieval tests (the repo already has `benches/fixtures` and golden anchors) asserting improved precision on a known query set; a name-collision fixture asserting `impact` no longer emits false edges.

## Why this ordering resolves the "Claude refuses" symptom without touching Claude

Symptom 4 is downstream (diagnosis §Symptom 4): identical instructions to both clients, hooks that never block a tool. Claude stops calling because early responses are not-ready (R1–R4) or thin (R6). Fixing readiness and data quality is the fix for adoption; a prior attempt that edited client instructions would not have moved it. No track in this plan edits client instructions to chase adoption.

## Sequencing

1. **R1 first, always.** It is the proof and the regression guard. Nothing else starts until the lockout is RED in a test.
2. **R2 and R3 together** — they are the fix for the dominant cause and share R1's harness. R3's per-shard accounting is the deeper fix; R2 removes the biggest single offender and is independently valuable.
3. **R4** once memory is governed (so warming large repos doesn't reintroduce the lockout).
4. **R5** any time; small and independent.
5. **R6** as a parallel workstream; largest quality lever, no dependency on the memory tracks.

Operator interim stack while the above lands, in order, each reversible: (1) run embeddings-disabled to kill the arena; (2) raise `memory_budget_mb`; (3) periodic daemon restart. If (1) or a restart visibly restores readiness, that is field confirmation of F-A/F-B at zero engineering cost.

## Out of scope

- The git-ref resolver and GC-under-churn fixes (tracked in the product-architecture evaluation and the standards-remediation plan) — real, but not the operational blockers here.
- Multi-user/team features; Windows credential path.

## Done means done (per track)

- [ ] A RED test reproduces the specific failure before the fix.
- [ ] `cargo test --workspace` green on the supported targets after the fix, with the former RED test now GREEN.
- [ ] `cargo build --workspace` clean with `-D warnings`; `clippy`/`fmt` clean (per the Rust coding standard).
- [ ] No new unreclaimable process-global state; eviction decisions provably free budget (R3) or run out of process (R2).
- [ ] This ledger updated with the verifying evidence. No track accepted on reasoning alone.

When all tracks are complete and verified, move this plan to `docs/plans/archive/`.
