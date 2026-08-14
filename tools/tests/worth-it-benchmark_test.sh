#!/usr/bin/env bash
# Tests for tools/lattice-worth-it-benchmark.sh.
#
# The harness invokes an external agent, so it is tested here against stub
# runners that implement the documented request/response contract. That keeps
# the test hermetic — no model, no network, no Lattice daemon — while still
# exercising the real validation, arm policing, scoring, and aggregation code
# paths end to end.
#
# The behaviour under test is the `diff-risk` health task added by Phase H5 of
# docs/plans/2026-08-13-health-engine.md: an answer that names the riskiest
# file but reports only a bare score must score differently from one that
# names the facts behind it. See
# docs/architecture/2026-08-13-worth-it-benchmark-contract.md
# § "Output And Scoring".

set -euo pipefail
IFS=$'\n\t'

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
harness="$repo_root/tools/lattice-worth-it-benchmark.sh"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

failures=0
checks=0

check() {
  local label=$1
  local actual=$2
  local expected=$3
  checks=$((checks + 1))
  if [[ "$actual" == "$expected" ]]; then
    printf 'ok   %s\n' "$label"
  else
    printf 'FAIL %s\n       expected: %s\n       actual:   %s\n' \
      "$label" "$expected" "$actual" >&2
    failures=$((failures + 1))
  fi
}

# Writes a stub runner that answers every task with the citations the scorer
# requires, and whose `diff-risk` answer is supplied by the caller.
write_runner() {
  local path=$1
  local diff_risk_answer=$2
  cat >"$path" <<RUNNER
#!/usr/bin/env bash
set -euo pipefail
request=\$1
response=\$2
arm=\$(jq -r '.arm' "\$request")
task=\$(jq -r '.task.id' "\$request")
verbs=\$(jq -r '.policy.permitted_lattice_verbs[0] // empty' "\$request")

# Cite exactly what each task's ground truth requires.
case "\$task" in
  find-implementation) citations='["daemon/crates/lattice-daemon/src/cli.rs"]' ;;
  blast-radius) citations='["daemon/crates/lattice-daemon/src/rpc/mcp.rs","daemon/crates/lattice-daemon/src/rpc/mcp_schema_tests/tool_list.rs"]' ;;
  diagnose-failure) citations='["daemon/crates/lattice-core/src/storage/mod.rs","daemon/crates/lattice-core/src/error.rs"]' ;;
  recall-decision) citations='["docs/architecture/2026-08-12-shared-memory-architecture.md","daemon/crates/lattice-core/src/memory/router.rs"]' ;;
  diff-risk) citations='["daemon/crates/lattice-daemon/src/rpc/mcp.rs"]' ;;
  *) citations='[]' ;;
esac

if [[ "\$task" == "diff-risk" ]]; then
  answer=\$(cat <<'ANSWER'
${diff_risk_answer}
ANSWER
)
else
  answer="A sufficient answer for \$task."
fi

if [[ "\$arm" == "lattice" ]]; then
  tool_calls=\$(jq -n --arg verb "\$verbs" '[{tool: ("lattice " + \$verb), arguments: {}}]')
else
  tool_calls='[{"tool":"read_file","arguments":{}}]'
fi

jq -n \
  --arg answer "\$answer" \
  --argjson citations "\$citations" \
  --argjson tool_calls "\$tool_calls" \
  '{
    answer: \$answer,
    citations: \$citations,
    input_tokens: 100,
    output_tokens: 50,
    tool_calls: \$tool_calls,
    runtime: {
      runner: "stub-runner",
      runner_version: "1.0.0",
      provider: "stub",
      model: "stub-model",
      settings: {}
    }
  }' >"\$response"
RUNNER
  chmod +x "$path"
}

run_harness() {
  local runner=$1
  local out=$2
  "$harness" --runner "$runner" --workspace "$repo_root" --output "$out" >/dev/null
}

# --- The task is registered and its scoring fixtures stay hidden -------------

tasks=$("$harness" --list-tasks --workspace "$repo_root")
check "the health task is registered" \
  "$(jq -r '[.tasks[] | select(.id == "diff-risk")] | length' <<<"$tasks")" "1"
