#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
codex_hooks_dir="$(cd "$script_dir/../hooks" && pwd)"
claude_hooks_dir="$(cd "$script_dir/../../claude-code/hooks" && pwd)"
temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/lattice-codex-hooks-test.XXXXXX")"
trap 'rm -rf -- "$temp_dir"' EXIT

fake_lattice="$temp_dir/lattice"
cat >"$fake_lattice" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

python3 - "$LATTICE_HOOK_COMMAND_LOG" "$@" <<'PY'
import json
import os
import sys

with open(sys.argv[1], "a", encoding="utf-8") as log:
    print(json.dumps({
        "argv": sys.argv[2:],
        "client_name": os.environ.get("LATTICE_CLIENT_NAME"),
        "client_channel": os.environ.get("LATTICE_CLIENT_CHANNEL"),
        "skip_metrics": os.environ.get("LATTICE_SKIP_METRICS"),
    }, separators=(",", ":")), file=log)
PY
case "$1" in
  status)
    if [[ "${LATTICE_HOOK_TEST_STALE:-0}" == "1" && "$*" == *"--files"* ]]; then
      printf '%s\n' '{"count":1,"memories":[{"is_stale":true,"verification_status":"stale","linked_files":["src/example.rs"]}]}'
    else
      printf '%s\n' 'ready'
    fi
    ;;
  recall) printf '%s\n' '{"content":[{"text":"{\"markdown\":\"remembered task\"}"}]}' ;;
  context)
    if [[ "$2" == "repo rules and operator workflow" ]]; then
      printf '%s\n' 'repository rules'
    else
      printf '%s\n' 'prompt context with enough useful detail to exceed the hook relevance-size guard for this regression test.'
    fi
    ;;
  impact)
    if [[ "${LATTICE_HOOK_TEST_HOTSPOT:-0}" == "1" ]]; then
      printf '%s\n' '- Hotspot warning: `src/example.rs` changed in 7/10 sampled commits; head `abc123`.'
      printf '%s\n' '- Hotspot warning: `src/other.rs` changed in 6/10 sampled commits; head `def456`.'
    fi
    printf '%s\n' 'affected dependent one' 'affected dependent two' 'affected dependent three'
    ;;
  remember) : ;;
  *) exit 1 ;;
esac
EOF
chmod +x "$fake_lattice"

run_hook() {
  local hooks_dir="$1"
  local hook="$2"
  local payload="$3"
  LATTICE_BIN="$fake_lattice" \
    LATTICE_HOOK_COMMAND_LOG="$temp_dir/commands" \
    "$hooks_dir/$hook" <<<"$payload"
}

assert_command() {
  local expected="$1"
  python3 - "$temp_dir/commands" "$expected" <<'PY'
import json
import sys

records = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8")]
expected = json.loads(sys.argv[2])
assert expected in records, (expected, records)
PY
}

assert_startup_working_set_delivery() {
  python3 - "$temp_dir/commands" <<'PY'
import json
import sys

records = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8")]

recalls = [record["argv"] for record in records if record["argv"][:1] == ["recall"]]
assert recalls, records
assert all("working set: branch " in argv[1] for argv in recalls), recalls

working_sets = [record["argv"] for record in records if record["argv"][:1] == ["context"] and "--mode" in record["argv"] and record["argv"][record["argv"].index("--mode") + 1] == "working_set"]
assert len(working_sets) == 2, working_sets
assert all("working set: branch " in argv[1] for argv in working_sets), working_sets
assert all("--timeout" in argv for argv in working_sets), working_sets
PY
}

assert_claude_envelope() {
  local event="$1"
  local output="$2"
  CLAUDE_HOOK_EVENT="$event" CLAUDE_HOOK_OUTPUT="$output" python3 - <<'PY'
import json
import os

payload = json.loads(os.environ["CLAUDE_HOOK_OUTPUT"])
result = payload["hookSpecificOutput"]
assert result["hookEventName"] == os.environ["CLAUDE_HOOK_EVENT"]
assert result["additionalContext"].strip()
PY
}

