#!/usr/bin/env bash
# End-to-end acceptance check for hook enforcement (docs/hook-enforcement.md).
#
# Runs the shipped Claude Code hook scripts against a PRIVATE daemon on its own
# port, under a sandbox HOME, in a scratch repository. It never contacts the
# operator's daemon and never writes outside its temporary directory.
#
#   integrations/claude-code/tests/enforcement_e2e.sh [path/to/lattice]
#
# Needs: bash, git, python3, sqlite3, nc.
set -uo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../../.." && pwd)"
BIN="${1:-$repo_root/daemon/target/release/lattice}"
HOOKS="$repo_root/integrations/claude-code/hooks"
[ -x "$BIN" ] || { printf 'lattice binary not found or not executable: %s\n' "$BIN" >&2; exit 2; }
S="$(mktemp -d "${TMPDIR:-/tmp}/lattice-enforcement-e2e.XXXXXX")"; S="$(cd "$S" && pwd -P)"
E="$S/e2e"; mkdir -p "$E/home" "$E/repo/src" "$E/repo/docs"
PORT="${LATTICE_E2E_PORT:-47991}"
export HOME="$E/home" XDG_STATE_HOME="$E/home/.local/state" LATTICE_DAEMON_ADDR="127.0.0.1:$PORT"
export LATTICE_LIFECYCLE_LOG_DIR="$E/home/logs" LATTICE_BIN="$BIN" GIT_CONFIG_GLOBAL=/dev/null
unset XDG_RUNTIME_DIR
DPID=""
cleanup() { [ -n "$DPID" ] && kill "$DPID" 2>/dev/null; [ -n "$DPID" ] && wait "$DPID" 2>/dev/null; rm -rf -- "$S"; }
trap cleanup EXIT
R="$E/repo"; R="$(cd "$R" && pwd -P)"
pass=0; fail=0
ok()   { pass=$((pass+1)); printf 'PASS  %s\n' "$1"; }
bad()  { fail=$((fail+1)); printf 'FAIL  %s\n      got: %s\n' "$1" "${2:-}"; }
check(){ if eval "$2"; then ok "$1"; else bad "$1" "${3:-}"; fi; }
O="$S/e2e-out.txt"
has()  { grep -qF -- "$1" "$O"; }
save() { printf '%s' "$1" > "$O"; }
denied()   { grep -qF '"permissionDecision":"deny"' "$O"; }
event_is() { grep -qF "\"hookEventName\":\"$1\"" "$O"; }

cd "$R"; git init -q; git config user.email t@example.test; git config user.name T
printf 'pub fn one() {}\n' > src/lib.rs; printf '# guide\n' > docs/guide.md; printf '.lattice/\n.claude/\n.codex/\n.mcp.json\nAGENTS.md\nCLAUDE.md\n' > .gitignore
git add . && git commit -qm fixture

out="$("$BIN" install claude-code --workspace "$R" --enforce --verify 2>&1)"; rc=$?
check "install --enforce --verify succeeds" "[ $rc -eq 0 ]" "$out"
check "policy recorded in workspace" "grep -q '\"enabled\": true' '$R/.lattice/workspace-policy.json'"
check "gate registered" "grep -q pre-tool-use.sh '$R/.claude/settings.json'"
check "PostToolUse matches Bash" "grep -q 'NotebookEdit|Bash|PowerShell' '$R/.claude/settings.json'"

if nc -z 127.0.0.1 "$PORT" 2>/dev/null; then
  printf 'port %s is already in use; set LATTICE_E2E_PORT\n' "$PORT" >&2; exit 2
fi
"$BIN" --daemon > "$E/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 50); do nc -z 127.0.0.1 "$PORT" 2>/dev/null && break; sleep 0.2; done
check "private daemon listening" "nc -z 127.0.0.1 $PORT 2>/dev/null"

