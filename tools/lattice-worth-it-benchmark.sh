#!/usr/bin/env bash
# Deterministic paired benchmark for Lattice-assisted agent tasks.

set -euo pipefail
IFS=$'\n\t'
umask 077

readonly SCHEMA_VERSION="lattice-worth-it/v1"
readonly DEFAULT_TIMEOUT_SECONDS=180

usage() {
  cat <<'USAGE'
Usage:
  tools/lattice-worth-it-benchmark.sh --runner <executable> [options]
  tools/lattice-worth-it-benchmark.sh --list-tasks [--workspace <path>]
  tools/lattice-worth-it-benchmark.sh --dry-run [--workspace <path>]

Options:
  --runner <path>       Agent runner implementing the v1 request/response contract.
  --workspace <path>    Lattice checkout to benchmark (default: git top level).
  --output <path>       Write the JSON report to a new file instead of stdout.
  --timeout <seconds>   Per-task runner timeout (default: 180).
  --require-clean       Refuse to benchmark a dirty checkout.
  --list-tasks          Print the fixed task set as JSON and exit.
  --dry-run             Print all runner requests as JSON and exit.
  -h, --help            Show this help.

Runner protocol:
  The executable is called as: RUNNER REQUEST_JSON RESPONSE_JSON
  It must write exactly one JSON object to RESPONSE_JSON. See
  docs/architecture/2026-08-13-worth-it-benchmark-contract.md.
USAGE
}

fail() {
  printf 'lattice-worth-it-benchmark: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

canonical_dir() {
  (cd "$1" 2>/dev/null && pwd -P) || return 1
}

tasks_json() {
  jq -n '
    [
      {
        id: "find-implementation",
        category: "find-the-implementation",
        prompt: "Find where the lattice CLI rejects a bare invocation and explain how explicit runtime modes are selected.",
        required_citations: ["daemon/crates/lattice-daemon/src/cli.rs"],
        suggested_lattice_verbs: ["context", "search"]
      },
      {
        id: "blast-radius",
        category: "blast-radius",
        prompt: "Identify the direct contract and test blast radius of changing the default MCP response renderer.",
        required_citations: [
          "daemon/crates/lattice-daemon/src/rpc/mcp.rs",
          "daemon/crates/lattice-daemon/src/rpc/mcp_schema_tests/tool_list.rs"
        ],
        suggested_lattice_verbs: ["impact", "context"]
      },
      {
        id: "diagnose-failure",
        category: "diagnose-a-failure",
        prompt: "Diagnose how corrupt derived graph storage is detected and recovered without treating authoritative data as disposable.",
        required_citations: [
          "daemon/crates/lattice-core/src/storage/mod.rs",
          "daemon/crates/lattice-core/src/error.rs"
        ],
        suggested_lattice_verbs: ["diagnose", "context"]
      },
      {
        id: "recall-decision",
        category: "recall-a-decision",
        prompt: "Recover the architectural decision for sharing memory across worktrees and explain its authority and isolation boundaries.",
        required_citations: [
          "docs/architecture/2026-08-12-shared-memory-architecture.md",
          "daemon/crates/lattice-core/src/memory/router.rs"
        ],
        suggested_lattice_verbs: ["recall", "context"]
      }
    ]'
}

request_json() {
  local arm=$1
  local task=$2
  local workspace=$3
  local revision=$4

  jq -n \
    --arg schema_version "$SCHEMA_VERSION" \
    --arg arm "$arm" \
    --arg workspace "$workspace" \
    --arg revision "$revision" \
    --argjson task "$task" \
    '{
      schema_version: $schema_version,
      arm: $arm,
      workspace: $workspace,
      revision: $revision,
      task: ($task | {id, category, prompt}),
      policy: {
        filesystem: "read-only",
        network: "disabled",
        clear_prior_conversation: true,
        permitted_lattice_verbs: (
          if $arm == "lattice"
          then $task.suggested_lattice_verbs
          else []
          end
        ),
        prohibited_actions: ["filesystem-write", "network", "git-mutation"]
      },
      response_schema: {
        answer: "string",
        citations: ["repository-relative/path"],
        input_tokens: "non-negative integer",
        output_tokens: "non-negative integer",
        tool_calls: [{tool: "string", arguments: "object"}],
        runtime: {
          runner: "string",
          runner_version: "string",
          provider: "string",
          model: "string",
          settings: "object"
        }
      }
    }'
}

