# Hook enforcement

Lattice hooks are best-effort by default: they add context, and when anything fails they say
nothing. A workspace can opt in to **enforcement**. In an enforcing workspace an agent cannot
edit product code without having asked Lattice for a change plan, edits made through the shell
get the same feedback as edits made through an edit tool, and a Lattice that is down, not
loaded or not capturing says so once instead of failing silent.

Enforcement never trades availability for control. Every infrastructure failure allows the
tool call. The only thing that blocks an edit is a healthy Lattice that has no current plan on
record.

## Enforcement mode

Turn it on or off per workspace. The installer is the only writer of Lattice's hook entries
and of the policy file.

```bash
lattice install claude-code --workspace /path/to/repo --enforce --verify
lattice install codex       --workspace /path/to/repo --enforce --verify
lattice install             --workspace /path/to/repo --enforce --verify   # both clients

lattice install claude-code --workspace /path/to/repo --no-enforce --verify
```

- The mode is recorded in `<workspace>/.lattice/workspace-policy.json`. It is never read from
  global configuration, so one repository opting in cannot change another.
- A rerun without either flag keeps the recorded mode. The default, with no policy file, is
  best-effort.
- Both clients follow the one workspace policy. Run the installer for each client you use, or
  use the default target, so their hook files match the policy.
- Restart or reconnect the agent client afterwards. Claude Code and Codex read hook
  configuration at session start.
- `.lattice/` is normally git-ignored, so the mode is per checkout. Each clone and each
  worktree opts in separately.

```json
{
  "schema_version": 1,
  "hook_enforcement": { "enabled": true }
}
```

An unreadable policy file is an error for the installer, which stops before writing any hook.
At run time it disables enforcement for that session and produces one notice.

### What the installer changes

| Hook | Best-effort | Enforcing |
|---|---|---|
| `PreToolUse` `pre-tool-use.sh` | not installed, and removed if present | matcher `apply_patch\|Edit\|Write\|MultiEdit\|NotebookEdit` |
| `PostToolUse` `post-tool-use.sh` | matcher `apply_patch\|Edit\|Write` | adds `MultiEdit\|NotebookEdit\|Bash\|PowerShell` |
| `SessionStart`, `UserPromptSubmit`, `Stop`, `SessionEnd` | unchanged | same scripts; behaviour follows the policy file |

Unrelated hooks in the same file, including other `PreToolUse` hooks, are preserved. Stale
paths and duplicate Lattice entries are replaced in place.

## The plan gate

Before an edit tool runs, the gate asks one question: is the target product code, and if so,
is there a current plan?

**Product code** is any path inside the workspace except:

- documentation: anything under the top-level `docs/` directory, and any `.md`, `.mdx`,
  `.markdown`, `.rst` or `.adoc` file;
- tool state: `.lattice/`, `.claude/`, `.codex/`, `.git/`;
- scratch: top-level `tmp/`, `temp/`, `.tmp/`, `scratch/`, `.scratch/`, and the host's own
  scratchpad directory when it names one;
- anything outside the workspace. Symlinks are resolved first, so a symlink cannot disguise a
  product path as documentation or an outside path as inside.

When the gate denies, the agent sees two lines: what to run, and that workspace policy
requires it. Running `lattice prepare_change "<task>"`, or the MCP tool `prepare_change`, and
retrying the edit is all it takes.

### How Lattice knows a plan was made

Neither Claude Code nor Codex passes its session identifier to an MCP server or to a shell
command, so a `prepare_change` call cannot be tied to one host session by anything the host
supplies. Lattice therefore records the fact itself. When the daemon serves a successful
`prepare_change` for a workspace, through MCP or the CLI, it records the checkout, the step
and the time in its protected state (`hook-sessions/workflow.db`). It records no query,
argument, result or path. The write happens before the response is sent, so an edit retried
straight after a plan sees it.

Two consequences follow, and both are deliberate:

- The fact is scoped to the **checkout**, not to one session. Two sessions working in the same
  checkout share plans. Use separate worktrees when sessions must plan independently.
- A workspace that is still loading gives a partial `prepare_change` answer. That still counts:
  the agent did what was asked of it.

### Freshness rule

A plan is current when all of these hold:

1. It was recorded after this session's model context last started fresh. A `SessionStart`
   with source `startup`, `clear` or `compact` moves that floor to now, because the model can
   no longer see the earlier plan. `resume` keeps it.
2. It is at most **8 hours** old.
3. At most **45 minutes** have passed since the later of the plan and the last product edit
   made under it.

Each allowed product edit slides the 45-minute window, so one plan covers a whole multi-file
change however long it runs, up to the 8-hour limit. The gate does not ask again on every
edit.

## Fail-open rule

The gate never blocks because Lattice is unhealthy. Each condition below allows the tool call
and produces one notice per session per condition, telling the agent what is wrong, what to
run, and that it must say so in its report.