out="$("$BIN" install --workspace "$R" --verify 2>&1)"; rc=$?
check "default target keeps the recorded mode and verifies MCP plus both clients" "[ $rc -eq 0 ]" "$out"
check "both clients carry the gate" "grep -q pre-tool-use.sh '$R/.claude/settings.json' && grep -q pre-tool-use.sh '$R/.codex/hooks.json'"

hook() { ( cd "$R" && printf '%s' "$2" | "$HOOKS/$1" ); }
edit() { printf '{"session_id":"%s","hook_event_name":"PreToolUse","tool_name":"Edit","tool_use_id":"t%s","tool_input":{"file_path":"%s","old_string":"SENTINEL-OLD","new_string":"SENTINEL-NEW"}}' "$1" "$RANDOM" "$2"; }
SID=e2e-session-1

out="$(hook session-start.sh "{\"session_id\":\"$SID\",\"source\":\"startup\"}")"; save "$out"
check "SessionStart runs" "true"

t0=$(python3 -c 'import time;print(time.time())')
out="$(hook pre-tool-use.sh "$(edit $SID "$R/src/lib.rs")")"; save "$out"
t1=$(python3 -c 'import time;print(time.time())')
check "product edit DENIED without a plan" "denied" "$out"
check "deny reason names prepare_change and policy" "has 'lattice prepare_change' && has 'workspace-policy.json'" "$out"
check "deny uses hookEventName PreToolUse" "event_is PreToolUse"
printf 'INFO  gate latency: %s ms\n' "$(python3 -c "print(round(($t1-$t0)*1000))")"

for exempt in "$R/docs/guide.md" "$R/README.md" "$R/.claude/settings.json" "$R/tmp/x.py" "/etc/hosts"; do
  out="$(hook pre-tool-use.sh "$(edit $SID "$exempt")")"; save "$out"
  check "exempt path allowed silently: ${exempt#$R/}" "[ ! -s "$O" ]" "$out"
done

out="$("$BIN" prepare_change "change one in lib" --workspace "$R" --timeout 20 2>&1)"; rc=$?
check "CLI prepare_change served by private daemon" "[ $rc -eq 0 ]" "$(printf '%s' "$out" | head -3)"

for n in 1 2 3; do
  out="$(hook pre-tool-use.sh "$(edit $SID "$R/src/lib.rs")")"; save "$out"
  check "product edit ALLOWED after plan (#$n, no nag)" "! printf '%s' '$out' | grep -q permissionDecision" "$out"
done

out="$(hook pre-tool-use.sh "$(edit other-session "$R/src/lib.rs")")"; save "$out"
printf 'INFO  other session same checkout: %s\n' "$(printf '%s' "$out" | grep -o '"permissionDecision":"[a-z]*"' || echo allowed)"

BASH_PAYLOAD="{\"session_id\":\"$SID\",\"tool_name\":\"Bash\",\"tool_use_id\":\"b1\",\"tool_input\":{\"command\":\"SENTINEL-COMMAND sed -i s/a/b/ src/lib.rs\"},\"tool_response\":{\"stdout\":\"SENTINEL-OUTPUT\"}}"
out="$(hook post-tool-use.sh "$BASH_PAYLOAD")"; save "$out"
check "first shell call records a silent baseline" "[ ! -s "$O" ]" "$out"
printf 'pub fn one() { 1; }\n' > src/lib.rs; printf 'pub fn two() {}\n' > src/new.rs; printf '# more\n' >> docs/guide.md
out="$(hook post-tool-use.sh "${BASH_PAYLOAD/b1/b2}")"; save "$out"
check "shell edit reported with product paths" "has 'shell changed 2 product file' && has 'src/lib.rs' && has 'src/new.rs'" "$out"
check "docs change not counted as product" "! has 'docs/guide.md'"
check "shell note is PostToolUse additionalContext" "event_is PostToolUse"
check "plan current, so no without-plan notice" "! has 'no current Lattice plan'"
out="$(hook post-tool-use.sh "${BASH_PAYLOAD/b1/b3}")"; save "$out"
check "unchanged tree reports nothing" "[ ! -s "$O" ]" "$out"

