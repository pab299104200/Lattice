#!/usr/bin/env bash
set -u

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
lattice_hook_adapter codex user-prompt-submit
exit 0
