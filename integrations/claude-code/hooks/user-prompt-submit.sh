#!/usr/bin/env bash
set -u

# shellcheck source=/home/pete/cadres/lattice/integrations/claude-code/hooks/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

if ! lattice_hook_ready; then
  exit 0
fi

payload="$(cat || true)"
prompt="$(
  printf '%s' "$payload" | python3 -c '
import json
import sys

try:
    data = json.load(sys.stdin)
except Exception:
    raise SystemExit

for key in ("prompt", "user_prompt", "message", "input"):
    value = data.get(key) if isinstance(data, dict) else None
    if isinstance(value, str) and value.strip():
        print(value)
        break
'
)"

if [[ -z "${prompt//[[:space:]]/}" ]]; then
  exit 0
fi

context="$(lattice_hook_call context "$prompt" --mode auto --min-relevance "${LATTICE_HOOK_MIN_RELEVANCE:-0.25}" --timeout "${LATTICE_HOOK_TIMEOUT:-1.8}" 2>/dev/null || true)"
if [[ ${#context} -lt 80 ]]; then
  exit 0
fi
case "$context" in
  *"No relevant"* | *"no relevant"* | *"no result"* | *"No result"*) exit 0 ;;
esac

content="$(printf '## Lattice Prompt Context\n\n%s\n' "$context" | lattice_limit_chars "${LATTICE_HOOK_PROMPT_CHAR_BUDGET:-4800}")"
lattice_emit_context "UserPromptSubmit" "$content"
exit 0
