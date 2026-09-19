# Handoff: enforced Lattice workflow in hooks

Branch: `feature/hook-enforcement` (from `master`). Brief:
`docs/plans/2026-09-19-hook-enforcement-brief.md`. This file is written incrementally.
Every claim is tagged **verified** (evidence named) or **hypothesis**.

## Status

COMPLETE on the branch, not rolled out. Nothing live was changed: the running daemon
(pid 57458) was not restarted, no product repository was touched, and `daemon/target/release`
was not rebuilt. See "One thing the brief got wrong" before rolling out.

## Host contract, checked against primary sources (2026-09-18)

Claude Code, from `https://code.claude.com/docs/en/hooks.md` (fetched raw, read directly):

- **verified** PreToolUse blocks with exit 0 plus
  `hookSpecificOutput.{hookEventName:"PreToolUse", permissionDecision:"deny", permissionDecisionReason}`.
  The reason on `deny` is "shown to Claude". Exit 2 "routes the same way as deny". Top-level
  `decision`/`reason` is deprecated for this event. Precedence is deny > defer > ask > allow.
- **verified** (hooks guide) a hook `deny` "blocks the tool even in bypassPermissions mode".
- **verified** `tool_input.file_path` is always absolute for Write, Edit and Read.
- **verified** matchers made only of letters, digits, `_`, `-`, spaces, `|`, `,` are exact
  alternatives, so `apply_patch|Edit|Write|MultiEdit|NotebookEdit` is valid and case-sensitive.
- **verified** `additionalContext`, `systemMessage` and plain stdout are capped at 10,000
  characters; PostToolUse takes `hookSpecificOutput.additionalContext`.
- **verified** Stop input carries `stop_hook_active` and `last_assistant_message`. Stop
  `hookSpecificOutput.additionalContext` is NOT passive: "The conversation continues so Claude
  can act on it", under the `stop_hook_active` flag and an 8-continuation cap.
- **verified** hooks fired in subagents add `agent_id` and `agent_type`. The docs do not say
  whether `session_id` is shared with the parent. **hypothesis**: it is shared.
- **not documented** `MultiEdit` (absent from the tools reference) and the NotebookEdit path
  field. The adapter accepts `file_path` and `notebook_path`; a gated tool with no readable
  path is allowed.
- **verified** `tool_response.bashEditDiff.changedFiles` exists (v2.1.269+), but the docs call
  it best effort, beta, and say "not to enforce a policy". Not used; repository state is.

Codex, from `openai/codex` `codex-rs/hooks/src` on the default branch (read directly):

- **verified** events include PreToolUse, PostToolUse, Stop, SessionEnd. PreToolUse blocks on
  exit 0 JSON `permissionDecision: "deny"` with a reason, or exit 2 with stderr.
- **verified** `apply_patch` hook `tool_input` is `{"command": <patch text>}`. There is no
  dedicated path field, so a per-path gate would need the patch text. Lattice does not read it.
- **verified** Codex Stop can only block-and-continue; there is no passive model-facing note.

## Diagnosis (requirement 7)

Investigated read-only by a subagent, then each code claim re-read by me.

### Capture gap after 2026-09-18 07:31 EDT

- **verified (code)** `sanitize_text` (`lattice-core/src/memory/session_digest.rs`) returns
  `None` when the text has any control character, and checks that BEFORE it collapses
  whitespace. A newline is a control character, so every multi-line final message is dropped.
  `normalize_turn_summary` (`session_capture.rs`) also rejects anything over 2,000 bytes instead
  of truncating, and `contains_shell_construct` rejects any backtick, `;`, `|`, `<`, `>`.
- **verified (code)** a dropped summary gives `payload: None`, and the Stop branch then returned
  silently before connecting. Client-side drops never reach capture metrics.
