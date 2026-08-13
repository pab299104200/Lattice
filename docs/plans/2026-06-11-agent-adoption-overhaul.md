# Agent Adoption Overhaul — Change Spec

**Date:** 2026-06-11
**Status:** Historical implementation specification (approved 2026-06-11)
**Audience:** Implementing agent (Codex). This spec is self-contained; do not assume access to the conversation that produced it.

> This document is a dated change specification, not a current-state contract. Its
> findings and acceptance criteria were assessed on 2026-06-11. The initial phases
> landed in commits `b2ae740`, `58d2b2a`, `99f57ee`, `89e051d`, and `73c64eb`,
> but later recovery work changed several implementation details. Revalidate
> behavior against the current code and the 2026-08-12 recovery plan before
> treating any statement below as an active guarantee.

## Problem statement

Lattice's differentiated value is (a) cross-session memory and (b) dependency/impact analysis — things grep and LSP cannot do. Despite this, coding agents (Claude in particular) routinely skip Lattice and fall back to grep. Investigation on 2026-06-11 found this is caused by four compounding issues, in order of severity:

1. **Silent connection failure.** In a live Claude Code session in this repo, the Lattice MCP server was not connected at all — zero `mcp__lattice__*` tools registered — while CLAUDE.md and the `lattice-workflow` skill mandated tools that did not exist. The agent never reports a missing server; it silently greps. Two conflicting registrations exist under the same server name `lattice`: a user-global one in `~/.claude.json` pointing at the VS Code extension binary with `--workspace /home/pete` (the entire home directory — hits the OS inotify limit: "File watcher failed for /home/pete: OS file watch limit reached"), and the project `.mcp.json` pointing at the release binary with seven workspace roots. There is no diagnostic command to detect any of this.
2. **Tool-surface dilution.** 50 advertised MCP tools (43 distinct + shims/aliases). Agents pick tools by salience; built-in Grep/Read are trained muscle memory, and each near-duplicate Lattice tool (`save_memory` vs `save_quick_memory` vs `propose_memory_evolution`; `get_context_capsule` vs `get_working_set_context` vs `summarize_subsystem`) dilutes the others. Under deferred tool loading (Claude Code), the agent must additionally ToolSearch + load schemas before the first call — two extra steps versus zero for grep.
3. **Reliance on the model choosing.** Adoption currently depends entirely on the agent voluntarily selecting an out-of-distribution MCP tool. Instruction text ("use when best fit", skill escape hatches) reads as permission to skip. The fix is to deliver Lattice context *ambiently* via harness hooks and to offer a grep-shaped CLI, so value arrives without a tool-selection decision.
4. **No adoption measurement.** Session metrics exist but do not segment by client (Claude vs Codex) or channel (MCP vs CLI vs hook), so adoption changes cannot be verified.

This spec replaces the current agent-facing surface. Per the project's no-prerelease-legacy-debt policy (CLAUDE.md → "Execution Philosophy"), superseded tool names, shims, and aliases are **removed**, not aliased.

## Architecture summary (2026-06-11 snapshot)

The following was verified against the repository as it existed on 2026-06-11.
It is retained as historical evidence for the change that followed; it is not a
claim that every path or line reference remains current.

- MCP tools are declared inline (name/description/inputSchema via `json!`) and dispatched in `daemon/crates/lattice-daemon/src/rpc/mcp.rs` (schemas ~lines 541–1488, dispatch ~lines 1501–1564). Memory-v2 tools live in `daemon/crates/lattice-daemon/src/rpc/memory_v2/*.rs`.
- Entry point `daemon/crates/lattice-daemon/src/main.rs`: flags `--daemon`, `--stdio`, `--workspace/-w`, `--focus-file`, `--focus-dir`, subcommand `memory-migrate`.
- A persistent global daemon (`--daemon`) serves TCP JSON-RPC (newline-delimited) on `127.0.0.1:47659` (env `LATTICE_DAEMON_ADDR`), sharded per workspace root (`daemon/crates/lattice-daemon/src/socket_server.rs`). `--stdio` is a thin proxy (`proxy.rs`) that sends a `ProxyHello` and forwards stdio ⇄ TCP.
- File watching: `daemon/crates/lattice-daemon/src/watcher.rs` (`notify` crate, 500 ms debounce); watch errors are logged and otherwise ignored.
- Metrics: `daemon/crates/lattice-daemon/src/rpc/session_metrics.rs` (`SessionMetrics`, `SessionToolTrace`, `record_tool_call()`).
- Tool list/schema tests: `daemon/crates/lattice-daemon/src/rpc/mcp_schema_tests.rs` (+ `tool_list.rs`, `round_trip.rs`, `backward_compat.rs`, `shims.rs`, `render_modes.rs`).
- Canonical tool doc: `docs/architecture/2026-05-16-mcp-tool-reference.md`.
- `.claude/skills/lattice-workflow` is a **symlink into the meridian repo** — the real skill file must be edited there.

