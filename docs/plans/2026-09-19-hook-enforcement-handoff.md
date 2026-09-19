# Handoff: enforced Lattice workflow in hooks

Branch: `feature/hook-enforcement` (from `master`). Brief:
`docs/plans/2026-09-19-hook-enforcement-brief.md`. This file is written incrementally.
Every claim is tagged **verified** (evidence named) or **hypothesis**.

## Status

IN PROGRESS. Investigation complete; implementation started. Nothing below the
"Design decisions" heading is built until a "Built" entry says so.

## Host contract, checked against primary sources (2026-09-18)

Claude Code, from `https://code.claude.com/docs/en/hooks.md` (fetched raw, read directly):

- **verified** PreToolUse blocks with exit 0 plus
  `hookSpecificOutput.{hookEventName:"PreToolUse", permissionDecision:"deny", permissionDecisionReason}`.
  The reason on `deny` is "shown to Claude". Exit 2 "routes the same way as deny". Top-level
  `decision`/`reason` is deprecated for this event. Precedence is deny > defer > ask > allow.
- **verified** (hooks guide) a hook `deny` "blocks the tool even in bypassPermissions mode".
- **verified** `tool_input.file_path` is always absolute for Write, Edit and Read.
- **verified** matchers made only of letters, digits, `_`, `-`, spaces, `|`, `,` are exact
  alternatives, so `apply_patch|Edit|Write|MultiEdit|NotebookEdit` is valid and case-sensitive.
- **verified** `additionalContext`, `systemMessage` and plain stdout are capped at 10,000
  characters; PostToolUse takes `hookSpecificOutput.additionalContext`.
- **verified** Stop input carries `stop_hook_active` and `last_assistant_message`. Stop
  `hookSpecificOutput.additionalContext` is NOT passive: "The conversation continues so Claude
  can act on it", under the `stop_hook_active` flag and an 8-continuation cap.
- **verified** hooks fired in subagents add `agent_id` and `agent_type`. The docs do not say
  whether `session_id` is shared with the parent. **hypothesis**: it is shared.
- **not documented** `MultiEdit` (absent from the tools reference) and the NotebookEdit path
  field. The adapter accepts `file_path` and `notebook_path`; a gated tool with no readable
  path is allowed.
- **verified** `tool_response.bashEditDiff.changedFiles` exists (v2.1.269+), but the docs call
  it best effort, beta, and say "not to enforce a policy". Not used; repository state is.

Codex, from `openai/codex` `codex-rs/hooks/src` on the default branch (read directly):

- **verified** events include PreToolUse, PostToolUse, Stop, SessionEnd. PreToolUse blocks on
  exit 0 JSON `permissionDecision: "deny"` with a reason, or exit 2 with stderr.
- **verified** `apply_patch` hook `tool_input` is `{"command": <patch text>}`. There is no
  dedicated path field, so a per-path gate would need the patch text. Lattice does not read it.
- **verified** Codex Stop can only block-and-continue; there is no passive model-facing note.

## Diagnosis (requirement 7)

See "Diagnosis" below; filled in as each point is re-verified by me.

## Design decisions

(to be completed)