- **subagent evidence, not re-run by me** transcript shows `compact_boundary` at
  2026-09-18T11:31:07Z; of 54 main-thread final messages after it, 39 fail on a newline and 15
  on length, none pass. Before it, 25 of 147 matched a journal row. Stop hook after 11:31 runs
  in 26-84 ms (client-side exit); captured Stops take 80-1,400 ms.
- **refuted** my first hypothesis (12-hour absolute binding expiry): the binding that wrote the
  last row was live until 22:14 UTC.
- **ruled out by the subagent** journal row cap (largest binding 9 rows of 16,385), daemon
  restart (pid 57458 up since Sep 14 06:30), wrong binary (`~/.local/bin/lattice` symlinks to
  `daemon/target/release/lattice`), rejections (metrics hold only `captured`), disk or panic.
- **Lattice defect: yes.** Fix status is under "Built".

### Two more capture defects found while building

- **verified (code + host docs)** Claude Code edit capture and the PostToolUse note never worked.
  Claude Code "always" sends an absolute `tool_input.file_path`; `is_safe_repository_path`
  rejects any path starting with `/`; `extract_edit_event` turned that into an error the adapter
  swallowed. The existing test used a relative path, so it passed. Matches the subagent's
  observation that Relay's journal has no `event` rows at all. So the brief's premise is
  incomplete: even Edit-tool edits got no feedback.
- **verified (code)** client/daemon idle-deadline desync. The daemon renews a binding's idle
  deadline on every admitted delivery; the client store renewed only on open/resume, and Stop
  never opened. After Stop-only traffic the client believes the binding expired, prunes it and
  opens fresh, and the daemon refuses because one is still open. Every hook then fails silently
  until the daemon's own deadline passes. **hypothesis** that this explains the 12 acceptable
  summaries the subagent found missing under a live daemon binding.
- **verified (Codex source)** Codex ignores plain-text stdout on PostToolUse, so the Codex
  edit-impact note never reached the model either.

### `indexing: true` / `graph_snapshot_state: not_loaded`

- **subagent evidence** `lattice status` for Relay returns `bootstrap.state: "deferred"` with
  "3 loaded workspace shards and all are active or indexing; defer /Users/pete/Cadres/relay",
  and `index_work` idle with 0 jobs. Slots are held by portal, keystone and beacon, each pinned
  by a live `--stdio` proxy, so none is evictable. No relay shard event in `lifecycle.jsonl`.
  Relay: 4,937 tracked files. Not a watcher storm, failed index or budget block.
- **verified (code)** `cold_index_status_payload` (`socket_server.rs`) set `status: "indexing"`
  and `indexing: true` whenever there was no bootstrap error, including when
  `deferred_reason` was set. Relay was never indexing; it was never admitted.
- **Lattice defect: yes, in the reporting.** Fixed (see "Built"). The shard cap itself is
  configuration: **operator action** to get Relay indexed is to close a portal, beacon or
  keystone Lattice session, or start the daemon with `LATTICE_MAX_LOADED_SHARDS` raised. A
  workspace with its own live proxy waiting forever behind the cap is a design weakness I have
  NOT changed; it needs an admission-policy decision.
- **subagent observations, not acted on** daemon process (Sep 14 06:30) is older than the binary
  on disk (Sep 14 17:57); `sessions.db-wal` is 4.1 MB and unchecked since Sep 16; about 100
  registry bindings stay `open` past their deadline because expiry is lazy.

## Follow-up from the coordinator, 2026-09-19: shard starvation

The coordinator observed from Relay `partial_reason: runtime_bootstrap_deferred`, with seven
stdio proxies for five workspaces (relay, beacon x2, portal x2, keystone, synapse) on one daemon.

**Correction to one inference in that message.** The deferral did not stop the capture journal
at 07:31. Hook routes are shard-independent. **verified**: on a private daemon capped at one
shard, with that shard pinned by an open proxy, a starved workspace still recorded an
`edited_path` and a `turn_summary`. Relay's own journal also kept rows until 09-18 although its
leases were deferred from 09-15. The 07:31 stop is the sanitizer defect above. Both were silent,
which is the real point.

