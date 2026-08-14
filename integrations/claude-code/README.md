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
- `SessionEnd`: sends an authenticated, content-free close marker for the host session. Per-turn `Stop` is deliberately not registered. The daemon reduces a verified close into repository-local session memory; unavailable capture is not reported as successful.

Every script locates the configured or installed Lattice binary and invokes the
bounded adapter. If the binary is missing, the daemon is unavailable, or the
adapter cannot finish within its two-second invocation deadline, the script
exits `0` without output so hooks never break a Claude session. Session recall
and rule lookup run concurrently.

The installer records a five-second outer timeout for every hook. The shipped
adapter uses one bounded two-second invocation deadline for each hook. These
limits leave process and output time inside Claude Code's hook budget.

Hook delivery is attributed by the daemon as `claude-code` / `hook`; installer fixture runs use a protected state root and skip metrics so verification does not pollute adoption reports.

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
