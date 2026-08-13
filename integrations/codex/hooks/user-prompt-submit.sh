#!/usr/bin/env bash
set -u

# shellcheck disable=SC1091 # Sibling source path is resolved dynamically for portable project installs.
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

if ! lattice_hook_ready; then
  exit 0
fi

payload="$(cat || true)"
prompt="$(printf '%s' "$payload" | lattice_extract_prompt 2>/dev/null || true)"

if [[ -z "${prompt//[[:space:]]/}" ]]; then
  exit 0
fi

context="$(lattice_hook_call context "$prompt" --mode auto --min-relevance "${LATTICE_HOOK_MIN_RELEVANCE:-0.25}" --timeout "${LATTICE_HOOK_TIMEOUT:-3.5}" 2>/dev/null || true)"
if [[ ${#context} -lt 80 ]]; then
  exit 0
fi
case "$context" in
  *"No relevant"* | *"no relevant"* | *"no result"* | *"No result"*) exit 0 ;;
esac

printf '## Lattice Prompt Context\n\n%s\n' "$context" | lattice_limit_chars "${LATTICE_HOOK_PROMPT_CHAR_BUDGET:-4800}"
exit 0
