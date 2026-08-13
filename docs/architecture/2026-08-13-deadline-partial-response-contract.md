# Cooperative deadlines and partial retrieval responses

**Status:** binding design for recovery workplan B3
**Date:** 2026-08-13

## Decision

Retrieval deadlines are cooperative control signals, not permission to discard
an in-flight future. A workflow that reaches its deadline returns the last
complete, deterministically ranked checkpoint through the normal workflow
renderer. A client cancellation stops work without producing a response.

Index freshness is a separate dimension from execution completeness. During
indexing, a request may run to completion against an eligible, immutable
published snapshot and return `partial: false` with a freshness banner. A
deadline-limited request returns `partial: true` even when its snapshot is
fresh. Neither state may be inferred from an empty result array.

This replaces the blanket five-second `tokio::time::timeout` around agent tool
dispatch. The timeout currently drops the request future and fabricates a
payload with no ranked results, handle, or budget metadata. It also leaves
`spawn_blocking` work able to continue after the caller has received the
placeholder. B3 removes that behavior rather than adding a second timeout
path.

## Request control

Every JSON-RPC request receives one `RequestControl` at ingress:

- a monotonic deadline (`tokio::time::Instant`), set to the server retrieval
  limit for ranked read tools;
- a cancellation token shared by the transport, async orchestration, and
  blocking ranking stages; and
- the request identifier used only for cancellation routing and diagnostics.

The deadline starts when the daemon accepts the request, so lock and worker
queue time count against it. Wall-clock timestamps must not control expiry.
The default remains five seconds for B3, but it is held in one typed policy
rather than repeated in handlers. It is not a public caller override until a
separate schema and abuse analysis explicitly introduce one.

`RequestHandler` must accept the request control (with a convenience entry
point for direct tests if useful). The stdio and socket servers create and
retain the token for every request. On `notifications/cancelled`, disconnect,
shutdown, or supersession of the same request ID, the server first marks the
token cancelled and then aborts the async task where appropriate. Marking the
token first is required because aborting an async wrapper does not stop a
Rust `spawn_blocking` closure.

Only ranked read workflows use partial-on-deadline semantics. Mutations such
as `remember`, proposal decisions, index publication, and clear operations
must remain atomic success-or-error operations; they never report a partial
write. Their dependency calls still need their own explicit bounded failure
contracts.

Cancellation and deadline have different wire behavior:

| Signal | Server behavior | Response |
|---|---|---|
| Deadline | Stop before the next stage or bounded work chunk; render the last complete ranked checkpoint | Normal successful tool result with `partial: true` |
| Client cancellation | Mark the token, stop all cooperative work, release permits/locks, and suppress the request response | No JSON-RPC response for the cancelled request |
| Internal stage failure | Apply that source's documented fallback or return an actionable tool error | Never relabel a failure as a deadline |

If cancellation and deadline become observable at the same checkpoint,
cancellation wins. No metrics or context-handle write may turn a cancelled
request back into a client response.

## Ranked-stage checkpoint model

The retrieval implementation must expose stage progress instead of treating
`QueryEngine::query` plus workflow shaping as one indivisible operation. The
exact Rust names are implementation details, but the state passed between
core and daemon must have the equivalent of:

```text
RetrievalProgress {
    snapshot: Arc<PublishedGraphSnapshot>,
    completed_stages: Vec<RetrievalStage>,
    ranked: Vec<RankedCandidate>,
    diagnostics: RankingDiagnostics,
    expansion_seed: ExpandContextSeed,
}
```

A checkpoint is publishable only when every candidate it contains has passed
normalization, scope enforcement, scoring, deterministic tie-breaking, and
deduplication. Raw candidates, partially traversed graphs, half-read memory
tiers, and an in-progress sort are never response material.

The canonical stage order is:

1. Validate arguments, repository/workspace scope, and capture one immutable
   published graph snapshot.
2. Parse filters, intent, anchors, and explicit file/symbol seeds.
3. Collect the bounded lexical and exact structural source, rank the complete
   accumulator, and publish the first result-bearing checkpoint.
4. Complete bounded graph expansion, rerank the accumulator, and publish a
   checkpoint.
5. Complete each independent optional source in deterministic order
   (semantic, repository memory, then shared memory), enforcing its scope and
   trust rules before reranking and publishing a checkpoint. A source failure
   follows its own partial-failure contract; it does not discard earlier
   sources.
6. Shape the chosen compact/full bundle, construct the expansion seed, and
   apply the requested token budget.
7. Persist the context handle, attach response metadata, and render.

The orchestrator checks cancellation and deadline before starting a stage and
after it completes. CPU-bound stages also check the same control at fixed,
bounded chunks so a single large graph cannot monopolize a worker past the
deadline. An interrupted stage discards only its uncommitted local output and
returns the preceding checkpoint. SQLite-backed candidate sources use bounded
queries plus an interrupt/progress mechanism where available; waiting forever
inside one database call is not cooperative cancellation.

