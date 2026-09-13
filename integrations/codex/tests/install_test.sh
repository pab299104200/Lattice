#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
installer="$(cd "$script_dir/.." && pwd)/install.sh"
temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/lattice-codex-install-test.XXXXXX")"
trap 'rm -rf -- "$temp_dir"' EXIT

fake_bin="$temp_dir/release/lattice"
mkdir -p "$(dirname "$fake_bin")" "$temp_dir/project/.codex" "$temp_dir/bin"
printf '#!/usr/bin/env bash\nexit 0\n' >"$fake_bin"
chmod +x "$fake_bin"

hooks_path="$temp_dir/project/.codex/hooks.json"
printf '%s\n' '{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"/old/integrations/codex/hooks/user-prompt-submit.sh","timeout":2}]}],"Custom":[{"hooks":[{"type":"command","command":"custom-hook"}]}]}}' >"$hooks_path"

run_installer() {
  HOME="$temp_dir/home" \
    LATTICE_INSTALL_BIN_DIR="$temp_dir/bin" \
    LATTICE_RELEASE_BIN="$fake_bin" \
    "$installer" "$hooks_path"
}

run_installer
run_installer

HOOKS_PATH="$hooks_path" python3 - <<'PY'
import json
import os
from pathlib import Path

settings = json.loads(Path(os.environ["HOOKS_PATH"]).read_text(encoding="utf-8"))
expected = {"SessionStart": 4, "UserPromptSubmit": 5, "PostToolUse": 5, "Stop": 4}
for event, timeout in expected.items():
    script = {
        "SessionStart": "session-start.sh",
        "UserPromptSubmit": "user-prompt-submit.sh",
        "PostToolUse": "post-tool-use.sh",
        "Stop": "stop.sh",
    }[event]
    matching = [
        hook
        for entry in settings["hooks"][event]
        for hook in entry.get("hooks", [])
        if Path(hook.get("command", "")).name == script
    ]
    assert len(matching) == 1, (event, matching)
    assert matching[0]["timeout"] == timeout, (event, matching[0])
assert settings["hooks"]["Custom"][0]["hooks"][0]["command"] == "custom-hook"
PY

[[ -L "$temp_dir/bin/lattice" ]]
[[ "$(readlink "$temp_dir/bin/lattice")" == "$fake_bin" ]]

rm "$temp_dir/bin/lattice"
printf 'existing binary\n' >"$temp_dir/bin/lattice"
if run_installer >/dev/null 2>&1; then
  printf 'installer overwrote a non-symlink CLI target\n' >&2
  exit 1
fi

printf 'Lattice Codex installer tests passed\n'
