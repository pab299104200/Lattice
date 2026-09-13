#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
hooks_path="${1:-$(pwd)/.codex/hooks.json}"
hooks_dir="$(dirname "$hooks_path")"
release_bin="${LATTICE_RELEASE_BIN:-$repo_root/daemon/target/release/lattice}"
install_bin_dir="${LATTICE_INSTALL_BIN_DIR:-${HOME:-}/.local/bin}"

if [[ -e "$hooks_dir" && ! -d "$hooks_dir" ]]; then
  printf 'Cannot install Codex hooks: %s exists and is not a directory.\n' "$hooks_dir" >&2
  printf 'Move that file aside, create %s as a directory, then rerun this installer.\n' "$hooks_dir" >&2
  exit 1
fi

mkdir -p "$hooks_dir"

if [[ ! -f "$hooks_path" ]]; then
  printf '{}\n' >"$hooks_path"
fi

HOOKS_PATH="$hooks_path" REPO_ROOT="$repo_root" python3 - <<'PY'
import json
import os
from pathlib import Path

hooks_path = Path(os.environ["HOOKS_PATH"])
repo_root = Path(os.environ["REPO_ROOT"])
hook_dir = repo_root / "integrations" / "codex" / "hooks"

try:
    settings = json.loads(hooks_path.read_text(encoding="utf-8"))
except json.JSONDecodeError:
    settings = {}

if not isinstance(settings, dict):
    settings = {}

hooks = settings.setdefault("hooks", {})
if not isinstance(hooks, dict):
    settings["hooks"] = hooks = {}

desired = {
    "SessionStart": [
        {
            "matcher": "startup|resume|clear|compact",
            "hooks": [
                {
                    "type": "command",
                    "command": str(hook_dir / "session-start.sh"),
                    "timeout": 4,
                    "statusMessage": "Loading Lattice session context",
                }
            ],
        }
    ],
    "UserPromptSubmit": [
        {
            "hooks": [
                {
                    "type": "command",
                    "command": str(hook_dir / "user-prompt-submit.sh"),
                    "timeout": 5,
                    "statusMessage": "Loading Lattice prompt context",
                }
            ],
        }
    ],
    "PostToolUse": [
        {
            "matcher": "apply_patch|Edit|Write",
            "hooks": [
                {
                    "type": "command",
                    "command": str(hook_dir / "post-tool-use.sh"),
                    "timeout": 5,
                    "statusMessage": "Checking Lattice edit impact",
                }
            ],
        }
    ],
    "Stop": [
        {
            "hooks": [
                {
                    "type": "command",
                    "command": str(hook_dir / "stop.sh"),
                    "timeout": 4,
                    "statusMessage": "Finalizing protected Lattice session capture",
                }
            ],
        }
    ],
}

for event, entries in desired.items():
    current = hooks.setdefault(event, [])
    if not isinstance(current, list):
        hooks[event] = current = []
    for entry in entries:
        desired_hook = entry["hooks"][0]
        desired_name = Path(desired_hook["command"]).name
        matched = False
        for existing_entry in current:
            if not isinstance(existing_entry, dict):
                continue
            for existing_hook in existing_entry.get("hooks", []):
                if not isinstance(existing_hook, dict):
                    continue
                command = existing_hook.get("command")
                if isinstance(command, str) and Path(command).name == desired_name:
                    existing_hook.update(desired_hook)
                    if "matcher" in entry:
                        existing_entry["matcher"] = entry["matcher"]
                    else:
                        existing_entry.pop("matcher", None)
                    matched = True
        if not matched:
            current.append(entry)

hooks_path.write_text(json.dumps(settings, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY

printf 'Installed Lattice Codex hooks in %s\n' "$hooks_path"

if [[ "${LATTICE_SKIP_CLI_INSTALL:-0}" != "1" ]]; then
  if [[ -z "$install_bin_dir" ]]; then
    printf 'Cannot install Lattice CLI: HOME is unset and LATTICE_INSTALL_BIN_DIR was not provided.\n' >&2
    exit 1
  fi
  if [[ ! -x "$release_bin" ]]; then
    printf 'Cannot install Lattice CLI: release binary is missing at %s.\n' "$release_bin" >&2
    printf 'Build it first with: cargo build --release --manifest-path %s/daemon/Cargo.toml\n' "$repo_root" >&2
    exit 1
  fi
  mkdir -p "$install_bin_dir"
  cli_path="$install_bin_dir/lattice"
  if [[ -e "$cli_path" && ! -L "$cli_path" ]]; then
    printf 'Cannot install Lattice CLI: %s already exists and is not a symlink.\n' "$cli_path" >&2
    exit 1
  fi
  ln -sfn "$release_bin" "$cli_path"
  printf 'Installed Lattice CLI symlink at %s\n' "$cli_path"
fi