Stage ordering is fixed by the contract and must not change based on elapsed
time. In particular, the implementation must not use an opaque "probably too
slow" heuristic. Optional semantic work may be skipped only after an actual
deadline/cancellation checkpoint or by its existing deterministic eligibility
rules.

The first result-bearing checkpoint is deliberately produced from cheap,
high-signal sources before graph and semantic enrichment. This makes a forced
slow-stage test return useful ranked results rather than an empty timeout
object. If the deadline expires before any result-bearing stage completes,
the response is still an honest normal partial with zero results and
`result_set_state: "not_evaluated"`; it must not claim that no matches exist.

## Partial response contract

Deadline expiry flows through the same shaping, handle, budget, dense-wire,
JSON, and Markdown paths as a complete workflow. The structured value adds
these fields before budget trimming:

```json
{
  "partial": true,
  "partial_reason": "deadline",
  "result_set_state": "ranked_so_far",
  "completed_stages": ["anchors", "lexical_structural", "graph_expansion"],
  "last_completed_stage": "graph_expansion",
  "omitted_stages": ["semantic", "repository_memory", "shared_memory"],
  "context_handle": "ctx-...",
  "context_origin": "prepare_change",
  "budget": "compact",
  "budget_max_tokens": 850,
  "approx_tokens": 612,
  "truncated": false
}
```

Exact field rules:

- `partial` is always present on ranked workflow results. It is `true` only
  when execution stopped before all eligible stages completed.
- `partial_reason` is present only when `partial` is true. B3 defines
  `deadline`; source-specific degradation remains in source diagnostics, and
  index unavailability uses `index_unavailable` as described below.
- `completed_stages` is ordered and contains only committed checkpoints.
  `last_completed_stage` is its last value, or `null` when no stage completed.
- `omitted_stages` lists eligible ranking stages that did not run to
  completion. It is bounded and uses stable enum strings.
- `result_set_state` is `ranked_so_far` when at least one result-bearing
  checkpoint exists, `not_evaluated` when none exists, `complete_no_matches`
  for a completed query with no matches, or `complete` for a completed query
  with results.
- Every deadline partial receives a context handle, including a
  `not_evaluated` partial. Its seed contains the normalized query, completed
  candidates, snapshot identity, and partial-stage diagnostics. The handle is
  an expansion/inspection handle, not a promise that a later call resumes the
  interrupted computation.
- Existing delivery, wire-format, context-origin, and budget metadata remain
  present. `truncated` continues to mean response-budget truncation and is
  independent of `partial`.
- The obsolete `timeout: true` field and timeout-placeholder message are
  removed. Metrics record the configured deadline and elapsed duration
  server-side; public payloads do not need a timing side channel.

The Markdown renderer begins a deadline-limited response with exactly one
plain warning line:

> Partial result: the retrieval deadline was reached after
> `<last_completed_stage>`; results below are ranked from completed stages.

It then renders the same bounded summary it would render for a complete
result, including the handle-backed next action. JSON returns the structured
fields. Neither renderer emits a special timeout envelope or bypasses normal
budget enforcement.

## Published snapshots while indexing

Index construction and graph reads must use publish/replace semantics. A
request captures one immutable `Arc<PublishedGraphSnapshot>` and uses that
generation for its entire run. It must never promote or read the indexer's
mutable, partially constructed graph. When a new graph is ready, publication
atomically replaces the pointer; existing requests finish on the old
generation and subsequent requests see the new one.

A published snapshot is eligible only when its metadata proves that it belongs
to the active checkout and is allowed by the current repository epoch. A warm
cached snapshot validated at startup is eligible. The previously published
snapshot during an explicit same-checkout reindex is eligible. A snapshot from
a prior branch/workspace epoch is not silently eligible merely because it is
non-empty; repository epoch validation remains a workspace-safety boundary.

When `indexing == true` and an eligible published snapshot exists, the workflow
runs normally against it and adds:

```json
{
  "indexing": true,
  "freshness": {
    "state": "refreshing",
    "served_snapshot": true,
    "snapshot_id": 42,
    "reason": "startup|reindex|workspace_refresh"
  },
  "freshness_banner": "Index refresh is in progress; results use the last published snapshot and may be stale."
}
```

`freshness_banner` is the exact one-line banner used by both renderers. It may
be shortened by the dense wire mapping but not omitted. A complete search of
that snapshot has `partial: false`: staleness is reported by `freshness`, not
by lying about whether ranking completed. If its deadline also expires, both
the freshness fields and the deadline-partial fields are present.

The current `workflow_repo_state_placeholder` must therefore become a snapshot
eligibility decision, and `promote_live_graph_for_workflow` must not copy the
live indexer graph. The `indexing_workflow_response` copy that tells assistants
to use `rg` is deleted; B3 neither advertises nor embeds a fallback command in
workflow results.

