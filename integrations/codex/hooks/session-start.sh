#!/usr/bin/env bash
set -u

# shellcheck disable=SC1091 # Sibling source path is resolved dynamically for portable project installs.
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

if ! lattice_hook_ready; then
  exit 0
fi

temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/lattice-codex-session.XXXXXX")" || exit 0
trap 'rm -rf -- "$temp_dir"' EXIT

lattice_hook_call recall "session start" --mode task --json --timeout "${LATTICE_HOOK_TIMEOUT:-3.5}" >"$temp_dir/recall" 2>/dev/null &
recall_pid=$!
lattice_hook_call context "repo rules and operator workflow" --mode rules --timeout "${LATTICE_HOOK_RULES_TIMEOUT:-3.5}" >"$temp_dir/rules" 2>/dev/null &
rules_pid=$!
wait "$recall_pid" 2>/dev/null || true
wait "$rules_pid" 2>/dev/null || true

recall_json="$(<"$temp_dir/recall")"
rules="$(<"$temp_dir/rules")"
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

if [[ -n "${content//[[:space:]]/}" ]]; then
  printf '%s\n' "$content"
fi
exit 0