Reproduced and **verified** on that private daemon, with the final binary:

```
status= deferred | indexing= False
summary= Deferred behind shard capacity: every shard slot is held by another workspace with an
  open session, so this workspace has not been loaded and nothing is indexing it. ...
SessionStart (enforcing): "lattice: Lattice has deferred loading this workspace because every
  shard slot is busy ... Edits are not blocked. ... state in your report ..."
repeat SessionStart: silent.  Gate with no plan while deferred: allowed.
```

Assessment of the four questions:

1. **Are idle shards evicted when sessions stay open for days? No, by design.** `is_evictable`
   requires zero connections, and the architecture doc says a runtime becomes evictable "once
   the last proxy disconnects". A session open for three days pins its shard for three days,
   used or not. With more open workspaces than slots the late arrival starves forever. This is
   a fairness defect in the design. **Not fixed**: a fair fix evicts a connection-pinned shard
   that has no request in flight (that is already tracked by `RuntimeWorkTracker`) and has been
   unused for some minutes, and makes every lease re-admit its shard on the next request, as
   `DeferredShardHandler` already does. Today an evicted `ShardEntry` would answer its open
   connections "indexing" forever. That is a change to the daemon's shard lifecycle and its
   shutdown tests, outside this brief, and it should be its own reviewed change.
2. **Is five workspaces a supported load here? Yes, easily.** **verified**: the live daemon uses
   191 MB resident with three shards loaded, on a 16 GiB machine. Admission already reserves
   256 MiB per shard against a 2 GiB budget, which allows 8. The count cap of 3 was a second,
   smaller, arbitrary gate, and the operator guide already documented the default as 8.
   **Fixed**: the default cap is now the view budget divided by the view reservation (8 with
   defaults, at most 16). `LATTICE_MAX_LOADED_SHARDS` still overrides it.
3. **Durable configuration.** There is none, and an environment variable cannot be made durable:
   **verified in code and by observation**, the daemon is spawned by whichever stdio proxy first
   finds it missing and inherits that client's environment. A hand-started daemon with the
   variable set loses a race with seven proxies that reconnect the moment the old one dies.
   Changing the default is therefore the durable fix. A daemon config file would be the general
   answer and does not exist.
4. **Should capture queue while deferred? No need.** Nothing is dropped; see the correction.

Restart, for Pete to decide. **Do not run it before the new binary is built**, or the proxies
respawn the old binary with the cap of 3:

```bash
cd /Users/pete/Cadres/lattice && git checkout feature/hook-enforcement
cargo build --manifest-path daemon/Cargo.toml --release
pkill -f 'lattice --daemon'
```

Effect on the other sessions, **verified** on a private daemon: an open stdio proxy survives
the kill, spawns a new daemon on its next request and answers it. No session ends and no client
needs reconnecting. What is lost: requests in flight at that instant fail once; every workspace
reloads its shard from its on-disk index on first use (seconds, and cold answers are marked
partial meanwhile); in-memory context handles from before the restart expire. Hook capture
bindings survive, because they are on disk. With five workspaces and a cap of 8, Relay loads.

## Second follow-up, 2026-09-19: durable shard cap, and memory measured honestly

### Correction: my "191 MB, comfortably supported" was wrong

I used resident size. On macOS that excludes compressed memory. **verified** with
`top -l 1 -pid 57458 -stats mem,cmprs`: the live daemon's footprint is **8,831 MB, of which
8,770 MB is compressed**; RSS was 66 MB at that moment. The machine has 16 GiB and was at
41 percent free, so it is coping.

Measured per-shard cost, **verified** on a private daemon under a sandbox `HOME`, loading
shallow scratch clones (no product repository or its `.lattice` state was touched):

