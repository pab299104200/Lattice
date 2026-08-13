# Lattice Codex Integration

This package installs Codex hooks that deliver Lattice context and session capture without relying on the model to choose an MCP tool.

Build the release binary, then install the project-local Codex hooks from the
workspace root:

```bash
cd daemon && cargo build --release
./target/release/lattice install codex --workspace "$PWD/.."
```

The Rust installer reconciles Lattice entries in the workspace's
`.codex/hooks.json` without editing global Codex configuration. It replaces
stale Lattice command paths and duplicate entries in place while preserving
unrelated hooks. Use `--verify` to fail if the resulting configuration or
required hook assets cannot be validated:

```bash
./target/release/lattice install codex --workspace "$PWD/.." --verify
```

Codex requires project `.codex/` layers to be trusted before project-local hooks run. Use `/hooks` in Codex to review and trust changed hook definitions.

## Hooks

- `SessionStart`: calls `lattice recall "session start" --mode task --json` and `lattice context "repo rules and operator workflow" --mode rules`, then prints compact markdown for Codex to consume when hook stdout is supported.
- `UserPromptSubmit`: extracts the prompt from the hook payload, calls `lattice context "<prompt>" --mode auto --min-relevance 0.25`, and emits nothing when the result is too small or not relevant.
- `PostToolUse`: for `apply_patch|Edit|Write`, extracts the edited file, calls `lattice impact <edited-file> --no-tests`, emits at most 10 lines, and skips leaf edits by default unless at least three impact/dependent lines are present.
- `Stop`: sends an authenticated, content-free close marker for the host session. The daemon reduces a verified close into repository-local session memory; unavailable capture is not reported as successful.

Every script locates the configured or installed Lattice binary and invokes the bounded adapter. If the binary is missing, the daemon is unavailable, or the adapter cannot finish within its two-second invocation deadline, the script exits `0` without output so hooks never break a Codex session. This is deliberately a no-injection result, not evidence that the hook configuration is absent: diagnose it with `lattice status --timeout 2` after the session is responsive. Session recall and rule lookup run concurrently. The installer records a five-second outer timeout for every hook, leaving process and serialization overhead around the adapter call.

Hook delivery is attributed by the daemon as `codex` / `hook`; installer fixture runs use a protected state root and skip metrics so verification does not pollute adoption reports.

## MCP

Codex can also use Lattice through MCP with a single workspace registration:

```toml
[mcp_servers.lattice]
command = "/absolute/path/to/lattice"
args = ["--stdio", "--workspace", "/path/to/workspace"]
cwd = "/path/to/workspace"
```

Use one `lattice` registration per scope. Do not point a workspace at `$HOME`; use the project directory.

The CLI twins from Phase 3 also work directly from Codex shell commands without MCP configuration:

```bash
/absolute/path/to/lattice context "where is memory verification handled?"
/absolute/path/to/lattice prepare_change "add adoption metrics to CLI"
/absolute/path/to/lattice impact daemon/crates/lattice-daemon/src/rpc/mcp.rs --no-tests
```

## Controls

- Set `LATTICE_BIN=/absolute/path/to/lattice` to force a binary path.
- Set `LATTICE_HOOK_MIN_RELEVANCE` to tune prompt context filtering.
- Set `LATTICE_HOOK_MIN_DEPENDENTS` to tune PostToolUse noise filtering.
- Disable a hook by removing its entry from `.codex/hooks.json`.

Run the Rust installer tests with `cd daemon && cargo test -p lattice-daemon cli::tests`,
then run `integrations/codex/tests/hooks_test.sh` to verify hook output, command
budgets, and the daemon-unavailable no-op contract.
