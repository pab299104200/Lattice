# Verification Freshness Design

This is the authoritative verification and freshness reference. It implements [## Verification Engine](../plans/2026-05-16-cognitive-workspace-fork-plan.md#verification-engine), [## Phase 7: Verification And Freshness](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-7-verification-and-freshness), and the memory status contract in [Memory Model Reference](./2026-05-16-memory-model-reference.md#verification-status-state-machine).

## Verification checks

Verification checks are:

- linked files still exist
- linked symbols still exist
- cited docs still exist
- linked tests still exist
- evidence text still matches when exact spans were captured
- implementation still matches memory claim where deterministic checks are possible
- contradicted and superseded states remain coherent
- branch-scoped memory is not leaking into unrelated branches
- time-bound memory has expired

Each check produces a structured result with target identity, status, summary, evidence, and next action when applicable.

## Verification outputs

Verification outputs are:

| Status | Meaning |
|---|---|
| `verified` | Evidence still supports the memory in its declared scope. |
| `unverified` | Evidence has not yet been checked or is insufficient. |
| `in_review` | Human or proposal review is required before trust can change. |
| `stale` | Linked artifacts changed or evidence no longer matches current workspace state. |
| `contradicted` | A valid contradiction or counter-memory applies. |
| `superseded` | A newer or stronger memory replaces this one. |
| `expired` | A time-bound memory exceeded its validity window. |
| `invalidated` | Memory must not be used as trusted guidance. |

## Incremental verification

Verification is incremental and tied to graph, doc, branch, workspace, time, and memory-link changes. Large repositories cannot tolerate full rescans for every memory. Verification jobs should select affected memories from indexes on linked files, symbols, docs, tests, scopes, freshness keys, and verification status.

Workflow bundles may surface stale or contradicted memory, but only with explicit labels and reasons. They must not present it as ordinary trusted guidance.

## Graph-change triggers

Graph-change triggers include file create/delete/move, symbol create/delete/rename, doc heading changes, test changes, edge changes, parser failures, partial-index events, and compaction snapshot reloads. The trigger maps changed graph identities to linked memories and queues targeted verification jobs.

When exact spans are available, verification compares span hashes or normalized evidence text. When deterministic checks are not possible, the memory remains `unverified` or `in_review` rather than pretending to be verified.

## Branch and workspace scope enforcement

Branch-scoped memory is eligible only when the current branch matches the memory branch or an explicit compatibility rule says the branch is equivalent. Workspace-scoped identities cannot be reused across repositories without re-resolution. User and organization memories still need workspace safety filters before retrieval.

Scope leakage is a production safety failure. APIs that list memory, event traces, proposals, or verification results must apply the same scope checks as assistant workflow retrieval.

## Time-bound expiry

Time-bound memory carries an expiry timestamp, duration, or freshness key. Expired memory moves to `expired` and is excluded from trusted guidance. Refreshing expired memory requires new evidence and a fresh verification event; it should not merely reset a timestamp without proof.
