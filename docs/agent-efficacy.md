# Agent efficacy evaluation

The release criterion is fewer repeated mistakes without worse patch correctness.
Harness tests, successful retrieval, and captured memories do not establish that
criterion. The current pilot did **not** pass it.

## Current acceptance status — September 13

The final binary (`cfb306cc811ef03c0e87ef4f9deffb440e59113f057a2c8ef6a16ce3784a2b3b`)
passes private delivery smoke v4 and all seven scripted fixture delivery checks,
including the superseded branch-policy correction and clean empty results.
These checks use zero agent calls and do not establish efficacy.

A complete fresh three-trial paired run remains unexecuted. The user requested
faster completion because little usage remained; the coordinator held the
remaining 87-call evaluation to preserve that usage. This is a usage constraint,
not a credential failure. Earlier actual runs failed on approval routing and
empty-result attribution; their artifacts are preserved in the execution tracker
and are not combined into a passing result. The historical pilot showed zero
measured benefit. The efficacy release gate remains open.

## Reproduction and isolation

```sh
python3 tools/tests/agent-efficacy_test.py
cd daemon && cargo build --release
LATTICE_EFFICACY_BINARY="$PWD/target/release/lattice" \
python3 ../tools/lattice-agent-efficacy.py \
  --runner ../tools/lattice-codex-efficacy-runner.py \
  --model gpt-5.6-terra --trials 3 \
  --output /tmp/lattice-agent-efficacy-results.json
```

The adapter requires a freshly built Lattice binary, the Codex CLI, and working
provider authentication. `LATTICE_EFFICACY_BINARY` must identify that artifact;
an already-running shared daemon is never evidence for the checked-out code.
Each invocation runs an ephemeral agent with fixed low
reasoning effort, a private daemon endpoint, and isolated lifecycle state. It
terminates only its own daemon. Baseline agents have no Lattice MCP configuration.
The adapter rejects conventional recorded Lattice use in a baseline turn: the
bare `lattice` CLI, a path whose executable name is `lattice`, and Lattice MCP
namespaces. It retains raw tool records. This audits recorded conventional
invocations; it does not prove that a deliberately concealed process was absent.
Explicit retrieval instructs the agent to call public-schema `recall` with full
rendering; briefing calls public-schema `prepare_change` with `render: "json"`,
`wire_format: "standard"`, and `budget: "full"`, then passes its actual result
to the agent. The default dense aliases are not accepted as delivery evidence:
the evaluator requires the standard response to bind an actual memory object,
its content, and one complete public delivery receipt.

The producer must create a patch that passes an independent grader before its
lesson may be captured. Consumers start from matched Git revisions on separate
worktrees. Arm order is randomized with a recorded seed. The grader executes
outside the writable checkout, and model self-reported success is not a score.
Reports retain patch hashes, producer `remember` responses bound to the saved
ID and lesson digest, consumer tool records, provider token counts, elapsed
time, and paired bootstrap intervals. Raw transcripts remain outside durable
documentation; reports identify their audit directory. Before trials, the
harness exclusively creates an audit manifest with that directory and intended
report path. Adapter failures and timeouts exclusively create a failure
manifest beside their request and response artifacts; any later grader,
validation, or report failure exclusively creates a run-level failure manifest.

Assisted rows are admitted only after `remember(kind: "durable")` returns a durable memory ID
and one repository authority bound to the SHA-256 digest of the exact seeded lesson. The adapter retains the
successful `recall` or JSON `prepare_change` response, and the harness
independently checks that one returned memory object contains both the expected
identity and the actual lesson text before recomputing its digest. Raw IDs,
structured `MemoryId` values, and `repository:<repository-id>:<local-id>` wire
identities compare as the same record only when the captured repository
authority agrees. A matching local ID under another repository or an
organization namespace is rejected; namespaces are never stripped before
comparison. The current raw `remember` wire proves that authority with the
same memory object's strict `memory:repo_<64-lowercase-hex>/<same-local-id>`
expansion handle. Malformed handles, a different handle-local ID, or conflicting
workspace/origin authority fields reject the capture. Unqualified fixture identities remain valid only by exact local-ID
comparison. Empty, bootstrap,
truncated, or error results, unrelated memories, ID-only references, and
assertions without the response payload fail the run. The harness extracts exactly
one complete repository-scoped receipt from the raw response and requires exact
parity with adapter evidence. Missing, incomplete, foreign, malformed-hash, or
multiple receipts fail, and the public receipt must mark acknowledgement as
required. Repeated encodings of the same wrapper count once. The
harness does not invent a receipt.
When a fixture expects a lesson, an automatic briefing without that delivered
memory object is not accepted. The unfamiliar control instead requires a
successful raw briefing with no seeded-memory expectation.

