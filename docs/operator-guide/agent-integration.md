# Agent Integration

Lattice exposes one agent-facing contract: a project-scoped `lattice` MCP server, the 8 public verbs, and optional CLI/hook integrations that call the same verbs.

## MCP Registration

Add exactly one registration named `lattice` in each client scope:

```json
{
  "mcpServers": {
    "lattice": {
      "type": "stdio",
      "command": "/home/pete/cadres/lattice/daemon/target/release/lattice",
      "args": ["--stdio", "--workspace", "/path/to/workspace"]
    }
  }
}
```

The workspace must be the project directory. Never configure the workspace as `$HOME` or `/`; those roots are rejected because they watch too much of the filesystem and hide project-level configuration errors.

Use the built-in installer to reconcile client configuration, then run `lattice doctor` to verify it. Doctor checks daemon reachability, workspace shard health, watcher state, the MCP self-handshake, duplicate registrations, hook timeouts, and binary skew.

```bash
lattice install mcp --workspace /path/to/workspace --verify
lattice install claude-code --workspace /path/to/workspace --verify
lattice install codex --workspace /path/to/workspace --verify
lattice doctor --workspace /path/to/workspace
```

`install mcp` writes `.mcp.json`; hook targets write `.claude/settings.json` and `.codex/hooks.json`. Installation is idempotent, preserves unrelated configuration, and rejects invalid targets or more than one workspace for a hook package. Use `LATTICE_ASSET_ROOT` when the executable cannot discover the directory containing `integrations/`. `--verify` performs a canonical read-back check after writing the configuration.

## Claude Code Hooks

Install the project-local hook package:

```bash
integrations/claude-code/install.sh
```

The supported primary path is `lattice install claude-code --workspace <path> --verify`, which locates bundled hook assets and idempotently merges entries into `.claude/settings.json`. The repository package `integrations/claude-code/install.sh [settings-path]` remains available for package-level installation and tests.

The installed hooks are:

- `SessionStart`: injects task memory and repo rules from `lattice recall --mode task --json`.
- `UserPromptSubmit`: injects `lattice context "<prompt>" --mode auto` only when the relevance threshold is met.
- `PostToolUse` on `Edit|Write`: injects a short `lattice impact <edited-file> --no-tests` summary for non-leaf edits.
- `Stop`: records edited files with `lattice remember --kind outcome`.

All hooks exit `0` without output if the binary is missing or the daemon is down. Hook failures must never block a coding session.

## Codex Hooks

Install the project-local hook package:

```bash
integrations/codex/install.sh
```

The supported primary path is `lattice install codex --workspace <path> --verify`, which idempotently reconciles Lattice hook entries in `.codex/hooks.json`. For package-level installation, `integrations/codex/install.sh [hooks-path]` also installs a symlink at `~/.local/bin/lattice`; it refuses to overwrite a non-symlink CLI target. Set `LATTICE_INSTALL_BIN_DIR` to choose another bin directory or `LATTICE_SKIP_CLI_INSTALL=1` for hooks only.

Codex requires project `.codex/` layers to be trusted before project-local hooks run. Use `/hooks` in Codex to review and trust new or changed hook definitions.

The installed hooks are:

- `SessionStart`: runs task-memory recall and repo-rule context concurrently, then prints their bounded results.
- `UserPromptSubmit`: prints `lattice context "<prompt>" --mode auto` only when the relevance threshold is met and hook stdout is supported.
- `PostToolUse` on `apply_patch|Edit|Write`: prints a short `lattice impact <edited-file> --no-tests` summary for non-leaf edits.
- `Stop`: records edited files with `lattice remember --kind outcome`.

All hooks exit `0` without output if the binary is missing, the daemon is down, or its bounded readiness probe does not answer in time. Hook failures must never block a coding session. A no-injection result is therefore not proof that hook wiring is missing; after the session is responsive, run `lattice status --timeout 2` to distinguish an unavailable or overloaded daemon from a configuration issue. Installed outer timeouts are five seconds for session/prompt context and four seconds for post-edit/stop work; internal calls have smaller budgets so shell and serialization overhead cannot consume the entire outer deadline.

## Codex MCP And CLI

Codex can use the same MCP registration shape in `config.toml`:

```toml
[mcp_servers.lattice]
command = "/home/pete/cadres/lattice/daemon/target/release/lattice"
args = ["--stdio", "--workspace", "/path/to/workspace"]
cwd = "/path/to/workspace"
```

Codex shell commands can also call the CLI directly:

```bash
lattice context "how does retrieval ranking work?" --mode docs
lattice prepare_change "add adoption metrics to CLI"
lattice impact daemon/crates/lattice-daemon/src/rpc/mcp.rs --no-tests
lattice status --scope index
```

Runtime modes are explicit (`--stdio` for the MCP proxy and `--daemon` for the long-lived server); a bare invocation does not silently select a runtime. The CLI is useful when MCP is not configured or when a prompt needs fast, grep-shaped access to the same 8 public verbs.

## Adoption Metrics

Run:

```bash
lattice metrics
```

The command renders the last 14 days of workspace-local adoption counters grouped by `client`, `channel`, and `tool`. Hook invocations set `client=claude-code` or `client=codex` with `channel=hook`; direct shell calls set `client=lattice-cli` and `channel=cli`; MCP calls use the initialized client name when available and otherwise fall back to `mcp`.
