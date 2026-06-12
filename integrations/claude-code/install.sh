#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
settings_path="${1:-$repo_root/.claude/settings.json}"

mkdir -p "$(dirname "$settings_path")"

if [[ ! -f "$settings_path" ]]; then
  printf '{}\n' >"$settings_path"
fi

SETTINGS_PATH="$settings_path" REPO_ROOT="$repo_root" python3 - <<'PY'
import json
import os
from pathlib import Path

settings_path = Path(os.environ["SETTINGS_PATH"])
repo_root = Path(os.environ["REPO_ROOT"])
hook_dir = repo_root / "integrations" / "claude-code" / "hooks"

with settings_path.open("r", encoding="utf-8") as fh:
    try:
        settings = json.load(fh)
    except json.JSONDecodeError:
        settings = {}

if not isinstance(settings, dict):
    settings = {}

hooks = settings.setdefault("hooks", {})

desired = {
    "SessionStart": [
        {
            "hooks": [
                {"type": "command", "command": str(hook_dir / "session-start.sh")},
            ],
        },
    ],
    "UserPromptSubmit": [
        {
            "hooks": [
                {"type": "command", "command": str(hook_dir / "user-prompt-submit.sh")},
            ],
        },
    ],
    "PostToolUse": [
        {
            "matcher": "Edit|Write",
            "hooks": [
                {"type": "command", "command": str(hook_dir / "post-tool-use.sh")},
            ],
        },
    ],
    "Stop": [
        {
            "hooks": [
                {"type": "command", "command": str(hook_dir / "stop.sh")},
            ],
        },
    ],
}

for event, entries in desired.items():
    current = hooks.setdefault(event, [])
    if not isinstance(current, list):
        hooks[event] = current = []
    existing_commands = {
        hook.get("command")
        for entry in current
        if isinstance(entry, dict)
        for hook in entry.get("hooks", [])
        if isinstance(hook, dict)
    }
    for entry in entries:
        commands = [
            hook["command"]
            for hook in entry.get("hooks", [])
            if isinstance(hook, dict) and "command" in hook
        ]
        if not any(command in existing_commands for command in commands):
            current.append(entry)

settings_path.write_text(json.dumps(settings, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY

printf 'Installed Lattice Claude Code hooks in %s\n' "$settings_path"
