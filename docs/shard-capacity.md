# Shard capacity

The daemon loads one shard per workspace checkout. How many shards it keeps loaded follows the
number of workspaces with a connected agent. Real memory is the limit, not a count.

## The rules

1. **Every workspace with a connected agent gets a shard.** A workspace is never deferred because
   a number of other workspaces are loaded. Several agents on the same checkout share one shard.
2. **Shards with no connection go after a grace period.** When the last connection to a workspace
   closes and no index work is running, its shard is unloaded after
   30 minutes (`LATTICE_WORKSPACE_IDLE_TTL_SECS`). A reconnect inside the grace period finds it still loaded.
3. **Memory is the limit.** The daemon measures its real memory footprint (on macOS this includes
   compressed memory, which `ps` resident size leaves out) against a budget. The default budget
   is a third of physical memory and never less than 2 GiB: 5,461 MiB on a 16 GiB machine.
4. **Under pressure, idle shards make room first.** When loading one more workspace would take the
   footprint within 512 MiB of the budget, the daemon unloads, least recently used first:
   - a shard with no connection;
   - then a shard whose agents are connected but have sent nothing for 10 minutes
     (`LATTICE_CONNECTED_IDLE_SECS`).

   It never unloads a shard that is loading, indexing or answering a request.
5. **An unloaded shard reloads on its next request.** An agent whose shard was unloaded while it
   sat idle keeps its connection. Its next request loads the index back from disk, which takes
   seconds, and gets a partial answer while that happens.
6. **Deferral is the last resort, and it is reported.** Only when nothing can be unloaded is a
   workspace deferred. Its agent keeps its connection and the daemon retries on each request.
   `lattice status`, `lattice doctor` and the hook notice all say why, with the numbers and what
   to raise.
7. **A background sweep keeps the budget.** Once a minute, if the footprint is over budget, the
   daemon unloads at most one idle shard. When nothing can be unloaded it logs
   `memory_over_budget_nothing_to_unload` once, because the memory is not in any shard.

"Connected" means a live connection to the daemon for that workspace. An agent session is a stdio
proxy started by Claude Code or Codex. Short-lived clients such as the CLI and `doctor` hold a
shard while they run but do not count as agents.

## Settings

Set these in the [daemon settings file](../README.md#daemon-settings-file), or for one daemon
start in the environment. The environment wins. An invalid value stops the daemon with an error
that names its source.

| Key | Environment variable | Default | Meaning |
| --- | --- | --- | --- |
| `memory_budget_mb` | `LATTICE_MEMORY_BUDGET_MB` | a third of physical memory, at least 2048 | The footprint the daemon keeps within, in MiB (512 to 4,194,304) |
| `max_loaded_shards` | `LATTICE_MAX_LOADED_SHARDS` | none | Optional hard ceiling on loaded shards (1 to 64). Leave it unset: a ceiling can defer a connected workspace. If it does, that is reported |
| — | `LATTICE_CONNECTED_IDLE_SECS` | `600` | How long a connected shard must be silent before it may be unloaded to make room |
| — | `LATTICE_WORKSPACE_IDLE_TTL_SECS` | `1800` | Grace period, in seconds, before a shard with no connection is unloaded |

`LATTICE_VIEW_RESERVATION_BYTES` used to default to 256 MiB for each workspace against a 2 GiB
logical budget. That refused the ninth workspace whatever memory said: a fixed count in disguise.
It now reserves nothing unless set.

## Choosing a budget

Measured on macOS in September 2026, a freshly loaded workspace costs about 0.12 MB of footprint
per indexed file: 360 MB for 3,066 files. Six mid-sized repositories need 3 to 4 GiB. The
per-workspace `load_cost_bytes` in `lattice status` shows what each shard cost when it loaded,
when nothing else was loading at the same time.

Raise the budget if `lattice status` reports deferrals by memory while the machine has memory to
spare. Lower it if the daemon competes with builds for memory.

## Memory that is not in a shard

Unloading a shard frees what the shard owned. Two things stay:

- **The embedding model**, about 265 MiB once semantic retrieval has run, shared by every
  workspace for the life of the daemon. The model runs over at most 16 texts at a time
  (`EMBEDDING_RUN_BATCH`). Before September 2026 it ran over a whole workspace's file summaries
  in one pass, and ONNX Runtime kept that pass's peak for good: 2,000 texts in one run left the
  process at 7 GiB. That, not the shards, was the daemon's steady growth.
- **Allocator slack.** The macOS system allocator keeps freed pages and still counts them in the
  footprint; it does not return them on request (`malloc_zone_pressure_relief` was measured to
  release nothing on macOS 26). Expect some tens of MiB to stay after an unload. The next load
  reuses them.

## What to watch

`lattice status` prints one `shard_capacity` line, for example:

```text
5 connected workspace(s) with 7 agent(s); 5 shard(s) loaded, 0 idle, 2 connected but idle and unloadable; memory 1443 MiB of 5461 MiB budget (built-in default) after 8 h up; no shard ceiling
```

The JSON form (`lattice status --json`, key `daemon`) adds per-workspace state (`in_use`,
`connected_idle`, `idle`, `loading`, `indexing`, `failed`, `not_loaded`), agent counts, idle
seconds and load cost, and `deferred` (`ceiling`, `memory` or null) for whether the next workspace
would be deferred.

The lifecycle log (`~/.lattice/logs/lifecycle.jsonl`) records each decision:

| Event | Meaning |
| --- | --- |
| `shard_capacity_eviction` | A shard was unloaded to admit another; says which, why (`ceiling` or `memory`) and whether it had connected agents |
| `shard_memory_eviction` | The background sweep unloaded a shard to get back within budget |
| `shard_evicted_idle` | A shard with no connection passed its grace period |
| `shard_memory_released` | Footprint before and after a shard was unloaded. If `footprint_after_bytes` does not fall, memory is outliving its shard |
| `memory_over_budget_nothing_to_unload` | Over budget with nothing idle to unload: the memory is not in any shard |
| `shard_deferred` | A connected workspace could not be loaded; once per episode, with `pressure` (`memory` or `ceiling`) and the full reason |
| `shard_admitted_after_deferral` | A deferred workspace was loaded on a later request |