| Step | Footprint |
|---|---|
| daemon, no shard | 3 MB |
| keystone loaded, 3,066 indexed files, 42,452 nodes | 359 MB |
| after two queries | 365 MB |
| synapse also loaded, 1,483 files, 16,142 nodes | 409 MB |
| idle 1, 2, 3 minutes (background work settles) | 528, 532, 532 MB |

So a fresh shard costs about 0.12 MB per indexed file. All six repositories (about 35,000
tracked files) come to roughly **3 to 4 GiB fresh**. Cold indexing keystone took 10 minutes,
I/O-bound in `ContentObjectStore::put`, with the machine also running two test suites.

**The real risk is growth, not the cap.** The three live shards (portal, keystone, beacon)
should cost about 2.3 GiB fresh and hold 8.8 GiB after 4.7 days: roughly 6.5 GiB accumulated,
about 1.4 GiB a day. **hypothesis**: an unbounded cache or a leak; I did not find the cause.
I did not run `vmmap` or `footprint` on the live daemon because both suspend the target while
they read it. For Pete, if he accepts a pause of a few seconds for every session:
`vmmap --summary 57458 | tail -40`. The restart in the rollout resets the footprint, which is
also why this will look fixed when it is not.

**Recommendation: 6 is sound** on fresh numbers and leaves about 12 GiB of headroom on this
machine. Re-check after a few days with `lattice doctor`, which now prints the real footprint
and uptime. If it is back above about 6 GiB, the growth needs its own investigation.

### Built

- `daemon_settings.rs`: `$XDG_CONFIG_HOME/lattice/daemon.toml`, default
  `~/.config/lattice/daemon.toml`. Lattice had no config convention; this mirrors its use of
  `XDG_STATE_HOME`. First key `max_loaded_shards`, 1 to 64.
- Precedence: `LATTICE_MAX_LOADED_SHARDS`, then the older `LATTICE_MAX_LOADED_WORKSPACES`, then
  the file, then the default. The file is validated even when the environment wins.
- Invalid value, wrong type, unknown key, broken TOML, a directory in the file's place, or an
  invalid environment value: the daemon exits 1 **before binding its port**, names the source,
  and logs `daemon_settings_invalid`. The old `env_usize` silently ignored a bad value; removed.
- `lattice status` gains `daemon` (value, source, file path, slots loaded and pinned, memory
  footprint, uptime) and a one-line `shard_capacity` for the plain view, which collapses nested
  objects. It is attached at the transport layer, because a shard cannot know daemon-wide facts.
- `lattice doctor` validates the file (FAIL if the next start would be refused), prints the
  running daemon's value, source, slot use, memory and uptime, and WARNs when a restart would
  change the cap, when every slot is pinned, when the daemon predates settings reporting, and
  for a deferred workspace, which it used to print as `PASS ... watcher=healthy`.
- Built-in default is now 6, no longer the budget-derived 8, which was not grounded in memory.
- `README.md`: "Daemon settings file" section directly above the environment table.

**verified** on private daemons with the final binary: invalid file exits 1 and the port stays
unbound; typo key and invalid environment are named in the error; valid file gives
`max_loaded_shards=6 from settings file ...`; environment 4 beats file 6; with the file changed
to 9 under a running daemon, doctor says "A restart would use 9 from settings file ...".

Not mine, still running when I finished: two `target/enforcement/debug/lattice --daemon`
processes on random ports (pids 71659, 71877, started 00:09). I never built debug there, so I
left them. **hypothesis**: orphans from the coordinator's test run against my target directory.

Two pre-existing tests failed once each under load and passed alone and on rerun:
`authenticated_ordinary_rpc_loads_its_shard_only_after_first_request` (250 ms wall-clock bound)
and `indexed_plan_reaches_cache_after_four_thousand_absent_rows` (one-second grace). Neither is
in code this branch changes. **hypothesis**: load-sensitive timing, with two suites and a
measurement daemon running at once.

## Design decisions

### How the daemon knows a `prepare_change` happened

