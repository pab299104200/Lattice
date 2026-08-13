# Worth-It Benchmark Contract

## Decision

Lattice's value is measured with a paired, agent-style benchmark rather than
with retrieval payload size alone. The fixed E5 suite runs each task twice at
the same git revision: once with ordinary repository inspection tools and once
with the task-relevant subset of Lattice's public verbs available. The result
records agent-reported token usage and tool calls, and independently checks
whether the answer cites the required repository files.

The executable contract is
`tools/lattice-worth-it-benchmark.sh`. It emits a versioned JSON report and does
not update a checked-in baseline. A reviewer intentionally captures a clean-run
report when accepting new baseline numbers.

## Fixed Task Set

The v1 task order is stable:

1. `find-implementation`: locate explicit CLI runtime-mode selection.
2. `blast-radius`: find the MCP response-rendering contract and its schema test.
3. `diagnose-failure`: explain corrupt derived-graph recovery and error typing.
4. `recall-decision`: recover the shared-memory authority and isolation decision.

Each task has an exact set of required repository-relative citations. Those
citations remain in the harness scorer and are not included in runner requests.
A run passes citation scoring only when every required file is present;
additional citations do not improve the score. This deliberately measures file
discovery, not whether an agent can repeat an answer key, prose similarity, or a
subjective grader preference.

The task definitions live in the harness rather than in generated fixtures so
task drift is visible in code review. Changing a prompt, citation, arm policy, or
task order changes the benchmark contract and requires a new schema version or
an explicit baseline reset.

## Paired Arms

Both arms receive the same prompt, checkout path, revision, response schema,
read-only filesystem policy, disabled-network policy, and empty-conversation
requirement.

- `baseline` permits the runner's ordinary read-only repository tools and
  provides no Lattice verbs. A response reporting a `lattice ...` tool call is
  rejected.
- `lattice` permits ordinary read-only tools plus the task's fixed public-verb
  subset: `context`/`search`, `impact`/`context`, `diagnose`/`context`, or
  `recall`/`context`, respectively. It must report at least one permitted
  Lattice call. Calls using any other Lattice verb are rejected.

The runner adapter is responsible for enforcing the sandbox and for clearing
model conversation state between requests. The harness audits the returned tool
names but does not claim that self-reported traces are a security boundary.
Production benchmark adapters must obtain token counts from the model provider
and tool calls from the agent runtime; byte-to-token estimates and reconstructed
traces are not valid baseline evidence.

## Runner Interface

Invoke the harness with one executable runner:

```bash
tools/lattice-worth-it-benchmark.sh \
  --runner /absolute/path/to/agent-runner \
  --workspace "$PWD" \
  --require-clean \
  --output /tmp/lattice-worth-it.json
```

The harness calls the runner once per arm and task:

```text
RUNNER REQUEST_JSON RESPONSE_JSON
```

`REQUEST_JSON` contains:

- `schema_version`, `arm`, `workspace`, and `revision`
- the fixed task id, category, and prompt (the scoring citations remain hidden)
- read-only execution policy and permitted Lattice verbs
- the required response shape

The runner must atomically finish writing one JSON object to `RESPONSE_JSON`
before it exits successfully:

```json
{
  "answer": "The bare invocation is rejected during CLI dispatch...",
  "citations": ["daemon/crates/lattice-daemon/src/cli.rs"],
  "input_tokens": 812,
  "output_tokens": 146,
  "tool_calls": [
    {"tool": "lattice context", "arguments": {"query": "explicit runtime mode"}}
  ],
  "runtime": {
    "runner": "codex-benchmark-adapter",
    "runner_version": "1.2.0",
    "provider": "openai",
    "model": "example-model",
    "settings": {"reasoning_effort": "medium"}
  }
}
```

Token fields are non-negative integers. `tool_calls` is the complete ordered
trace and every `arguments` value is an object. Citations must be normalized,
repository-relative paths: absolute paths, empty components, `.` components,
and `..` traversal are rejected. An empty answer or malformed response fails the
whole benchmark instead of being silently omitted. Runtime identity fields must
be non-empty strings and `settings` must be an object. The harness rejects the
paired run if any runtime metadata changes between requests.

Each runner process has a bounded timeout. On timeout, the harness terminates
the runner's process group and fails without emitting a partial report.

## Output And Scoring

The `lattice-worth-it/v1` report includes the workspace, exact git revision,
dirty-checkout flag, model/provider/runner identity, aggregate scores for both
arms, their signed delta, and the full per-task evidence. Per task it records:

- answer and citations
- missing required citations and `cites_right_files`
- input, output, and total tokens
- tool-call count and the complete tool trace

The aggregate citation accuracy is passing tasks divided by four. Token and
tool-call deltas are `lattice - baseline`, so negative values mean Lattice used
less. Citation delta is also `lattice - baseline`, so positive values mean
Lattice found required files more reliably.

Reports from dirty worktrees set `comparable_baseline` to `false`; they are useful
for harness development but must not replace published baseline numbers. Use
`--require-clean` for review and CI evidence. The harness refuses to overwrite
an existing output file so a mistaken rerun cannot destroy prior evidence.

## Safe Inspection And Validation

The following commands do not invoke an agent:

```bash
bash -n tools/lattice-worth-it-benchmark.sh
tools/lattice-worth-it-benchmark.sh --help
tools/lattice-worth-it-benchmark.sh --list-tasks
tools/lattice-worth-it-benchmark.sh --dry-run | jq -e '.requests | length == 8'
```

`--list-tasks` exposes the scoring fixtures. `--dry-run` emits all eight runner
requests in their execution order: four baseline requests followed by four
Lattice requests. Neither command starts Lattice, mutates the checkout, calls a
network service, or writes a baseline.

## Baseline Review Rule

A baseline is acceptable only when all of the following are recorded with the
review artifact:

1. the checkout was clean and `comparable_baseline` is `true`;
2. both arms used the same model, model settings, runner version, and task order;
3. each request began without prior conversation state;
4. the baseline arm had no Lattice access and the Lattice arm used only public
   verbs;
5. tokens came from provider accounting and tool traces came from the runtime;
6. the complete JSON report, not only aggregate claims, is retained.

This task intentionally does not commit generated baseline data. Baseline
capture is an evidence-producing run against a named agent runtime, not a source
generation step, and must remain attributable to that runtime and revision.