## Phase 0 — Fix the broken configuration (do first; small, unblocks everything)

1. **Remove the user-global server registration.** In `~/.claude.json`, delete the top-level `mcpServers.lattice` entry (the one running the extension binary with `--workspace /home/pete`). The project `.mcp.json` is the single canonical registration for this repo. Watching `$HOME` is never valid.
2. **Verify the project registration connects.** `.mcp.json` already points at `daemon/target/release/lattice --stdio --workspace …` with seven roots. After removing the global entry, confirm via `claude mcp list` (or `/mcp` in a session) that `lattice` connects and `tools/list` returns.
3. **Refuse pathological workspaces.** In `main.rs` workspace parsing (`parse_workspace_roots()`), reject (hard error with clear message) a workspace root that is the user's home directory or filesystem root, and warn when a root contains more than `LATTICE_MAX_WARM_GRAPH_FILES` candidate files.

**Acceptance:** a fresh Claude Code session in this repo shows the lattice server connected; `lattice --stdio --workspace /home/pete` exits with a clear error instead of degrading.

## Phase 1 — `lattice doctor` + watcher degradation handling

### 1a. `lattice doctor` subcommand (`main.rs` + new `doctor.rs` in lattice-daemon)

A self-diagnostic that prints a pass/fail checklist and exits non-zero on any failure:

- Daemon reachable at `LATTICE_DAEMON_ADDR` (TCP connect + JSON-RPC ping); if not, say how to start it.
- For each configured workspace: shard exists, index status (files indexed, last index time, indexing-in-progress), watcher health (see 1b).
- MCP self-handshake: spawn `self --stdio --workspace <root>` as a child, run `initialize` + `tools/list`, report tool count and round-trip latency.
- Config scan: detect duplicate `lattice` server registrations across `~/.claude.json`, `<workspace>/.mcp.json`, and `<workspace>/.claude/settings*.json`; flag conflicts (different binaries/args under the same name) and stale binary paths. Read-only — report, don't edit.
- Binary skew: compare version/mtime of the running daemon binary vs `extension/bin/lattice` and `~/.vscode/extensions/lattice.lattice-0.1.0/bin/lattice`; warn on mismatch (the deploy step in CLAUDE.md is easy to forget).

### 1b. Watcher degraded mode (`watcher.rs`, `index_status`)

When `notify` watch setup fails (inotify limit etc.): fall back to periodic polling re-scan (configurable interval, default 30 s, env `LATTICE_POLL_INTERVAL_SECS`), record a `watch_degraded: true` flag + reason on the shard, and surface it in `index_status` output and `lattice doctor`. Never silently run with a dead watcher.

**Acceptance:** unit tests for config-scan conflict detection; integration test that forces a watch failure (e.g., injected error) and asserts polling fallback engages and `index_status` reports degraded; `lattice doctor` run manually against the live daemon shows all green.

## Phase 2 — Consolidate the agent-facing tool surface to 8 verbs

Replace the 50-name surface with exactly these MCP tools. Internal capabilities are kept; only the agent-facing API collapses. Implement by adding a thin routing layer in `mcp.rs` that maps the new verb + `mode`/params onto the existing handler methods — do not rewrite handler internals in this phase.

| New tool | Absorbs (current handlers) | Key params |
|---|---|---|
| `context` | `get_context_capsule`, `get_working_set_context`, `summarize_subsystem`, `get_docs_capsule`, `get_skeleton`, `get_repo_playbook`, `get_project_rules`, `expand_context` | `query`, `mode` (`auto`\|`focused`\|`subsystem`\|`docs`\|`skeleton`\|`working_set`\|`rules`\|`expand`), `files[]`, `handle` |
| `prepare_change` | `prepare_change`, `plan_edit`, `trace_scenario` | `task`, `mode` (`prepare`\|`plan_edit`\|`trace`) |
| `impact` | `get_impact_graph`, `impact_from_diff`, `get_dependents`, `get_dependencies`, `find_relevant_tests` | `target` (symbol\|file\|diff), `direction`, `include_tests` (default true) |
| `diagnose` | `diagnose_failure` | `failure_text`, `context_files[]` |
| `search` | `search_symbols`, `search_logic_flow`, `get_symbol`, `get_backlinks`, `get_outgoing_links` | `query`, `kind` (`symbol`\|`flow`\|`links`) |
| `remember` | `save_memory`, `save_quick_memory`, `record_workflow_outcome` | `content`, `kind` (`quick`\|`durable`\|`outcome`), scope fields from existing schemas |
| `recall` | `search_memory`, `get_task_memory`, `verify_explain_memory` | `query`, `mode` (`search`\|`task`\|`verify`), `task_id` |
| `status` | `index_status`, `find_stale_docs`, `list_stale_memories`, `list_memory_conflicts` | `scope` (`index`\|`docs`\|`memory`) |