validate_runner_response() {
  local response=$1
  local arm=$2
  local task=$3

  jq -e '
    type == "object" and
    (.answer | type == "string" and length > 0) and
    (.citations | type == "array" and all(.[]; type == "string" and length > 0)) and
    (.input_tokens | type == "number" and floor == . and . >= 0) and
    (.output_tokens | type == "number" and floor == . and . >= 0) and
    (.tool_calls | type == "array" and all(.[];
      type == "object" and
      (.tool | type == "string" and length > 0) and
      (.arguments | type == "object")
    )) and
    (.runtime | type == "object") and
    (.runtime.runner | type == "string" and length > 0) and
    (.runtime.runner_version | type == "string" and length > 0) and
    (.runtime.provider | type == "string" and length > 0) and
    (.runtime.model | type == "string" and length > 0) and
    (.runtime.settings | type == "object")
  ' "$response" >/dev/null || return 1

  jq -e '
    all(.citations[];
      (startswith("/") | not) and
      (split("/") | all(.[]; . != ".." and . != "." and length > 0))
    )
  ' "$response" >/dev/null || return 1

  if [[ "$arm" == "baseline" ]]; then
    jq -e 'all(.tool_calls[].tool; startswith("lattice ") | not)' "$response" >/dev/null || return 1
  else
    jq -e --argjson allowed "$(jq '.suggested_lattice_verbs' <<<"$task")" '
      any(.tool_calls[].tool; startswith("lattice ")) and
      all(.tool_calls[].tool;
        if startswith("lattice ")
        then (split(" ") as $parts |
          ($parts | length) == 2 and ($allowed | index($parts[1])) != null)
        else true
        end
      )
    ' "$response" >/dev/null || return 1
  fi
}

score_response() {
  local response=$1
  local task=$2
  local arm=$3

  jq -n \
    --arg arm "$arm" \
    --argjson task "$task" \
    --slurpfile response "$response" \
    '($response[0]) as $r |
     ($task.required_citations - $r.citations) as $missing |
     {
       arm: $arm,
       task_id: $task.id,
       category: $task.category,
       answer: $r.answer,
       citations: $r.citations,
       required_citations: $task.required_citations,
       missing_citations: $missing,
       cites_right_files: ($missing | length == 0),
       input_tokens: $r.input_tokens,
       output_tokens: $r.output_tokens,
       total_tokens: ($r.input_tokens + $r.output_tokens),
       tool_call_count: ($r.tool_calls | length),
       tool_calls: $r.tool_calls,
       runtime: $r.runtime
     }'
}

run_with_timeout() {
  local timeout_seconds=$1
  local runner=$2
  local request=$3
  local response=$4

  python3 - "$timeout_seconds" "$runner" "$request" "$response" <<'PY'
import os
import signal
import subprocess
import sys

timeout = int(sys.argv[1])
command = sys.argv[2:]
process = subprocess.Popen(command, start_new_session=True)
try:
    result = process.wait(timeout=timeout)
except subprocess.TimeoutExpired:
    os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
    print(f"runner exceeded {timeout}s timeout", file=sys.stderr)
    raise SystemExit(124)
raise SystemExit(result)
PY
}

runner=""
workspace=""
output=""
timeout_seconds=$DEFAULT_TIMEOUT_SECONDS
require_clean=false
mode="run"

