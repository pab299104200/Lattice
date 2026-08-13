# Memory access attribution contract

**Status:** proposed implementation contract
**Date:** 2026-08-13
**Scope:** connect graph-backed `MemoryAccess` rows, MCP retrieval and terminal
events, and adoption metrics without inferring use from text or proximity.

## Problem and boundary

`memory_graph::mark_used` deliberately accepts a result only when it is tied to
a distinct, same-workspace downstream outcome event. A retrieval is not proof
that an assistant used a memory; an MCP tool result is not proof either. The
existing daemon has three separate records that must be joined deliberately:

- `memory_graph::MemoryAccess` is the authoritative per-memory attribution
  record. It identifies the memory, retrieval event, accessor, inclusion
  reason, and later terminal outcome.
- `EventCapture` writes the event stream. A normal MCP call emits a
  `ToolCalled`, a `ToolResult`, and a terminal `WorkflowSucceeded` or
  `WorkflowFailed` event. Retrieval events are observable, but are not yet
  persisted as graph access rows.
- `AdoptionMetricsStore` is a bounded operational ledger. Its
  `MemoryRetrieval` and `MemoryUse` records are aggregate telemetry; they are
  not the source of truth for individual-memory attribution.

The bridge belongs at the daemon runtime boundary that owns both the graph
store and the event writer. It must not be implemented by making the legacy
`MemoryStore` table look like the graph schema, by guessing from a final
summary, or by treating an arbitrary next tool call as evidence of use.

## Required event sequence

For every MCP result that surfaces zero or more memories, the bridge uses the
following transactionally ordered sequence. Empty retrievals produce metrics
but no `MemoryAccess` rows.

1. The MCP dispatcher records `ToolCalled` and retains its `EventId`.
2. The retrieval adapter has the selected memory identities and explicit
   inclusion reasons. It records `MemoryRetrieved`, yielding
   `retrieval_event_id`.
3. In one graph-store transaction, it inserts one pending `MemoryAccess` row
   per surfaced memory, with `accessed_in_event = retrieval_event_id`, the
   actual daemon/tool actor, a non-empty reason, and both `was_used` and
   `downstream_outcome_event` unset. The deterministic `access_id` is derived
   from the retrieval event id and canonical `MemoryId`, not from client text.
4. The dispatcher persists the tool result and the terminal workflow outcome.
   The terminal outcome must be a newly appended event after the retrieval;
   the retrieval event itself and `ToolCalled`/`ToolResult` cannot resolve an
   access.
5. Only an explicit, attributable outcome resolver calls `mark_used` for the
   pending rows. It supplies that terminal event and `was_used = true` only
   when the outcome carries the exact retrieval/access ids and states that the
   surfaced memory informed the completed work. A terminal failure may resolve
   the rows with `was_used = false` when it carries the same attribution. An
   unannotated success or failure leaves rows pending.
6. After a successful graph commit, the bridge appends the matching
   `MemoryUse` adoption metric. It never emits `MemoryUse` before the graph
   write and never retries it by changing the access outcome.

This makes absence meaningful: an unresolved access is unknown, not unused.
It also makes a negative result explicit rather than allowing a later report
to reinterpret the original retrieval.

## Bridge interface

The daemon-owned adapter has two operations, both receiving a trusted runtime
workspace id rather than request-controlled scope.

```text
record_retrieval(retrieval_event, tool_call_event, actor, retrieval_id,
                 [(memory_id, inclusion_reason)])
  -> PendingAccessSet { retrieval_id, retrieval_event, access_ids }

resolve_accesses(retrieval_id, terminal_outcome_event, disposition,
                 cited_access_ids)
  -> resolved_count | typed error
```

`disposition` is `used` or `not_used`. `cited_access_ids` must be a subset of
the pending set for `retrieval_id`; omitting an id keeps that access pending.
The resolver must reject a request that has only a memory id, summary text,
session id, time-window match, or a different retrieval id. This API lets a
future MCP outcome field, hook payload, or reviewed workflow action make a
claim without creating a second attribution path.

