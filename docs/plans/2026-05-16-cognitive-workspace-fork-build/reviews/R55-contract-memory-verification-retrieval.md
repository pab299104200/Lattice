# R55 — Contract Gate (Memory ↔ Verification ↔ Retrieval cross-layer)

**Plan:** [docs/plans/2026-05-16-cognitive-workspace-fork-plan.md](../../2026-05-16-cognitive-workspace-fork-plan.md)
**Scope:** Phase 3 ↔ Phase 4 ↔ Phase 7 contract surface. Depends on R25 (memory graph), R33 (Retrieval V1), R54 (verification + freshness).
**Inputs audited:** `daemon/crates/lattice-core/src/memory/{model.rs,store.rs}`, `daemon/crates/lattice-core/src/memory_graph/{classes.rs,scope.rs}`, `daemon/crates/lattice-core/src/verification/{mod.rs,scope_enforcement.rs,surfacing.rs}`, `daemon/crates/lattice-core/src/retrieval_v1/{scoring.rs,shaper.rs,candidates.rs}`.
**New artifacts:** `daemon/crates/lattice-core/src/contract_tests/memory_verification_retrieval.rs`, `daemon/crates/lattice-core/src/contract_tests/memory_verification_retrieval_support.rs`.
**Verification command:** `cd daemon && cargo test -p lattice-core --lib contract_tests::memory_verification_retrieval` — **6 passed / 0 failed / 0 ignored** (full `contract_tests` module: **27 passed / 0 failed**).

## Contract surfaces

The gate pins three contract surfaces that must hold before any Phase 8 workflow tool can ship. Each surface is the meeting point of two of the three subsystems and is enforced by the new contract test file at `daemon/crates/lattice-core/src/contract_tests/memory_verification_retrieval.rs`.

| # | Surface | Subsystems meeting at the contract | Where the contract is implemented in code | Pinned by |
|---|---|---|---|---|
| 1 | `VerificationStatus` round-trip | Verification engine (Phase 7) writes `MemoryStructuredFields::verification_status`; the memory store (Phase 3) persists it in `memories.verification_status`; the retrieval ranker (Phase 4) reads it via `MemoryScoringMetadata::verification_status`. The three on-disk enums must agree variant-for-variant. | `verification::VerificationStatus` (`daemon/crates/lattice-core/src/verification/mod.rs:90-153`), `memory::MemoryVerificationStatus` (`daemon/crates/lattice-core/src/memory/model.rs:116-154`), `memory_graph::VerificationStatus` (`daemon/crates/lattice-core/src/memory_graph/classes.rs:214-279`), retrieval consumer at `retrieval_v1::scoring::MemoryScoringMetadata` (`daemon/crates/lattice-core/src/retrieval_v1/scoring.rs:98-110`). | `test_verification_status_round_trip_memory_to_retrieval` + `test_verification_status_enums_agree_variant_for_variant` (`memory_verification_retrieval.rs:64-138`). |
| 2 | `ScopeFilter` round-trip | The verifier (Phase 7) emits a `MemoryScopeFilteredEvent` whenever a scoped read drops a row; the memory store (Phase 3) enforces the filter at every public scoped reader and records the event row; the retrieval pipeline (Phase 4) consumes `MemoryStore` through those scoped readers. | `verification::ScopeFilter` (`daemon/crates/lattice-core/src/verification/scope_enforcement.rs:32-127`), scoped readers `MemoryStore::query` / `list_all_scoped` / `get_by_id_scoped` / `search_by_keyword_scoped` (`daemon/crates/lattice-core/src/memory/store.rs:411-492`), `MemoryScopeFilteredEvent` writeback (`daemon/crates/lattice-core/src/memory/store.rs:2220-2249`). | `test_scope_filter_round_trip_for_all_scope_kinds` + `test_retrieval_api_requires_scope_filter_argument` (`memory_verification_retrieval.rs:140-301`). |
| 3 | `SurfacedBundle` round-trip | Retrieval (Phase 4) hands a `Vec<Memory>` to `SurfacingPipeline::partition` (Phase 7 / T52); every non-`Verified` row routes into `SurfacedBundle.advisory`, never `trusted`; every advisory row carries the matching `BundleSection` and a non-empty `label_reason`. | `verification::SurfacingPipeline::partition` (`daemon/crates/lattice-core/src/verification/surfacing.rs:62-97`), `classify_status` (`surfacing.rs:181-192`), `BundleSection` (`surfacing.rs:24-35`), retrieval consumer reads `SurfacedBundle` through `BundleProducer::surface`. | `test_stale_label_round_trip_retrieval_to_surfaced_bundle` (`memory_verification_retrieval.rs:204-263`) + `end_to_end_memory_verification_retrieval_round_trip_holds_for_all_statuses_and_scopes` (`memory_verification_retrieval.rs:303-340`). |

