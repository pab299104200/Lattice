#!/usr/bin/env bash
set -u

# shellcheck disable=SC1091 # Sibling source path is resolved dynamically for portable project installs.
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

if ! lattice_hook_ready; then
  exit 0
fi

payload="$(cat || true)"
edited_file="$(printf '%s' "$payload" | lattice_extract_files 2>/dev/null | head -n 1 || true)"

if [[ -z "${edited_file//[[:space:]]/}" ]]; then
  exit 0
fi

impact="$(lattice_hook_call impact "$edited_file" --no-tests --timeout "${LATTICE_HOOK_TIMEOUT:-2.5}" 2>/dev/null || true)"
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

if [[ -n "${summary//[[:space:]]/}" ]]; then
  printf '## Lattice Edit Impact\n\n%s\n' "$summary"
fi
exit 0
