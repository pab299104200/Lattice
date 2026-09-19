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
- [ ] (c) Doctor output reviewed end to end on a private daemon
- [ ] (f) Memory growth: attribute the live allocations that survive unloading; fix; allocator retention
- [ ] (g) Docs: `docs/shard-capacity.md`, README daemon settings, shard architecture doc, hook-enforcement.md
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

To be written when the branch is complete.

## Lattice's own tools

The first builder ran `lattice context`, `prepare_change` and `impact` on the Lattice repository
before editing (pivot files `daemon_settings.rs`, `socket_server.rs`; no relevant memories).