Neither host passes its session id to an MCP server or to a shell command, so an MCP or CLI
`prepare_change` cannot be tied to one host session by anything the host supplies. Adoption
metrics are the wrong authority: telemetry, skippable (`LATTICE_SKIP_METRICS`), keyed by the
daemon's own MCP session id.

Built instead: a **daemon-recorded workflow fact**. At the one point where the transport layer
serves a public `tools/call` (`run_json_rpc_connection`, MCP and CLI alike), a successful
`prepare_change`, `remember` or `status` with scope `docs` is recorded in
`<state root>/hook-sessions/workflow.db` as (checkout, step, time). It is written before the
response, so an edit retried straight after a plan sees it. Only the daemon can write it, and
only for the checkout it resolved for that connection. No query, argument, result or path is
stored. The hook session, authenticated by its binding capability, reads it on the same
`hook/session_open` call that already carries presentations.

**Known limit, by construction:** the fact is checkout-scoped, not session-scoped. Two sessions
in one checkout share plans. A cold or deferred workspace's partial `prepare_change` answer
counts as a plan, because the agent did what was asked of it.

### Freshness rule (`hook_enforcement::evaluate_plan`)

A plan is current when it was recorded at or after the session's context floor, is at most
8 hours old, and at most 45 minutes have passed since the later of the plan and the last product
edit made under it. Each allowed edit slides the 45-minute window, so one plan covers a long
multi-file change. The context floor moves to "now" on SessionStart with source `startup`,
`clear` or `compact` (the model can no longer see the old plan); `resume` keeps it. Workflow
session identity is the keyed authority fingerprint of integration + host session + checkout,
because binding ids are re-minted every time an idle binding expires.

### Fail-open rule

Only a reachable daemon with a loaded or loadable index and no current plan denies. Daemon
down, daemon refusal (including a pre-enforcement daemon, which rejects the unknown
`enforcement` field), adapter deadline, local adapter failure, index indexing/deferred/failed,
unreadable policy, and an uninterpretable answer all allow the tool call and emit one notice
per session per condition.

### Stop reminder

Claude Code's Stop `additionalContext` is not passive; it continues the conversation once. The
reminder is claimed once per session by the daemon and never requested while
`stop_hook_active` is true, so it cannot loop. Codex has no such channel, so it gets
`systemMessage` (operator-visible) instead.

## One thing the brief got wrong

The brief says to build to `daemon/target/release` and not to reinstall the binary.
**verified** `~/.local/bin/lattice` is a symlink to `daemon/target/release/lattice`, and the
live daemon runs from that path. A release build there IS a reinstall: it swaps the hook adapter
under every live session in every repository. I built to `daemon/target/enforcement` (ignored
by git) and tested against private daemons on ports 47991 to 47993 under a sandbox `HOME`.
**verified** afterwards: `daemon/target/release/lattice` is still dated Sep 14 17:57, one daemon
process, same pid, and its lifecycle log has no entry for any scratch workspace.

## What is built

| Requirement | Where | State |
|---|---|---|
| 1. Opt-in per workspace | `hook_enforcement.rs` (policy file), `cli.rs` (`--enforce`, `--no-enforce`, mode kept on plain rerun), `install.rs` (`HookMode`) | done |
| 2. PreToolUse gate | `hook_adapter.rs` (`pre-tool-use` kind, path class), `hook_session_route.rs` (`decide_enforcement`), `hook_workflow_state.rs`, `socket_server.rs` (records the served step) | done |
| 3. Fail open, loudly | `hook_adapter.rs` (`failure_output`, `claim_notice`), 11 conditions in `hook_enforcement.rs` | done |
| 4. Shell edits | `hook_shell_changes.rs`, `run_post_tool_use` in the adapter | done |
| 5. Stop reminder | daemon claims it once; adapter never asks while `stop_hook_active` | done |
| 6. Codex parity | `integrations/codex/hooks/pre-tool-use.sh`, JSON output on tool events, README states the four missing guarantees | done |
| 7. Diagnosis | above; three Lattice defects fixed, one design weakness reported | done |
| 8. Tests | below | done |
| 9. Docs | `docs/hook-enforcement.md` (new, citable), both integration READMEs, `README.md`, `docs/agent-workflow.md` | done |

