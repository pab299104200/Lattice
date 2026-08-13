#!/usr/bin/env bash
set -u

# shellcheck disable=SC1091 # Sibling source path is resolved dynamically for portable project installs.
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

if ! lattice_hook_ready; then
  exit 0
fi

payload="$(cat || true)"
edited_files="$(printf '%s' "$payload" | lattice_extract_files 2>/dev/null || true)"

if [[ -z "${edited_files//[[:space:]]/}" ]]; then
  exit 0
fi

edited_files_summary="$(
  printf '%s\n' "$edited_files" | awk '
    BEGIN { separator = "" }
    NF { printf "%s%s", separator, $0; separator = ", " }
    END { print "" }
  '
)"
summary="Session edited files: $edited_files_summary"
lattice_hook_call remember "$summary" --kind outcome --timeout "${LATTICE_HOOK_TIMEOUT:-2.5}" >/dev/null 2>&1 || true
exit 0