## Empty-index semantics

An empty array has three materially different meanings and the response must
name which one applies:

| State | Required fields | Meaning |
|---|---|---|
| Indexing, no eligible published snapshot | `indexing: true`, `partial: true`, `partial_reason: "index_unavailable"`, `result_set_state: "not_evaluated"`, `freshness.served_snapshot: false` | Ranking did not run. Return a bounded retry/status next action and no "use rg" copy. |
| Ready, successfully published empty graph | `indexing: false`, `partial: false`, `result_set_state: "complete_empty_index"` | The workspace has no indexable graph content. Status diagnostics explain exclusions or parse failures. |
| Non-empty snapshot, completed query with no matches | `partial: false`, `result_set_state: "complete_no_matches"` | Ranking ran and found no candidates under the request filters. |

`index_unavailable` is not a deadline and must not report completed ranking
stages. It uses the normal renderer and budget metadata. It may issue a
retry/status handle only if that handle has a real bounded status seed; it must
not fabricate ranked evidence. A published snapshot containing files but zero
symbol nodes is classified from explicit snapshot/index health metadata, not
from `node_count == 0` alone.

## Observability

Session/adoption metrics record, without parsing rendered text:

- request deadline policy and elapsed duration;
- completion outcome (`complete`, `deadline_partial`, `cancelled`, or
  `index_unavailable`);
- last completed and omitted stages;
- result count at the returned checkpoint;
- snapshot ID, snapshot eligibility decision, indexing state, and freshness
  reason; and
- whether response-budget truncation also occurred.

Cancelled requests record a terminal cancellation metric even though they
produce no response. A blocking stage that remains alive after cancellation or
deadline is an error metric and a test failure, not acceptable background
cleanup.

## Required verification

Tests use an injected manual clock and stage gates/fakes. Wall-clock sleeps are
not precise enough to prove the contract.

1. Force the semantic stage to cross the deadline after a lexical/structural
   checkpoint. Assert useful ranked results, deterministic order,
   `partial: true`, `partial_reason: "deadline"`, completed/omitted stages, a
   resolvable context handle, and complete budget metadata. Assert the old
   timeout-only message and `timeout` field are absent.
2. Run the same forced partial through Markdown and JSON. Assert both use the
   normal renderer, Markdown contains one partial warning and the handle-backed
   next action, and JSON remains a single valid structured payload.
3. Expire before the first result-bearing stage. Assert zero results with
   `result_set_state: "not_evaluated"`, not `complete_no_matches`, plus a
   resolvable diagnostic handle.
4. Cancel before work starts, during a bounded CPU ranking chunk, while queued
   for a query permit, and during a SQLite-backed source. Assert no response,
   prompt worker termination, released permits/locks, no handle persistence,
   and no late metrics that claim success.
5. Race cancellation with deadline at the same checkpoint. Assert cancellation
   wins and repeated execution is deterministic.
6. Set indexing active with an eligible warm published snapshot. Assert real
   ranked results, the exact freshness banner, stable snapshot ID, and no
   `use rg` text. Publish a replacement concurrently and prove the request does
   not mix generations; prove the next request observes the replacement.
7. Set indexing active with no eligible snapshot and separately with a
   prior-epoch snapshot. Assert `index_unavailable`, `not_evaluated`, no stale
   graph evidence, and an actionable status/retry path.
8. Publish a valid empty index and query a non-empty index with no matches.
   Assert `complete_empty_index` and `complete_no_matches` are distinct and
   neither is a partial timeout.
9. Combine a warm snapshot with deadline expiry and response-budget trimming.
   Assert freshness, `partial`, and `truncated` are all independently truthful
   in payload and metrics.
10. Run the workspace tests with a deliberately slow stage and assert the
    blocking worker count and query permit count return to zero before the test
    completes. This guards against future-dropping regressions.

## Implementation boundaries

- `lattice-core` owns cooperative staged retrieval, deterministic checkpoint
  construction, and bounded-loop cancellation checks.
- `lattice-daemon` owns request control, snapshot eligibility/publication,
  stage orchestration, partial response fields, handles, rendering, and
  metrics.
- Transport servers own request-ID cancellation routing and response
  suppression.
- The indexer builds privately and publishes immutable snapshots; workflow
  handlers never inspect an in-progress mutable graph.

B3 must update the public README/MCP behavior documentation when implemented.
It must preserve D2's repository/shared-memory router as the only memory source
boundary: cooperative stages call that router and may not introduce unscoped
or parallel ad hoc memory reads.

## Non-goals

B3 does not make interrupted retrieval resumable, expose caller-selected
deadlines, weaken repository epoch validation, turn index construction into a
streaming public graph, or redefine token-budget truncation. Those would each
require a separate contract and verification surface.