The adapter owns a small durable retrieval-to-access index. It is keyed by
`retrieval_id` and records the event id, workspace id, and exact generated
access ids. It is needed to validate later requests even after a process
restart; process-local maps are not sufficient.

## Scope and identity rules

- The graph store, every `MemoryId`, every access event, and the resolving
  outcome event must have the same canonical workspace id. Organization-tier
  recall remains advisory until it has a repository-local memory identity that
  can be represented in the repository graph; a cross-repository source must
  not be written under the receiving workspace's id.
- `retrieval_id` is daemon-generated, opaque, and stable across a retry of the
  same persisted retrieval. It includes no path, query text, memory content,
  organization id, or client-controlled component.
- `access_id` is deterministic for `(retrieval_event_id, memory_id)`. A retry
  reuses the row; a second retrieval of the same memory creates a different
  access because it has a different retrieval event.
- `mark_used` is the only resolver. Its same-workspace, downstream-event,
  immutable-outcome checks are mandatory contract enforcement, not optional
  validation duplicated in the MCP handler.
- A graph write failure is visible as a typed internal failure/diagnostic and
  leaves the tool's retrieval response truthful. It must not emit a successful
  use metric, silently manufacture an outcome, or write an access to the
  legacy memory database.

## Idempotence and partial failure

The graph write is idempotent by deterministic access id. The resolver is
idempotent only for the same access id, disposition, and terminal event;
different second attempts are conflicts. The durable bridge index records the
terminal resolution so a restart can distinguish a safe retry from conflict.

Event and graph persistence cannot be assumed to be one atomic store. The
recovery worker therefore reconciles only fully persisted event ids:

- event present, graph access absent: create the pending access once;
- pending access, terminal event present, explicit attribution present:
  resolve once;
- graph resolution present, adoption metric absent: append the metric once
  using a metric id derived from `(access_id, terminal_event_id)`;
- unresolved or contradictory records: retain the graph row, emit a bounded
  diagnostic, and require an explicit retry/review rather than guessing.

The recovery worker never scans nearby prose, calls, edits, or sessions to
create attribution. All retries are safe after a crash at any listed boundary.

## Adoption metrics contract

`MemoryRetrieval` is written after the retrieval event and access rows commit.
It retains zero-count misses. `MemoryUse` is written only after one or more
`mark_used(..., true, terminal_event)` operations commit and uses the exact
retrieval id. `used_count` is the number of newly resolved `true` rows, never
the response's retrieved count. A `not_used` resolution is retained in the
graph but does not inflate adoption-use counters.

Metrics are retained for 90 days under their existing operational policy. The
graph access history follows the graph retention/migration policy and may
outlive metrics. Pruning metrics must not modify graph attribution; pruning a
graph row must preserve required aggregate/history semantics through the
graph's existing migration or archival path.

## Required verification

The implementation must add daemon integration tests, using a real temporary
event log and graph database, for all of the following:

1. A repository-scoped MCP retrieval creates one pending access per returned
   memory, with explicit reason and correct retrieval event.
2. An explicit outcome with the exact retrieval/access ids resolves only those
   rows, writes the same terminal event, and adds the matching adoption-use
   metric after graph commit.
3. Empty results produce retrieval telemetry but no access rows or use metric.
4. A successful tool result without explicit attribution leaves accesses
   pending; it must not be credited merely because it is later in the stream.
5. Foreign-workspace memory ids or outcome events, retrieval event reuse as an
   outcome, missing outcome events, unknown retrieval ids, and an access id
   from a different retrieval are rejected without a partial resolution.
6. Replaying the same retrieval and resolution is idempotent; a competing
   outcome or disposition fails deterministically and preserves the first
   resolution.
7. Crashes between event, graph, and metrics writes recover to exactly one
   access row and at most one metric event per derived metric id.
8. Shared/organization-tier results cannot bypass repository identity and
   scope checks.
9. Retention compaction removes only operational metrics beyond policy and
   never converts pending access into used or unused.

The focused tests run serially because they assert durable ordering and
recovery. The workspace suite remains the release gate.
