#!/usr/bin/env bash
set -u

lattice_hook_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
lattice_repo_root="$(cd "$lattice_hook_dir/../../.." && pwd)"
lattice_bin=""

lattice_find_bin() {
  if [[ -n "${LATTICE_BIN:-}" && -x "${LATTICE_BIN:-}" ]]; then
    printf '%s\n' "$LATTICE_BIN"
    return 0
  fi
  if command -v lattice >/dev/null 2>&1; then
    command -v lattice
    return 0
  fi
  if [[ -x "$lattice_repo_root/daemon/target/release/lattice" ]]; then
    printf '%s\n' "$lattice_repo_root/daemon/target/release/lattice"
    return 0
  fi
  return 1
}

lattice_hook_ready() {
  lattice_bin="$(lattice_find_bin)" || return 1
  LATTICE_SKIP_METRICS=1 "$lattice_bin" status --timeout "${LATTICE_HOOK_PROBE_TIMEOUT:-0.2}" >/dev/null 2>&1
}

lattice_hook_call() {
  LATTICE_CLIENT_NAME="${LATTICE_CLIENT_NAME:-claude-code}" \
    LATTICE_CLIENT_CHANNEL="${LATTICE_CLIENT_CHANNEL:-hook}" \
    "$lattice_bin" "$@"
}

lattice_limit_chars() {
  local max_chars="$1"
  python3 -c '
import sys

limit = int(sys.argv[1])
text = sys.stdin.read()
sys.stdout.write(text[:limit])
' "$max_chars"
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

lattice_json_text() {
  python3 -c '
import json
import sys

raw = sys.stdin.read()
try:
    data = json.loads(raw)
except Exception:
    print(raw)
    raise SystemExit

content = data.get("content") if isinstance(data, dict) else None
if isinstance(content, list) and content:
    text = content[0].get("text") if isinstance(content[0], dict) else None
    if isinstance(text, str):
        try:
            nested = json.loads(text)
        except Exception:
            print(text)
        else:
            markdown = nested.get("markdown") if isinstance(nested, dict) else None
            print(markdown if isinstance(markdown, str) else json.dumps(nested, indent=2))
        raise SystemExit

print(json.dumps(data, indent=2))
'
}
