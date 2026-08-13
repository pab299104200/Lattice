# Agent Integration

Lattice exposes one agent-facing contract: a project-scoped `lattice` MCP server, the 8 public verbs, and optional CLI/hook integrations that call the same verbs.

## MCP Registration

Add exactly one registration named `lattice` in each client scope:

```json
{
  "mcpServers": {
    "lattice": {
      "type": "stdio",
      "command": "/absolute/path/to/lattice",
      "args": ["--stdio", "--workspace", "/path/to/workspace"]
    }
  }
}
```

The workspace must be the project directory. Never configure the workspace as `$HOME` or `/`; those roots are rejected because they watch too much of the filesystem and hide project-level configuration errors.

Use the built-in installer to reconcile client configuration, then run `lattice doctor` to verify it. Doctor checks daemon reachability, workspace shard health, watcher state, the MCP self-handshake, duplicate registrations, hook paths and timeouts, bounded configured-hook fixtures, and binary skew.

```bash
lattice install mcp --workspace /path/to/workspace --verify
lattice install claude-code --workspace /path/to/workspace --verify
lattice install codex --workspace /path/to/workspace --verify
lattice doctor --workspace /path/to/workspace
```

`install mcp` writes `.mcp.json`; hook targets write `.claude/settings.json` and `.codex/hooks.json`. Installation is idempotent, preserves unrelated configuration, removes duplicate Lattice entries, and rejects invalid targets or more than one workspace for a hook package. Use `LATTICE_ASSET_ROOT` when the executable cannot discover the directory containing `integrations/`. `--verify` re-reads the file, checks canonical serialization and required assets, then exercises the configured MCP process (`initialize` and `tools/list`) or all four configured hooks with bounded fixture input. Hook verification uses a disposable protected state root, an unavailable loopback endpoint, and checks that no fixture prompt, tool payload, or transcript sentinel was retained; it cannot create live capture state. It fails loudly on protocol errors, tool-surface drift, missing assets, broken hooks, or invalid output.

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
- `Stop`: makes a best-effort protected close-marker attempt for the current
  host session. It does not write an ordinary outcome memory or read a
  transcript; unavailable capture is silent and never blocks shutdown.

All hooks exit `0` without output if the binary is missing or the daemon is down. The sole recovery exception is an already authenticated `SessionStart` whose adapter later cannot reach or validate the daemon: it emits `lattice: daemon unreachable — run 'lattice doctor'` once for that host session, in Claude Code's hook output envelope. Hook failures must never block a coding session.

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
- `Stop`: makes a best-effort protected close-marker attempt for the current
  host session. It does not write an ordinary outcome memory or read a
  transcript; unavailable capture is silent and never blocks shutdown.

All hooks exit `0` without output if the binary is missing, the daemon is down, or the bounded adapter call cannot complete. The adapter has a two-second invocation deadline and performs no separate health check. The sole recovery exception is an already authenticated `SessionStart` whose adapter later cannot reach or validate the daemon: it emits `lattice: daemon unreachable — run 'lattice doctor'` once for that host session. Hook failures must never block a coding session. A no-injection result is therefore not proof that hook wiring is missing; after the session is responsive, run `lattice status --timeout 2` to distinguish an unavailable or overloaded daemon from a configuration issue. Installed outer timeouts are five seconds for all four hooks, leaving process and serialization overhead around the bounded adapter call.

### D3a session boundary

The installed adapters accept only bounded structured host fields. `SessionStart`
uses the opaque host session identifier; an edit event uses the allowlisted
tool kind and dedicated file path; `Stop` carries only a close marker because
the supported hosts do not currently provide an admitted final-summary field.
The adapter ignores `cwd`, repository/checkout/scope claims, prompts,
transcripts, tool input/output, commands, terminal output, and environment
values. It never opens a host `transcript_path`.

D3a state is repository-local and bound to the exact canonical checkout by a
daemon-minted capability. The short-lived adapter stores only protected
capability metadata and sanitized pending deliveries under the user's private
state directory; delivery IDs make retries idempotent, and bounded pending
state is pruned after acknowledgement, expiry, or the retry grace period. A
verified Stop close is reduced into deterministic repository-local session
memories and is available to task-scoped recall; a failed or unavailable Stop
is not reported as a successful capture. No LLM call is made by default; the
separate consolidation workflow is explicit, repository-scoped, and
proposal-only.

## Codex MCP And CLI

Codex can use the same MCP registration shape in `config.toml`:

```toml
[mcp_servers.lattice]
command = "/absolute/path/to/lattice"
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

Use `lattice metrics --memory` to inspect memory adoption separately. Markdown reports one row per day, client, and channel with `retrievals`, `memories_returned`, `memories_used`, `use_rate`, `injections`, `memories_shown`, `injection_actions`, and `action_rate`. The rates are percentages: used divided by returned, and actions divided by shown. `--json` returns `{ "days": [...] }`; each row contains `day`, `client`, `channel`, `retrievals`, `memories_returned`, `memories_used`, `injections`, `memories_shown`, and `injection_actions` (the derived rate columns are Markdown-only). The command applies the normal workspace and `--days` filters and prints `_no memory metrics recorded_` when there are no matching events.

Doctor is the runtime health check after installation. It checks daemon reachability, each requested workspace's index status, the MCP self-handshake and tool count, discovered registrations and conflicts, hook path/timeout invariants, and executes each valid configured hook with a bounded structured fixture. Fixture runs use disposable protected state and an unavailable loopback endpoint, so they cannot create live capture state. Doctor also checks orphaned stdio proxies and binary skew. A `FAIL` contributes to the final failure count and makes the command unsuccessful; a `WARN` is visible but does not fail the command. The final line is a JSON summary with `failures` and `warnings`. Start the daemon with `lattice --daemon` when the reachability check fails, then rerun doctor.