Defects fixed along the way, each with a test:

1. Multi-line turn summaries dropped whole (`sanitize_text` ordering). Long summaries dropped
   instead of bounded. Host-side only, Markdown code spans and fences become `[code]` before
   the unchanged strict check. The deliberate rule that a summary with a backticked command is
   refused on the wire is kept, and its existing test still passes.
2. Claude Code absolute edit paths rejected, so edit capture and the PostToolUse note never
   worked. Paths are now made checkout-relative; outside paths yield no fact.
3. Client and daemon idle deadlines drifting apart. An idle-expired binding is now offered to
   the daemon for resume; only a refusal discards it. Stop resumes or opens its binding.
4. One refused delivery blocked the whole ordered queue forever. A refused non-close delivery
   is dropped and the rest drain; a close is never dropped.
5. Codex ignored plain-text tool-event output. Both hosts now get JSON there.
6. `lattice status` claimed `indexing: true` for a deferred workspace. It now reports
   `status: "deferred"`, `indexing: false`. Merged views still treat it as incomplete.
7. `integrations/codex/tests/hooks_test.sh` used `rg`; without ripgrep installed the guard
   against public CLI calls in hook scripts passed vacuously. It uses `grep` now, and I watched
   it fail when a script called `lattice impact`.

## Test results (observed, final code)

```
cargo test --workspace            2,341 passed, 0 failed, 39 ignored
  lattice-core 1,298 | lattice-daemon lib 384 | lattice-daemon bin 651 | integration 8
cargo fmt --all -- --check        pass
integrations/claude-code/tests/enforcement_e2e.sh <binary>    43 passed, 0 failed (5 runs in a row)
integrations/codex/tests/hooks_test.sh                        pass
integrations/codex/tests/install_test.sh                      pass
```

About 75 new unit tests. Tests proven to bite by injecting the bug: the workflow-session key
(restoring the per-generation key makes `a_plan_survives_binding_regeneration...` fail with
`deny`), the script guard above, and the end-to-end harness (17 failures against the old
binary). The end-to-end harness measured the gate at 66 to 71 ms.

**`cargo clippy -- -D warnings` does not pass, and did not pass on `master`.** Observed: 133
distinct diagnostics on the branch with my work stashed, almost all in `lattice-core`
(`consolidation/`, `health/`, `storage/`). The three new files are clean and I introduced no
new diagnostic; the one I did introduce (an unused import) is fixed. Clearing the rest is a
separate change across code this brief does not touch. `cargo fmt --check` also failed on
`master` for two files; those are formatted in their own commit.

Version skew, **verified** with the new adapter against the OLD daemon binary on a private
port: a best-effort workspace captures edits and multi-line summaries at once with nothing
left in the client queue; an enforcing workspace allows the edit and emits the
"daemon refused" notice once.

## Rollout (coordinator)

```bash
# 1. Build. This swaps the hook adapter for every live session immediately, because
#    ~/.local/bin/lattice is a symlink into this directory. Verified safe against the old daemon.
cd /Users/pete/Cadres/lattice && git checkout feature/hook-enforcement
cargo build --manifest-path daemon/Cargo.toml --release

# 1b. Make the shard cap durable BEFORE the restart, so the new daemon reads it at start.
#     An invalid file stops the daemon from starting, so check it first.
mkdir -p ~/.config/lattice
printf 'max_loaded_shards = 6\n' > ~/.config/lattice/daemon.toml
lattice doctor --workspace /Users/pete/Cadres/lattice | grep 'daemon settings'
#     expect: PASS daemon settings: max_loaded_shards=6 from settings file ...
#     (the old daemon is still running, so the "running daemon" line will WARN; that is right)

# 2. Restart the daemon so it records plans. Until then enforcing workspaces fail open with a
#    "daemon refused" notice. This ends every live MCP session; clients reconnect on next use.
pkill -f 'lattice --daemon' ; sleep 2        # AGENTS.md says a proxy starts the daemon on
                                             # demand. Documented, NOT observed by me. If
                                             # status fails: lattice --daemon &
lattice status --workspace /Users/pete/Cadres/lattice --timeout 5 | grep shard_capacity
#     expect: max_loaded_shards=6 from settings file /Users/pete/.config/lattice/daemon.toml; ...

# 3. Acceptance, touches nothing live:
bash integrations/claude-code/tests/enforcement_e2e.sh

# 4. Opt each product repository in (both clients), then restart its agent sessions.
for repo in relay synapse beacon keystone portal; do
  lattice install --workspace "/Users/pete/Cadres/$repo" --enforce --verify
done
```

