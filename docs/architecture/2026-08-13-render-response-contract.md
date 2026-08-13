# MCP render and response contract

**Status:** B2 migration contract (design)
**Date:** 2026-08-13

This note is the authoritative contract for the B2 response cleanup. It is
deliberately limited to the daemon MCP response boundary; tool-specific
payloads remain structured values and are not redesigned here.

## Current implementation (verified)

The response path currently has four relevant layers:

1. `daemon/crates/lattice-daemon/src/rpc/mcp.rs` defines
   `WorkflowRenderMode` (`Json`, `Markdown`, `Hybrid`) and
   `WorkflowResponseOptions` (around lines 320–345).
2. `parse_workflow_response_options` (around lines 7067–7085) treats an
   absent `render` as `Hybrid`. The MCP tool schemas also advertise `hybrid`
   as the default in the workflow definitions around lines 1418–1937 and in
   the context/impact definitions around lines 919–1002.
3. `wrap_workflow_tool_result` (around lines 12549–12576) emits JSON only for
   `Json`, a Markdown summary plus a hidden metrics comment for `Markdown`,
   and a Markdown summary followed by a fenced full JSON payload for
   `Hybrid`. `wrap_tool_result` is the non-workflow JSON-text fallback.
4. `build_workflow_metrics_comment` and
   `build_workflow_metrics_payload` (around lines 12579–12725) serialize
   workflow metadata into `<!-- lattice-metrics: ... -->`. The current
   `record_tool_metrics` path (around lines 4454–4495) extracts metadata from
   the wrapped result, including parsing that comment. This is an internal
   coupling that must be removed when the comment is removed.

The CLI has a separate presentation boundary in
`daemon/crates/lattice-daemon/src/cli.rs`: `parse_args` (around lines 286–296)
injects `render=markdown` for non-`--json` requests, while `render_output`
(around lines 686–710) renders the daemon's returned text for terminal use.
The CLI behavior is not a second MCP default: it is an explicit client
request and must continue to work after the daemon default changes.

## Target contract

### Rendering

- An omitted `render` means `markdown` at every workflow MCP schema and in
  `parse_workflow_response_options`.
- `render=markdown` returns exactly one text block containing the bounded
  Markdown summary. It does not contain a fenced structured payload and does
  not contain an HTML metrics comment.
- `render=json` returns exactly one text block containing the serialized
  structured payload. It must remain valid JSON and must not be prefixed by a
  summary or suffixed by metadata.
- `render=hybrid` is removed from public schemas and rejected as an invalid
  render value. The prerelease contract has no reason to preserve a mode that
  duplicates the payload. If an intentional compatibility decision later
  requires accepting it, it must be an explicit migration exception with its
  own tests; it is not the B2 default.
- All modes preserve the existing MCP envelope (`content[0].type=text`),
  bounded summary behavior, handles, budget metadata, and error semantics.

### Next action

Every rendered workflow response exposes at most one actionable next step:

- The canonical field is `agent_retrieval_contract.next_action`.
- It is a single non-empty string, capped to the existing summary truncation
  bound (96 characters), or omitted when no safe next action exists.
- The Markdown summary may print that same value once as `- Next action: …`.
  It must not separately print `next_steps[0]`, `suggested_expand`, or another
  contract action when they refer to the same continuation.
- The JSON payload may retain source fields needed for provenance, but the
  rendered contract and metrics projection must select one deterministic
  `next_action`; clients must not infer priority by iterating multiple action
  arrays.
- The action must be derived server-side from the response's handles and
  available ranked pivots. It must never be fabricated by the CLI renderer.

### Metrics

Workflow metadata is telemetry, not user response content:

- The daemon records delivery mode, wire format, handle/origin, budget,
  truncation, anchor/fallback state, suggested expansion, and the single
  next-action value in session/adoption metrics before/while returning the
  response.
- No `lattice-metrics` HTML comment is emitted in Markdown or JSON.
- `record_tool_metrics` must consume the structured value before rendering (or
  an equivalent internal typed metadata object), so telemetry does not depend
  on parsing client-visible text.
- Metrics failures remain best-effort and observable through the existing
  warning/error path; they must not change the rendered response or cause an
  otherwise successful tool call to fail.

## Migration sequence

1. Change all public schema defaults and parser fallback to `markdown`; remove
   `hybrid` from schemas and enum parsing.
2. Make `wrap_workflow_tool_result` implement only the Markdown and JSON
   contracts above. Remove comment generation and the text parser dependency.
3. Move metrics extraction to the pre-render structured response path and
   include the normalized single `next_action`.
4. Normalize summary construction so only one next action is displayed.
5. Update render-mode, schema round-trip, metrics, and CLI tests; record the
   fixed-query byte/token sizes for omitted-render Markdown versus the old
   hybrid baseline.
6. Update public README/MCP behavior documentation in the implementation
   workload. This architecture note remains the rationale and exact contract.

## Non-goals

This change does not alter ranking, handle lifetime, token-budget selection,
tool payload schemas unrelated to rendering, or the metrics definitions. It
only changes the public render default, removes duplicate payload output, and
separates telemetry from presentation.
