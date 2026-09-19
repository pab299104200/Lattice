#!/usr/bin/env bash
set -u

# Installed only in a workspace that has opted in to enforcement. The adapter
# decides and, when it denies, says so with the documented PreToolUse JSON on
# stdout. This wrapper always exits 0: a missing binary, an unreachable daemon
# or a slow adapter must never block an edit. See docs/hook-enforcement.md.
# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
lattice_hook_adapter codex pre-tool-use
exit 0
