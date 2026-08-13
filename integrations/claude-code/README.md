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
- `Stop`: extracts edited files from the hook payload and calls `lattice remember --kind outcome` so the next session can recall the work.

Every script first checks whether the Lattice binary exists and whether the
daemon answers `lattice status` within `LATTICE_HOOK_PROBE_TIMEOUT` seconds.
The readiness probe uses the same verb-first CLI shape as delivery calls, so
normal CLI workspace detection remains authoritative. If either check fails,
the script exits `0` without output so hooks never break a Claude session.
Session recall and rule lookup run concurrently.

The installer records a five-second outer timeout for every hook. The shipped
session-start and prompt hooks use a 3.5-second inner query timeout; edit and
stop hooks use 2.5 seconds. The probe defaults to 0.5 seconds. These limits
leave process and output time inside Claude Code's hook budget.

Actual hook calls set `LATTICE_CLIENT_NAME=claude-code` and `LATTICE_CLIENT_CHANNEL=hook` before invoking the CLI. Readiness probes set `LATTICE_SKIP_METRICS=1`, so `lattice metrics` measures delivered hook value instead of probe noise.

## Controls

- Set `LATTICE_BIN=/absolute/path/to/lattice` to force a binary path.
- Set `LATTICE_HOOK_PROBE_TIMEOUT` to tune the readiness probe timeout; the default is `0.5` seconds.
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
