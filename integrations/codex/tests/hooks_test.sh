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
    'user-prompt-submit.sh:user-prompt-submit'
    'pre-tool-use.sh:pre-tool-use'
    'post-tool-use.sh:post-tool-use'
    'stop.sh:stop'
    'session-end.sh:session-end'
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

}

: >"$temp_dir/commands"
exercise_package codex "$codex_hooks_dir"
exercise_package claude-code "$claude_hooks_dir"

# No package may retain the former unauthenticated outcome write or public
# query fan-out.
# grep, not rg: a missing rg made this condition false and the guard vacuous.
if grep -rnE 'remember|recall|context|impact|status' \
  "$codex_hooks_dir" "$claude_hooks_dir" >/dev/null; then
  printf 'hook package retained a public CLI call\n' >&2
  exit 1
fi

# Host session termination must remain successful and silent when capture is unavailable.
unavailable="$temp_dir/unavailable"
python3 - "$unavailable" <<'PY'
from pathlib import Path
import sys
Path(sys.argv[1]).write_text('#!/usr/bin/env bash\nexit 23\n', encoding='utf-8')
PY
chmod 755 "$unavailable"
for hooks_dir in "$codex_hooks_dir" "$claude_hooks_dir"; do
  for hook in stop.sh session-end.sh; do
    output="$(LATTICE_BIN="$unavailable" "$hooks_dir/$hook" <<<'{"session_id":"unavailable","last_assistant_message":"safe summary"}')"
    [[ -z "$output" ]]
  done
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
  (
    cd "$e2e_repo"
    HOME="$e2e_home" XDG_RUNTIME_DIR="$e2e_runtime" XDG_STATE_HOME="$e2e_state" \
      LATTICE_DAEMON_ADDR="$e2e_address" "$LATTICE_HOOK_E2E_BIN" --daemon
  ) >"$e2e_root/daemon.out" 2>"$e2e_root/daemon.err" &
  daemon_pid=$!
  for _ in $(seq 1 50); do
    if find "$e2e_runtime/lattice" -maxdepth 1 -type f 2>/dev/null | head -n 1 | read -r; then
      break
    fi
    sleep 0.1
  done

  # Bootstrap the authority-bound repository store, then seed one linked
  # Constraint directly in the fixture database. This keeps the package test
  # independent of public CLI recall/remember paths while exercising the real
  # adapter, daemon lease, router, presentation, and metric path end to end.
  (
    cd "$e2e_repo"
    HOME="$e2e_home" XDG_RUNTIME_DIR="$e2e_runtime" XDG_STATE_HOME="$e2e_state" \
      LATTICE_DAEMON_ADDR="$e2e_address" LATTICE_BIN="$LATTICE_HOOK_E2E_BIN" \
      "$codex_hooks_dir/session-start.sh" \
      <<<'{"session_id":"fixture-bootstrap","hook_event_id":"bootstrap-1"}' >/dev/null
  )
  python3 - "$e2e_repo/.lattice/memories.db" <<'PY'
import sqlite3
import sys

connection = sqlite3.connect(sys.argv[1])
repository_id = connection.execute(
    "select value from lattice_memory_store_metadata where key = 'repository_id'"
).fetchone()[0]
connection.execute(
    """insert into memories
       (id, session_id, content, memory_type, scope, confidence,
        linked_symbols, linked_files, workspace_id, memory_class,
        assertion_type, verification_status, created_at, last_accessed)
       values (?, ?, ?, ?, ?, ?, '[]', ?, ?, ?, ?, ?, 1, 1)""",
    (
        "constraint-fixture",
        "fixture-prior-session",
        "Fixture writes must remain atomic to protect recovery.",
        "observation",
        "repo",
        0.99,
        '["src/example.rs"]',
        repository_id,
        "constraint",
        "constraint",
        "unverified",
    ),
)
connection.commit()
PY
  printf '%s\n' 'dirty working-set fixture' >"$e2e_repo/src/example.rs"
  failure_check="$e2e_repo/fixture-failure-check"
  recovery_check="$e2e_repo/fixture-recovery-check"
  printf '%s\n' '#!/bin/sh' '[ "$FIXTURE_CHECK" = true ] || exit 99' 'printf "%s\\n" "private-check-output"' 'exit 17' >"$failure_check"
  printf '%s\n' '#!/bin/sh' '[ "$FIXTURE_CHECK" = true ] || exit 99' 'exit 0' >"$recovery_check"
  chmod 755 "$failure_check" "$recovery_check"
  python3 - "$e2e_repo/.lattice/verification-checks.json" "$failure_check" "$recovery_check" <<'PY'