# Shell edit with NO plan, in a fresh session.
hook post-tool-use.sh "${BASH_PAYLOAD/$SID/noplan}" >/dev/null
hook session-start.sh '{"session_id":"noplan","source":"compact"}' >/dev/null
printf 'pub fn one() { 2; }\n' > src/lib.rs
out="$(hook post-tool-use.sh "${BASH_PAYLOAD/$SID/noplan}")"; save "$out"
check "shell edit without plan says so" "has 'no current Lattice plan on record'" "$out"
printf 'pub fn one() { 3; }\n' > src/lib.rs
out="$(hook post-tool-use.sh "${BASH_PAYLOAD/$SID/noplan}")"; save "$out"
check "without-plan notice is once per session" "! has 'no current Lattice plan on record'" "$out"
out="$(hook pre-tool-use.sh "$(edit noplan "$R/src/lib.rs")")"; save "$out"
check "plan made before a compact does not cover the new context" "denied" "$out"

STOP='{"session_id":"'$SID'","stop_hook_active":false,"last_assistant_message":"Fixed the parser.\n\n- ran `cargo test SENTINEL-CODE`\n- updated docs\n\nDone."}'
out="$(hook stop.sh "$STOP")"; save "$out"
check "Stop reminds once about stale-docs and remember" "has 'stale-docs' && has 'lattice remember'" "$out"
check "Stop never blocks" "! has decision\\\":"
out="$(hook stop.sh "$STOP")"; save "$out"
check "Stop reminder not repeated" "[ ! -s "$O" ]" "$out"
out="$(hook stop.sh "${STOP/false/true}")"; save "$out"
check "Stop silent while stop_hook_active" "[ ! -s "$O" ]" "$out"

cp "$R/.lattice/hook-capture.db" "$S/e2e-journal.db"; for x in wal shm; do [ -f "$R/.lattice/hook-capture.db-$x" ] && cp "$R/.lattice/hook-capture.db-$x" "$S/e2e-journal.db-$x"; done
sqlite3 "$S/e2e-journal.db" "select normalized_json from hook_capture_journal" > "$O"
check "multi-line summary captured with code redacted" "has 'Fixed the parser. - ran [code] - updated docs Done.'" "$(tail -3 "$O")"
check "edited_path facts captured for shell edits" "has '\"path\":\"src/new.rs\"'"

leak="$(grep -rl 'SENTINEL' "$E/home" "$R/.lattice" 2>/dev/null)"
check "no command, output, edit content or code text retained anywhere" "[ -z '$leak' ]" "$leak"

kill "$DPID"; wait "$DPID" 2>/dev/null; DPID=""
out="$(hook pre-tool-use.sh "$(edit down-session "$R/src/lib.rs")")"; save "$out"
check "daemon down: edit allowed" "! printf '%s' '$out' | grep -q permissionDecision" "$out"
check "daemon down: one loud notice" "has 'daemon is unreachable' && has 'state in your report'" "$out"
out="$(hook pre-tool-use.sh "$(edit down-session "$R/src/lib.rs")")"; save "$out"
check "daemon down: notice de-duplicated" "[ ! -s "$O" ]" "$out"

out="$("$BIN" install claude-code --workspace "$R" --no-enforce --verify 2>&1)"; rc=$?
check "install --no-enforce --verify succeeds" "[ $rc -eq 0 ]" "$out"
check "gate removed" "! grep -q pre-tool-use.sh '$R/.claude/settings.json'"
out="$(hook pre-tool-use.sh "$(edit down-session-2 "$R/src/lib.rs")")"; save "$out"
check "best-effort workspace: gate script is inert and silent" "[ ! -s "$O" ]" "$out"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