The private delivery smoke separately proves the public acknowledgement path:
attempted delivery leaves retention untouched, a forged receipt is rejected,
the exact receipt renews once, and its replay is idempotent. It does not
simulate the 90/180-day lifecycle clock; controlled retention expiry and purge
remain covered by the dedicated Rust/MCP integration fixture.

The protocol remains `lattice-agent-efficacy/v1`. Each request has one task
object containing its ID, problem, review source, allowed files, and seed.
Reference patches and graders remain inside the harness and are excluded from
agent requests. Producer, consumer, patch guard, scorer selection, and report
matrix use the same task identity so scenario labels cannot drift from fixtures.

The stale-lesson variant instead requires a successful bounded retrieval that
proves the seeded stale ID and text were absent, followed by a patch that passes
the current-contract grader. Observed stale content fails the run as misleading
advice. This assertion is computed from the response and expected fixture, not
from a runner-provided score.

## Current fixture matrix

Every broken base patch must fail its own hidden grader and the reference patch
must pass before an actual agent run is accepted. Graders run in a fresh Python
interpreter outside the writable checkout and cover success and failure paths.
Its `-I` flag isolates Python imports; it is not a hostile-submission filesystem
or process sandbox. The current fixture evaluation must therefore not be used
to make an adversarial-code-containment claim.
Every matched repository also commits a task-specific `CONTRACT.md` before any
producer or consumer worktree is created. It states the public API, valid and
invalid inputs, return/error behavior, and mutation rules needed to solve the
fixture; hidden graders add coverage but do not supply undisclosed task rules.

| Scenario | Required decision | File | Verified source |
|---|---|---|---|
| Scope before candidate budget | Filter scope before limiting candidates | `selector.py` | `docs/memory-retrieval.md:37` |
| Retry and durable idempotency | Reuse equal retry; reject conflicting reuse | `capture.py` | `daemon/crates/lattice-core/src/memory/store.rs:1214-1249` |
| Path boundary | Canonical containment including symlink escape | `boundary.py` | `daemon/crates/lattice-core/src/security/workspace.rs:48-98` |
| Stale contradiction trust | Exclude stale and contradicted guidance | `trust.py` | `docs/memory-retrieval.md:13` |
| Transactional changed state | Failed batch cannot partially mutate state | `publish.py` | repository review, transactional graph deltas |
| Unfamiliar control | Current identifier contract with no memory | `identifier.py` | control |
| Changed-branch supersession | Apply revision v2 policy and exclude superseded v1 advice | `exports.py` | `docs/memory-trust.md`, auditable lesson revision |

The scope fixture seeds 10,000 irrelevant observations after validated capture.
The trust fixture marks its captured lesson stale and requires its exact ID and
content to be absent from assisted delivery. The unfamiliar control has no
captured lesson.

## September 12 pilot (historical, invalid for the current fixture contract)

The actual run used six task variants, two trials, and three arms: 36 consumer
patches. All 12 patches in every arm passed the independent grader. Both assisted
arms had correctness difference **0** and mistake recurrence reduction **0**;
their paired bootstrap intervals were **[0, 0]**. No reduced-mistake benefit was
demonstrated.

| Arm | Correct patches | Input tokens | Output tokens | Mean elapsed seconds |
|---|---:|---:|---:|---:|
| Baseline | 12/12 | 1,288,281 | 22,919 | 46.56 |
| Explicit retrieval | 12/12 | 1,993,634 | 26,605 | 54.56 |
| Automatic briefing | 12/12 | 1,001,289 | 19,711 | 40.53 |

These token totals are provider-reported inputs, including cached context; they
are not billing estimates. Timing includes adapter and daemon setup. The raw
report is `/tmp/lattice-agent-efficacy-real-20260912.json`; its `audit_artifacts`
field locates the retained requests, responses, and worktrees.

