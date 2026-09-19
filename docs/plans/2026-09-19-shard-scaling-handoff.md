# Shard capacity scales with connected agents — handoff

Branch `feature/demand-driven-shards`, off Lattice `master` at `71552ca`.
Worktree: `/private/tmp/claude-501/-Users-pete-Cadres-relay/edf2e56f-9e94-40bf-a13f-08b7a1e86e23/scratchpad/wt-shards`.
This file is the resume point. Update it and commit after every task.

## Ruling (Pete, 2026-09-19, verbatim)

> "the shard index should just be scaling with the number of agents connected."

## Intent

- A workspace with at least one connected agent always gets a loaded shard. Capacity is demand-driven,
  not a fixed number that starves whoever connects last.
- Several agents on one checkout share one shard: the unit is distinct connected workspaces.
- A shard with no connected agent and no index work becomes evictable after an idle grace period,
  so capacity also scales back down.
- Memory, measured as the real footprint (including compressed memory), is the safety limit.

## Design as built

- **Connected** means a live connection to the daemon for that workspace. Agent sessions (stdio
  proxies from Claude Code or Codex) are counted as agents; CLI and doctor clients are counted
  separately and do not make a workspace "connected". A deferred workspace is still counted.
- **Admission** (`GlobalDaemon::shard_for`): no count defers a connected workspace. Pressure comes
  only from an operator ceiling (`max_loaded_shards`, now optional, default none) or the memory
  budget (`memory_budget_mb`, default a third of physical memory, minimum 2 GiB). Under pressure
  the daemon unloads, least recently used first: shards with no connection, then shards whose
  agents have been silent for `LATTICE_CONNECTED_IDLE_SECS` (default 600). It never unloads a
  shard that is loading, indexing or answering a request. Only when nothing qualifies is the
  workspace deferred, with the reason in plain words (ceiling or memory, the numbers, what to raise).
- **Transparent reload**: each lease resolves its shard per request (`LeaseShardHandler`). A
  retired shard is replaced by a fresh one on the next request; a deferred workspace retries
  admission on each request. `try_retire` and `RuntimeLease::begin_work` handshake through the
  runtime mutex, so exactly one side wins a race between unloading and a new request.
- **Idle eviction**: unconnected shards go after `LATTICE_WORKSPACE_IDLE_TTL_SECS` (unchanged).
- **Memory sweep**: once per cleanup tick (at most every 60 s), if the footprint is over budget,
  unload at most one idle shard. When nothing can be unloaded, log
  `memory_over_budget_nothing_to_unload` once per episode.
- **Hidden count removed**: the 256 MiB-per-view logical reservation against a 2 GiB budget was a
  cap of eight in disguise. It now reserves nothing unless `LATTICE_VIEW_RESERVATION_BYTES` is set.
- **Visibility**: `lattice status` `shard_capacity` line and the `daemon` report show connected
  workspaces, agents, loaded, idle and connected-idle shards, footprint against budget and its
  source, the ceiling and its source, whether the next workspace would be deferred and why, and
  per-workspace state, agents, idle seconds and attributable load cost.

## Tasks

- [x] (a) Connection-driven admission, optional ceiling reported honestly — `8c3bca8`, `de6c2ff`, `42abae7`
- [x] (b) Idle eviction after grace, reload on reconnect — same commits; tests pass
- [x] (d) Memory-budget eviction order: unconnected, then connected-idle, then honest deferral
- [x] (e) Transparent unload of idle-but-connected shards
- [x] Test hygiene: unit tests ran against the real HOME (see Findings) — `0116284`
- [x] Memory sweep unloads at most one shard per tick
- [x] (c) Status and doctor visibility (unit-tested; exercised by the acceptance run)
- [x] (f) Memory growth found and fixed: the embedding model ran whole workspaces in one pass — `c6e49ab`
- [x] (g) Docs: `docs/shard-capacity.md`, README daemon settings, resource budgets, shard architecture, hook-enforcement — `c588d20`, `ab22819`
- [ ] Full suite three times with `--no-fail-fast`, fmt, clippy, no orphan processes, real HOME untouched
- [ ] Private-daemon acceptance: N workspaces load, idle eviction, reconnect reload, memory pressure

## Findings so far

1. **Unit tests wrote to the operator's real state** (pre-existing on master). The memory-retention
   registry `~/.local/state/lattice/memory-retention-stores.json` held 77 entries for deleted test
   directories out of 84, and `resource-budget-homes.json` 783 entries. Tests also read the real
   `daemon.toml` and contended with the live daemon for the registry lock, which failed shard
   bootstraps under a parallel run. Fixed at the process level: `src/test_isolation.rs` points
   `HOME` and the XDG roots of every unit-test process at a private directory before `main`.
   The stale entries in the real registries are still there; cleaning them is an operator step.
2. **A test helper self-deadlocked** (`wait_for_shard_published` held the runtime lock across a
   condition that took it again). That, not a product defect, was the "shard never quiets down".
3. **Memory after unloading, live daemon, 2026-09-19 10:0x** (up 9 h 27 min, 9 loads and 9
   idle evictions since the 00:43 restart, no shard loaded): footprint 1.6 GiB, resident 12 MB.
   `vmmap -summary`: default malloc zone 639 MiB in live allocations, 688 MiB freed but retained
   (52 % fragmentation), most of it swapped and compressed.
4. **macOS 26.6.1 system malloc does not return freed memory on request**:
   `malloc_zone_pressure_relief(NULL, 0)` returns 0 and changes nothing; after freeing all of
   400 MiB of 100 KB blocks the footprint stays at 350 MiB (probe `relief.c` in the session scratchpad).
