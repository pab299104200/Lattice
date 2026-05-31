# Memory Model Reference

This is the authoritative memory graph reference for the cognitive workspace successor. It implements [## Memory Graph](../plans/2026-05-16-cognitive-workspace-fork-plan.md#memory-graph), extends the schema detail in [## Required Fields](./2026-05-16-memory-graph-schema.md#required-fields), and supports the Phase 11 documentation requirement in [## Documentation Requirements](../plans/2026-05-16-cognitive-workspace-fork-plan.md#documentation-requirements).

## Memory classes

Memory records are typed claims or episodes. The memory class controls review expectations, consolidation behavior, and retrieval trust.

| Class | Purpose |
|---|---|
| `Observation` | A factual note from code, docs, test output, or user statement. |
| `Decision` | A chosen design or implementation direction. |
| `Constraint` | A rule that limits future changes. |
| `Pattern` | A repeated successful implementation or workflow shape. |
| `AntiPattern` | A repeated shape to avoid. |
| `WorkflowOutcome` | A completed task result with verification evidence. |
| `FailurePattern` | A recurring failure mode and known response. |
| `Procedure` | A repeatable operational or coding sequence. |
| `Preference` | A durable user or organization preference. |
| `ArchitectureInvariant` | A structural truth that should remain stable. |
| `DocsContract` | A documentation-backed contract that implementation must honor. |
| `OpenQuestion` | A tracked uncertainty that should not be treated as fact. |
| `CounterMemory` | A first-class counter-claim that asserts a prior memory is wrong or no longer applicable. It carries its own evidence, scope, lifecycle, and verification state, distinct from a `contradicts` link between two existing memories. |

## Memory record fields

Every record must carry the fields below. Type names describe the persisted contract, not a single Rust struct.

| Field | Type | Nullable | Default | Notes |
|---|---|---:|---|---|
| `id` | stable memory identity | no | generated | Used by links, evidence, access history, and MCP expansion handles. |
| `content` | string | no | none | Human-readable assertion or episode summary. |
| `memory_class` | enum | no | none | One of [## Memory classes](#memory-classes). |
| `assertion_type` | enum/string | no | mirrors class when unspecified | Lets tools distinguish observation, decision, procedure, outcome, preference, and counter claims. |
| `scope` | enum | no | `session` for legacy observation writes | See [## Scope semantics](#scope-semantics). |
| `verification_status` | enum | no | `unverified` | See [## Verification status state machine](#verification-status-state-machine). |
| `trust_status` | enum/string | derived | derived | Response-only trust tier: `trusted`, `advisory`, or `stale`. |
| `trust_reason` | enum/string | derived | derived | Response-only reason such as `verified`, `unverified`, `verification_in_review`, `missing_evidence`, or `git_head_changed`. |
| `risk_domains` | string list | derived | empty list | Response-only high-risk tags such as `security`, `tenancy`, `migration`, `deploy`, `dependency`, and `test_suite`. |
| `requires_reverification` | boolean | derived | false | Response-only flag requiring a current-code re-check before relying on high-risk or stale memory. |
| `reverification_reason` | enum/string | derived | `not_high_risk` | Response-only reason for the re-verification requirement. |
| `confidence` | decimal 0.0-1.0 | no | none | Required for durable memory writes. |
| `confidence_reason` | string | no | none | Explanation for the confidence score. |
| `freshness_policy` | enum | no | scope-dependent | See [## Freshness policy](#freshness-policy). |
| `validity_conditions` | string list | yes | empty list | Conditions under which the memory can be trusted. |
| `invalidation_triggers` | string list | yes | empty list | Conditions that force review or invalidation. |
| `provenance_events` | event identity list | yes | empty list | Event-backed origin trail. |
| `evidence_references` | evidence identity list | yes | empty list | See [## Memory evidence](#memory-evidence). |
| `linked_files` | file identity list | yes | empty list | Stable file links. |
| `linked_symbols` | symbol identity list | yes | empty list | Stable symbol links. |
| `linked_docs` | doc or section identity list | yes | empty list | Documentation evidence and contracts. |
| `linked_tests` | test identity list | yes | empty list | Verification or regression coverage. |
| `linked_memories` | memory identity list | yes | empty list | Related memory records. |
| `contradiction_links` | memory link list | yes | empty list | First-class relation records. |
| `supersession_links` | memory link list | yes | empty list | First-class relation records. |
| `access_history` | access event summary | yes | empty list | Read and use history for scoring. |
| `usefulness_scores` | metric map | yes | empty map | Retrieval and later-use metrics. |
| `last_verified_state` | verification report reference | yes | null | Last persisted verification output. |
| `checkout_state` | object | derived | derived | Response-only recorded/current Git HEAD comparison. New memory writes add `git_head_ref` and `git_head_oid` provenance entries when the workspace is a Git checkout. |
| `recheck_commands` | string list | derived | empty list | Response-only bounded command suggestions derived from linked tests, files, docs, symbols, and evidence references. |

`trust_status` is intentionally louder than `verification_status`. A memory can be `verified` but still advisory when it has no evidence or when its recorded Git HEAD no longer matches the current checkout. Assistants must treat advisory memory as a hypothesis or historical clue until current code, docs, and tests confirm the claim. `recheck_commands` are convenience probes for that confirmation step; they are not verification evidence until executed and reviewed. High-risk domains require explicit current-code confirmation unless the memory is verified, evidence-backed, tied to the current checkout, and has a persisted verification timestamp.

## Memory link types

Memory links are first-class records with source memory, target memory or graph node, link type, strength, reason, evidence event, creator, creation time, and verification status.

| Link type | Reciprocal rule |
|---|---|
| `supports` | Reciprocal support may be added only when both records independently support each other. |
| `contradicts` | Symmetric; reverse edge is equivalent for conflict discovery. |
| `supersedes` | Directional from newer or stronger memory to older memory. Reverse relation is implied as superseded-by, not stored as another `supersedes` edge. |
| `refines` | Directional from narrower or improved claim to broader prior claim. |
| `generalizes` | Directional from broad claim to specific claim set. |
| `specializes` | Directional inverse of `generalizes`; store the edge that best matches the authored reason. |
| `co_occurs_with` | Symmetric; reverse edge may be materialized for query speed but must share provenance. |
| `derived_from` | Directional from derived memory to source memory or event. |
| `applies_to` | Directional from memory to graph node, scope, procedure, or workflow. |
| `validated_by` | Directional from memory to evidence, test, event, or verification result. |
| `invalidated_by` | Directional from memory to evidence, event, counter-memory, or verification result that invalidated it. |

## Scope semantics

Scopes determine visibility, retrieval eligibility, review requirements, and invalidation blast radius:

- `session`: visible only to the current session or explicit session recall paths.
- `branch`: visible only when workspace and branch match or when a compatibility policy explicitly maps branches.
- `repo`: visible across branches inside the same workspace/repository when verification permits.
- `user`: visible across workspaces for the same user when not blocked by workspace-boundary rules.
- `organization`: highest-scope memory; requires review discipline and must not be silently rewritten by consolidation.

Higher scopes require stronger evidence and stricter review. Branch and workspace scope enforcement is part of [Verification Freshness Design](./2026-05-16-verification-freshness-design.md#branch-and-workspace-scope-enforcement).

## Verification status state machine

Statuses are defined by [## Verification Engine](../plans/2026-05-16-cognitive-workspace-fork-plan.md#verification-engine):

- New memory starts as `unverified` unless it is created from a completed verification workflow.
- `unverified` can move to `verified`, `in_review`, `stale`, `contradicted`, `superseded`, `expired`, or `invalidated`.
- `verified` moves to `stale` when linked artifacts change or evidence can no longer be confirmed.
- `verified` moves to `contradicted` when a verified contradiction or counter-memory applies.
- `verified` moves to `superseded` when a stronger successor memory is applied.
- `in_review` marks a proposal or uncertain state that needs human or deterministic resolution.
- `expired` applies to time-bound memory after its expiry condition.
- `invalidated` is terminal for normal retrieval; a refreshed memory should preserve provenance or supersede the invalidated record rather than erasing it.

## Freshness policy

Freshness policy tells verification when to re-check trust:

- `session_scoped`: valid only for the current task/session.
- `branch_scoped`: re-check when branch changes or linked branch artifacts change.
- `repo_scoped`: re-check on linked graph/doc/test changes in the workspace.
- `time_bound`: expires at a recorded time or duration.
- `manual_review`: remains in review until explicitly accepted, refreshed, or invalidated.

Freshness interacts with retrieval ranking through [Retrieval Ranking Design](./2026-05-16-retrieval-ranking-design.md#ranking-signals).

## Validity conditions and invalidation triggers

Validity conditions describe when a memory may be used. Invalidation triggers describe when a memory must be rechecked or removed from trusted guidance. Examples include "only for branch `main`", "only while file identity X exists", "until MCP compatibility phase boundary", or "invalid if schema field Y changes".

These fields are required for high-scope memories because replay and consolidation need explicit guardrails. They are not substitutes for verification; they are inputs to verification and retrieval.

## Memory evidence

Evidence records connect memory content to files, symbols, docs, tests, events, exact text spans, user corrections, command outputs, or verification reports. Evidence must include enough identity and summary data to explain why the memory exists without injecting full event streams into prompts.

Exact-span evidence should be validated when possible. Large payload evidence should reference payload hashes or spillover locations rather than copying full content into the memory row.

## Memory access history and usefulness scores

Access history records retrievals, expansions, later use, ignored candidates, user corrections, and workflow outcomes. Usefulness scores are derived from that history and feed the Phase 9 metrics in [## Required metrics](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-9-metrics-and-evaluation).

Scores must remain explainable. A memory can be useful because it was later expanded, because it predicted relevant tests, because it reduced discovery calls, or because it prevented repeated failure. Low-use or contradicted memory can be demoted by consolidation, but high-scope changes must preserve provenance.
