#!/usr/bin/env bash
set -u

lattice_hook_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The Codex package owns the shared call, readiness, parsing, and truncation
# library. Claude's package supplies only its client identity and JSON envelope.
# shellcheck disable=SC1091 # The sibling library is resolved from this installed package.
source "$lattice_hook_dir/../../codex/hooks/common.sh"
lattice_client_name="claude-code"

lattice_hook_ready() {
  lattice_bin="$(lattice_find_bin)" || return 1
  LATTICE_SKIP_METRICS=1 lattice_hook_call status \
    --timeout "${LATTICE_HOOK_PROBE_TIMEOUT:-0.5}" >/dev/null 2>&1
}

lattice_emit_context() {
  local event_name="$1"
  local content="$2"
  if [[ -z "${content//[[:space:]]/}" ]]; then
    return 0
  fi
  LATTICE_HOOK_EVENT="$event_name" LATTICE_HOOK_CONTENT="$content" python3 -c '
import json
import os

event = os.environ["LATTICE_HOOK_EVENT"]
content = os.environ["LATTICE_HOOK_CONTENT"]
print(json.dumps({
    "hookSpecificOutput": {
        "hookEventName": event,
        "additionalContext": content,
    }
}, separators=(",", ":")))
'
}
