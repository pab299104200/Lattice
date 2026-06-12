#!/usr/bin/env bash
set -u

# shellcheck source=/home/pete/cadres/lattice/integrations/claude-code/hooks/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

if ! lattice_hook_ready; then
  exit 0
fi

recall_json="$("$lattice_bin" recall "session start" --mode task --json --timeout "${LATTICE_HOOK_TIMEOUT:-1.6}" 2>/dev/null || true)"
rules="$("$lattice_bin" context "repo rules and operator workflow" --mode rules --timeout "${LATTICE_HOOK_RULES_TIMEOUT:-1.0}" 2>/dev/null || true)"

recall_text="$(printf '%s' "$recall_json" | lattice_json_text 2>/dev/null || true)"
content="$(
  {
    printf '## Lattice Session Context\n\n'
    if [[ -n "${recall_text//[[:space:]]/}" ]]; then
      printf '### Task Memory\n%s\n\n' "$recall_text"
    fi
    if [[ -n "${rules//[[:space:]]/}" ]]; then
      printf '### Repo Rules\n%s\n' "$rules"
    fi
  } | lattice_limit_chars "${LATTICE_HOOK_SESSION_CHAR_BUDGET:-6000}"
)"

lattice_emit_context "SessionStart" "$content"
exit 0