All six historical variants exercised the same small invoice implementation. The
baseline ceiling, limited task diversity, and small sample prevent a broader
efficacy conclusion. The run predates the final storage integration, so it is
also not a benchmark of the final release artifact. Delivery evidence requires
inspection of actual returned content: a tool call alone does not prove an
applicable lesson reached the agent. The pilot adapter accepted tool records
without the exact-ID/content checks now required, including failed or empty
calls. Its assisted delivery evidence is therefore invalid under the current
contract; its correctness, token, and timing values remain historical raw
measurements. The September 13 private smoke passed the strengthened delivery contract below.
A subsequent actual Codex producer on the new matrix failed before capture
because its public contract left dictionary versus attribute records ambiguous.
That invalid fixture run establishes no consumer efficacy outcome. The historical
values cannot establish the revised matrix's gate.

Storage reuse, peak resource use, and worktree churn are separate acceptance
measurements. Neither this pilot nor the mocked adapter tests substitute for them.

New runs use a dedicated `lattice-efficacy-runs` temporary namespace (or `--artifacts-directory`). The producing harness retires only marked expired runs after seven days, retains at most eight unexpired runs, and bounds each inventory to 128 entries. Unknown directories are preserved. Reports selected for a durable audit must be copied explicitly before that horizon; the earlier pilot predates this producer retention policy.

The private smoke also exercises `prepare_change` with its actual defaults
(no mode, render, wire-format, or budget override). It requires the complete
seeded lesson and identity in the Markdown memory section, and acknowledges
that section's receipt. Explicit standard/full JSON remains the paired harness
format so exact content can be audited without interpreting dense aliases.

## Historical September 13 private delivery acceptance

The private smoke `/tmp/lattice-private-delivery-smoke-sep13-final-wire.json`
passed against release SHA-256
`6c3b0c4acaf83a30f515e0d1c70fe9cfeda40ded2d5d56dbeb99043c4cb1d9a9`.
It proves exact content and identity in genuine default Markdown, linked-checkout
standard JSON briefing and explicit recall, attempted delivery without retention
renewal, rejected forged acknowledgement, one exact renewal, idempotent replay,
and exclusion after a marked fixture-only semantic-stale transition. It does
not establish reduced mistakes or replace the controlled-clock expiry tests.
The raw Markdown identity is bound to the captured repository by its single
matching delivery receipt; ambiguous or foreign receipts are rejected.


The expanded private smoke
`/tmp/lattice-private-delivery-smoke-sep13-final-closed.json` passed on
release SHA-256
`e03f8b3ee04d565cb9886b436f6213afccdc156368f48372c5c29a11ca0f5ef0`.
It additionally proves a canonical pending access from actual public recall,
resolution through public outcome capture after restarting the consumer daemon,
one idempotent resolution on retry, rejected conflicting feedback, and no
renewal of the lesson's recall timestamp by feedback. This closes the earlier
feedback gap; it is still not evidence of reduced agent mistakes.

Private daemon fixtures explicitly override inherited and home-configured
organization destinations into their own temporary directories. Codex keeps
its original authentication home. SQLite inspection connections and private
proxy pipes are explicitly closed; private-daemon cleanup runs even if proxy
cleanup fails.


## Fixture v2 correction and preregistration

The actual September 13 producer failure is retained at
`/tmp/lattice-efficacy-runs-sep13/run-g3l9uxtn/run-failure.json`. Provider access
worked; the public API ambiguity was an evaluator defect. Independent review
also found that v1 defined recurrence as the complement of correctness and
converted grader infrastructure errors into observed mistakes. Those metrics
cannot support a separate repeated-mistake claim.

The replacement fixture contract names one target decision and grades its
recurrence separately from independent correctness checks. Unassessable
submissions and grader process, timeout, or schema failures invalidate the run;
they are never silently counted as repeated mistakes. Reference patches,
original defects, and mutants that fix the target but break another requirement
are checked independently before provider execution.

