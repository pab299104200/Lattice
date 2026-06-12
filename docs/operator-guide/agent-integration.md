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

Use `lattice doctor` after editing client configuration. Doctor checks daemon reachability, workspace shard health, watcher state, the MCP self-handshake, duplicate registrations, and binary skew.

## Claude Code Hooks

Install the project-local hook package:

```bash
integrations/claude-code/install.sh
```

This repository uses `integrations/claude-code/install.sh` rather than a `lattice install claude-code` subcommand. The installer idempotently merges hook entries into `.claude/settings.json`.

The installed hooks are:

- `SessionStart`: injects task memory and repo rules from `lattice recall --mode task --json`.
- `UserPromptSubmit`: injects `lattice context "<prompt>" --mode auto` only when the relevance threshold is met.
- `PostToolUse` on `Edit|Write`: injects a short `lattice impact <edited-file> --no-tests` summary for non-leaf edits.
- `Stop`: records edited files with `lattice remember --kind outcome`.

All hooks exit `0` without output if the binary is missing or the daemon is down. Hook failures must never block a coding session.

## Codex

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
lattice impact daemon/crates/lattice-daemon/src/rpc/mcp.rs --no-tests
lattice status --scope index
```

The CLI is useful when MCP is not configured or when a prompt needs fast, grep-shaped access to the same 8 public verbs.
