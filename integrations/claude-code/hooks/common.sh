#!/usr/bin/env bash
set -u

lattice_hook_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
lattice_repo_root="$(cd "$lattice_hook_dir/../../.." && pwd)"

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

lattice_hook_adapter() {
  local event="$1"
  local lattice_bin
  lattice_bin="$(lattice_find_bin)" || return 0
  "$lattice_bin" __hook-adapter claude-code "$event" 2>/dev/null || true
}
