#!/usr/bin/env bash
set -u

# shellcheck source=/home/pete/cadres/lattice/integrations/claude-code/hooks/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

if ! lattice_hook_ready; then
  exit 0
fi

payload="$(cat || true)"
edited_file="$(
  printf '%s' "$payload" | python3 -c '
import json
import sys

try:
    data = json.load(sys.stdin)
except Exception:
    raise SystemExit

preferred = ("file_path", "path", "filename", "file")

def walk(value):
    if isinstance(value, dict):
        for key in preferred:
            item = value.get(key)
            if isinstance(item, str) and item.strip():
                print(item)
                raise SystemExit
        for item in value.values():
            walk(item)
    elif isinstance(value, list):
        for item in value:
            walk(item)

walk(data)
'
)"

if [[ -z "${edited_file//[[:space:]]/}" ]]; then
  exit 0
fi

impact="$("$lattice_bin" impact "$edited_file" --no-tests --timeout "${LATTICE_HOOK_TIMEOUT:-1.5}" 2>/dev/null || true)"
if [[ -z "${impact//[[:space:]]/}" ]]; then
  exit 0
fi
if [[ "$impact" == *"not found"* || "$impact" == *"Missing required parameter"* ]]; then
  exit 0
fi

summary="$(
  printf '%s\n' "$impact" | python3 -c '
import os
import sys

threshold = int(os.environ.get("LATTICE_HOOK_MIN_DEPENDENTS", "3"))
lines = [line.rstrip() for line in sys.stdin if line.strip()]
dependent_lines = [
    line for line in lines
    if "dependent" in line.lower() or "affected" in line.lower() or "caller" in line.lower()
]
if threshold > 0 and len(dependent_lines) < threshold:
    raise SystemExit
for line in lines[:10]:
    print(line)
'
)"

if [[ -z "${summary//[[:space:]]/}" ]]; then
  exit 0
fi

lattice_emit_context "PostToolUse" "## Lattice Edit Impact"$'\n\n'"$summary"
exit 0