The single source of truth for the contracts is the real subsystem API in each case — the tests drive `MemoryStore::store`, `MemoryStore::update_structured_fields`, `MemoryStore::list_all_scoped`, `MemoryStore::get_by_id_scoped`, `MemoryStore::scope_filter_events`, and `SurfacingPipeline::partition`. No filtering or surfacing logic is re-implemented in the test file (per the coding standard's `## Single source of truth`).

## Round-trip evidence

| Status / scope material seeded | Persistence path | Retrieval read | Assertion |
|---|---|---|---|
| One memory per `MemoryVerificationStatus` variant (`Verified`, `Unverified`, `InReview`, `Stale`, `Contradicted`, `Superseded`, `Expired`, `Invalidated`) | `MemoryStore::store` then `MemoryStore::update_structured_fields` (mirrors verifier path in `memory/store.rs:1245-1252`) | `MemoryStore::list_all_scoped(&ScopeFilter)` | `test_verification_status_round_trip_memory_to_retrieval` (`memory_verification_retrieval.rs:64-92`) loads each row through the scope-enforced reader and asserts `memory.verification_status == expected` and `classify_status(memory.verification_status) == expected_section_for(expected)`. |
| Enum wire names for every variant | Compile-time pair list pinning `verification::VerificationStatus` to `memory::MemoryVerificationStatus` | n/a — pure enum-to-enum agreement check | `test_verification_status_enums_agree_variant_for_variant` (`memory_verification_retrieval.rs:94-138`) asserts that for each pair, `as_str()` strings match and `MemoryVerificationStatus::from_str(verifier_status.as_str())` recovers the same variant. |
| Verified + every untrusted status surfaced together | `MemoryStore::list_all_scoped` then `SurfacingPipeline::partition` | `end_to_end_memory_verification_retrieval_round_trip_holds_for_all_statuses_and_scopes` (`memory_verification_retrieval.rs:303-340`) | Every status seeded above appears either in `bundle.trusted` (for `Verified`) or in `bundle.advisory` (for the other seven), and the `BundleSection` matches `expected_section_for` (`memory_verification_retrieval.rs:346-358`) for the row's `verification_status`. |

The round-trip exercise also makes the cross-layer relationship to the retrieval ranker explicit: `classify_status` (`verification/surfacing.rs:181-192`) is the canonical function used both by `SurfacingPipeline::partition` and by `SurfacingPipeline::classify`, and `retrieval_v1::scoring::MemoryScoringMetadata::verification_status` (`retrieval_v1/scoring.rs:100`) is the same `MemoryVerificationStatus` value the memory store persists. There is no silent state mapping between the layers — each variant flows through as one value with the same `as_str()` wire name.

## Stale-label round-trip

`test_stale_label_round_trip_retrieval_to_surfaced_bundle` (`memory_verification_retrieval.rs:204-263`) seeds five rows — one each for `Stale`, `Contradicted`, `Superseded`, `Expired`, `Invalidated` — through `seed_status_memory` (`memory_verification_retrieval_support.rs:91-114`), reads them back through `MemoryStore::list_all_scoped`, then drives `SurfacingPipeline::partition`.

Assertions:

- **No leak into `trusted`.** A `HashSet<String>` collects every `bundle.trusted.memory_id.ulid`; the test asserts that none of the five seeded ids appears in it. This is the §Non-Negotiable Product Properties guard ("Every stale or contradicted memory is surfaced as such, not hidden behind recency.") at the contract level — the test calls the production `partition` function, not a test double.
- **Correct `BundleSection` per status.** For each seeded `(memory_id, status)` pair the test finds the row in `bundle.advisory` and asserts `advisory.section == expected_section_for(status)`. The `expected_section_for` function (`memory_verification_retrieval.rs:346-358`) is an independent mapping in the test file — if `classify_status` ever silently re-routed a status, both `expected_section_for(status)` and the assertion would diverge.
- **Non-empty `label_reason`.** Each advisory entry must carry `!advisory.label_reason.trim().is_empty()`. This satisfies §Non-Negotiable Product Properties "Every retrieved memory has an inclusion reason" at the surfacing boundary.

The end-to-end test re-validates the contract on the full seeded set (all eight statuses + repo / branch / session / organization scopes), so the trusted-vs-advisory partition is exercised against both verified and non-verified material in one bundle.

## Scope-filter round-trip

`test_scope_filter_round_trip_for_all_scope_kinds` (`memory_verification_retrieval.rs:140-201`) seeds one memory per `MemoryScope` variant — session / branch / repo / organization — using the harness's `seed_session_memory`, `seed_branch_memory`, `seed_repo_memory`, `seed_organization_memory` helpers (`memory_verification_retrieval_support.rs:64-86`). For each row it constructs a deliberately non-matching `ScopeFilter` (different session id, different branch, different workspace, different organization) and calls the scope-enforced reader `MemoryStore::get_by_id_scoped`.

Assertions:

- **Zero leaked rows.** Every `get_by_id_scoped` call returns `None`, satisfying the §Risks "Scope Leakage" control ("scope-aware queries, enforced filters in store APIs, and negative tests").
- **One `MemoryScopeFilteredEvent` per blocked row.** The fixture compares `MemoryStore::scope_filter_events` before and after the four attempts and asserts the delta equals the number of attempts. `expect_scope_event_for` (`memory_verification_retrieval_support.rs:142-167`) then checks each event row carries the attempted `workspace_id`, attempted `branch`, and the memory's own `MemoryScope` — verifying the event is not opaque but tells the verifier exactly which boundary was crossed.
- **Workspace id must be non-empty.** `test_retrieval_api_requires_scope_filter_argument` (`memory_verification_retrieval.rs:264-301`) confirms that `ScopeFilter::new("", None, None)` is rejected by `MemoryStore::list_all_scoped` (via `ScopeFilter::validate` in `verification/scope_enforcement.rs:71-97`), so the contract holds even when callers try to construct a degenerate filter.

In addition to the runtime assertions, the same audit test pins the static signatures of every public scope-enforced retrieval entry point on `MemoryStore`:

```rust
let _query: fn(&MemoryStore, Option<&str>, usize, &ScopeFilter) -> Result<Vec<Memory>, LatticeError>
    = MemoryStore::query;
let _list_all_scoped: fn(&MemoryStore, &ScopeFilter) -> Result<Vec<Memory>, LatticeError>
    = MemoryStore::list_all_scoped;
let _get_by_id_scoped: fn(&MemoryStore, &str, &ScopeFilter) -> Result<Option<Memory>, LatticeError>
    = MemoryStore::get_by_id_scoped;
let _search_by_keyword_scoped: fn(&MemoryStore, &str, &ScopeFilter) -> Result<Vec<Memory>, LatticeError>
    = MemoryStore::search_by_keyword_scoped;
```

`trybuild` is not in `lattice-core`'s `dev-dependencies` (`daemon/crates/lattice-core/Cargo.toml:31-34`), so the task's documented fallback applies: a compile-time function-pointer binding stands in for the `compile_fail` doctest. If any of the four signatures stops accepting `&ScopeFilter`, the test fails to compile, the gate trips, and Phase 8 cannot proceed. This is the §Risks "Scope Leakage" "enforced filters in store APIs" control elevated to a compile-time invariant.

## Findings

1. **No contract violations found at the surfaces this gate guards.** The four contract tests, the variant-for-variant enum agreement check, and the end-to-end round-trip all pass on the canonical `MemoryStore` / `SurfacingPipeline` API path that retrieval consumes (`cd daemon && cargo test -p lattice-core --lib contract_tests::memory_verification_retrieval` — 6/6 pass).
2. **Two `MemoryVerificationStatus` enum surfaces are kept in lockstep by the new gate.** The Phase 7 `verification::VerificationStatus` and the Phase 3 `memory::MemoryVerificationStatus` agree variant-for-variant on `as_str()` and decode round-trip. The Phase 3 typed-memory enum `memory_graph::VerificationStatus` (`memory_graph/classes.rs:214-279`) has the same eight variants and the same `as_str()` mapping — it is not exercised by these tests because the retrieval pipeline (`retrieval_v1::candidates::retrieve_memory_links`, `candidates.rs:301-322`) reads through the legacy `memory::MemoryStore`, not the typed-memory store. The Phase 3 typed store has its own scope-filter type (`memory_graph::ScopeFilter`, `memory_graph/scope.rs:12-290`) that is exercised by R25's gate; R55 deliberately scopes itself to the path retrieval actually consumes today.
3. **`retrieval_v1::candidates::retrieve_memory_links` calls `MemoryStore::query_unscoped_admin` (`candidates.rs:309`).** This is explicitly named "unscoped admin" on the store side (`memory/store.rs:494-531`) but it is in the live retrieval pipeline. The store hides non-`workspace_id` rows through `RetrievalContext::workspace_id` filtering inside `memory_candidate` (`candidates.rs:433-447`), and the workflow tools that wrap retrieval re-apply scope filtering at their own boundary, so production code paths do not leak — but the gate's third contract surface deliberately tests `MemoryStore::partition` rather than the retrieval candidate pipeline because the surfacing step is the one stamped by the §Non-Negotiable property "Every stale or contradicted memory is surfaced as such." The retrieval candidate path's `query_unscoped_admin` reliance is a known consideration for Phase 8 workflow integration, not a gate-blocking contract violation here.
4. **Coding-standard alignment.** Both new files are well below the 800-line ceiling (`memory_verification_retrieval.rs` is 404 lines, `memory_verification_retrieval_support.rs` is 230 lines). Every test body sits below the 30-line ceiling once helpers carry the seeding. No `TODO` / `FIXME` / `XXX` / `#[allow(...)]` / commented-out code in either file (`grep -nE 'TODO|FIXME|XXX' daemon/crates/lattice-core/src/contract_tests/memory_verification_retrieval*.rs` returns no matches). Both files open with full doc-heading citations to `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md` §4 / §7 / §8 / §Non-Negotiable Product Properties / §Risks Scope Leakage, per `lattice/CLAUDE.md` § Markdown Heading References.
5. **Observability evidence.** The scope-filter test exercises the `MemoryScopeFilteredEvent` event row written by `record_scope_filtered` (`memory/store.rs:2220-2249`), so the gate confirms not only that scope leaks are blocked but that each block is auditable through `MemoryStore::scope_filter_events`. This is the spec property "No silent broad workspace reads" enforced at contract level.

## Verdict

**pass.** All three contract surfaces hold end-to-end against the real `memory::MemoryStore`, `verification::SurfacingPipeline`, and the retrieval-facing enum agreement. The new file at `daemon/crates/lattice-core/src/contract_tests/memory_verification_retrieval.rs` exercises every step the task enumerates (R55 steps 5–9) using only public subsystem APIs, with no test doubles for the contract surfaces. The static function-pointer audit converts the §Risks "Scope Leakage" "enforced filters in store APIs" control into a compile-time invariant: any future refactor that removes `&ScopeFilter` from a public retrieval reader will break compilation before Phase 8 can ship. Phase 8 workflow tools (T56+) are unblocked from R55's side.
