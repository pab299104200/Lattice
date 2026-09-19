# Handoff: enforced Lattice workflow in hooks

Branch: `feature/hook-enforcement` (from `master`). Brief:
`docs/plans/2026-09-19-hook-enforcement-brief.md`. This file is written incrementally.
Every claim is tagged **verified** (evidence named) or **hypothesis**.

## Status

IN PROGRESS. Investigation complete; implementation started. Nothing below the
"Design decisions" heading is built until a "Built" entry says so.

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

## Built so far (all committed on the branch)

- `hook_enforcement.rs` policy file `.lattice/workspace-policy.json`, path classes, freshness
  rule, notice and deny wording. `hook_workflow_state.rs` workflow facts and session state.
  `hook_shell_changes.rs` bounded `git status --porcelain=v2` snapshot comparison.
- `hook_session_route.rs` `enforcement` request/answer on `hook/session_open`;
  `socket_server.rs` step recording, non-loading `index_state_for`, deferred status fix.
- `hook_adapter.rs` `pre-tool-use` kind, gate, shell feedback, notices, JSON rendering for both
  hosts on tool events, checkout-relative edit paths, Stop re-opens its binding.
- `hook_session_client.rs` idle-expired bindings are resumed rather than discarded.

## Still to do when this was written

Sanitizer fix, installer `--enforce`/`--no-enforce` and `--verify`, hook scripts, Codex parity,
docs, adapter unit tests, end-to-end run against a private daemon, clippy and fmt.