5. Hooks do not grow memory: 900 hook calls with no shard loaded added about 2.5 MB.
6. **Shard load and unload do not leak.** Under malloc stack logging, three load, query and
   unload cycles of one workspace left 545 KB of live allocations (1,944 blocks).
7. **The growth is the embedding model's ONNX Runtime arena.** The live heap held single blocks
   of 256, 128, 64, 32 and 16 MiB, each 16 KiB over a power of two: an arena that doubles and
   never shrinks, held by the process-wide embedding engine. `vector_sync` embedded every file
   summary of a workspace in one run. Measured: one run of 2,000 texts leaves 7,049 MiB for the
   life of the process; runs of 16 leave 296 MiB and are 2.3 times faster (17.4 s against 40.0 s).
   Fixed in `EmbeddingEngine::embed_batch` (`EMBEDDING_RUN_BATCH = 16`), with a model-free unit
   test and a real-model equivalence test (`--ignored`, needs `ORT_DYLIB_PATH`).
8. **mimalloc was tried and rejected**: over four load and unload cycles it held 186 to 285 MiB
   after each unload against 32 to 136 MiB for the system allocator.
9. **Private-daemon experiments before 11:00 ran without embeddings**: ONNX Runtime is loaded
   from beside the executable, and worktree builds do not have it. Copy
   `libonnxruntime.dylib` next to the binary, and set `LATTICE_EMBEDDING_MODEL_DIR` to the
   versioned directory (`~/.lattice/models/all-minilm-l6-v2-1110a243`), not its parent.
10. Outside this change, seen on the way (hypotheses, not investigated): index jobs on four
    fresh clones of real repositories ran one at a time and mostly waited rather than computed
    (11 jobs in 20 minutes, the daemon near 0 % CPU); memory maintenance logged
    `snapshot content hash mismatch` for a freshly created keystone memory store.

## Build and test

```bash
cd daemon
cargo build -p lattice-daemon --bin lattice --tests
cargo test -p lattice-daemon --bin lattice -- socket_server::tests daemon_settings test_isolation doctor::
cargo test --no-fail-fast            # full suite; run three times at the end
cargo fmt --all --check
```

Do not rebuild `/Users/pete/Cadres/lattice/daemon/target/release` or restart the live daemon:
`~/.local/bin/lattice` links there. Build in this worktree, whose `daemon/target` is separate,
and test against a private daemon (sandbox `HOME`, `XDG_STATE_HOME`, `LATTICE_DAEMON_ADDR` on
another port, `LATTICE_LIFECYCLE_LOG_DIR`).

## Rollout and rollback

Owner: the coordinator or Pete. Nothing below has been run against the live install.

1. Merge: `git -C /Users/pete/Cadres/lattice merge --ff-only feature/demand-driven-shards`
   (branch is based on `master` at `71552ca`).
2. Remove the ceiling written on 2026-09-19. It is honoured by the new build and would bring back
   count-based deferral (reported, but still deferral): delete the line `max_loaded_shards = 6`
   from `~/.config/lattice/daemon.toml`. Leave `memory_budget_mb` unset unless there is a reason;
   the default on this 16 GiB machine is 5,461 MiB.
3. Build: `cargo build --release --manifest-path /Users/pete/Cadres/lattice/daemon/Cargo.toml`.
   This replaces the hook adapter for every live session at once, because `~/.local/bin/lattice`
   links into that target directory.
4. `lattice doctor` must print `WARN running daemon still uses a fixed shard count; restart it`.
5. Restart: `pkill -f 'lattice --daemon'`. Effect on the open sessions (relay, synapse, beacon,
   keystone, portal): each proxy restarts the daemon on its next request; shards reload from their
   persisted indexes, so answers are partial for seconds, not minutes; hook session state is on
   disk and survives. The live daemon also gives back its 1.6 GiB.
6. Verify: `lattice doctor` shows `PASS running daemon: N connected workspace(s) …; no shard
   ceiling`; `lattice status --workspace /Users/pete/Cadres/relay` shows the `shard_capacity`
   line; after a few hours `grep shard_memory_released ~/.lattice/logs/lifecycle.jsonl` shows the
   footprint falling at each unload. Expected, from the private-daemon runs (not yet observed
   live): about 300 MiB with the embedding model loaded, plus the loaded shards.
7. Optional clean-up of the dead test entries in the real registries (while the daemon is
   stopped, between steps 5 and its restart, to avoid the registry lock):
   ```bash
   python3 - <<'PY'
   import json, os, pathlib
   p = pathlib.Path.home() / ".local/state/lattice/memory-retention-stores.json"
   d = json.loads(p.read_text()); before = len(d["stores"])
   d["stores"] = [s for s in d["stores"] if os.path.exists(s)]
   p.write_text(json.dumps(d)); print(before, "->", len(d["stores"]))
   PY
   ```
   `resource-budget-homes.json` is keyed by repository id, not path; leave it to Lattice's own
   cache GC.

**Rollback:** remove `memory_budget_mb` from `daemon.toml` if it was added (the old binary
rejects unknown keys and will not start), restore `max_loaded_shards = 6`,
`git -C /Users/pete/Cadres/lattice reset --hard 71552ca` on `master` (or check out that commit),
rebuild as in step 3, and restart as in step 5.

## Lattice's own tools

The first builder ran `lattice context`, `prepare_change` and `impact` on the Lattice repository
before editing (pivot files `daemon_settings.rs`, `socket_server.rs`; no relevant memories).
