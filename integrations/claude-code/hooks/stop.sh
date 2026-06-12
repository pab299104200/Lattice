#!/usr/bin/env bash
set -u

# shellcheck source=/home/pete/cadres/lattice/integrations/claude-code/hooks/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

if ! lattice_hook_ready; then
  exit 0
fi

payload="$(cat || true)"
edited_files="$(
  printf '%s' "$payload" | python3 -c '
import json
import sys

try:
    data = json.load(sys.stdin)
except Exception:
    raise SystemExit

seen = []

def add(value):
    if not isinstance(value, str):
        return
    item = value.strip()
    if not item or item.startswith("http:") or item.startswith("https:"):
        return
    if item not in seen:
        seen.append(item)

def walk(value):
    if isinstance(value, dict):
        for key, item in value.items():
            lowered = key.lower()
            if "file" in lowered or lowered in ("path", "paths"):
                if isinstance(item, list):
                    for child in item:
                        add(child)
                else:
                    add(item)
            walk(item)
    elif isinstance(value, list):
        for item in value:
            walk(item)

walk(data)
for item in seen[:20]:
    print(item)
'
)"

if [[ -z "${edited_files//[[:space:]]/}" ]]; then
  exit 0
fi

summary="Session edited files: $(printf '%s' "$edited_files" | paste -sd ', ' -)"
lattice_hook_call remember "$summary" --kind outcome --timeout "${LATTICE_HOOK_TIMEOUT:-1.5}" >/dev/null 2>&1 || true
exit 0
