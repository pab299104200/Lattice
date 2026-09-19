# Brief: enforced Lattice workflow in Claude Code (and Codex) hooks

Authorized by Pete, 2026-09-19: "make your updates", then roll the same change out to
Relay, Synapse, Beacon, Keystone and Portal.

## Problem, as observed in a two-day Relay session (evidence)
- The session ran in bypass-permissions mode. Hooks DID fire (`relay/.lattice/hook-capture.db`
  holds 32 `turn_summary` rows), so permission mode is not the cause.
- The agent never called `context`, `prepare_change`, `impact`, `remember` or the stale-docs
  check, although every repo's instructions require them. Nothing made it.
- `install.rs` `HOOKS` registers no `PreToolUse` entry, and `PostToolUse` matches only
  `apply_patch|Edit|Write`. The agent made almost every file change through the **Bash** tool
  (heredocs, `python3 - <<E`, `sed -i`), so the edit-impact hook never ran.
- Every hook script ends `exit 0` and the adapter swallows errors, by design ("hooks never
  break a session"). `lattice status` reported `indexing: true`, `graph_snapshot_state:
  not_loaded` for hours, and no notice ever reached the agent.
- The capture journal stops at 2026-09-18 07:31 EDT although the session continued for 15+
  hours. Cause not established.

## Outcome wanted
When a workspace opts in to **enforcement**, an agent cannot edit product code without having
asked Lattice for a change plan in that session, shell-made edits get the same impact
feedback as tool-made edits, and a daemon that is down, still indexing or not capturing says
so visibly once, instead of failing silent. Non-enforcing workspaces keep today's behaviour.

## Requirements
1. **Opt-in, per workspace.** `lattice install claude-code --workspace <dir> --enforce` (and a
   matching `--no-enforce`) records the mode in the workspace's Lattice configuration, not in
   global config. `--verify` exercises the new hooks. The installer stays the only writer of
   Lattice's entries in `.claude/settings.json`; unrelated hooks are preserved.
2. **PreToolUse gate** for `apply_patch|Edit|Write|MultiEdit|NotebookEdit`: if the target path
   is product code (not docs-only, not under `.lattice/`, `.claude/`, scratch or temp
   directories, and inside the workspace) and the current session binding has no recorded
   `prepare_change` (MCP or CLI, this workspace, within a sensible window such as the current
   task/turn sequence), block with Claude Code's blocking contract (exit 2 / the documented
   JSON decision) and a two-line actionable reason: what to run, and that it is enforced by
   workspace policy. Decide and document the exact freshness rule; it must not nag on every
   edit of a multi-file change that one plan covered. Find how the daemon can know a
   `prepare_change` happened in this session (adoption metrics, session binding, a new
   authenticated fact) and build the missing piece at the right layer.
3. **Fail open on infrastructure, loudly.** Daemon down, indexing, snapshot not loaded,
   adapter timeout, no binding: never block. Emit one bounded, de-duplicated notice per
   session per condition (reuse the session-start notice claim mechanism) telling the agent
   that Lattice is unavailable, why, and that it must say so in its report, as the repo
   instructions already require. Silence is no longer acceptable in enforcing workspaces.
4. **Shell edits.** Add `Bash` to the PostToolUse matcher in enforcing workspaces. Do NOT read,
   forward or store the command text, output, environment or cwd beyond what is already
   allowed (there is an existing test that a `command` field must never leak). Detect changed
   files from repository state instead (for example a bounded `git status --porcelain`
   comparison against a per-session snapshot kept under the protected state root), cap the
   number of paths, run the same impact presentation, and when product files changed via the
   shell with no `prepare_change` on record, say so once as additional context (it cannot be
   blocked after the fact). Keep the two-second invocation deadline; degrade rather than
   exceed it on large repositories.
5. **Stop-hook reminder** in enforcing workspaces: when the session edited product files and
   the stale-docs check or `remember` was never invoked, add one bounded reminder. Never
   block Stop (no loops).
6. **Codex parity** in `integrations/codex` for every event Codex supports; state plainly in
   the README which guarantees do not exist there.
7. **Diagnose, bounded:** why `hook-capture.db` for `/Users/pete/Cadres/relay` stopped
   receiving turn summaries after 2026-09-18 07:31 EDT, and why `lattice status` still shows
   `indexing: true` / `graph_snapshot_state: not_loaded`. Logs are under `~/.lattice/logs`.
   Fix the cause if it is a Lattice defect; otherwise write what it is.
8. **Tests:** unit tests for each decision (blocked, allowed after plan, docs-only allowed,
   outside workspace allowed, daemon down allows with one notice, notice de-duplication, Bash
   path detection without command capture, caps and deadline), installer reconcile tests
   (enforce on, off, idempotent rerun, unrelated hooks preserved, stale entries replaced),
   and the existing privacy tests still pass. `cargo test`, `cargo clippy -- -D warnings`,
   `cargo fmt --check`.
9. **Docs:** `integrations/claude-code/README.md`, `integrations/codex/README.md`, the
   product docs that describe hooks, and a short "Enforcement mode" section that a product
   repo's CLAUDE.md can cite. Note that product repos currently state "hook packages must stay
   best-effort … exit 0 quickly and produce no output"; give the replacement wording.

## Constraints
- Work on a branch in this repository. There is an unrelated uncommitted change to
  `.claude/settings.json` in the working tree: do not touch or commit it.
- Do not restart the long-lived daemon, reinstall the binary to `~/.local/bin`, or run the
  installer against any product repository. The coordinator does the rollout after review.
  Build to `daemon/target/release` and test there, with a protected state root.
- Privacy posture is unchanged: no transcript, no command text, no file content leaves the
  adapter.
- Commit on the branch with clear messages. Write `docs/plans/2026-09-19-hook-enforcement-handoff.md`
  incrementally: design decisions, files changed, real test output, verified versus
  hypothesis, the exact rollout commands, and rollback.
Reply to the coordinator in under 250 words.
