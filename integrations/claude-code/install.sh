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
                {"type": "command", "command": str(hook_dir / "session-start.sh"), "timeout": 5},
            ],
        },
    ],
    "UserPromptSubmit": [
        {
            "hooks": [
                {"type": "command", "command": str(hook_dir / "user-prompt-submit.sh"), "timeout": 5},
            ],
        },
    ],
    "PostToolUse": [
        {
            "matcher": "Edit|Write",
            "hooks": [
                {"type": "command", "command": str(hook_dir / "post-tool-use.sh"), "timeout": 5},
            ],
        },
    ],
    "SessionEnd": [
        {
            "hooks": [
                {"type": "command", "command": str(hook_dir / "session-end.sh"), "timeout": 5},
            ],
        },
    ],
}

# Remove superseded Lattice Stop hooks without disturbing foreign Stop hooks.
for entry in hooks.get("Stop", []):
    if isinstance(entry, dict) and isinstance(entry.get("hooks"), list):
        entry["hooks"] = [
            hook for hook in entry["hooks"]
            if not (
                isinstance(hook, dict)
                and isinstance(hook.get("command"), str)
                and Path(hook["command"]).name == "stop.sh"
                and "integrations" in Path(hook["command"]).parts
                and "hooks" in Path(hook["command"]).parts
                and any(part in {"codex", "claude-code"} for part in Path(hook["command"]).parts)
            )
        ]
hooks["Stop"] = [
    entry for entry in hooks.get("Stop", [])
    if not isinstance(entry, dict) or entry.get("hooks")
]
if not hooks.get("Stop"):
    hooks.pop("Stop", None)

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

settings_path.write_text(json.dumps(settings, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY

printf 'Installed Lattice Claude Code hooks in %s\n' "$settings_path"
