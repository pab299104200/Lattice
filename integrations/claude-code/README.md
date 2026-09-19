# Lattice Claude Code Integration

This package provides Claude Code hooks that deliver Lattice context without
requiring the model to choose an MCP tool.

Build and install from the repository root:

```bash
cargo build --manifest-path daemon/Cargo.toml --release
./daemon/target/release/lattice install claude-code --workspace "$PWD" --verify
```

The Rust installer reconciles Lattice entries in the project-local
`.claude/settings.json`. It updates stale command paths, matchers, timeouts,
and duplicate Lattice entries in place while preserving unrelated hooks. It
does not edit global Claude configuration. `--verify` re-reads the resulting
file, validates the hook assets, and exercises every installed hook against a
deliberately unreachable daemon.

Add `--enforce` to opt this workspace in to enforcement, and `--no-enforce` to
opt it out again. See [Enforcement mode](#enforcement-mode).

The installer records the absolute executable and hook-asset paths resolved
from the running installation. No checkout-specific path is required; use the
same command after moving or reinstalling Lattice.

## Hooks

- `SessionStart`: calls `lattice recall "session start" --mode task --json`, adds current task memory and repo rules as `additionalContext`, and clips output to about 1500 tokens.
- `UserPromptSubmit`: calls `lattice context "<prompt>" --mode auto --min-relevance 0.25`, clips output to about 1200 tokens, and emits nothing when the result is too small or not relevant.
- `PreToolUse`: installed only in an enforcing workspace. See [Enforcement mode](#enforcement-mode).
- `PostToolUse`: for `apply_patch|Edit|Write`, records the edited path as a capture fact and returns a bounded note of at most about 240 tokens for that file. Claude Code sends an absolute `file_path`; the adapter converts it to a checkout-relative path and ignores a path outside the checkout. Until 2026-09 absolute paths were rejected, so this hook produced nothing in Claude Code.
- `Stop`: sends only the bounded top-level `last_assistant_message` as a nonterminal turn-summary fact. It never opens a transcript. It resumes the session binding, or opens one when none exists, so a long run of turns keeps capturing. Layout is collapsed, Markdown code spans and fences are replaced with `[code]`, and a long message keeps its first 2,000 bytes. A message that still contains a secret, an external path or a shell construct is dropped whole.
- `SessionEnd`: sends an authenticated, content-free close marker for the host session. This is the sole terminal event. The daemon reduces a verified close into repository-local session memory; unavailable capture is not reported as successful.

Every script locates the configured or installed Lattice binary and invokes the
bounded adapter, and always exits `0`, so hooks never break a Claude session.
In a best-effort workspace a missing binary, an unavailable daemon or an
adapter that cannot finish within its two-second invocation deadline produces
no output, apart from one `SessionStart` recovery notice. In an enforcing
workspace the same failures still allow the tool call but are reported once.

## Enforcement mode

```bash
./daemon/target/release/lattice install claude-code --workspace "$PWD" --enforce --verify
./daemon/target/release/lattice install claude-code --workspace "$PWD" --no-enforce --verify
```

The mode is recorded in the workspace's `.lattice/workspace-policy.json`, never
in global configuration. A rerun without either flag keeps it. Restart Claude
Code afterwards.

In an enforcing workspace:

- `PreToolUse` on `apply_patch|Edit|Write|MultiEdit|NotebookEdit` denies an edit
  to product code until `prepare_change` has been served for this checkout,
  through MCP or the CLI. It returns the documented
  `hookSpecificOutput.permissionDecision: "deny"` with a two-line reason.
  Anthropic documents that a hook `deny` applies in `bypassPermissions` mode
  too. Documentation, `.lattice/`, `.claude/`, scratch and out-of-workspace
  paths are exempt. One plan covers a whole multi-file change.
- `PostToolUse` also matches `Bash|PowerShell`. The command, its output, its
  environment and its working directory are not read. Changed files are found
  from a bounded `git status` comparison, get the same note as tool-made edits,
  and are recorded as capture facts. Product files changed through the shell
  with no current plan are reported once.
- `Stop` adds one reminder per session when product files were edited and the
  stale-docs check or `remember` never ran. Claude Code continues the
  conversation once for a `Stop` hook's `additionalContext`; Lattice never
  sends it while `stop_hook_active` is true and never uses `decision: "block"`.
- A daemon that is down, refusing, slow, still indexing, deferred or not
  capturing produces one notice per session per condition. Nothing is blocked.

`--verify` in an enforcing workspace additionally proves, with the daemon
unreachable, that the gate allows a product edit and emits exactly the
unreachable-daemon notice once, stays silent for a documentation path, and that
a shell call records its baseline without retaining the command.

The full contract, the freshness rule and the replacement wording for a product
repository's instructions are in
[docs/hook-enforcement.md](../../docs/hook-enforcement.md).

The installer records a five-second outer timeout for every hook. The shipped
adapter uses one bounded two-second invocation deadline for each hook. These
limits leave process and output time inside Claude Code's hook budget.

Hook delivery is attributed by the daemon as `claude-code` / `hook`; installer fixture runs use a protected state root and skip metrics so verification does not pollute adoption reports.

## Declared verification checks

A trusted Claude Code plugin may explicitly run a check declared in the
checkout's `.lattice/verification-checks.json` with the private
`lattice __hook-verify claude-code <host-session-id> <check-id>` entry point.
This is not installed or triggered automatically. The strict v2 manifest
requires `schema_version: 2` and each check's safe ID, fixed display label,
argv array, positive `timeout_ms`, and explicitly declared environment. The
executable must be an absolute path or an explicit `./` checkout-relative
path; shell executables are rejected. Optional `evidence_reference` and
categorical `error` metadata may also be declared. Lattice executes argv
directly without a shell, clears the environment before applying declarations,
discards all process streams, and forwards only the label and categorical
outcome under an existing session binding. It never forwards or stores the ID,
argv, command output, environment, cwd, transcript, or exit code. Malformed
configuration, unknown IDs, missing/closed bindings, and transport failure
produce no capture output and never create a binding. The runner currently
fails closed on Windows.

## Controls

- Set `LATTICE_BIN=/absolute/path/to/lattice` to force a binary path.
- Set `LATTICE_HOOK_MIN_RELEVANCE` to tune prompt context filtering.
- Set `LATTICE_HOOK_MIN_DEPENDENTS` to tune PostToolUse noise filtering.
- Disable a hook by removing its entry from `.claude/settings.json`.

Claude Code's MCP registration is separate from hooks. For MCP, use an
explicit stdio runtime and scope it to the project workspace:

```json
{
  "mcpServers": {
    "lattice": {
      "command": "/absolute/path/to/lattice",
      "args": ["--stdio", "--workspace", "/path/to/workspace"]
    }
  }
}
```

Do not register a bare `lattice` command or point a workspace at `$HOME`.

## Long-running agent work

Hook injection does not replace direct Lattice calls during a plan. Include the
[agent workflow](../../docs/agent-workflow.md) in the consuming repository's
agent instructions and verify actual task-boundary tool calls in acceptance
tests. A best-effort installation does not enforce this behavior. An enforcing
one enforces exactly one step, a change plan before a product edit, and
reminds about two more.

For complete project setup, run `lattice install --workspace /path/to/project`.
It installs both clients' MCP configurations and hooks and maintains workflow
instructions in `AGENTS.md` and `CLAUDE.md`. The explicit client target and this
standalone shell package remain hook-only. Client trust and approval settings
are not changed.
