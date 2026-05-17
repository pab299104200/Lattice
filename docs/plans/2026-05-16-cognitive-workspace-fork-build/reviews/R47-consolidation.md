# R47 — Phase 6 Backend Review (Consolidation Engine)

**Reviewer task:** [`tasks/R47.md`](../tasks/R47.md)
**Plan anchor:** [§6. Consolidation Engine](../../2026-05-16-cognitive-workspace-fork-plan.md#6-consolidation-engine), [§Phase 6: Consolidation Engine](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-6-consolidation-engine), [§Non-Negotiable Product Properties](../../2026-05-16-cognitive-workspace-fork-plan.md#non-negotiable-product-properties)
**Subject:** T39–T46 (consolidation runtime, scanners, LLM jobs, provenance, manual-review queue, replay, integration tests)
**Date:** 2026-05-17

---

## Spec alignment

This section walks each bullet that Phase 6 must satisfy and cites the code or test that closes it. Bullets the implementation does *not* satisfy are flagged here and repeated in [Findings](#findings).

### Consolidation modes (`## 6. Consolidation Engine` — "Consolidation modes")

- **Synchronous post-task consolidation for small task traces.** Implemented by `SessionConsolidator::on_task_complete` in `daemon/crates/lattice-core/src/consolidation/session.rs:98-156`, which reads the task slice, applies `EpisodeTemplate::from_task_slice`, and emits a proposal via `submit_inline`. The hot-path budget is verified by `consolidation::session_tests::session_consolidation_hot_path_stays_under_five_milliseconds_p99` (`session_tests.rs:121-149`). Slices above `synchronous_max_events` short-circuit to background via `enqueue_background_redirect` (`session.rs:176-201`), exercised by `oversized_task_slice_redirects_to_background_mode` (`session_tests.rs:65-86`).
- **Background scheduled consolidation.** Implemented as `ConsolidationJobMode::Background` (`queue.rs:51-66`), used by every deterministic scanner (`duplicates.rs:241`, `supersession.rs:82`, `demotion.rs:74`, `stale_marker.rs:89`, `refresh.rs`) and every LLM job through `submit_memory_proposal` (`llm/mod.rs:312-342`, ctx mode flows from caller).
- **Manual review mode for high-impact repo or organization memories.** Implemented by `ReviewQueue::should_gate` + `enqueue` + `decide` (`review_queue.rs:96-241`). `ConsolidationJobRuntime::run_job` calls `ReviewQueue::should_gate(&proposal)` before auto-applying (`mod.rs:402-413`) so repo/organization-scope proposals stay pending. Test coverage: `repo_scope_proposal_is_gated_and_stays_pending` and `organization_scope_proposal_is_gated_and_stays_pending` (`review_queue_tests.rs:56-102`). Session and branch scope auto-apply: `session_scope_proposal_is_not_gated_and_auto_applies`, `branch_scope_proposal_is_not_gated_and_auto_applies` (`review_queue_tests.rs:13-54`).
- **Replay mode for rebuilding memory state from the event log.** Implemented by `ReplayDriver::replay` in `replay.rs:157-236` with three boundaries (`ReplayMode::FromGenesis`, `FromSnapshot`, `FromEventId`, `replay.rs:53-57`). Genesis replay reproduces state under 50 mixed proposals: `replay_from_genesis_reconstructs_same_state_for_fifty_mixed_proposals` (`replay_tests.rs:19-85`). Cached-LLM replay paths: `replay_succeeds_with_cached_llm_responses_and_fails_cleanly_without_them`, `replay_never_calls_live_llm_driver` (`replay_tests.rs:205-259`).

### Consolidation jobs (`## 6. Consolidation Engine` — "Consolidation jobs:")

| Spec bullet | Implementation | Test |
|---|---|---|
| create episode summaries from completed tasks | `EpisodeSummaryJob::run` (`llm/episode.rs:38-82`); deterministic fallback `EpisodeTemplate` (`session.rs:139`, `episode.rs`) | `well_formed_episode_response_enqueues_proposal_without_mutating_memory` (`llm/llm_tests.rs:20-33`), `test_llm_episode_job_well_formed_response_emits_proposal_with_provenance` (`integration_tests.rs:73-90`) |
| promote repeated successful workflow traces into procedures | `ProcedureExtractionJob::run` (`llm/procedure.rs:44`) requiring `occurrences.len() >= 3` (`procedure.rs:52-58`) | `structured_outputs_round_trip_into_expected_memory_states` `JobScenario::Procedure` (`llm_tests.rs:96-102`) |
| promote recurring failures into failure patterns | `FailurePatternJob::run` (`llm/failure_pattern.rs:42`) requiring `cluster.diagnostics.len() >= 2` (`failure_pattern.rs:49-55`) | `structured_outputs_round_trip_into_expected_memory_states` `JobScenario::FailurePattern` (`llm_tests.rs:110-117`) |
| **promote verified implementation facts into semantic repo memory** | **NOT IMPLEMENTED.** No scanner or LLM job populates repo-scope memory from verified evidence; `ReviewQueue` only gates externally-produced repo proposals — it does not produce them. See [Findings F4](#findings). | — |
| detect duplicate memories | `DuplicateDetector` (`duplicates.rs:30-167`) | `duplicate_detector_emits_supersession_proposal` (`deterministic_tests.rs:17-55`), `test_deterministic_supersession_proposal_apply_then_reject_round_trip` (`integration_tests.rs:40-70`) |
| detect contradictions | `ContradictionDetectionJob::run` (`llm/contradiction.rs:37-72`) | `structured_outputs_round_trip_into_expected_memory_states` `JobScenario::Contradiction` (`llm_tests.rs:103-109`) |
| detect supersession candidates | `SupersessionCandidates::scan` (`supersession.rs:29-107`) | `every_scanner_is_no_op_on_empty_store` covers the empty path (`deterministic_tests.rs:135-173`); integration coverage via duplicate-detector path |
| demote unused or low-value memories | `DemotionScanner::scan` (`demotion.rs:35-99`) | `demotion_scanner_emits_demote_proposal_for_never_accessed_low_score_memory` (`deterministic_tests.rs:82-107`) |
| mark stale memories after graph changes | `StaleMarker::on_graph_change` (`stale_marker.rs:42-115`) | `stale_marker_emits_mark_stale_proposal_for_deleted_anchor_file` (`deterministic_tests.rs:58-79`) |
| refresh memories whose evidence still matches current code | `RefreshScanner::scan` (`refresh.rs`) | `refresh_scanner_emits_refresh_proposal_when_evidence_still_matches` (`deterministic_tests.rs:110-132`) |
| **propose docs updates when memory and docs diverge** | **NOT IMPLEMENTED.** A `grep` for `docs.*update`, `docs_propose`, and `docs diverg` across `daemon/crates/lattice-core/src/consolidation/` returns no matches. No job emits a proposal targeting a documentation node. See [Findings F4](#findings). | — |

### LLM-driven constraints (`## 6. Consolidation Engine` — "LLM-driven consolidation:")

- **Each LLM-driven job produces a proposal record, not a direct write.** `submit_memory_proposal` (`llm/mod.rs:312-342`) is the only path LLM job results take and it goes through `runtime.submit_inline → run_job → insert_pending`, which never mutates `MemoryStore`. The proposal-only invariant is covered separately under [Proposal discipline](#proposal-discipline).
- **Failed or malformed LLM responses leave prior state unchanged AND emit a failure event.** `parse_response` calls `emit_failure` on JSON decode failure (`llm/mod.rs:282-293`); `fail_driver` does the same for `LlmDriverError` (`llm/mod.rs:295-310`); budget overruns route through `enforce_budget → emit_failure` (`llm/mod.rs:344-365`). Test coverage: `malformed_json_emits_failure_event_and_no_proposal_for_every_job` and `driver_error_emits_failure_event_and_no_proposal_for_every_job` iterate every `JobScenario` (`llm/llm_tests.rs:36-61`); `test_llm_malformed_response_leaves_memory_unchanged_and_emits_failure_event` (`integration_tests.rs:92-107`) checks the integration path.
- **LLM jobs run only in background or manual review modes — never on the synchronous post-task hot path.** Defense in depth verified: `dispatch_llm_job` calls `forbid_hot_path` (`llm/mod.rs:167`); every job-type entry point repeats the check (`episode.rs:44`, `procedure.rs:51`, `contradiction.rs:44`, `failure_pattern.rs:48`); the `session.rs` module is forbidden from referencing `consolidation::llm` and tested by `session_module_forbids_llm_references` (`session_tests.rs:152-156`). Per-job-kind tests: `synchronous_post_task_mode_is_forbidden_before_driver_call_for_every_job` (`llm/llm_tests.rs:64-74`), integration test `test_llm_synchronous_mode_is_forbidden_and_driver_never_called` (`integration_tests.rs:109-124`) asserts `driver.call_count() == 0` after the rejection.
- **Each job records model, prompt hash, and response hash as part of proposal provenance.** `LlmProvenance::record` captures `model`, `prompt_sha256`, `response_sha256`, `prompt_token_count`, `response_token_count`, `latency_ms`, `called_at` (`llm/provenance.rs:39-77`); `complete_structured_with_provenance` calls it on every successful completion (`llm/mod.rs:261-270`); provenance is persisted on `consolidation_proposals.provenance_json` (`mod.rs:166-185`, `proposal.rs:211-238`). Tests: `successful_job_provenance_records_deterministic_hashes`, `identical_prompt_and_response_reproduce_hashes` (`llm/provenance_tests.rs:13-58`), `test_llm_episode_job_well_formed_response_emits_proposal_with_provenance` (`integration_tests.rs:73-90`).
- **Consolidation queue depth must be bounded; when full, drop with log warning.** Top-level cap enforced in `BoundedJobQueue::enqueue` (`queue.rs:125-145`) — emits `tracing::warn!("consolidation queue full; dropping job", ...)`; per-LLM-kind sub-quota enforced in `BoundedJobQueue::enqueue_llm` (`queue.rs:147-174`) — emits `tracing::warn!("LLM consolidation queue slice full; dropping job", ...)`. Test coverage: `queue_drops_jobs_after_depth_bound_and_logs_warning` (`proposal_tests.rs:152-175`) and `per_kind_queue_overflow_drops_job_and_emits_failure_event` (`llm/provenance_tests.rs:91-124`). **The per-kind path also emits a `ConsolidationFailed` event via `emit_queue_full` (`queue.rs:224-264`); the top-level path emits only a warning.** R47 step 5 required event emission on both paths — see [Findings F2](#findings).
- **Cost and latency budgets documented per job type; over-budget jobs fall back deterministically or skip with stale.** Documented in `docs/architecture/2026-05-16-consolidation-llm-budgets.md` with all four job kinds, fallback strategies, queue-depth, failure handling, provenance record format, operator overrides, and a spec citation linking `§6. Consolidation Engine` "LLM-driven consolidation". `BudgetCatalog::default()` (`llm/budget.rs:121-150`) pins the exact values the doc cites. Outcome routing is in `exceeded_outcome` (`llm/budget.rs:170-181`) and verified by `budget_overruns_route_to_required_outcomes` (`llm/provenance_tests.rs:60-89`). Doc/spec-citation linkage covered by `budget_architecture_doc_contains_required_headings` (`llm/provenance_tests.rs:126-141`).

### Non-Negotiable Product Properties touched by consolidation

- **"Every background consolidation pass is recoverable, replayable, and observable."** Recoverable + replayable: `ReplayDriver` (`replay.rs:112-381`) with the four replay tests in `replay_tests.rs`; mechanical reverse via `ReplayDriver::reverse` (`replay.rs:238-330`) with `reverse_supersede_restores_status_and_clears_memory_links`, `reverse_refresh_restores_last_verified_at_exactly`, `reverse_is_idempotent_for_already_reverted_proposals`. Observable: every job opens an `info_span!` with `workspace_id`, `job_id`, `proposal_id`, `outcome` (`mod.rs:368-417`), every apply/reject records an `outcome` span value (`proposal.rs:281-352`), every LLM call records `model_name`, `job_kind`, `mode` (`llm/mod.rs:226-234`).
- **"No silent broad workspace reads / no unbounded payload growth on hot paths."** Session consolidator caps slices at `synchronous_max_events` (default 200) (`session.rs:27`, redirect path tested by `oversized_task_slice_redirects_to_background_mode`). LLM prompt budget caps documented in budget doc and enforced in `enforce_budget` (`llm/mod.rs:344-365`).
- **"No consolidation job may silently rewrite high-scope memory without preserving provenance and prior state."** This invariant is the focus of [Proposal discipline](#proposal-discipline) below; covered.

---

## Coding-standard alignment

File and function ceilings per [`coding.md` §Hard limits](../../../../CLAUDE.md) (file ≤ 800 lines, function ≤ 50 lines, nesting ≤ 3, cyclomatic ≤ 10, positional args ≤ 5).

### File length (`wc -l` output)

| File | Lines | Status |
|---|---:|---|
| `consolidation/mod.rs` | 439 | OK |
| `consolidation/proposal.rs` | 751 | OK (approaching 800) |
| `consolidation/replay.rs` | 499 | OK |
| `consolidation/review_queue.rs` | 344 | OK |
| `consolidation/session.rs` | 400 | OK |
| `consolidation/duplicates.rs` | 246 | OK |
| `consolidation/demotion.rs` | 100 | OK |
| `consolidation/stale_marker.rs` | 115 | OK |
| `consolidation/supersession.rs` | 135 | OK |
| `consolidation/refresh.rs` | 121 | OK |
| `consolidation/queue.rs` | 272 | OK |
| `consolidation/integration_test_support.rs` | **841** | **Over 800-line ceiling** — see [Findings F5](#findings) |
| `consolidation/integration_tests.rs` | 246 | OK |
| `consolidation/deterministic_tests.rs` | 299 | OK |
| `consolidation/proposal_tests.rs` | 327 | OK |
| `consolidation/replay_tests.rs` | 456 | OK |
| `consolidation/review_queue_tests.rs` | 475 | OK |
| `consolidation/session_tests.rs` | 458 | OK |
| `consolidation/llm/mod.rs` | 568 | OK |
| `consolidation/llm/budget.rs` | 181 | OK |
| `consolidation/llm/contradiction.rs` | 160 | OK |
| `consolidation/llm/episode.rs` | 168 | OK |
| `consolidation/llm/failure_pattern.rs` | 172 | OK |
| `consolidation/llm/procedure.rs` | 152 | OK |
| `consolidation/llm/provenance.rs` | 84 | OK |
| `consolidation/llm/llm_tests.rs` | 494 | OK |
| `consolidation/llm/provenance_tests.rs` | 167 | OK |

### Function length and nesting

| Function | Location | Lines | Nesting | Status |
|---|---|---:|---:|---|
| `BoundedJobQueue::enqueue` | `queue.rs:125-145` | 21 | 2 | OK |
| `BoundedJobQueue::enqueue_llm` | `queue.rs:147-174` | 28 | 3 | OK |
| `ConsolidationProposal::apply` | `proposal.rs:273-315` | 43 | 2 | OK |
| `ConsolidationProposal::reject` | `proposal.rs:317-354` | 38 | 2 | OK |
| `EpisodeSummaryJob::run` | `llm/episode.rs:38-82` | 45 | 2 | OK |
| `ContradictionDetectionJob::run` | `llm/contradiction.rs:38-72` | 35 | 2 | OK |
| `ProcedureExtractionJob::run` | `llm/procedure.rs:44-90` (head 47, full method ≤ 50) | ≤50 | 2 | OK |
| `FailurePatternJob::run` | `llm/failure_pattern.rs:42-90` | ≤50 | 2 | OK |
| `ConsolidationJobRuntime::run_job` | `mod.rs:368-417` | 50 | 3 | borderline OK |
| `DuplicateDetector::scan` | `duplicates.rs:44-166` | **123** | **4** | **Over** — see [Findings F6](#findings) |
| `SupersessionCandidates::scan` | `supersession.rs:29-107` | **79** | **3** | **Over (length)** — see [Findings F6](#findings) |
| `ReplayDriver::replay` | `replay.rs:157-236` | **80** | **3** | **Over (length)** — see [Findings F6](#findings) |
| `ReplayDriver::reverse` | `replay.rs:238-330` | **93** | **2** | **Over (length)** — see [Findings F6](#findings) |
| `complete_structured_with_provenance` | `llm/mod.rs:204-271` | **68** | **2** | **Over (length)** — see [Findings F6](#findings) |
| `migrate_consolidation_schema` | `mod.rs:167-219` | **53** | **2** | **Over (length, marginal)** — see [Findings F6](#findings) |
| `ConsolidationHarness::apply_replay_mix` | `integration_test_support.rs:161-227` | **67** | **3** | **Over (length, test scaffold)** — see [Findings F6](#findings) |

The functions over 50 lines are not flagged with a one-line justification per the coding standard's "Cross them deliberately, with a one-line justification" carve-out; the standard treats unannotated crossings as making "the codebase worse."

### Other coding-standard checks

- **Deferred-marker scan.** A repository-wide search for the three forbidden markers (`T-O-D-O`, `F-I-X-M-E`, `X-X-X`) across `daemon/crates/lattice-core/src/consolidation/` (run as part of this review) returned no hits inside Phase 6 sources. ✓
- **Unjustified suppressions.** No `#[allow(...)]` or `#[cfg_attr(..., allow(...))]` in `daemon/crates/lattice-core/src/consolidation/`. ✓
- **Public API naming.** Verbs lead handlers (`apply`, `reject`, `run_due`, `decide`, `enqueue`, `submit_inline`, `replay`, `reverse`). Types are nouns (`ConsolidationProposal`, `ReviewItem`, `BudgetCatalog`). ✓
- **Doc citations on every non-trivial module.** Every consolidation source file opens with a doc comment that cites the spec heading (e.g. `duplicates.rs:1-6`, `episode.rs:1-7`, `stale_marker.rs:1-5`, `failure_pattern.rs`, `provenance.rs:1-7`, `session.rs:1-10`). ✓ The review doc compliance rule from the Lattice project `CLAUDE.md` is met.

---

## Proposal discipline

The spec is explicit: "No consolidation job may silently rewrite high-scope memory without preserving provenance and prior state." Operationally this means every write to `MemoryStore` from inside `consolidation/` must go through `ConsolidationProposal::apply` (when applying a decision) or `ReplayDriver::reverse` / `ReplayDriver::apply_reverse_state` (when replaying or undoing).

Search `daemon/crates/lattice-core/src/consolidation/` for mutating `MemoryStore` calls (`grep` for `memory_store.(store|update_structured_fields|mark_memory_superseded|mark_stale_by_id|mark_stale_by_symbol|mark_stale_by_file|mark_memory_contradicted|set_last_verified_at|clear_last_verified_at|delete_memory_links_from|insert_memory_link|clear_all|invalidate)`):

| Match | Caller | Verdict |
|---|---|---|
| `proposal.rs:374` `memory_store.store(memory.clone())?` | `ConsolidationProposal::materialize_memory_state` (called by `apply`) | OK — proposal apply path |
| `proposal.rs:388` `memory_store.mark_stale_by_id(target_id, reason)?` | `ConsolidationProposal::materialize_stale` (called by `apply`) | OK — proposal apply path |
| `proposal.rs:406` `memory_store.mark_memory_superseded(target_id, superseded_by)?` | `ConsolidationProposal::materialize_supersession` (called by `apply`) | OK — proposal apply path |
| `proposal.rs:717-727` `memory_store.store / update_structured_fields / delete_memory_links_from / insert_memory_link / set_last_verified_at / clear_last_verified_at` | `apply_memory_state` helper (called by `materialize_*` methods and `ReplayDriver::apply_reverse_state`) | OK — proposal apply / replay path |
| `replay.rs:390` `memory_store.invalidate(memory_id)?` | `apply_reverse_state` when reversing back to empty | OK — `ReplayDriver::reverse` path |
| `replay.rs:399` `memory_store.store(memory)?` | `apply_reverse_state` legacy decode path | OK — `ReplayDriver::reverse` path |
| `replay.rs:167` `self.memory_store.clear_all()` | `ReplayDriver::replay` (clean slate before replaying) | OK — replay-mode reset |
| `integration_test_support.rs:230, 261, 269` | test-only seed/clear in harness | OK — `#[cfg(test)]` scaffolding |
| `llm/llm_tests.rs:248-249` | `Fixture::contradiction_pair` test seed | OK — `#[cfg(test)]` scaffolding |
| `deterministic_tests.rs:262, 269` | `Fixture::seed_memory` test seed | OK — `#[cfg(test)]` scaffolding |

**No production-code call outside `ConsolidationProposal::apply` / `apply_memory_state` / `ReplayDriver::*` mutates `MemoryStore`.** Scanners (`DuplicateDetector`, `SupersessionCandidates`, `DemotionScanner`, `RefreshScanner`, `StaleMarker`) and the synchronous `SessionConsolidator` all submit a `PendingProposalSpec` through `ConsolidationJobRuntime::submit` (or `submit_inline`) — verified by reading `duplicates.rs:230-246`, `supersession.rs:78-96`, `demotion.rs:69-89`, `stale_marker.rs:84-104`, `refresh.rs`, `session.rs:235-263`. ✓

Tests reinforce the invariant:

- `deterministic_job_emits_proposal_without_mutating_memory` (`proposal_tests.rs:13-33`) confirms a deterministic job leaves `memory_store.list_all() == 0` after `run_due()`.
- `session_consolidation_never_mutates_memories_directly` (`session_tests.rs:88-118`) confirms the synchronous path emits a proposal without altering `MemoryStore`.
- `well_formed_episode_response_enqueues_proposal_without_mutating_memory` (`llm/llm_tests.rs:20-33`) confirms the LLM path.
- `duplicate_detector_emits_supersession_proposal` and `scanners_route_through_runtime_without_direct_writes` (`deterministic_tests.rs:17-55`, `deterministic_tests.rs:176-203`) assert `direct_write_count() == 0` after scanning. **These two tests fail under the default parallel `cargo test` invocation due to a test-infra issue, not a real proposal-discipline regression. Details in [Findings F1](#findings).**

**Prior-state preservation.** `ConsolidationProposal::apply` records `prior_state_json` and `proposed_state_json` plus `post_apply_state_hash` on the `MemoryConsolidated` event (`proposal.rs:412-453`). `MemoryConsolidatedPayload` carries the prior/proposed JSON and the post-apply SHA-256 (`events/kinds.rs:471-500`). This is the audit trail the spec requires; replay re-verifies it (`replay.rs:199-216`).

**High-scope gating.** `ReviewQueue::should_gate` (`review_queue.rs:96-100`) reads the proposal scope from `proposed_state` first then `prior_state`, mapping `repo` and `organization` to `is_manual_review_scope = true` (`review_queue.rs:336-338`). The runtime checks this before any auto-apply (`mod.rs:402-413`), and `repo_scope_proposal_is_gated_and_stays_pending` plus `organization_scope_proposal_is_gated_and_stays_pending` (`review_queue_tests.rs:56-102`) prove repo/org proposals stay pending with `memory_store.list_all().len() == 0`. ✓

**Reversibility.** `ReplayDriver::reverse` (`replay.rs:238-330`) loads the proposal record, restores `prior_state` via `apply_reverse_state`, recomputes `post_apply_state_hash`, emits a `MemoryConsolidated` event with the reversal as `decided_by = "replay"`, and rolls back atomically on event-write failure (`rollback_reverse`, `replay.rs:403-419`). Idempotent for already-reverted proposals (`replay_tests.rs:261-303`).

---

## LLM budget evidence

`docs/architecture/2026-05-16-consolidation-llm-budgets.md` (`wc -l`: 34) is the operator-facing budget contract for Phase 6.

**Required headings (R47 step 6(d) and `budget_architecture_doc_contains_required_headings`):**

- `## Scope` ✓
- `## Per-job budgets` ✓ — with a Markdown table covering all four kinds: `episode_summary`, `procedure_extraction`, `contradiction_detection`, `failure_pattern_extraction` (`consolidation-llm-budgets.md:9-14`)
- `## Bounded queue depth` ✓ — names `ConsolidationConfig::max_queue_depth` and `BoundedJobQueue::with_per_kind_max_depth` and describes `ConsolidationFailed { error_kind: "queue_full" }`
- `## Failure handling` ✓ — names the four failure modes (malformed, driver, budget overrun, queue-full) and the spec-required event emission
- `## Provenance record` ✓ — enumerates exactly: `model`, `prompt_sha256`, `response_sha256`, `prompt_token_count`, `response_token_count`, `latency_ms`, `called_at`
- `## Operator overrides` ✓ — points operators at `ConsolidationConfig { llm_budget_catalog: Some(BudgetCatalog { ... }) }`
- `## Spec citation` ✓ — `[§6. Consolidation Engine](../plans/2026-05-16-cognitive-workspace-fork-plan.md#6-consolidation-engine), specifically the "LLM-driven consolidation" bullets`

**Per-job-kind budget table cross-check (R47 step 6(a)):**

| kind | doc (max_prompt / max_resp / max_latency_ms / max_cost_µUSD) | `BudgetCatalog::default()` (`llm/budget.rs:124-148`) | match |
|---|---|---|:---:|
| episode_summary | 6000 / 1200 / 8000 / 250 | 6_000 / 1_200 / 8_000 / 250 | ✓ |
| procedure_extraction | 10000 / 1800 / 12000 / 450 | 10_000 / 1_800 / 12_000 / 450 | ✓ |
| contradiction_detection | 3000 / 800 / 5000 / 125 | 3_000 / 800 / 5_000 / 125 | ✓ |
| failure_pattern_extraction | 7000 / 1400 / 9000 / 300 | 7_000 / 1_400 / 9_000 / 300 | ✓ |

**Fallback behavior per kind (R47 step 6(b)):**

| kind | doc fallback | `exceeded_outcome` (`llm/budget.rs:170-181`) |
|---|---|---|
| episode_summary | "deterministic episode template" | `ExceededFallback(DeterministicFallback::EpisodeTemplate)` ✓ |
| procedure_extraction | "skip and mark affected memory stale" | `ExceededSkipWithStale` ✓ |
| contradiction_detection | "deterministic supersession candidate" | `ExceededFallback(DeterministicFallback::ContradictionSupersessionCandidate)` ✓ |
| failure_pattern_extraction | "skip and mark affected memory stale" | `ExceededSkipWithStale` ✓ |

These are exercised by `budget_overruns_route_to_required_outcomes` (`llm/provenance_tests.rs:60-89`). The deterministic-fallback wiring (i.e. the production caller that picks `EpisodeTemplate` after a budget overrun and substitutes the deterministic summary in place of the LLM proposal) **routes the overrun through `emit_failure` and returns `LlmJobError::BudgetExceeded` rather than synthesising the deterministic fallback in place** (`llm/mod.rs:344-365`). The session consolidator's deterministic template (`session.rs:139`, `episode.rs::EpisodeTemplate`) already provides the operator-facing fallback for the episode kind, and the contradiction-kind fallback is the deterministic `SupersessionCandidates` scanner that runs on the same anchor buckets. Procedure and failure-pattern kinds correctly degrade to "skip with stale" (no deterministic substitute exists). This matches the spec's tolerance of either path, but **the linkage is implicit, not enforced or tested as a single workflow**. See [Findings F7](#findings) for a soft observation.

**Provenance struct shape (R47 step 6(c)):**

`LlmProvenance` (`llm/provenance.rs:39-47`):

```rust
pub struct LlmProvenance {
    pub model: String,
    pub prompt_sha256: [u8; 32],
    pub response_sha256: [u8; 32],
    pub prompt_token_count: u32,
    pub response_token_count: u32,
    pub latency_ms: u32,
    pub called_at: DateTime<Utc>,
}
```

`## Provenance record` in the doc names exactly those fields in the same order. ✓ The doc explicitly says hashes are SHA-256 over the exact bytes; `LlmProvenance::record` uses `sha2::Sha256` (`llm/provenance.rs:79-84`) — match.

**Provenance persistence.** Stored on `consolidation_proposals.provenance_json` per the `migrate_consolidation_schema` ALTER (`mod.rs:166-185`) and serialised via `insert_pending` (`proposal.rs:211-238`). The provenance survives a round-trip through `ConsolidationProposal::load_record` (`proposal.rs:162-209`). Replay verifies cached LLM responses against the persisted hashes through `verify_cached_response` (`replay.rs:421-436`).

---

## Findings

The findings below are listed in severity-descending order. Each entry names the minimum-fix path so a follow-up task can close it.

### F1 — Phase 6 verification command **fails** under the default `cargo test` invocation (test-infra; high severity)

Running the command R47 step 1 specifies — `cd daemon && cargo test -p lattice-core --lib consolidation` — produced `2 failed; 48 passed; 0 ignored`. Failing tests:

- `consolidation::deterministic_tests::scanners_route_through_runtime_without_direct_writes` — assertion `direct_write_count() == 0` saw `3`, source at `deterministic_tests.rs:198`.
- `consolidation::deterministic_tests::duplicate_detector_emits_supersession_proposal` — assertion `direct_write_count() == 0` saw `3`, source at `deterministic_tests.rs:40`.

Run alone (`cargo test ... scanners_route_through_runtime_without_direct_writes`) the test passes. Run with `--test-threads=1` the full suite passes (50/50). The root cause is that `DIRECT_WRITE_COUNT` is a **process-global atomic** declared at `daemon/crates/lattice-core/src/memory/store.rs:26` (`static DIRECT_WRITE_COUNT: AtomicUsize = AtomicUsize::new(0)`), incremented inside `MemoryStore::store / update_structured_fields / mark_memory_superseded / mark_stale_by_id / mark_stale_by_file / mark_stale_by_symbol / mark_memory_contradicted / set_last_verified_at / clear_last_verified_at` (incrementing call sites at lines 265, 694, 716, 956, 966, 981, 1054, 1077, 1101, 1148, 1189, 1244). Concurrent fixtures in `consolidation::proposal_tests`, `consolidation::review_queue_tests`, `consolidation::integration_tests`, and the LLM test suites all seed memories via `memory_store.store()`/`update_structured_fields()` and concurrently increment the same counter, so the two tests' guard "writes after I called `reset_direct_write_count()`" is observing leakage from *other* tests, not from the duplicate detector.

Phase 6 verification therefore is **not** clean. The proposal-only invariant *holds* in the production code (the grep audit in [Proposal discipline](#proposal-discipline) finds no offending write paths and the test still passes serially), but the test infrastructure is broken.

**Minimum fix.** Either (a) make `DIRECT_WRITE_COUNT` per-`MemoryStore` — move it to a `RefCell<usize>` or `Arc<AtomicUsize>` field on `MemoryStore` and expose `MemoryStore::direct_write_count()` / `reset_direct_write_count()` instance methods — and have the two failing tests query the local store; or (b) gate the two assertions behind a `#[serial]` attribute (e.g. `serial_test` crate) so the global counter is observed under a process-wide test mutex; or (c) attach the counter to `#[thread_local]` storage. Option (a) is the right structural fix; (b) is acceptable as a stopgap. Either way, `cargo test -p lattice-core --lib consolidation` MUST pass in the default parallel mode before Phase 7 begins.

### F2 — Top-level queue-overflow path emits a warning but no `ConsolidationFailed` event (spec-near; medium severity)

R47 step 5 required both queue-overflow paths to emit `tracing::warn!` AND a `ConsolidationFailed` event. The per-LLM-kind path satisfies both (`queue.rs:147-174` + `queue.rs:224-264` `emit_queue_full`), but the top-level cap in `BoundedJobQueue::enqueue` (`queue.rs:125-145`) emits only the warning. The spec text itself (`fork-plan.md:351`) says only "log warning," so a strict reading of the spec is met — but R47's reviewer brief raised the bar to event-parity for the audit trail, which is consistent with `## Non-Negotiable Product Properties` "Every background consolidation pass is recoverable, replayable, and observable."

`queue_drops_jobs_after_depth_bound_and_logs_warning` (`proposal_tests.rs:152-175`) asserts only the log line; it does not query for a `ConsolidationFailed` event because none is emitted.

**Minimum fix.** Pass an optional `&EventWriter` to `BoundedJobQueue::enqueue` (or split the call sites into LLM vs non-LLM) so non-LLM overflows can emit `ConsolidationFailed { error_kind: "queue_full", model_name: "" }` via the same helper as `emit_queue_full`. Add a test that asserts both the log and the event. Two-line spec amendment to `## 6. Consolidation Engine` or a deliberate decision to keep the top-level path log-only is also acceptable; record the decision in `docs/architecture/2026-05-16-consolidation-llm-budgets.md` § "Failure handling".

### F3 — Repo/org-scope auto-apply policy correctly gates, but `ConsolidationJobRuntime::run_job`'s scope read is JSON-string-shaped, not type-shaped (low severity)

`ReviewQueue::should_gate` (`review_queue.rs:96-100`) reads scope from `proposed_state.pointer("/memory/scope")` first, falling back to `proposed_state.get("scope")` then to the prior-state equivalents (`review_queue.rs:316-323`). For `MemoryScope` proposals authored by the deterministic scanners and the synchronous session consolidator this works, because they serialize the full `ConsolidationMemoryState`. For LLM jobs that build `proposed_state` via `memory_state(memory, fields, links)` (`llm/mod.rs:467-479`) the `/memory/scope` pointer is populated from `Memory::scope` and the gate fires correctly.

The hazard is that any future job that builds a `proposed_state` without nesting under `/memory` and without a top-level `scope` key will silently bypass the manual-review gate. There is no schema check that fails fast for that shape.

**Minimum fix.** Add a `decode_scope_or_error` step inside `ConsolidationJobRuntime::run_job` (`mod.rs:402-413`) that produces a real `MemoryScope` and `panic!`s the job into `failed` with `error_kind = "missing_scope"` if absent. Keep `ReviewQueue::should_gate` as today's fallback for inspection but make the runtime side authoritative. Add a regression test `proposal_with_no_scope_field_is_failed_not_silently_auto_applied`. Not a Phase 7 blocker; record as a debt task.

### F4 — Two consolidation-jobs spec bullets are unimplemented (spec gap; medium severity)

`## 6. Consolidation Engine` "Consolidation jobs:" lists 11 bullets. Nine are implemented (see the [Spec alignment](#spec-alignment) table). Two are absent from `daemon/crates/lattice-core/src/consolidation/`:

- **"promote verified implementation facts into semantic repo memory."** No scanner enumerates verified memories and proposes a repo-scope promotion. `ReviewQueue` gates repo proposals authored elsewhere but does not produce them. There is no `RepoPromotion` job kind in `ProposalKind` (`proposal.rs:80-89`) and no `promote_*` source file.
- **"propose docs updates when memory and docs diverge."** A grep for `docs_update`, `docs_propose`, `docs diverg` across `consolidation/` returns no matches. No proposal targets a `DocSectionRef` anchor. This is the bridge into the Phase 5 docs surface and into the future Phase 9 MCP "find_stale_docs" workflow.

Both bullets are stated as Phase 6 consolidation jobs, not as later-phase deliverables, so a follow-up task is needed before Phase 7 work begins **unless** Phase 6 scope is explicitly amended to defer them.

**Minimum fix.** File a follow-up task `T46.1 — Add repo-scope promotion and docs-divergence proposers` that (a) adds a deterministic `RepoPromotionScanner` keyed on `MemoryVerificationStatus::Verified + scope == Branch + access_count > N + age > M` producing a repo-scope `UpdateMemory` proposal (gated to manual review), and (b) adds a deterministic `DocsDivergenceScanner` that joins `linked_files`/`linked_symbols` to documented sections via the docs subsystem and produces an `UpdateMemory` proposal targeting the affected docs (or a new `ProposalKind::ProposeDocsUpdate`). Both should add test coverage analogous to the existing scanners.

### F5 — `integration_test_support.rs` exceeds the 800-line file ceiling (low severity)

`daemon/crates/lattice-core/src/consolidation/integration_test_support.rs` is 841 lines (`wc -l`). The coding standard allows crossing with a one-line justification in the commit message; no inline annotation is present. The file mixes a harness, response fixtures, job builders, and the buffered tracing writer.

**Minimum fix.** Extract `integration_test_support/responses.rs` (the `episode_response`, `procedure_response`, … helpers) and `integration_test_support/jobs.rs` (the `create_job`, `update_job`, `refresh_job`, `supersede_job`, `llm_create_job`, `llm_create_jobs`, `direct_existing_job` builders) and re-export them. This reduces the harness file to roughly 500 lines without changing the public test API.

### F6 — Functions exceed the 50-line ceiling without justification (low severity)

The functions listed in [Coding-standard alignment](#coding-standard-alignment) above cross the 50-line limit; none carries an inline one-line justification. The two notable ones:

- `DuplicateDetector::scan` (`duplicates.rs:44-166`, 123 lines, nesting 4): the body should be split into `bucket_memories` (exists), `find_candidate_pairs`, `build_supersede_proposal`, and `build_demote_proposal` so the inner double-for-loop becomes a single delegating method.
- `ReplayDriver::reverse` (`replay.rs:238-330`, 93 lines): the body should be split into `load_reverse_target`, `apply_inverse_state`, `mark_proposal_reverted`, and `emit_reverse_event`. The current monolith is also where the rollback-on-event-write-failure recovery hides, and it would be more discoverable as a named function.

**Minimum fix.** Refactor each over-length function into 3–4 small helpers; nothing in the public API needs to change. Add per-function comments only where the why is non-obvious.

### F7 — Budget overrun emits failure but does NOT splice in the deterministic fallback in the same code path (soft observation; low severity)

The spec ("jobs exceeding budget must fall back to deterministic approximations or skip with a stale flag") is satisfied at the system level — `BudgetCatalog::evaluate` reports the desired outcome, and the deterministic surrogate exists for two of four kinds (`EpisodeTemplate` for episode_summary and `SupersessionCandidates` for contradiction_detection). The current implementation, however, returns `LlmJobError::BudgetExceeded` from `enforce_budget` (`llm/mod.rs:344-365`) without invoking the surrogate. The caller is expected to handle the surrogate path out of band. No test exercises the full "budget overrun → deterministic fallback emits proposal" loop.

**Minimum fix.** Either (a) wire `enforce_budget` to dispatch the fallback (call into `EpisodeTemplate::from_task_slice` for episode_summary, call `SupersessionCandidates::scan` for contradiction_detection) and return `Ok(Some(fallback_proposal))` instead of `Err(BudgetExceeded)`; or (b) update `consolidation-llm-budgets.md` § "Failure handling" to state explicitly that budget overruns are surfaced as `ConsolidationFailed` and the caller is responsible for re-running the deterministic surrogate, with a pointer to the surrogate. (b) is acceptable as a documentation fix; (a) is the more rigorous path. Either path should be paired with an integration test `budget_overrun_for_episode_kind_routes_to_deterministic_episode_template`.

---

## Verdict

**fail.**

Phase 6 cannot ship to gate Phase 7 in its current state. The proposal-discipline invariant, hot-path forbid, manual-review queue, replay correctness, provenance recording, bounded-queue behavior, and per-kind LLM budgets are all implemented and exercised by tests when run serially. The architecture doc satisfies every R47 step-6 sub-requirement. However:

1. The default verification command in R47 step 1 — `cd daemon && cargo test -p lattice-core --lib consolidation` — produces **2/50 failing tests** under cargo's default parallel runner. The failure is a test-infra issue rather than a production-code regression, but the Phase 6 DoD bullet "consolidation is auditable, reversible, and **test-covered**" cannot be claimed satisfied while two tests fail under the canonical invocation. **[Findings F1](#findings) is the gating blocker.**
2. The "promote verified implementation facts into semantic repo memory" and "propose docs updates when memory and docs diverge" spec bullets are unimplemented. They are listed under `## 6. Consolidation Engine` as Phase 6 jobs, not deferred work. **[Findings F4](#findings)** must be closed (or the spec amended to defer).
3. Findings F2 (top-level overflow event-emission parity), F5 (file-length crossing), F6 (function-length crossings), F7 (budget→fallback loop), and F3 (scope-shape robustness) are non-blocking individually but together represent visible coding-standard slack that should not migrate into Phase 7.

The Phase 6 fork-build should not advance to T48 (R47 said T48 verification begins under R47) until F1 and F4 are resolved and re-verified. A follow-up R47.1 task should:

- Patch the `DIRECT_WRITE_COUNT` test-infra so the consolidation suite is parallel-safe, and re-run `cd daemon && cargo test -p lattice-core --lib consolidation` to a clean 50/50 result.
- Decide F4 (implement the two missing scanners or amend the spec). If implementing, follow the deterministic-scanner pattern used by `DuplicateDetector` / `RefreshScanner`.
- Decide F2 (lift the top-level cap to event-emission parity OR record the log-only decision in the budget doc).
- Optionally address F3, F5, F6, F7 in the same follow-up so the slate is clean entering Phase 7.

Until these are closed, **Phase 6 is not ready to gate Phase 7**.