from pathlib import Path
import json
import sys

fingerprint = "sha256:" + ("4" * 64)
Path(sys.argv[1]).write_text(json.dumps({
    "schema_version": 2,
    "checks": [
        {
            "id": "fixture-failure",
            "label": "fixture declared failure",
            "argv": [sys.argv[2], "private-check-argument"],
            "timeout_ms": 120000,
            "env": {"FIXTURE_CHECK": "true"},
            "error": {"category": "test", "fingerprint": fingerprint}
        },
        {
            "id": "fixture-recovery",
            "label": "fixture declared recovery",
            "argv": [sys.argv[3]],
            "timeout_ms": 120000,
            "env": {"FIXTURE_CHECK": "true"},
            "error": {"category": "test", "fingerprint": fingerprint}
        }
    ]
}), encoding="utf-8")
PY

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
        <<<"{\"session_id\":\"$session\",\"transcript_path\":\"/tmp/never-open-$client\"}" \
        >"$e2e_root/$client-session-start.out"
      env "${common_env[@]}" "$hooks_dir/user-prompt-submit.sh" \
        <<<"{\"session_id\":\"$session\",\"hook_event_id\":\"$client-prompt-1\",\"prompt\":\"describe the fixture memory\",\"transcript_path\":\"/tmp/never-open-$client\"}" \
        >"$e2e_root/$client-user-prompt.out"
      env "${common_env[@]}" "$hooks_dir/post-tool-use.sh" \
        <<<"{\"session_id\":\"$session\",\"transcript_path\":\"/tmp/never-open-$client\",\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"src/example.rs\",\"content\":\"sentinel-$client\"}}" \
        >"$e2e_root/$client-post-tool.out"
      set +e
      env "${common_env[@]}" "$LATTICE_HOOK_E2E_BIN" \
        __hook-verify "$client" "$session" fixture-failure
      verification_status=$?
      set -e
      [[ "$verification_status" -eq 17 ]]
      env "${common_env[@]}" "$LATTICE_HOOK_E2E_BIN" \
        __hook-verify "$client" "$session" fixture-recovery
      env "${common_env[@]}" "$hooks_dir/stop.sh" \
        <<<"{\"session_id\":\"$session\",\"last_assistant_message\":\"bounded turn summary for $client\",\"transcript_path\":\"/tmp/never-open-$client\",\"cwd\":\"/forged\"}"
      env "${common_env[@]}" "$hooks_dir/post-tool-use.sh" \
        <<<"{\"session_id\":\"$session\",\"transcript_path\":\"/tmp/never-open-$client\",\"tool_name\":\"Edit\",\"tool_input\":{\"file_path\":\"src/example.rs\",\"content\":\"second-sentinel-$client\"}}" \
        >"$e2e_root/$client-post-tool-second.out"
      env "${common_env[@]}" "$hooks_dir/session-end.sh" \
        <<<"{\"session_id\":\"$session\",\"transcript_path\":\"/tmp/never-open-$client\",\"reason\":\"private-$client\",\"cwd\":\"/forged\",\"final_summary\":\"private summary\"}"
    )
    rg -q 'constraint-fixture' "$e2e_root/$client-session-start.out"
    rg -q 'constraint-fixture' "$e2e_root/$client-user-prompt.out"
    rg -q 'constraint-fixture' "$e2e_root/$client-post-tool.out"
  done

  kill "$daemon_pid"
  wait "$daemon_pid" 2>/dev/null || true
  daemon_pid=""

  # A first SessionStart must expose daemon unavailability even when this
  # host session has no prior binding or local state. The private marker is
  # atomic, so a repeated hook delivery for the same session remains silent.
  fresh_notice_state="$e2e_root/fresh-notice-state"
  mkdir -p "$fresh_notice_state"
  chmod 700 "$fresh_notice_state"
  fresh_notice_session="fresh-daemon-down-private-session"
  fresh_notice_env=(
    "HOME=$e2e_home"
    "XDG_RUNTIME_DIR=$e2e_runtime"
    "XDG_STATE_HOME=$fresh_notice_state"
    "LATTICE_DAEMON_ADDR=$e2e_address"
    "LATTICE_BIN=$LATTICE_HOOK_E2E_BIN"
  )
  first_notice="$({
    cd "$e2e_repo"
    env "${fresh_notice_env[@]}" "$codex_hooks_dir/session-start.sh" \
      <<<"{\"session_id\":\"$fresh_notice_session\"}"
  })"
  second_notice="$({
    cd "$e2e_repo"
    env "${fresh_notice_env[@]}" "$codex_hooks_dir/session-start.sh" \
      <<<"{\"session_id\":\"$fresh_notice_session\"}"
  })"
  [[ "$first_notice" == "lattice: daemon unreachable — run 'lattice doctor'" ]]
  [[ -z "$second_notice" ]]
  notice_marker_count="$(
    find "$fresh_notice_state/lattice/hook-notices" -maxdepth 1 -type f | wc -l | tr -d ' '
  )"
  notice_marker="$(
    find "$fresh_notice_state/lattice/hook-notices" -maxdepth 1 -type f -print
  )"
  [[ "$notice_marker_count" -eq 1 ]]
  [[ "$(basename "$notice_marker")" =~ ^session-start-[0-9a-f]{32}$ ]]
  [[ "$notice_marker" != *"$fresh_notice_session"* ]]

  # Malformed envelopes remain silent and do not consume a notice claim.
  invalid_notice_state="$e2e_root/invalid-notice-state"
  mkdir -p "$invalid_notice_state"
  chmod 700 "$invalid_notice_state"
  invalid_notice="$({
    cd "$e2e_repo"
    HOME="$e2e_home" XDG_RUNTIME_DIR="$e2e_runtime" \
      XDG_STATE_HOME="$invalid_notice_state" LATTICE_DAEMON_ADDR="$e2e_address" \
      LATTICE_BIN="$LATTICE_HOOK_E2E_BIN" \
      "$codex_hooks_dir/session-start.sh" <<<'{"missing_session_id":true}'
  })"
  [[ -z "$invalid_notice" ]]
  [[ ! -e "$invalid_notice_state/lattice/hook-notices" ]]

  if rg -a -n 'never-open|forged-|sentinel-|e2e-codex|e2e-claude' \
    "$e2e_state" "$e2e_repo/.lattice" "$e2e_root/daemon.out" "$e2e_root/daemon.err"; then
    printf 'raw host envelope crossed the protected adapter boundary\n' >&2
    exit 1
  fi
  if rg -a -n "$fresh_notice_session" "$fresh_notice_state"; then
    printf 'raw host session identity leaked into notice state\n' >&2
    exit 1
  fi
  python3 - "$e2e_repo/.lattice/memories.db" <<'PY'
import sqlite3
import sys

connection = sqlite3.connect(sys.argv[1])
count = connection.execute("select count(*) from session_digest_deliveries").fetchone()[0]
assert count >= 2, count
classes = dict(connection.execute(
    "select memory_class, count(*) from memories group by memory_class"
).fetchall())
assert classes.get("workflow_outcome", 0) >= 4, classes
assert classes.get("failure_pattern", 0) >= 2, classes
PY
  if rg -a -n 'private-check-output|private-check-argument|fixture-failure|fixture-recovery' \
    "$e2e_repo/.lattice/memories.db" "$e2e_state" \
    "$e2e_root/daemon.out" "$e2e_root/daemon.err"; then
    printf 'verification execution authority crossed the typed capture boundary\n' >&2
    exit 1
  fi
  python3 - "$e2e_repo/.lattice/adoption_metrics.jsonl" <<'PY'
import json
import sys

events = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8") if line.strip()]
injections = [event for event in events if event.get("kind") == "memory_injection"]
assert len(injections) >= 6, len(injections)
assert all(event.get("injection_id", "").startswith("hinj_") for event in injections)
PY
fi

printf 'Lattice protected Codex and Claude Code hook package tests passed\n'