assert_git_history_warning() {
  local client_name="$1"
  local hooks_dir="$2"
  local output

  # The warning is independently useful, so it must survive the dependent
  # threshold. The fake impact output deliberately has only three dependents.
  output="$(LATTICE_BIN="$fake_lattice" LATTICE_HOOK_COMMAND_LOG="$temp_dir/commands" LATTICE_HOOK_TEST_HOTSPOT=1 LATTICE_HOOK_MIN_DEPENDENTS=4 LATTICE_HOOK_MAX_HOTSPOT_WARNINGS=1 "$hooks_dir/post-tool-use.sh" <<<'{"file_path":"src/example.rs"}')"
  [[ "$output" == *'## Lattice Git History Warning'* ]]
  [[ "$(grep -o '## Lattice Git History Warning' <<<"$output" | wc -l | tr -d ' ')" == "1" ]]
  [[ "$output" == *'`src/example.rs` changed in 7/10 sampled commits'* ]]
  [[ "$output" != *'`src/other.rs` changed in 6/10 sampled commits'* ]]
  [[ "$output" != *'## Lattice Edit Impact'* ]]

  if [[ "$client_name" == "claude-code" ]]; then
    assert_claude_envelope "PostToolUse" "$output"
  fi
}

exercise_package() {
  local client_name="$1"
  local hooks_dir="$2"
  local session_output prompt_output impact_output

  session_output="$(run_hook "$hooks_dir" session-start.sh '')"
  [[ "$session_output" == *'## Lattice Session Context'* ]]
  [[ "$session_output" == *'remembered task'* ]]
  [[ "$session_output" == *'repository rules'* ]]
  [[ "$session_output" == *'### Working Set'* ]]

  prompt_output="$(run_hook "$hooks_dir" user-prompt-submit.sh '{"prompt":"explain hook delivery"}')"
  [[ "$prompt_output" == *'## Lattice Prompt Context'* ]]
  [[ "$prompt_output" == *'prompt context with enough useful detail'* ]]

  impact_output="$(run_hook "$hooks_dir" post-tool-use.sh '{"file_path":"src/example.rs"}')"
  [[ "$impact_output" == *'## Lattice Edit Impact'* ]]
  [[ "$impact_output" == *'affected dependent three'* ]]

  run_hook "$hooks_dir" stop.sh '{"files":["src/example.rs","README.md"]}' >/dev/null

  if [[ "$client_name" == "claude-code" ]]; then
    assert_claude_envelope "SessionStart" "$session_output"
    assert_claude_envelope "UserPromptSubmit" "$prompt_output"
    assert_claude_envelope "PostToolUse" "$impact_output"
  fi

  stale_output="$(LATTICE_BIN="$fake_lattice" LATTICE_HOOK_COMMAND_LOG="$temp_dir/commands" LATTICE_HOOK_TEST_STALE=1 "$hooks_dir/post-tool-use.sh" <<<'{"file_path":"src/example.rs"}')"
  [[ "$stale_output" == *'## Lattice Memory Warning'* ]]
  [[ "$stale_output" == *'verification_status'* ]]
  if [[ "$client_name" == "claude-code" ]]; then
    assert_claude_envelope "PostToolUse" "$stale_output"
  fi

  assert_git_history_warning "$client_name" "$hooks_dir"
}

exercise_package codex "$codex_hooks_dir"
exercise_package claude-code "$claude_hooks_dir"
assert_startup_working_set_delivery

# The probe and real calls must put the verb first. This catches the old Claude
# invocation that formed `lattice --workspace <path> status`, which the CLI
# interpreted as daemon startup rather than a status query.
assert_command '{"argv":["status","--timeout","0.5"],"client_name":"claude-code","client_channel":"hook","skip_metrics":"1"}'
assert_command '{"argv":["context","explain hook delivery","--mode","auto","--min-relevance","0.25","--timeout","3.5"],"client_name":"claude-code","client_channel":"hook","skip_metrics":null}'
assert_command '{"argv":["context","explain hook delivery","--mode","auto","--min-relevance","0.25","--timeout","3.5"],"client_name":"codex","client_channel":"hook","skip_metrics":null}'
assert_command '{"argv":["remember","Session edited files: src/example.rs, README.md","--kind","outcome","--timeout","2.5"],"client_name":"claude-code","client_channel":"hook","skip_metrics":null}'
assert_command '{"argv":["status","--files","src/example.rs","--timeout","1.5"],"client_name":"codex","client_channel":"hook","skip_metrics":null}'
assert_command '{"argv":["status","--files","src/example.rs","--timeout","1.5"],"client_name":"claude-code","client_channel":"hook","skip_metrics":null}'

unavailable_lattice="$temp_dir/unavailable-lattice"
printf '#!/usr/bin/env bash\nexit 1\n' >"$unavailable_lattice"
chmod +x "$unavailable_lattice"
unavailable_output="$(LATTICE_BIN="$unavailable_lattice" "$codex_hooks_dir/user-prompt-submit.sh" <<<'{"prompt":"must not fail"}')"
[[ -z "$unavailable_output" ]]

printf 'Lattice Codex and Claude Code hook tests passed\n'