Then in each product repository replace the "hook packages must stay best-effort ... produce no
output" sentence with the wording under "Wording for a product repository's instructions" in
`docs/hook-enforcement.md`. Lattice's own `CLAUDE.md` and `AGENTS.md` carry the same sentence;
they are git-ignored and untracked here, so I left them unchanged rather than make an edit
nobody can review. Apply the same wording there by hand.

Relay gets indexed by step 2: the cap becomes 6, from the settings file and also as the new built-in default, for five product repositories plus Lattice.

## Rollback

```bash
# Per repository, immediate and complete:
lattice install --workspace /Users/pete/Cadres/<repo> --no-enforce --verify
# Emergency, no installer needed (the adapter reads the policy on every call):
rm /Users/pete/Cadres/<repo>/.lattice/workspace-policy.json
# Whole change:
cd /Users/pete/Cadres/lattice && git checkout master
cargo build --manifest-path daemon/Cargo.toml --release && pkill -f 'lattice --daemon'
```

Rolling the binary back while a repository is still enforcing is safe. **verified**: the old
binary run as `__hook-adapter claude-code pre-tool-use` exits 0, prints nothing and creates no
state, so the leftover `PreToolUse` entry is inert. Run
`--no-enforce` afterwards to remove it. `workflow.db` and the notice and snapshot directories
under `~/.local/state/lattice` can be deleted at any time.

## Open risks

- **The gate is open while a workspace loads.** For the seconds a shard spends indexing after
  a daemon restart or first contact, product edits are allowed with one notice. That is the
  fail-open rule working, and it is how I found a race in my own harness: it asserted a denial
  while the index was still loading and failed 4 runs in 5 until it waited for `ready`.
- **Checkout-scoped plans.** With many agents in one checkout, one agent's plan satisfies every
  session already running there. Nothing a host supplies can fix this. Worktrees can.
- **hypothesis** Claude Code subagent hooks carry the parent's `session_id`. The docs do not
  say. If they carry their own, each subagent needs its own plan, which is stricter, not unsafe.
- **Shell attribution under concurrency.** A change made by another agent during a shell call is
  listed for that call. The wording says "changed", not "you changed".
- **NotebookEdit path field** is undocumented. `notebook_path` is accepted; if the real name
  differs the gate allows that tool. `MultiEdit` is no longer documented at all.
- **Stop reminder costs one continuation** in Claude Code, by the host's design.
- **Many summaries are still dropped.** Any remaining `;`, `|`, `<`, `>` outside code, or an
  absolute path such as `/Users/...`, drops the summary. That is the existing privacy rule and
  I did not weaken it. Whether to redact instead of drop is a product decision for Pete.
- **Deferred workspaces.** The three-shard cap still leaves a workspace with its own live proxy
  waiting indefinitely. Now reported honestly, not solved.
- **Codex** is verified from `openai/codex` source, not from a Codex run. No Codex session was
  exercised.
- Not acted on: 4.1 MB unchecked `sessions.db-wal`; about 100 bindings left `open` past their
  deadline because expiry is lazy.

