# Agent-directed Lattice use

A prompt hook supplies an initial briefing. The agent must also call Lattice
while executing its own plan. There is no automatic detector for a transition
between arbitrary tasks inside an agent's reasoning, and hook availability does
not prove that the agent used the returned context.

For each bounded task, use `prepare_change` with its file/symbol anchors, or
`context` for investigation. Use scoped `recall` before relying on a prior
lesson, `impact` before broader edits, and `diagnose` for failures. Reuse current
context inside a task, refreshing after material code/dependency changes,
branch/worktree switches, expired handles, or compaction. Delegate this same
contract with every subtask; a parent's briefing is not a child's current view.

Keep task status, dependencies, and verification in one durable execution
tracker. Capture a reusable correction through `remember` only after verifying
its evidence; avoid creating a lesson for every edit or completion. Return the
relevant Lattice evidence in the task handoff. Acknowledge delivery only for
content actually received, according to the returned receipt contract.

Installed integrations provide session, prompt, edit, and turn hooks; working
memory has bounded checkpoint support. These complement direct tool calls.
By default they do not enforce task-by-task adoption; a workspace can opt in to
[hook enforcement](hook-enforcement.md), which denies a product edit until a
change plan has been served. The default `lattice install --workspace <path>` maintains this workflow in
managed sections of both `AGENTS.md` and `CLAUDE.md`, alongside MCP and hook
configuration. Explicit client hook targets remain hook-only. Lattice unavailability
must be recorded, followed by direct inspection and a retry at the next task
boundary. Hooks never break a session: every Lattice failure allows the tool
call. In a best-effort workspace an unavailable hook is silent; in an enforcing
one it reports the failure once and the agent must repeat it in its report.

## Beacon acceptance exercise

Use the exact deployed build and a representative long-running plan with at
least 100 bounded tasks. Record model/settings, source revision, task/worktree
identity, actual direct tool calls, delivered memory IDs/receipts, changes,
independent test results, and unavailable/partial responses. Distinguish agent
calls from prompt/session/edit hooks. Require task-boundary retrieval or a
recorded current-context reuse justification, including after compaction and
when a sub-agent takes over.

Introduce a verified correction early and a related task much later. Confirm
that the agent independently retrieves the applicable lesson and avoids the
same mistake. Also exercise branch divergence, stale/superseded lessons,
failed tests, and temporary daemon unavailability. Inspect intermediate task
traces, not only the final answer or total call count. Compare matched runs
with and without Lattice before claiming fewer mistakes or lower token use.
This long-plan exercise is a separate acceptance gate; the seven-fixture
transport preflight and unit tests do not establish it.