Demoted off the MCP surface (no longer advertised or callable as MCP tools, but kept as JSON-RPC methods on the daemon socket for the extension/CLI): `submit_lsp_edges`, `workspace_setup`, `get_session_metrics`, `get_memory_metrics`, `get_event_trace`, `inspect_working_memory`, `consolidate_session`, `propose_memory_evolution`, `apply_memory_evolution`.

Deleted outright (no-legacy-debt policy): the legacy aliases `query_context`, `blast_radius`, `get_file_context`, `recall_memories`, and the shims `verify_memory`, `explain_memory`, `apply_memory_evolution`-as-MCP-shim. Remove `mcp_schema_tests/backward_compat.rs` and `shims.rs`; rewrite `tool_list.rs` to assert exactly the 8 names above.

**Tool descriptions are product copy.** Each of the 8 descriptions must state, in the first sentence, the situation where the tool beats grep — e.g. `impact`: "Returns every symbol, file, and test affected by changing a target — the blast radius grep cannot compute. Call before any multi-file or non-obvious change." Keep each description under ~80 words. One sentence of "don't use when" is allowed; no hedging like "can optionally be used".

**Docs to update in the same change:** rewrite `docs/architecture/2026-05-16-mcp-tool-reference.md` as the 8-verb reference (rename with the new date); update `CLAUDE.md` and `CLAUDE.example.md` tool sections (shrink the list to the 8 verbs + one-line routing guidance); update the `lattice-workflow` skill **at its symlink target in the meridian repo** (`.claude/skills/lattice-workflow` → resolve with `readlink -f`) so its mandatory-first-call table uses the new verbs and the ToolSearch load line lists them.

**Acceptance:** `cargo test --workspace` green; `tools/list` over a real `--stdio` handshake returns exactly 8 tools; calling any removed name returns the standard unknown-tool error; round-trip and render-mode tests updated to the new names.

## Phase 3 — CLI query interface

