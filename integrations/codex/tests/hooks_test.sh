#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
codex_hooks_dir="$(cd "$script_dir/../hooks" && pwd)"
claude_hooks_dir="$(cd "$script_dir/../../claude-code/hooks" && pwd)"
temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/lattice-hook-package-test.XXXXXX")"
daemon_pid=""
cleanup() {
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$temp_dir"
}
trap cleanup EXIT

fake_lattice="$temp_dir/lattice"

# The fixture is an adapter boundary: it records only the private command and
# a digest/length of stdin, proving wrappers do not parse paths or manufacture
# ordinary remember calls.
python3 - "$fake_lattice" <<'PY'
from pathlib import Path
import sys

Path(sys.argv[1]).write_text(r'''#!/usr/bin/env bash
set -euo pipefail
payload="$(python3 -c 'import sys; data=sys.stdin.buffer.read(); print(len(data))')"
printf '%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$payload" >>"$LATTICE_HOOK_COMMAND_LOG"
''', encoding="utf-8")
PY
chmod 755 "$fake_lattice"

run_hook() {
  local hooks_dir="$1"
  local hook="$2"
  local payload="$3"
  LATTICE_BIN="$fake_lattice" \
    LATTICE_HOOK_COMMAND_LOG="$temp_dir/commands" \
    "$hooks_dir/$hook" <<<"$payload"
}

exercise_package() {
  local integration="$1"
  local hooks_dir="$2"
  local before after output payload hook event
  local -a cases=(
    'session-start.sh:session-start'
    'post-tool-use.sh:post-tool-use'
    'stop.sh:stop'
  )

  for item in "${cases[@]}"; do
    hook="${item%%:*}"
    event="${item##*:}"
    payload="{\"session_id\":\"package-session\",\"transcript_path\":\"/tmp/must-not-open\",\"tool_name\":\"Edit\",\"tool_input\":{\"file_path\":\"src/example.rs\"}}"
    before="$(wc -l <"$temp_dir/commands" 2>/dev/null || printf 0)"
    output="$(run_hook "$hooks_dir" "$hook" "$payload")"
    [[ -z "$output" ]]
    after="$(wc -l <"$temp_dir/commands")"
    [[ "$after" -eq $((before + 1)) ]]
    record="$(tail -n 1 "$temp_dir/commands")"
    python3 - "$integration" "$event" "$record" <<'PY'
import sys

line = sys.argv[3].split("\t")
assert line[:3] == ["__hook-adapter", sys.argv[1], sys.argv[2]], line
assert int(line[3]) > 0, line
PY
  done

  before="$(wc -l <"$temp_dir/commands")"
  output="$(run_hook "$hooks_dir" user-prompt-submit.sh '{"prompt":"must not enter capture"}')"
  [[ -z "$output" ]]
  after="$(wc -l <"$temp_dir/commands")"
  [[ "$after" -eq "$before" ]]
}

: >"$temp_dir/commands"
exercise_package codex "$codex_hooks_dir"
exercise_package claude-code "$claude_hooks_dir"

# No package may retain the former unauthenticated outcome write or public
# query fan-out.
if rg -n 'remember|recall|context|impact|status' \
  "$codex_hooks_dir" "$claude_hooks_dir" >/dev/null; then
  printf 'hook package retained a public CLI call\n' >&2
  exit 1
fi

# Host shutdown must remain successful and silent when capture is unavailable.
unavailable="$temp_dir/unavailable"
python3 - "$unavailable" <<'PY'
from pathlib import Path
import sys
Path(sys.argv[1]).write_text('#!/usr/bin/env bash\nexit 23\n', encoding='utf-8')
PY
chmod 755 "$unavailable"
for hooks_dir in "$codex_hooks_dir" "$claude_hooks_dir"; do
  output="$(LATTICE_BIN="$unavailable" "$hooks_dir/stop.sh" <<<'{"session_id":"unavailable"}')"
  [[ -z "$output" ]]
done

if [[ -n "${LATTICE_HOOK_E2E_BIN:-}" ]]; then
  e2e_root="$temp_dir/e2e"
  e2e_repo="$e2e_root/repo"
  e2e_runtime="$e2e_root/runtime"
  e2e_state="$e2e_root/state"
  e2e_home="$e2e_root/home"
  mkdir -p "$e2e_repo/src" "$e2e_runtime" "$e2e_state" "$e2e_home"
  chmod 700 "$e2e_runtime" "$e2e_state" "$e2e_home"
  git -C "$e2e_repo" init -q
  git -C "$e2e_repo" config user.email lattice@example.test
  git -C "$e2e_repo" config user.name Lattice
  touch "$e2e_repo/src/example.rs"
  git -C "$e2e_repo" add src/example.rs
  git -C "$e2e_repo" commit -qm fixture
  e2e_address="${LATTICE_HOOK_E2E_ADDR:-127.0.0.1:48763}"
  HOME="$e2e_home" XDG_RUNTIME_DIR="$e2e_runtime" XDG_STATE_HOME="$e2e_state" \
    LATTICE_DAEMON_ADDR="$e2e_address" "$LATTICE_HOOK_E2E_BIN" --daemon \
    >"$e2e_root/daemon.out" 2>"$e2e_root/daemon.err" &
  daemon_pid=$!
  for _ in $(seq 1 50); do
    if find "$e2e_runtime/lattice" -maxdepth 1 -type f 2>/dev/null | head -n 1 | read -r; then
      break
    fi
    sleep 0.1
  done

  for client in codex claude-code; do
    if [[ "$client" == codex ]]; then
      hooks_dir="$codex_hooks_dir"
    else
      hooks_dir="$claude_hooks_dir"
    fi
    session="e2e-$client"
    common_env=(
      "HOME=$e2e_home"
      "XDG_RUNTIME_DIR=$e2e_runtime"
      "XDG_STATE_HOME=$e2e_state"
      "LATTICE_DAEMON_ADDR=$e2e_address"
      "LATTICE_BIN=$LATTICE_HOOK_E2E_BIN"
    )
    (
      cd "$e2e_repo"
      env "${common_env[@]}" "$hooks_dir/session-start.sh" \
        <<<"{\"session_id\":\"$session\",\"transcript_path\":\"/tmp/never-open-$client\"}"
      env "${common_env[@]}" "$hooks_dir/post-tool-use.sh" \
        <<<"{\"session_id\":\"$session\",\"transcript_path\":\"/tmp/never-open-$client\",\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"src/example.rs\",\"content\":\"sentinel-$client\"}}"
      env "${common_env[@]}" "$hooks_dir/stop.sh" \
        <<<"{\"session_id\":\"$session\",\"transcript_path\":\"/tmp/never-open-$client\",\"files\":[\"forged-$client\"]}"
    )
  done

  kill "$daemon_pid"
  wait "$daemon_pid" 2>/dev/null || true
  daemon_pid=""
  if rg -a -n 'never-open|forged-|sentinel-|e2e-codex|e2e-claude' \
    "$e2e_state" "$e2e_repo/.lattice" "$e2e_root/daemon.out" "$e2e_root/daemon.err"; then
    printf 'raw host envelope crossed the protected adapter boundary\n' >&2
    exit 1
  fi
  python3 - "$e2e_repo/.lattice/memories.db" <<'PY'
import sqlite3
import sys

connection = sqlite3.connect(sys.argv[1])
count = connection.execute("select count(*) from session_digest_deliveries").fetchone()[0]
assert count >= 2, count
PY
fi

printf 'Lattice protected Codex and Claude Code hook package tests passed\n'
