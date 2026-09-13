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
file, validates the hook assets, and exercises the context-producing hooks.

The installer records the absolute executable and hook-asset paths resolved
from the running installation. No checkout-specific path is required; use the
same command after moving or reinstalling Lattice.

## Hooks

- `SessionStart`: calls `lattice recall "session start" --mode task --json`, adds current task memory and repo rules as `additionalContext`, and clips output to about 1500 tokens.
- `UserPromptSubmit`: calls `lattice context "<prompt>" --mode auto --min-relevance 0.25`, clips output to about 1200 tokens, and emits nothing when the result is too small or not relevant.
- `PostToolUse`: for `Edit|Write`, calls `lattice impact <edited-file> --no-tests`, emits at most 10 lines, and skips leaf edits by default unless at least three impact/dependent lines are present.
- `Stop`: sends only the bounded top-level `last_assistant_message` as a nonterminal turn-summary fact under the existing authenticated binding. It never opens a transcript or creates a binding.
- `SessionEnd`: sends an authenticated, content-free close marker for the host session. This is the sole terminal event. The daemon reduces a verified close into repository-local session memory; unavailable capture is not reported as successful.

Every script locates the configured or installed Lattice binary and invokes the
bounded adapter. If the binary is missing, the daemon is unavailable, or the
adapter cannot finish within its two-second invocation deadline, the script
exits `0` without output so hooks never break a Claude session. Session recall
and rule lookup run concurrently.

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
tests. Installation alone does not enforce this behavior.

For complete project setup, run `lattice install --workspace /path/to/project`.
It installs both clients' MCP configurations and hooks and maintains workflow
instructions in `AGENTS.md` and `CLAUDE.md`. The explicit client target and this
standalone shell package remain hook-only. Client trust and approval settings
are not changed.