Before the first agent invocation, v2 records fixture/version hashes, SHA-256
hashes for the harness, adapter, fixture helper, and Lattice binary, task IDs,
seed, exact model/settings, at least three matched trials, and its fixed analysis
rule. It verifies those files before and after every invocation and requires each
response to report the preregistered binary hash and model/settings.
Every producer, capture, and consumer invocation is bounded to its declared files:
the harness checks the Git HEAD, staged and untracked paths, and rejects symlinked
or escaping targets before and after the invocation. Every agent runs with Python
bytecode writes disabled and is instructed to use `python -B`
for verification so normal imports do not create undeclared cache files. All arms
must leave only declared source changes and may not stage or commit them.
Baseline and briefing agents
may not originate Lattice calls. The raw briefing preflight is retained. Explicit
rows require a successful public `recall`, including controls without seeded
memory. Failed responses and plain CLI output remain part of stale-content checks.
Automatic briefing is the primary comparison; explicit retrieval is diagnostic,
so a favorable secondary result cannot substitute for primary acceptance.
Bootstrap resampling clusters trials by task. Primary acceptance requires a
strictly positive lower 95% bound for recurrence reduction, no assisted-pair
correctness regression, and zero misleading advice. Passing delivery assertions
or obtaining an all-correct, zero-effect outcome cannot pass this gate.

The six existing task IDs remain in the matrix. A seventh changed-branch
supersession fixture is implemented: separately graded producers establish
old and replacement decisions on distinct committed revisions; all consumers
start at the same new revision. Public auditable supersession and conflict
inspection must establish the relation. Assisted delivery must include the
replacement and exclude the predecessor in the same response. This measures
resolution of superseded evidence, not an unimplemented contradiction mutation.
Actual paired v2 acceptance remains open pending integration and execution.


## Context expansion acceptance

Private smoke protocol `lattice-agent-efficacy/private-delivery-smoke/v3` also
requires actual `context(mode: expand)` delivery through the authority-qualified
memory target, complete lesson text, its public receipt and acknowledgement. It
then expands a predecessor before public supersession, and requires the same
handle to reject that predecessor after supersession and after daemon restart.
Rejected expansion must return neither the old lesson text nor a delivery receipt.

The v2 smoke passed on release
`1eb5d32d675120f83a2e287857c0f9d121fc943af7c0868f247a8f2458a52396`
(`/tmp/lattice-private-delivery-smoke-sep13-public-budget-accepted.json`), but did
not cover expansion. Review subsequently found that expansion replayed cached
lesson payloads without canonical lifecycle validation. The reference-only cache
and current-state expansion repair, v3 smoke execution and actual paired trials
remain acceptance gates tracked in the
[execution tracker](plans/2026-09-12-remediation-execution.md). Controlled-clock
purge coverage remains a separate Rust/MCP gate; this smoke does not fake expiry.

The fresh v3 smoke passed on release
`1080d27b4d11b2706922e1cceb6816f705f38ee8d190d81428ea97d5b8ae527b`
(`/tmp/lattice-private-delivery-smoke-sep13-expansion-final.json`), with no
warnings. This verifies expanded content and receipt delivery, acknowledgement,
and superseded-handle rejection before and after restart. The complete Rust
workspace passed 2,234 tests; the actual paired v2 efficacy run remains a separate
acceptance gate. Neither result alone proves reduced mistakes.

The first frozen-release paired attempt stopped at explicit retrieval because
Codex rejected the MCP call with `MCP tool call requires approval, but approval
policy is never`. The failed run is retained at
`/tmp/lattice-efficacy-runs-sep13-v2/run-spqc77sc/run-failure.json`; successful
producer and briefing calls do not make it a completed comparison. The adapter
now uses the installed CLI's `--approve-for-me` mode consistently for producers
and all consumer arms. This retains the workspace sandbox and routes required
approvals to automatic review; it does not bypass review or approve all tools.
The common permission mode is preregistered alongside model/settings and hashes.
See [Codex approval configuration](https://developers.openai.com/codex/config-reference).

A private real-agent probe then completed public recall under automatic review
(`/tmp/lattice-reviewed-recall-probe-v2.log`). The adapter uses
`--approve-for-me` alone: this CLI flag selects workspace-write sandboxing and
conflicts with a separate `--sandbox` flag. The probe establishes runner
connectivity, not agent usefulness; a fresh complete preregistered run is required.

## Empty recall acceptance

The reviewed paired run stopped at the first stale-memory control because empty
recall incorrectly attempted a zero-access attribution write and returned a
spurious storage error. Its failed manifest is retained at
`/tmp/lattice-efficacy-runs-sep13-reviewed/run-v164j70d/run-failure.json`. Ten
completed consumer invocations are partial evidence, not a completed comparison.
The production replacement skips attribution for zero attributable memories,
while preserving failures for nonempty delivery. Private smoke protocol v4 adds
a clean empty-store recall before capture and verifies zero attribution, access,
and delivery rows. Fresh release verification and paired trials are required.
