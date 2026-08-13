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

lattice_hook_call_raw() {
  LATTICE_CLIENT_NAME="${LATTICE_CLIENT_NAME:-${lattice_client_name:-codex}}" \
    LATTICE_CLIENT_CHANNEL="${LATTICE_CLIENT_CHANNEL:-hook}" \
    "$lattice_bin" "$@"
}

# Collect a small, local startup anchor without reading repository content. The
# public CLI can use these paths to build a memory-aware working-set capsule.
lattice_collect_session_working_set() {
  LATTICE_SESSION_WORKING_SET_QUERY=""
  LATTICE_SESSION_WORKING_SET_FILES=()

  git rev-parse --is-inside-work-tree >/dev/null 2>&1 || return 1

  local branch status path files_summary=""
  branch="$(git branch --show-current 2>/dev/null || true)"
  [[ -n "$branch" ]] || branch="detached HEAD"

  while IFS= read -r status; do
    path="${status:3}"
    # Porcelain v1 represents a rename as "old -> new". The destination is
    # the relevant current working-set file.
    [[ "$path" == *" -> "* ]] && path="${path##* -> }"
    [[ -n "${path//[[:space:]]/}" ]] || continue
    LATTICE_SESSION_WORKING_SET_FILES+=("$path")
    [[ ${#LATTICE_SESSION_WORKING_SET_FILES[@]} -ge "${LATTICE_HOOK_WORKING_SET_FILE_LIMIT:-8}" ]] && break
  done < <(git status --porcelain=v1 --untracked-files=normal 2>/dev/null)

  if [[ ${#LATTICE_SESSION_WORKING_SET_FILES[@]} -gt 0 ]]; then
    files_summary="$(IFS=', '; printf '%s' "${LATTICE_SESSION_WORKING_SET_FILES[*]}")"
  else
    files_summary="no dirty files"
  fi
  LATTICE_SESSION_WORKING_SET_QUERY="session startup working set: branch $branch; $files_summary"
}

lattice_hook_call() {
  # SessionStart previously recalled only the literal task "session start".
  # Replacing that query with bounded local anchors lets the existing public
  # recall endpoint rank memories against the actual branch and changed files.
  if [[ "${1:-}" == "recall" && "${2:-}" == "session start" ]]; then
    lattice_collect_session_working_set || {
      lattice_hook_call_raw "$@"
      return
    }
    shift 2
    lattice_hook_call_raw recall "$LATTICE_SESSION_WORKING_SET_QUERY" "$@"
    return
  fi

  # Keep the rules response, then add the CLI's memory-aware working-set
  # capsule. This exact call shape is used only by the SessionStart hooks.
  if [[ "${1:-}" == "context" && "${2:-}" == "repo rules and operator workflow" ]]; then
    lattice_hook_call_raw "$@"
    local primary_status=$?
    lattice_collect_session_working_set || return "$primary_status"

    local -a working_set_call=(context "$LATTICE_SESSION_WORKING_SET_QUERY" --mode working_set)
    local file
    for file in "${LATTICE_SESSION_WORKING_SET_FILES[@]}"; do
      working_set_call+=(--files "$file")
    done
    working_set_call+=(--timeout "${LATTICE_HOOK_WORKING_SET_TIMEOUT:-3.5}")
    printf '\n### Working Set\n'
    lattice_hook_call_raw "${working_set_call[@]}" || true
    return "$primary_status"
  fi

  if [[ "${1:-}" == "impact" && -n "${2:-}" ]]; then
    local edited_file="$2" impact memory_warning
    impact="$(lattice_hook_call_raw "$@")"
    local primary_status=$?

    # `status --files` is deliberately queried with the exact file selected by
    # PostToolUse. Keep this advisory bounded and silent for unavailable,
    # empty, or ordinary index/status responses.
    memory_warning="$(
      lattice_hook_call_raw status --files "$edited_file" --timeout "${LATTICE_HOOK_MEMORY_TIMEOUT:-1.5}" 2>/dev/null \
        | python3 -c '
import sys

raw = sys.stdin.read()
signals = ("stale", "unverified", "invalidated", "expired", "contradicted", "superseded")
lines = [line.strip() for line in raw.splitlines() if line.strip()]
matches = []
for line in lines:
    lowered = line.lower()
    if "is_stale" in lowered and "false" in lowered:
        continue
    if "verification_status" in lowered and not any(signal in lowered for signal in signals):
        continue
    if any(signal in lowered for signal in signals):
        matches.append(line)
if not matches:
    raise SystemExit

text = "\n".join(matches[:5])[:1200].strip()
if text:
    print(text)
'
    )"

    [[ -n "${impact//[[:space:]]/}" ]] && printf '%s\n' "$impact"
    if [[ -n "${memory_warning//[[:space:]]/}" ]]; then
      printf '## Lattice Memory Warning\n\nThe edited file has file-linked memory that may need verification:\n%s\n' "$memory_warning"
    fi
    return "$primary_status"
  fi

  lattice_hook_call_raw "$@"
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