| Condition | What the agent is told to do |
|---|---|
| Daemon unreachable | `lattice doctor` |
| Daemon refused the request, including a daemon older than enforcement | restart the daemon, then `lattice doctor` |
| Adapter exceeded its two-second deadline | `lattice status --timeout 2` |
| Adapter failed locally, such as unreadable private state | `lattice doctor` |
| Index still indexing | `lattice status` |
| Index deferred because every shard slot is busy | close another workspace's session or raise `LATTICE_MAX_LOADED_SHARDS` |
| Index failed to load | `lattice doctor` |
| Policy file unreadable | rerun the installer with `--enforce` |
| Repository changes could not be listed in time | run `lattice impact <file>` by hand |
| Turn summary not captured (shown to the operator at `Stop`) | `lattice doctor` |

A workspace that simply has no shard loaded yet is not a failure. The gate asks for a plan,
and asking for one is what loads the workspace.

Notices are claimed with a marker file under `$XDG_STATE_HOME/lattice/hook-notices` (default
`~/.local/state/lattice/hook-notices`). The file name is a digest; it holds no session,
repository or path text. Markers expire after 24 hours.

## Shell edits

A shell command can rewrite any file, and Lattice does not read the command, its output, its
environment or its working directory. It finds shell-made edits from repository state.

After each shell tool call the adapter runs one bounded
`git status --porcelain=v2 --branch -z --no-renames --untracked-files=all` with
`GIT_OPTIONAL_LOCKS=0`, so it never contends for `index.lock`. It compares the result with a
per-session snapshot under `hook-shell-snapshots` in the protected state root. A path counts as
changed when it is newly dirty, or its size or modification time moved. Git-ignored files are
never listed.

- The first shell call in a session records a baseline and reports nothing, so work that was
  already dirty is never blamed on the shell.
- When `HEAD` moves (commit, checkout, pull, reset) the baseline resets and nothing is
  reported for that call.
- Edits made through an edit tool are folded into the snapshot, so they are not reported again
  by the next shell call.
- At most 20 changed product paths are recorded per call. The note names the first five and
  counts the rest. The impact note covers the first path.
- Bounds: 700 ms for `git status` inside the two-second adapter deadline, 1 MiB of output,
  4,096 dirty entries. Past any bound the call degrades to one notice instead of a partial
  answer that looks complete.

A shell edit cannot be blocked after the fact. When product files changed through the shell
with no current plan, the agent is told once per session.

**Limit:** when several agents write to one checkout at the same time, a change made by one
can be attributed to another's shell call. Treat the list as what changed while the command
ran, not proof of who changed it.

## Stop reminder

When a session edited product files and never ran the stale-docs check
(`lattice status --scope docs --files <changed files>`) or `lattice remember`, it gets one reminder at `Stop` naming
the missing step.

In Claude Code a `Stop` hook's `additionalContext` is not passive. The host continues the
conversation once so the agent can act on it. Lattice claims the reminder once per session in
the daemon and never sends it while `stop_hook_active` is true, so it costs at most one
continuation and cannot loop. It never uses `decision: "block"`.

## Privacy

Enforcement adds no new captured content. The adapter still retains only the host session
identifier and an edit tool's dedicated path field. For a shell tool it reads nothing from
`tool_input` or `tool_response`. Paths of shell-changed files come from `git status`, and are
stored in the same capture journal as tool-edited paths. The enforcement request carries an
event name, a session source word, and a count.

## Guarantees that differ by client

| | Claude Code | Codex |
|---|---|---|
| Gate blocks a product edit without a plan | yes, per path | yes, but for **every** patch |
| Documentation and scratch exemptions | yes | **no** |
| Gate applies in bypass-permissions mode | yes (documented by Anthropic) | not applicable |
| Shell-edit feedback | yes | yes |
| Stop reminder reaches the model | yes, one continuation | **no**, shown to the operator |

Codex's `apply_patch` hook input is the raw patch text and nothing else. Telling a
documentation patch from a code patch would mean reading it, which Lattice does not do. The
same fact means Codex edits are found from repository state, like shell edits.

## Wording for a product repository's instructions

Product repositories currently say that hook packages "must stay best-effort: if the daemon
is down, they exit `0` quickly and produce no output". In a repository that enables
enforcement, replace that sentence with:

> This workspace enforces the Lattice workflow
> (`lattice/docs/hook-enforcement.md`, "Enforcement mode"). Hooks still never break a session:
> every Lattice failure allows the tool call. They are no longer silent. An edit to product
> code is denied until `prepare_change` has been run for the task; one plan covers the whole
> change. When a hook reports that Lattice is unavailable, continue with direct inspection and
> state that fact, with the reason given, in your report. Hook scripts always exit `0`; a
> denial is returned as the host's documented `PreToolUse` decision, not as a failure.

Repositories that have not enabled enforcement keep the existing sentence.

## Rolling back

```bash
lattice install --workspace /path/to/repo --no-enforce --verify
```

That removes the gate, restores the narrow `PostToolUse` matcher and records the mode. Then
restart the agent client. Deleting `.lattice/workspace-policy.json` has the same run-time
effect immediately, because the adapter reads the policy on every call; rerun the installer
afterwards so the hook file matches.
