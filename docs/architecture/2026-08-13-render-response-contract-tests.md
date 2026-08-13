# Render response contract test design

This is the standalone verification note for B2. Tests should exercise the
daemon's structured-to-MCP boundary and the CLI request boundary without
editing production code in the test plan.

## Required cases

| Case | Input | Assertions |
| --- | --- | --- |
| Default render | workflow call with no `render` | schema and parser select `markdown`; one text block; no fenced JSON; no `lattice-metrics` comment |
| Explicit Markdown | `render=markdown` | same single summary; no duplicate next action; no comment |
| Explicit JSON | `render=json` | one text block; `serde_json` parses it; no Markdown heading or comment |
| Removed hybrid | `render=hybrid` | schema rejects it and handler returns invalid-argument; no silent fallback |
| Single action | payload containing `next_steps`, `suggested_expand`, and retrieval contract | rendered summary has at most one `Next action`; normalized contract has at most one value |
| No action | payload without an actionable handle/step | no fabricated action and no empty action line |
| Server telemetry | successful Markdown and JSON calls | session/adoption metrics contain delivery/render metadata and next action, while returned text contains no telemetry comment |
| Telemetry failure | metrics sink returns an error | tool response remains successful and unchanged; warning is observable |
| Size regression | fixed fixture query, omitted render | record Markdown byte/token size and compare with the pre-B2 hybrid fixture; assert the new response excludes the serialized duplicate payload |
| CLI compatibility | CLI call without `--json`, then with `--json` | CLI explicitly requests Markdown/JSON respectively; terminal rendering remains parseable and does not reintroduce hybrid output |

## Suggested test placement

- Extend `daemon/crates/lattice-daemon/src/rpc/mcp_schema_tests/render_modes.rs`
  for schema/default/invalid-mode and MCP envelope assertions.
- Extend the existing `mcp.rs` render tests only for pure helper behavior if
  the implementation keeps those helpers there; do not make tests depend on
  parsing `lattice-metrics` comments.
- Add structured telemetry assertions beside the existing session/adoption
  metrics tests, using the same fixture store and session identity.
- Keep CLI assertions in `daemon/crates/lattice-daemon/src/cli.rs` tests or
  its existing integration harness; they should inspect the request sent to a
  fixture daemon as well as terminal output.

## Invariants to preserve

The tests must continue to prove the existing bounded budget, stable context
handle/origin, MCP `content` envelope, and JSON validity. A smaller response
is not sufficient if it drops handles or turns telemetry into user-visible
boilerplate.
