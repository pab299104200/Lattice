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
- `PreToolUse`: installed only in an enforcing workspace. See [Enforcement mode](#enforcement-mode).
- `PostToolUse`: for `apply_patch|Edit|Write`, records the edited path when the host names one and returns a bounded note as `hookSpecificOutput.additionalContext`. Codex ignores plain-text stdout on tool events, so plain text written here before 2026-09 never reached the model. Codex's `apply_patch` hook input is the raw patch text with no path field, and Lattice does not read it; a best-effort workspace therefore gets no per-edit note from Codex. An enforcing workspace finds Codex edits from repository state.
- `Stop`: sends only the bounded top-level `last_assistant_message` as a nonterminal turn-summary fact. It never opens a transcript. It resumes the session binding, or opens one when none exists. Layout is collapsed, Markdown code spans and fences are replaced with `[code]`, and a long message keeps its first 2,000 bytes. A message that still contains a secret, an external path or a shell construct is dropped whole.
- `SessionEnd`: sends an authenticated, content-free close marker for the host session. This is the sole terminal event. The daemon reduces a verified close into repository-local session memory; unavailable capture is not reported as successful.

Every script locates the configured or installed Lattice binary and invokes the bounded adapter, and always exits `0`, so hooks never break a Codex session. In a best-effort workspace a missing binary, an unavailable daemon or an adapter that cannot finish within its two-second invocation deadline produces no output. This is deliberately a no-injection result, not evidence that the hook configuration is absent: diagnose it with `lattice status --timeout 2` after the session is responsive. In an enforcing workspace the same failures still allow the tool call but are reported once. Session recall and rule lookup run concurrently. The installer records a five-second outer timeout for every hook, leaving process and serialization overhead around the adapter call.

Hook delivery is attributed by the daemon as `codex` / `hook`; installer fixture runs use a protected state root and skip metrics so verification does not pollute adoption reports.

## Enforcement mode

```bash
./target/release/lattice install codex --workspace "$PWD/.." --enforce --verify
./target/release/lattice install codex --workspace "$PWD/.." --no-enforce --verify
```

The mode is recorded in the workspace's `.lattice/workspace-policy.json` and is
shared with Claude Code. Review and trust the changed hooks with `/hooks`, then
restart Codex. The contract is in
[docs/hook-enforcement.md](../../docs/hook-enforcement.md).

Codex supports `PreToolUse`, `PostToolUse` and `Stop`, and blocks a tool call on
the same `hookSpecificOutput.permissionDecision: "deny"` JSON as Claude Code.
Every enforcement event is therefore wired. These guarantees do **not** exist
in Codex:

- **No path exemptions.** `apply_patch` exposes only patch text. Lattice does
  not read it, so it cannot tell a documentation patch from a code patch. In
  Codex every patch needs a current plan, including documentation and scratch
  files. The denial says so.
- **No per-path note from the edit tool.** For the same reason Codex edits are
  found from `git status`, exactly like shell edits. A change to a git-ignored
  file is not seen.
- **No model-facing Stop reminder.** Codex's `Stop` hook can only block and
  continue, which Lattice never does. The reminder is sent as `systemMessage`,
  which the operator sees and the model does not.
- **No bypass-mode statement.** Anthropic documents that a hook `deny` applies
  in bypass-permissions mode. Lattice has no equivalent documented statement
  for Codex approval policies.

What is the same: the plan gate itself, the freshness rule, shell-edit
detection for `Bash`, the one-notice-per-condition fail-open rule, and the
privacy posture.

## Declared verification checks

A trusted Codex plugin may explicitly run a check declared in the checkout's
`.lattice/verification-checks.json` with the private
`lattice __hook-verify codex <host-session-id> <check-id>` entry point. This is
not installed or triggered automatically. The strict v2 manifest requires
`schema_version: 2` and each check's safe ID, fixed display label, argv array,
positive `timeout_ms`, and explicitly declared environment. The executable
must be an absolute path or an explicit `./` checkout-relative path; shell
executables are rejected. Optional `evidence_reference` and categorical `error`
metadata may also be declared. Lattice executes argv directly without a shell,
clears the environment before applying declarations, discards all process
streams, and forwards only the label and categorical outcome under an existing
session binding. It never forwards or stores the ID, argv, command output,
environment, cwd, transcript, or exit code. Malformed configuration, unknown
IDs, missing/closed bindings, and transport failure produce no capture output
and never create a binding. The runner currently fails closed on Windows.

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

## Long-running agent work

Hook injection does not replace direct Lattice calls during a plan. Include the
[agent workflow](../../docs/agent-workflow.md) in the consuming repository's
agent instructions and verify actual task-boundary tool calls in acceptance
tests. A best-effort installation does not enforce this behavior. An enforcing
one enforces exactly one step, a change plan before an edit, and reminds the
operator about two more.

For complete project setup, run `lattice install --workspace /path/to/project`.
It installs both clients' MCP configurations and hooks and maintains workflow
instructions in `AGENTS.md` and `CLAUDE.md`. The explicit client target and this
standalone shell package remain hook-only. Client trust and approval settings
are not changed.