while (($# > 0)); do
  case "$1" in
    --runner)
      (($# >= 2)) || fail "--runner requires an executable path"
      runner=$2
      shift 2
      ;;
    --workspace)
      (($# >= 2)) || fail "--workspace requires a directory"
      workspace=$2
      shift 2
      ;;
    --output)
      (($# >= 2)) || fail "--output requires a path"
      output=$2
      shift 2
      ;;
    --timeout)
      (($# >= 2)) || fail "--timeout requires seconds"
      timeout_seconds=$2
      shift 2
      ;;
    --require-clean)
      require_clean=true
      shift
      ;;
    --list-tasks)
      mode="list"
      shift
      ;;
    --dry-run)
      mode="dry-run"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

require_command git
require_command jq
require_command python3

[[ "$timeout_seconds" =~ ^[1-9][0-9]*$ ]] || fail "--timeout must be a positive integer"

if [[ -z "$workspace" ]]; then
  workspace=$(git rev-parse --show-toplevel 2>/dev/null) || fail "not inside a git checkout; pass --workspace"
fi
workspace=$(canonical_dir "$workspace") || fail "workspace is not a readable directory: $workspace"
git -C "$workspace" rev-parse --is-inside-work-tree >/dev/null 2>&1 || fail "workspace is not a git checkout: $workspace"
revision=$(git -C "$workspace" rev-parse HEAD)
dirty=false
if [[ -n "$(git -C "$workspace" status --porcelain=v1 --untracked-files=normal)" ]]; then
  dirty=true
fi
if [[ "$require_clean" == true && "$dirty" == true ]]; then
  fail "workspace has uncommitted changes; commit them or omit --require-clean"
fi

tasks=$(tasks_json)
while IFS= read -r required_path; do
  [[ -f "$workspace/$required_path" ]] || fail "fixed task fixture is missing: $required_path"
done < <(jq -r '.[].required_citations[]' <<<"$tasks" | sort -u)

if [[ "$mode" == "list" ]]; then
  jq -n --arg schema_version "$SCHEMA_VERSION" --argjson tasks "$tasks" \
    '{schema_version: $schema_version, tasks: $tasks}'
  exit 0
fi

if [[ "$mode" == "dry-run" ]]; then
  for arm in baseline lattice; do
    while IFS= read -r task; do
      request_json "$arm" "$task" "$workspace" "$revision"
    done < <(jq -c '.[]' <<<"$tasks")
  done | jq -s --arg schema_version "$SCHEMA_VERSION" '{schema_version: $schema_version, requests: .}'
  exit 0
fi

[[ -n "$runner" ]] || fail "--runner is required unless --list-tasks or --dry-run is used"
[[ -f "$runner" && -x "$runner" ]] || fail "runner is not an executable file: $runner"
runner=$(cd "$(dirname "$runner")" && printf '%s/%s\n' "$(pwd -P)" "$(basename "$runner")")

if [[ -n "$output" && -e "$output" ]]; then
  fail "refusing to overwrite existing output: $output"
fi

scratch=$(mktemp -d "${TMPDIR:-/tmp}/lattice-worth-it.XXXXXXXX")
trap 'rm -rf -- "$scratch"' EXIT HUP INT TERM
runs_file="$scratch/runs.jsonl"
: >"$runs_file"

for arm in baseline lattice; do
  while IFS= read -r task; do
    task_id=$(jq -r '.id' <<<"$task")
    request="$scratch/${arm}-${task_id}-request.json"
    response="$scratch/${arm}-${task_id}-response.json"
    request_json "$arm" "$task" "$workspace" "$revision" >"$request"
    if ! run_with_timeout "$timeout_seconds" "$runner" "$request" "$response"; then
      fail "runner failed for $arm/$task_id"
    fi
    [[ -s "$response" ]] || fail "runner wrote no response for $arm/$task_id"
    validate_runner_response "$response" "$arm" "$task" || fail "invalid runner response for $arm/$task_id"
    score_response "$response" "$task" "$arm" >>"$runs_file"
  done < <(jq -c '.[]' <<<"$tasks")
done

runtime_count=$(jq -s '[.[].runtime | tojson] | unique | length' "$runs_file")
[[ "$runtime_count" == 1 ]] || fail "runner runtime metadata changed between paired requests"

report="$scratch/report.json"
jq -s \
  --arg schema_version "$SCHEMA_VERSION" \
  --arg workspace "$workspace" \
  --arg revision "$revision" \
  --argjson dirty "$dirty" \
  --argjson task_count "$(jq 'length' <<<"$tasks")" \
  '
    def aggregate($arm):
      [.[] | select(.arm == $arm)] as $rows |
      {
        task_count: ($rows | length),
        citation_pass_count: ([$rows[] | select(.cites_right_files)] | length),
        citation_accuracy: (([$rows[] | select(.cites_right_files)] | length) / ($rows | length)),
        total_tokens: ([$rows[].total_tokens] | add),
        total_tool_calls: ([$rows[].tool_call_count] | add)
      };
    . as $runs |
    (aggregate("baseline")) as $baseline |
    (aggregate("lattice")) as $lattice |
    {
      schema_version: $schema_version,
      workspace: $workspace,
      revision: $revision,
      dirty_checkout: $dirty,
      comparable_baseline: ($dirty | not),
      runtime: $runs[0].runtime,
      task_count: $task_count,
      aggregates: {
        baseline: $baseline,
        lattice: $lattice,
        delta: {
          tokens: ($lattice.total_tokens - $baseline.total_tokens),
          tool_calls: ($lattice.total_tool_calls - $baseline.total_tool_calls),
          citation_accuracy: ($lattice.citation_accuracy - $baseline.citation_accuracy)
        }
      },
      runs: ($runs | map(del(.runtime)))
    }
  ' "$runs_file" >"$report"

if [[ -n "$output" ]]; then
  output_parent=$(dirname "$output")
  [[ -d "$output_parent" ]] || fail "output directory does not exist: $output_parent"
  cp "$report" "$output"
else
  cat "$report"
fi