check "the health task is scored on impact-style verbs" \
  "$(jq -r '.tasks[] | select(.id == "diff-risk") | .suggested_lattice_verbs | join(",")' <<<"$tasks")" \
  "impact,context"

requests=$("$harness" --dry-run --workspace "$repo_root")
check "both arms request every task" \
  "$(jq -r '.requests | length' <<<"$requests")" "10"
# The runner must not be able to read the answer key out of its own request.
check "required citations never reach the runner" \
  "$(jq -r '[.requests[] | select(has("required_citations"))] | length' <<<"$requests")" "0"
check "the fact vocabulary never reaches the runner" \
  "$(jq -r '[.requests[] | select(.task | has("required_fact_vocabulary"))] | length' <<<"$requests")" "0"
check "the health task prompt asks for the evidence, not only a ranking" \
  "$(jq -r '[.requests[] | select(.task.id == "diff-risk") | select(.task.prompt | test("evidence"))] | length' <<<"$requests")" "2"

# --- An answer naming the facts scores as citing its evidence ---------------

write_runner "$scratch/good-runner" \
  "The MCP request surface file is riskiest: it has by far the highest fan-in in the change set, heavy churn across many commits, and high complexity concentrated in long functions."
run_harness "$scratch/good-runner" "$scratch/good.json"
good=$(cat "$scratch/good.json")

check "schema version is v2" "$(jq -r '.schema_version' <<<"$good")" "lattice-worth-it/v2"
check "every task ran in both arms" "$(jq -r '.runs | length' <<<"$good")" "10"

good_row=$(jq -c '.runs[] | select(.arm == "lattice" and .task_id == "diff-risk")' <<<"$good")
check "a well-evidenced answer cites the right file" \
  "$(jq -r '.cites_right_files' <<<"$good_row")" "true"
check "a well-evidenced answer names the required facts" \
  "$(jq -r '.names_required_facts' <<<"$good_row")" "true"
check "a well-evidenced answer passes the evidence dimension" \
  "$(jq -r '.cites_evidence' <<<"$good_row")" "true"
check "the named facts are recorded for review" \
  "$(jq -r '.named_facts | length >= 2' <<<"$good_row")" "true"
check "evidence accuracy is aggregated" \
  "$(jq -r '.aggregates.lattice.evidence_accuracy' <<<"$good")" "1"

# --- A bare score cites the right file but fails the evidence dimension ------

write_runner "$scratch/bare-runner" \
  "The MCP request surface file is the riskiest of the three. Its risk band is high, with a score of 812."
run_harness "$scratch/bare-runner" "$scratch/bare.json"
bare=$(cat "$scratch/bare.json")

bare_row=$(jq -c '.runs[] | select(.arm == "lattice" and .task_id == "diff-risk")' <<<"$bare")
check "a bare score still cites the right file" \
  "$(jq -r '.cites_right_files' <<<"$bare_row")" "true"
check "a bare score names no required facts" \
  "$(jq -r '.names_required_facts' <<<"$bare_row")" "false"
check "a bare score fails the evidence dimension" \
  "$(jq -r '.cites_evidence' <<<"$bare_row")" "false"
check "citation accuracy is unchanged by the bare answer" \
  "$(jq -r '.aggregates.lattice.citation_accuracy' <<<"$bare")" "1"
check "evidence accuracy separates the bare answer from the evidenced one" \
  "$(jq -r '.aggregates.lattice.evidence_accuracy < 1' <<<"$bare")" "true"

# --- Tasks without a vocabulary are unaffected ------------------------------

check "a task with no fact vocabulary is vacuously evidenced" \
  "$(jq -r '.runs[] | select(.arm == "lattice" and .task_id == "find-implementation") | .cites_evidence' <<<"$bare")" \
  "true"
check "a task with no fact vocabulary requires no facts" \
  "$(jq -r '.runs[] | select(.task_id == "find-implementation") | .required_fact_count' <<<"$bare" | sort -u)" \
  "0"

printf '\n%d checks, %d failures\n' "$checks" "$failures"
[[ "$failures" -eq 0 ]]
