#!/usr/bin/env bash
set -u

lattice_hook_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
lattice_repo_root="$(cd "$lattice_hook_dir/../../.." && pwd)"
lattice_bin=""

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

lattice_hook_ready() {
  lattice_bin="$(lattice_find_bin)" || return 1
  LATTICE_SKIP_METRICS=1 "$lattice_bin" status --timeout "${LATTICE_HOOK_PROBE_TIMEOUT:-0.5}" >/dev/null 2>&1
}

lattice_hook_call() {
  LATTICE_CLIENT_NAME="${LATTICE_CLIENT_NAME:-${lattice_client_name:-codex}}" \
    LATTICE_CLIENT_CHANNEL="${LATTICE_CLIENT_CHANNEL:-hook}" \
    "$lattice_bin" "$@"
}

lattice_limit_chars() {
  local max_chars="$1"
  python3 -c '
import sys

limit = int(sys.argv[1])
text = sys.stdin.read()
sys.stdout.write(text[:limit])
' "$max_chars"
}

lattice_json_text() {
  python3 -c '
import json
import sys

raw = sys.stdin.read()
try:
    data = json.loads(raw)
except Exception:
    print(raw)
    raise SystemExit

content = data.get("content") if isinstance(data, dict) else None
if isinstance(content, list) and content:
    text = content[0].get("text") if isinstance(content[0], dict) else None
    if isinstance(text, str):
        try:
            nested = json.loads(text)
        except Exception:
            print(text)
        else:
            markdown = nested.get("markdown") if isinstance(nested, dict) else None
            print(markdown if isinstance(markdown, str) else json.dumps(nested, indent=2))
        raise SystemExit

print(json.dumps(data, indent=2))
'
}

lattice_extract_prompt() {
  python3 -c '
import json
import sys

raw = sys.stdin.read()
try:
    data = json.loads(raw)
except Exception:
    print(raw.strip())
    raise SystemExit

def first_text(value):
    if isinstance(value, str) and value.strip():
        return value.strip()
    if isinstance(value, dict):
        for key in ("prompt", "user_prompt", "message", "input", "text", "content"):
            found = first_text(value.get(key))
            if found:
                return found
        for item in value.values():
            found = first_text(item)
            if found:
                return found
    if isinstance(value, list):
        for item in value:
            found = first_text(item)
            if found:
                return found
    return None

prompt = first_text(data)
if prompt:
    print(prompt)
'
}

lattice_extract_files() {
  python3 -c '
import json
import re
import sys

raw = sys.stdin.read()
seen = []

def add(value):
    if not isinstance(value, str):
        return
    item = value.strip()
    if not item or item.startswith(("http:", "https:")):
        return
    if "\n" in item:
        return
    if item not in seen:
        seen.append(item)

def walk(value):
    if isinstance(value, dict):
        for key, item in value.items():
            lowered = key.lower()
            if lowered in ("file", "filename", "file_path", "path") or "file" in lowered:
                if isinstance(item, list):
                    for child in item:
                        add(child)
                else:
                    add(item)
            walk(item)
    elif isinstance(value, list):
        for item in value:
            walk(item)

try:
    data = json.loads(raw)
except Exception:
    data = None

if data is not None:
    walk(data)
else:
    for pattern in (
        r"^\+\+\+ b/(.+)$",
        r"^--- a/(.+)$",
        r"^\*\*\* Update File: (.+)$",
        r"^\*\*\* Add File: (.+)$",
        r"^\*\*\* Delete File: (.+)$",
    ):
        for match in re.finditer(pattern, raw, flags=re.MULTILINE):
            add(match.group(1))

for item in seen[:20]:
    print(item)
'
}
