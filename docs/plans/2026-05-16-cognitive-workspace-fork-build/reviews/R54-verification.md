# R54 — Phase 7 Backend Review (Verification & Freshness)

**Plan:** [docs/plans/2026-05-16-cognitive-workspace-fork-plan.md](../../2026-05-16-cognitive-workspace-fork-plan.md)
**Scope:** T48 (verifier core + existence), T49 (span validation), T50 (scope enforcement), T51 (expiry + incremental), T52 (surfacing + label discipline), T53 (integration tests).
**Inputs audited:** `daemon/crates/lattice-core/src/verification/**`, `daemon/crates/lattice-core/src/memory/store.rs`, `daemon/crates/lattice-core/src/events/{mod.rs,kinds.rs}`.
**Verification command:** `cd daemon && cargo test -p lattice-core --lib verification` — **36 passed / 1 failed / 0 ignored** (`verification::integration_tests::test_scope_leak_blocked_at_store_boundary_for_all_scopes` panics on `assertion failed: log_output.contains("scope_leak_blocked")`, at `daemon/crates/lattice-core/src/verification/integration_tests.rs:139`).

## Spec alignment

| Spec clause | Where addressed | Evidence |
|---|---|---|
| §8 "linked files still exist" | `verification/existence.rs:494-513` `FileExistenceCheck` | `existence_tests.rs:25-56` `live_file_evidence_emits_verified_proposal`, `existence_tests.rs:58-82` `deleting_file_emits_invalidated_proposal` |
| §8 "linked symbols still exist" | `existence.rs:515-553` `SymbolExistenceCheck` | `existence_tests.rs:84-110` `deleted_symbol_emits_invalidated_proposal` |
| §8 "cited docs still exist" | `existence.rs:555-591` `DocSectionExistenceCheck` | `existence_tests.rs:112-134` `existing_doc_section_emits_verified_proposal` |
| §8 "linked tests still exist" | `existence.rs:593-643` `TestExistenceCheck` | `existence_tests.rs:136-163` `deleted_test_emits_invalidated_proposal` |
| §8 "evidence text still matches when exact spans were captured" | `verification/spans.rs:198-320` `SpanValidator` | `spans_tests.rs:12-135` exercises `unchanged`, `modified`, `truncated`, `crlf_flip`, `no-span`, `bounded-read` paths |
| §8 "implementation still matches memory claim where deterministic checks are possible" | `existence.rs:225-266` calls span check after existence; `existence.rs:645-677` `aggregate_verdicts` returns `Unverified` for non-deterministic outcomes | Partial coverage — the deterministic surface is the span path; non-span "implementation matches claim" checks are explicitly downgraded to `Unverified` and there is no positive test that asserts that downgrade (see Findings #6). |
| §8 "contradicted/superseded states remain coherent" | Surfacing classifies both via `surfacing.rs:181-192` `classify_status`; contradiction is produced by a `ContradictionDetectionJob` not by the verifier | `integration_tests.rs:71-82` `test_contradicted_memory_never_appears_in_trusted_bundle` exercises the surfacing leg; no test or check asserts coherence between a memory's `superseded_by_memory_id` / `contradicted_by_memory_ids` and the live memory store (Findings #6). |
| §8 "branch-scoped memory is not leaking into unrelated branches" | `verification/scope_enforcement.rs:100-139` `allows` + `ScopeEnforcement::audit_memory`; `memory/store.rs:462-466,468-482,484-492` scoped readers + `store.rs:2173-2218` `enforce_scope_boundary` | `scope_leak_tests.rs:8-98` covers all four scopes (Branch / Repo / Organization / Session); `scope_leak_tests.rs:100-118` asserts `Invalidated`/`ScopeLeak` verdict |
| §8 "time-bound memory has expired" | `verification/expiry.rs:46-148` `ExpiryScanner` routes through `ProposalKind::MarkExpired` (line 120) via `runtime.submit_inline` / `decide` | `incremental_tests.rs:71-106` `expiry_scanner_marks_expired_and_is_idempotent`; `integration_tests.rs:84-96` `test_expired_memory_never_appears_in_trusted_bundle` |
| §8 outputs (`verified`, `unverified`, `in_review`, `stale`, `contradicted`, `superseded`, `expired`, `invalidated`) | `verification/mod.rs:91-153` `VerificationStatus` enum + `as_str` + `FromStr` (8 variants) | `surfacing_tests.rs:95-114` `classify_covers_all_verification_status_variants` exhaustively maps every variant; `surfacing_tests.rs:13-41` `verified_memories_land_in_trusted_and_all_other_states_are_advisory` |
| §8 "Verification must be incremental and tied to workspace changes. Large repos cannot tolerate full rescans" | `verification/incremental.rs:131-191` `on_graph_delta` walks only `find_impacted_memory_ids_for_graph_delta` (`memory/store.rs:991-998`); bounded by `work_budget`; debug-asserts upper bound via `count_memory_evidence_rows_for_graph_delta` (`store.rs:1000-1009`) | `incremental_tests.rs:23-69` `graph_delta_reverifies_only_impacted_memories` (1 of 3 impacted); `incremental_tests.rs:196-237` `bounded_work_budget_reenqueues_remaining_memories` (1000 impacted, budget 50, requeues 950); `integration_tests.rs:163-172` `test_incremental_verification_narrows_to_changed_files_only` (1 of 100 reverified). |
| §Phase 7 DoD "stale or contradicted memory cannot appear as normal trusted guidance" | `surfacing.rs:62-97` `partition` + `surfacing.rs:149-178` `enforce_trusted_invariant` (debug-assertions panic, release-build retain + error log) | `surfacing_tests.rs:43-58` `debug_assertion_fires_when_non_trusted_memory_is_injected_into_trusted`; `integration_tests.rs:55-110` four `*_never_appears_in_trusted_bundle` integration tests |
| §Phase 7 DoD "verification results are explainable and queryable" | `verification/spans.rs:56-80` `SpanMismatchReason` JSON payload; `existence.rs:307-340` `emit_proposal` records `verification_status` + `reason` in the proposal evidence JSON; verification jobs persisted in `verification_jobs` table (`existence.rs:367-491`) | `existence_tests.rs:166-184` `verification_schema_round_trips_under_runtime_migration`; `integration_tests.rs:175-192` `test_verifier_emits_explainable_label_reason_for_every_non_verified_state` walks every non-verified state's reason string |
| §Non-Negotiable Product Property: "Every stale or contradicted memory is surfaced as such, not hidden behind recency." | `surfacing.rs:99-101` `classify`; `surfacing.rs:118-128` `trusted_ranked_top_n` filters every non-`Verified` regardless of score | `surfacing_tests.rs:77-92` `ranker_top_n_excludes_every_non_verified_memory_regardless_of_score` |
| §Non-Negotiable Product Property: "No silent broad workspace reads that bypass ignore rules or workspace boundaries." | Scope filter is required at the store boundary; non-scoped reads are restricted to admin (`memory/store.rs:495-531` `query_unscoped_admin`) | `scope_leak_tests.rs:120-144` `debug_guard_panics_when_leaked_row_reaches_boundary` confirms a debug panic; `integration_tests.rs:113-140` exercises four scopes but fails the log assertion (see Findings #1). |
| §Risks Stale Memory Leakage control: "freshness indexes, graph-change-triggered verification, stale labels in all memory surfaces, and tests that stale memory cannot rank as trusted." | `memory/store.rs:99` `last_verified_graph_snapshot_id` column + `store.rs:251-253` index; incremental graph-delta path (`incremental.rs:131-191`); ranker `trusted_ranked_top_n` (`surfacing.rs:118-128`) | `incremental_tests.rs:109-148` `successful_verification_advances_last_verified_graph_snapshot_id`; `integration_tests.rs:55-69` `test_stale_memory_never_appears_in_trusted_bundle`; `integration_tests.rs:143-160` `test_ranker_excludes_non_verified_from_top_n` |
| §Risks Scope Leakage control: "scope-aware queries, enforced filters in store APIs, and negative tests." | `memory/store.rs:462-492` scoped readers; `store.rs:2173-2218` `enforce_scope_boundary` (debug panic / release error log + SQL event row); `scope_enforcement.rs:100-127` `allows` per `MemoryScope` variant | `scope_leak_tests.rs:8-98` negative tests across all four scopes; `scope_leak_tests.rs:146-182` `scope_filter_event_is_emitted_for_each_store_boundary_drop` |

## Coding-standard alignment

| Check | Status | Evidence |
|---|---|---|
| File length ≤800 lines | **2 violations** | `verification/existence.rs` is 803 lines (`wc -l`); `verification/integration_tests.rs` is 922 lines (`wc -l`). Neither file carries the one-line justification the coding standard requires for crossing the ceiling (`coding.md` §Hard limits). |
| Function length ≤50 lines | **2 violations** | `VerifierCore::verify_memory_inner` (`existence.rs:198-277`, 80 lines); `IncrementalVerifier::on_graph_delta` (`incremental.rs:131-191`, 61 lines). |
| Nesting ≤3 | **OK** | `verify_memory_inner` `for` over evidence → `match outcome` → arm bodies (depth 3); `on_graph_delta` `for budget` → `let Some(task)` (depth 2); `partition_for_caller` (`surfacing.rs:73-97`) `for memories` → `if` (depth 2). |
| No unjustified suppressions | **1 violation** | `incremental.rs:97` `#[allow(clippy::too_many_arguments)]` has no inline justification comment; coding standard requires "an inline one-line comment explaining *why the rule is wrong here*" (`coding.md` §No broken windows). Real fix: collapse the eight parameters of `IncrementalVerifier::new` into a `Services` / `Context` struct rather than suppress. |
| No `TODO` / `FIXME` / `XXX` / commented-out code in `verification/**` | **OK** | `grep -nE 'TODO|FIXME|XXX'` returns no matches. |
| Doc-heading citation discipline | **OK** | Every verification module file opens with a doc-comment citing `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md` §8 / §Phase 7 / §Non-Negotiable Product Properties / §Risks — see `verification/mod.rs:3-30`, `scope_enforcement.rs:3-23`, `surfacing.rs:1-15`, `incremental.rs:1-29`, `expiry.rs:1-11`, `spans.rs:1-14`. Tests likewise carry the citations (`integration_tests.rs:1-17`, `surfacing_tests.rs`/`scope_leak_tests.rs` via the implicit module). |
| Review-document length ≤800 lines | **OK** | `wc -l` of this file is under the ceiling. |

## Stale-label discipline

The product property "Every stale or contradicted memory is surfaced as such, not hidden behind recency" is enforced at two layers:

1. **Surfacing partition.** `SurfacingPipeline::partition_for_caller` (`verification/surfacing.rs:73-97`) routes every non-`Verified` memory into `advisory` (BundleSection::Stale / Contradicted / Superseded / Expired / Invalidated / InReview / Unverified). `classify` (`surfacing.rs:99-101`) delegates to the exhaustive `classify_status` (`surfacing.rs:181-192`), which has one arm per `MemoryVerificationStatus` variant — `surfacing_tests.rs:95-114` `classify_covers_all_verification_status_variants` asserts the full mapping.
2. **Trusted invariant.** `enforce_trusted_invariant` (`surfacing.rs:149-178`) panics under `cfg(debug_assertions)` (`stale_in_trusted_bundle memory_id=… status=… caller_module=…`) and under release builds re-enters `retain` + `error!("stale_in_trusted_bundle")`. The debug panic is asserted by `surfacing_tests.rs:43-58` `debug_assertion_fires_when_non_trusted_memory_is_injected_into_trusted` with `#[should_panic(expected = "stale_in_trusted_bundle")]`.

The ranker hard precondition matches the spec: `SurfacingPipeline::trusted_ranked_top_n` (`surfacing.rs:118-128`) filters by `MemoryScoringMetadata::verification_status == Verified` via `ranked_candidate_is_trusted_memory` (`surfacing.rs:194-208`), regardless of `total_score`. `surfacing_tests.rs:77-92` proves that even with `stale` ranked at 100.0 and `verified` at 1.0, only the verified candidate reaches `trusted`.

`label_reason` references the verifier's structured payload: `format_reason` (`surfacing.rs:103-116`) interleaves `MemoryVerificationStatus::as_str()` with `evidence_reason` (`surfacing.rs:220-232`), which folds `stale_reason` (the verifier's persisted reason from `existence.rs:679-697` `project_state`), the first `linked_files` path, and the scope / branch context. `integration_tests.rs:175-192` `test_verifier_emits_explainable_label_reason_for_every_non_verified_state` walks all seven non-verified states and asserts every reason string contains the verifier's own substring (e.g. `"contradicted by mem-a"`, `"expiry 2026-05-17T00:00:00Z"`, `"file id src/deleted.rs"`).

End-to-end coverage of "stale or contradicted cannot rank as trusted":

- `integration_tests.rs:55-69` stale (T49 span flip)
- `integration_tests.rs:71-82` contradicted (T47 contradiction job)
- `integration_tests.rs:84-96` expired (T51 expiry scanner)
- `integration_tests.rs:98-110` invalidated (T48 deleted file)
- `integration_tests.rs:143-160` ranker top-N exclusion

## Scope-leak evidence

The §Risks Scope Leakage controls are enforced at the store boundary, not at the surfacing layer — which matches the spec ("enforced filters in store APIs"). Every scoped read path requires a `ScopeFilter`:

- `MemoryStore::query_memories_scoped` (`store.rs:443-460`) — pipes through `enforce_scope_boundary`.
- `MemoryStore::list_all_scoped` (`store.rs:462-466`) — pipes through `filter_scope_boundary` (which `record_scope_filtered`s every drop).
- `MemoryStore::get_by_id_scoped` (`store.rs:468-482`) — single-memory variant; `record_scope_filtered` runs on every drop.
- `MemoryStore::search_by_keyword_scoped` (`store.rs:484-492`) — keyword variant.

Non-scoped reads are gated behind `query_unscoped_admin` (`store.rs:495-531`), reserved for migrations / snapshots / admin paths, satisfying the §Non-Negotiable property "No silent broad workspace reads."

`ScopeFilter::validate` (`scope_enforcement.rs:71-97`) rejects empty workspace / branch / organization / session ids; all four scoped readers call `scope.validate()` first.

The debug-assertions invariant lives in `handle_scope_boundary_failure` (`store.rs:2190-2218`): under `cfg(debug_assertions)` it panics with `scope leak blocked in {context}: memory_id=… scope=…`; under release it drops to `tracing::error!("scope_leak_blocked", …)` plus an `INSERT INTO memory_scope_filter_events` row. `scope_leak_tests.rs:120-144` proves the debug panic with `catch_unwind`.

Per-scope negative coverage is complete:

| Scope | Test |
|---|---|
| Branch | `scope_leak_tests.rs:8-26` `branch_scope_memory_does_not_list_for_unrelated_branch` |
| Repo | `scope_leak_tests.rs:28-47` `repo_scope_memory_does_not_list_for_unrelated_workspace` |
| Organization | `scope_leak_tests.rs:49-72` `organization_scope_requires_matching_organization` |
| Session | `scope_leak_tests.rs:74-98` `session_scope_memory_does_not_list_for_different_session` |
| Audit verdict | `scope_leak_tests.rs:100-118` `audit_memory_invalidates_leaked_memory` |
| Persisted event row | `scope_leak_tests.rs:146-182` `scope_filter_event_is_emitted_for_each_store_boundary_drop` |

**Gap (Finding #1, #2 below):** the integration assertion that `scope_leak_blocked` is observed at the tracing surface in a four-scope sweep (`integration_tests.rs:113-140`) is currently failing — see Findings.

## Findings

### 1. `test_scope_leak_blocked_at_store_boundary_for_all_scopes` panics (BLOCKING)

`cargo test -p lattice-core --lib verification` reports `1 failed`:

```
thread 'verification::integration_tests::test_scope_leak_blocked_at_store_boundary_for_all_scopes'
  panicked at crates/lattice-core/src/verification/integration_tests.rs:139:5:
assertion failed: log_output.contains("scope_leak_blocked")
```

Root cause: the test sets a thread-local `tracing` subscriber via `tracing::subscriber::set_default` (`integration_tests.rs:116-120`), but a global `tracing` subscriber is already installed by an earlier test in the same `cargo test` binary, so the thread-local writer never sees the `tracing::warn!("scope_leak_blocked", …)` emitted by `MemoryStore::record_scope_filtered` (`store.rs:2225-2231`). The persistence assertions (`assert_eq!(events.len(), 4)`, per-event `attempted_workspace_id`) pass, so the *store enforcement* works; the *tracing path* assertion is broken either because the test set-up is incompatible with shared-binary global subscriber state or because the log is suppressed by the global subscriber's filter.

Spec impact: the §Phase 7 DoD demands "verification results are explainable" and §Risks Scope Leakage names tracing/audit as part of the control surface. A red bar in T53 also blocks the next phases (Phase 8 Workflow Engine consumes the same surfacing contract; Phase 10 Review UI shows the scope-filter event stream).

### 2. Typed `EventPayload::MemoryScopeFiltered` is defined but never emitted (BLOCKING)

`events/kinds.rs:521-528` defines `MemoryScopeFilteredPayload { memory_id, attempted_workspace_id, attempted_branch, memory_scope }`, and `events/kinds.rs:555` registers it in `EventPayload`. The doc comment on `events/kinds.rs:21` notes "`MemoryScopeFiltered` is the Phase 7 extension required by §Risks Scope Leakage." However, `grep` for `EventPayload::MemoryScopeFiltered(` and `MemoryScopeFiltered(` across `daemon/**` shows the constructor is never called and `EventWriter::append` never receives a `MemoryScopeFiltered` payload. The store records only to a private SQLite table (`memory_scope_filter_events`, `store.rs:2232-2247`) and to the `tracing` stream, but the event log — the canonical replayable audit surface — never sees it.

Spec impact: §Risks Scope Leakage control requires "enforced filters in store APIs, and negative tests" but the Phase 7 design (see the dedicated payload definition) intends the typed event to land in the event log so downstream phases can query/replay it. Phase 10's "stale memory view" and "event trace view" will need this payload; Phase 11 "corrupted-event handling" needs it on the wire.

### 3. File-length ceiling crossed without justification

`verification/existence.rs` is 803 lines; `verification/integration_tests.rs` is 922 lines. The coding-standard ceiling is 800 (`coding.md` §Hard limits). No commit-message justification is recorded. Recommended split:

- `existence.rs`: extract `FileExistenceCheck`, `SymbolExistenceCheck`, `DocSectionExistenceCheck`, `TestExistenceCheck` into `existence/checks.rs`, leaving `VerifierCore` + `aggregate_verdicts` + `project_state` in `existence.rs`. Brings the host file under 500 lines.
- `integration_tests.rs`: split the `VerificationHarness` (which is roughly half the file, lines 194-685) into `integration_tests/harness.rs` and keep the `#[test]` bodies in the parent; keep `FakeDriver` / `BufferWriter` / `ObservationRecorder` with the harness.

### 4. Function-length ceiling crossed

`VerifierCore::verify_memory_inner` (`existence.rs:198-277`) is 80 lines; `IncrementalVerifier::on_graph_delta` (`incremental.rs:131-191`) is 61 lines. Ceiling is 50 (`coding.md` §Hard limits). Recommended split for `verify_memory_inner`: extract the per-evidence dispatch loop into `verify_existence_evidence(&self, memory_id, evidence, &resolver) -> Vec<VerificationVerdict>`; extract `emit_proposal_for_verdict` (already separate) and `verify_scope` (already separate) only need a thin orchestration wrapper. For `on_graph_delta`: extract the `info_span!` boilerplate + budget loop into `drain_pending_budget(&mut self, span: &Span, report: &mut IncrementalReport)`.

### 5. Unjustified `#[allow(clippy::too_many_arguments)]`

`incremental.rs:97` carries `#[allow(clippy::too_many_arguments)]` on `IncrementalVerifier::new` with no inline justification. `coding.md` §No broken windows: "each requires an inline one-line comment explaining why the rule is wrong here (not what the code does)". The real fix per `coding.md` §Hard limits ("Group into a dataclass / TypedDict / Pydantic model") is to introduce `IncrementalVerifierServices<'a>` (store, runtime, graph, file_index, parsed_files, span_reader, event_writer, decided_by) and accept `(services, workspace_id, work_budget)`. Same shape works for `VerifierCore::new` (7 args).

### 6. Spec checks with thin coverage

These are not strictly DoD failures but are spec items that the Phase 7 test surface does not exercise:

- **"implementation still matches memory claim where deterministic checks are possible"** (§8). The only deterministic implementation-match path is the span hash check (`SpanValidator::validate`, `spans.rs:201-241`). For evidence without a span the verifier returns `Verified` after existence (`existence.rs:286-290`) — i.e. "we can't tell, so we trust." There is no test that asserts the "deterministic check unavailable → leave Unverified" downgrade path, nor any future-proofing comment that names this as intentional. Recommend a single test: `evidence_without_span_does_not_assert_implementation_match` that pins the current behavior so a future "always Verified" optimization can't quietly slip in.
- **"contradicted/superseded states remain coherent"** (§8). The verifier classifies contradicted/superseded memories correctly at the surfacing layer (`classify_status`) but never re-checks the coherence of the back-pointers (`superseded_by_memory_id`, `contradicted_by_memory_ids`). If a memory cites `superseded_by_memory_id = "mem-Z"` and `mem-Z` is later invalidated or deleted, the surviving memory still sits in `BundleSection::Superseded`. Phase 7 doesn't appear to be the right phase to implement that coherence check (it leans on Phase 6 consolidation contradictor jobs), but the gap should be tracked.

## Verdict

**Fail.**

The verifier core, span validator, scope enforcement, expiry scanner, incremental refresher, and surfacing pipeline are all in place and exercised — 36 of 37 verification tests pass, the §Verification Engine output enum is exhaustive, every store read path requires a `ScopeFilter`, the trusted-bundle invariant has a debug-assertion guard, and the ranker hard-precondition test proves stale/contradicted memories cannot win on score. The §Non-Negotiable property "Every stale or contradicted memory is surfaced as such, not hidden behind recency" holds at the partition + ranker layers; §Phase 7 DoD "stale or contradicted memory cannot appear as normal trusted guidance" has four end-to-end integration tests. However, R54 cannot pass to R55 / Phase 8 with a red verification bar (Finding #1) and with the typed `MemoryScopeFiltered` event payload — the Phase 7 extension that §Risks Scope Leakage explicitly names — defined but never emitted to the event log (Finding #2); both findings are observable surface gaps that downstream phases (Phase 8 workflow consumers, Phase 10 review UI, Phase 11 replay/corruption tests) will rely on. The coding-standard misses (Findings #3 – #5) are independent of the verdict but must be cleaned up in the same minimum-fix tasks because R55 will inherit the violations otherwise.

### Minimum-fix tasks required before R55 / Phase 8

1. **R54-fix-1:** Repair `test_scope_leak_blocked_at_store_boundary_for_all_scopes`. Either (a) make the test resilient to a pre-installed global subscriber — e.g. capture via a dedicated `tracing::Dispatch` + `with_default`, or use `tracing_test::traced_test` — or (b) replace the log-substring assertion with an assertion over the now-typed event log once Finding #2 is fixed. Whichever path, `cargo test -p lattice-core --lib verification` must end green.
2. **R54-fix-2:** Emit `EventPayload::MemoryScopeFiltered` from `MemoryStore::record_scope_filtered` (and on the release branch of `handle_scope_boundary_failure`). The store needs an `EventWriter` handle — pass it on construction or via a `MemoryStoreServices` struct (which the §Hard-limits fix in R54-fix-4 also wants). Add a test that asserts a `MemoryScopeFiltered` event lands in the event log for each of the four scopes.
3. **R54-fix-3:** Split `verification/existence.rs` (803 → ≤500 lines) by extracting the four `*ExistenceCheck` structs into `verification/existence/checks.rs`. Split `verification/integration_tests.rs` (922 → ≤500 lines) by extracting `VerificationHarness` into a sibling module. No behavior change required.
4. **R54-fix-4:** Replace `#[allow(clippy::too_many_arguments)]` on `IncrementalVerifier::new` with an `IncrementalVerifierServices<'a>` struct holding the eight collaborator borrows (store, runtime, graph, file_index, parsed_files, span_reader, event_writer, decided_by); apply the same shape to `VerifierCore::new`. Bring `verify_memory_inner` (80 → ≤50 lines) and `on_graph_delta` (61 → ≤50 lines) under the function-length ceiling by extracting the per-evidence dispatch loop and the budget-drain loop respectively.

R55 may proceed once R54-fix-1 and R54-fix-2 land green; R54-fix-3 and R54-fix-4 can land in the same change set or be tracked as an immediate Phase-7 cleanup PR before Phase 8 begins, but must not slip past Phase 8 kickoff (the eight-arg constructor + 922-line test file will compound under Phase 8's workflow-engine extensions).