Add agent-friendly subcommands to the existing binary (extend the arg parsing in `main.rs`; new module `cli/` in lattice-daemon). Each is a thin TCP client to the global daemon (reuse the proxy's connect + `ProxyHello` path or speak JSON-RPC directly), auto-detecting the workspace root by walking up from `$PWD` to a root known to the daemon:

```
lattice context "<query>" [--mode …] [--files …]
lattice impact <symbol|path|--diff> [--no-tests]
lattice search "<query>" [--kind symbol|flow|links]
lattice diagnose [-]            # reads failure text from arg or stdin
lattice remember "<content>" [--kind quick|durable|outcome]
lattice recall "<query>" [--mode search|task]
lattice status [--scope index|docs|memory]
lattice doctor                  # from Phase 1
lattice metrics                 # from Phase 5
```

Rationale: agents reach for Bash constantly; a grep-shaped command is in-distribution in a way MCP tools are not, and it works in any harness (Codex, Gemini, plain shells) with zero MCP registration.

Behavior contract:

- Output is compact markdown to stdout (same renderer as MCP markdown mode); `--json` for machine output.
- If the daemon is not running: print one actionable line to stderr (`lattice daemon not running — start with: lattice --daemon`) and exit 2. Never hang.
- Hard wall-clock budget: 5 s default, `--timeout`; on expiry print whatever partial results exist plus a note, exit 3.
- Exit 0 with results, 1 with no results, 2 daemon unreachable, 3 timeout.

**Acceptance:** integration test (can shell out to the built binary against a daemon started in a temp workspace) covering each subcommand's happy path + the daemon-down path; `lattice context` end-to-end latency under 2 s on this repo (assert in test with generous CI margin or verify manually and record numbers in the PR description).

## Phase 4 — Harness integration package (ambient context via hooks)

This is the highest-leverage change: stop depending on the model choosing a tool. New top-level directory `integrations/`:

### 4a. `integrations/claude-code/`

Hook scripts (bash, calling the Phase 3 CLI; every script must no-op cleanly with exit 0 if `lattice` is missing or the daemon is down — hooks must never break a session):

- **SessionStart** → `lattice recall --mode task --json` rendered to a short markdown block injected as additionalContext: open task memory, recent outcomes, repo rules. Budget: ≤ 1500 tokens of output, 2 s timeout.
- **UserPromptSubmit** → `lattice context "<prompt>" --mode auto` with a tight budget (≤ 1200 tokens, 2 s). Inject only when the relevance score clears a threshold — emit nothing rather than noise. (Add a `--min-relevance` flag to the CLI for this; the daemon already computes ranking scores in retrieval.)
- **PostToolUse on Edit|Write** → `lattice impact <edited-file> --no-tests` summarized to ≤ 10 lines: direct dependents + covering tests. Skip when the file has fewer than N dependents (default 3) to avoid noise on leaf edits.
- **Stop** → `lattice remember --kind outcome` fed a compact summary of the session's edited files (from the hook payload), so the next session's SessionStart recall has material.

Plus `install.sh` (or a `lattice install claude-code` subcommand — implementer's choice, document whichever) that merges the hook config into the project's `.claude/settings.json` idempotently, and a README explaining each hook, its budget, and how to disable it.

### 4b. `integrations/codex/`

A documented `config.toml` snippet for Codex's `mcp_servers.lattice` (single-workspace, release-binary path — mirroring the invocation already observed working) plus a note that the Phase 3 CLI works from Codex's shell tool without any MCP config.

### 4c. Documentation

New `docs/operator-guide/agent-integration.md`: one canonical page covering the `.mcp.json` registration, the hooks package, the CLI, and the rule "exactly one registration named `lattice` per scope; never workspace = `$HOME`" — with `lattice doctor` as the verification step.

**Acceptance:** hooks installed in this repo's `.claude/settings.json`; manual verification that a fresh session receives the SessionStart injection and that all hooks exit 0 with the daemon stopped; scripts pass `shellcheck`.

## Phase 5 — Latency/output contract + adoption metrics

### 5a. Response contract (enforced, not aspirational)

For the 8 MCP verbs and their CLI twins:

- p50 < 1 s, hard cap 5 s server-side; on cap, return ranked partial results with a `partial: true` marker instead of erroring.
- While a shard is still indexing, answer from whatever is indexed and prefix the response with one line: `index N% complete — results may be incomplete`. Never block a query on indexing.
- Every result item carries `file:line`. Default response budget per call ≤ ~2000 tokens (reuse the existing budget/clipping machinery from memory retrieval, e.g. `clip_to_budget`); `expand` mode / `handle` is the escape hatch for more, not bigger defaults.

Add timing assertions to the existing metrics surface tests where feasible; otherwise record measured latencies for the 8 verbs on this repo in the PR description.

### 5b. Adoption metrics (`session_metrics.rs`)

- Tag every recorded tool call with `client` (from MCP `clientInfo.name`, the `ProxyHello`, or `cli`/`hook` for those channels) and `channel` (`mcp` | `cli` | `hook`).
- Persist daily per-client/per-channel/per-tool counters on the daemon (not just per-session), plus a follow-through signal: whether an `impact`/`context` result's suggested files were subsequently reported edited (the event-capture pipeline in `event_capture` already sees edits via the watcher — correlate by path within the session window).
- `lattice metrics` CLI: table of the last 14 days — calls by client × channel × tool, plus follow-through rate. This is the instrument for judging whether adoption actually moved.

**Acceptance:** metrics tests extended for the new dimensions; `lattice metrics` shows real data after a session that used the hooks.

## Phase ordering and dependencies

0 → 1 → 2 → 3 → 4 → 5. Phase 3 depends on 2 (CLI verbs mirror the MCP verbs). Phase 4 depends on 3. Phase 5a can land with 2/3; 5b last. Each phase is a separate commit/PR with its tests; `cargo test --workspace` and the extension build must be green at every phase boundary. After daemon changes, redeploy per CLAUDE.md → "Deploy" (both binary locations) or the running daemon and extension keep using the old surface.

## Out of scope

- Rewriting handler internals, retrieval ranking, or the memory graph schema.
- VS Code extension UI changes (it talks to the daemon socket; the demoted-tool RPC methods keep it working).
- Multi-machine/remote daemon support.

## Success criteria for the whole effort

1. A fresh Claude Code session in this repo has the `lattice` server connected with exactly 8 tools, and receives Lattice context via hooks without invoking any tool.
2. `lattice doctor` exists and passes; a broken config or dead watcher is loudly visible instead of silent.
3. `lattice metrics` shows nonzero Claude-channel usage (hook + MCP/CLI) within normal working sessions — measured, not anecdotal.
