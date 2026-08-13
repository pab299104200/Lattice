# MCP Tool Reference

This is the canonical agent-facing MCP surface after the 2026-06-11 agent adoption overhaul. The public `tools/list` response contains exactly 8 tools. Older workflow, graph, docs, and memory capabilities still exist internally, but assistants reach them through these verbs instead of picking from dozens of near-duplicate names.

Implementation source of truth:

- Public MCP tool list: `daemon/crates/lattice-daemon/src/rpc/mcp.rs::handle_agent_tools_list`
- Public MCP dispatch: `daemon/crates/lattice-daemon/src/rpc/mcp.rs::handle_agent_tools_call`
- Daemon-internal raw tool list: `lattice/tools/list_all`
- Daemon-internal raw tool call path for CLI and first-party internals: `lattice/tool_call`

## Public MCP Tools

| Tool | When It Beats Grep | Main Routing |
|---|---|---|
| `context` | Finds the relevant code, docs, repo rules, file skeleton, working set, or handle expansion before the caller knows exact strings. | Routes by `mode`: `auto`, `focused`, `subsystem`, `docs`, `skeleton`, `working_set`, `rules`, `expand`, or `repo`. |
| `prepare_change` | Builds an implementation map with likely edit files, symbols, tests, risks, and memory. | `mode=prepare` routes to change prep, `mode=plan_edit` to patch planning, and `mode=trace` to scenario tracing. |
| `impact` | Computes dependency blast radius and relevant tests that literal search cannot infer. | Routes to impact graph, dependents, dependencies, diff impact, or relevant-test selection from `target`, `direction`, and `include_tests`. |
| `diagnose` | Maps compiler, test, or runtime failure text to likely culprit code and tests. | Routes to failure diagnosis using `failure_text`, optional `kind`, and optional context files. |
| `search` | Uses graph identity for symbol and document-section lookup, symbol details, call paths, backlinks, and outgoing links. | Default symbol search ranks exact names/files, phrase matches, and normalized all-term matches across names and paths; `symbol_detail`, `flow`, and `links` select the other routes. |
| `remember` | Writes reusable cross-session memory or workflow outcomes. | Routes by `kind=quick`, `durable`, or `outcome`. |
| `recall` | Retrieves task memory, searches durable memory, or verifies/explains memory with trust diagnostics. | Routes by `mode=search`, `task`, or `verify`. |
| `status` | Reports operational health that grep cannot observe. | Routes by `scope=index`, `docs`, `memory`, or `conflicts`. |

## Removed Public Names

The old agent-facing MCP names are not advertised and are not callable through `tools/call`. Removed legacy aliases are:

- `query_context`
- `blast_radius`
- `get_file_context`
- `recall_memories`

Removed MCP shims are:

- `verify_memory`
- `explain_memory`
- `apply_memory_evolution`

Calling any removed name through MCP returns the standard unknown-tool dispatcher error.

## Demoted Internal Capabilities

The implementation keeps lower-level capabilities for the CLI and daemon-internal consumers behind `lattice/tool_call` and `lattice/tools/list_all`. They are not part of the agent-facing MCP contract. Examples include detailed memory administration, event traces, session metrics, working-memory inspection, raw graph helper calls, and LSP edge submission.

This split is intentional: MCP agents get a compact 8-verb surface, while first-party code can still compose the lower-level daemon operations without reintroducing public shims or aliases.

## Response And Handle Contract

The 8 public verbs reuse the existing compact workflow renderers and structured payloads. Workflow responses accept only `render=markdown` (the default) or `render=json`; there is no hybrid or HTML rendering mode. First-pass responses should remain bounded and should prefer handles over large payloads. When a result includes `context_handle` or `suggested_expand`, callers should use `context` with `mode=expand`, `handle`, and `focus` for the next delta. Rendered workflow contracts may include a bounded `next_action` string, but must not smuggle telemetry or machine-only diagnostics into markdown.

Memory returned through `recall` is recall, not proof. Callers must use the trust diagnostics, checkout state, evidence links, and verification modes before relying on durable memory in high-risk work.

## CLI Twins

The `lattice` binary exposes shell equivalents for the same public verbs:

```bash
lattice context "<query>" [--mode ...] [--files ...]
lattice impact <symbol|path|--diff> [--no-tests]
lattice search "<query>" [--kind symbol|flow|links]
lattice diagnose [-]
lattice remember "<content>" [--kind quick|durable|outcome]
lattice recall "<query>" [--mode search|task]
lattice status [--scope index|docs|memory]
```

The CLI speaks the same daemon protocol as the stdio proxy but does not auto-start the daemon. Markdown is the default stdout format; `--json` returns the raw MCP result. `lattice context` defaults to `--mode subsystem` to keep shell calls query-relevant and under the latency budget; pass `--mode auto` or a more specific mode for broader routing. Exit code `2` means the daemon is unreachable, and exit code `3` means the CLI timeout expired.

Root-level files such as `AGENTS.md` are path anchors even without a directory separator. `prepare_change` also resolves existing workspace-file tokens directly into entry files, so an explicit filename outranks generic same-heading matches in archived or unrelated documents.

## Validation

The phase-2 contract is guarded by:

- `cargo test -p lattice-daemon --bin lattice mcp_schema_tests::tool_list`
- `cargo test -p lattice-daemon --bin lattice mcp_compat_tests`
- `cargo test --workspace`
- A real `--stdio` MCP handshake whose `tools/list` response contains exactly the 8 public names above.

The phase-3 CLI contract is guarded by:

- `cargo test -p lattice-daemon --test cli_query_tests`
- A live `lattice context ... --timeout 2` smoke test against the daemon with measured latency under 2 seconds.
