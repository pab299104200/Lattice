//! Cross-layer contract tests for Memory <-> Verification <-> Retrieval.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Non-Negotiable Product Properties`:
//!
//! - "Every retrieved memory has an inclusion reason."
//! - "Every stale or contradicted memory is surfaced as such, not hidden
//!   behind recency."
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 4. Memory Graph` lists `verification status` as a required field on
//! every memory record.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 7. Retrieval Engine` names verification status, freshness, scope, and
//! contradiction/supersession state as ranking signals the retrieval pipeline
//! must honor.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 8. Verification Engine` defines the eight verification outputs that
//! must round-trip losslessly between the verifier (Phase 7), the memory
//! store (Phase 3), and the retrieval ranker (Phase 4):
//! `verified`, `unverified`, `in_review`, `stale`, `contradicted`,
//! `superseded`, `expired`, `invalidated`.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Risks -- Scope Leakage`: "scope-aware queries, enforced filters in
//! store APIs, and negative tests."
//!
//! This file is the R55 contract gate. It pins three cross-layer surfaces:
//!
//! 1. `VerificationStatus round-trip` -- every status persisted on a memory
//!    row reads back as the same enum variant and the on-disk / wire enums
//!    (`memory::MemoryVerificationStatus`,
//!    `verification::VerificationStatus`) agree variant-for-variant.
//! 2. `ScopeFilter round-trip` -- every public scope-enforced retrieval
//!    entry point on `memory::MemoryStore` accepts a `&ScopeFilter`,
//!    applies it, drops non-matching rows, and emits a
//!    `MemoryScopeFilteredEvent` for each blocked row.
//! 3. `SurfacedBundle round-trip` -- every non-verified status read out of
//!    the memory store partitions into `SurfacedBundle.advisory`
//!    (never `trusted`) via `SurfacingPipeline::partition`, with a non-empty
//!    `label_reason` and the matching `BundleSection`.

use std::collections::HashSet;

use super::memory_verification_retrieval_support::{
    expect_scope_event_for, scope_filtered_event_count, seed_status_memory,
    seeded_memory_ids_for_status_round_trip, ContractFixture, BRANCH, NON_MATCHING_BRANCH,
    NON_MATCHING_ORG, NON_MATCHING_SESSION, NON_MATCHING_WORKSPACE, WORKSPACE,
};

use crate::events::BranchRef;
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryVerificationStatus};
use crate::verification::{
    classify_status, BundleSection, ScopeFilter, SurfacedBundle, SurfacingPipeline,
    VerificationStatus,
};

// ---------------------------------------------------------------------------
// CONTRACT 1 -- VerificationStatus round-trip (Phase 7 <-> Phase 3 <-> Phase 4)
// ---------------------------------------------------------------------------

#[test]
fn test_verification_status_round_trip_memory_to_retrieval() {
    let fixture = ContractFixture::new();
    let seeded = seeded_memory_ids_for_status_round_trip(&fixture);

    let scope = fixture.matching_repo_filter();
    let loaded = fixture
        .store
        .list_all_scoped(&scope)
        .expect("scoped list succeeds");

    for (memory_id, expected) in &seeded {
        let memory = loaded
            .iter()
            .find(|row| row.id == *memory_id)
            .unwrap_or_else(|| panic!("memory `{memory_id}` not surfaced by retrieval"));

        assert_eq!(
            memory.verification_status, *expected,
            "memory `{memory_id}` round-tripped to the wrong status"
        );

        assert_eq!(
            classify_status(&memory.verification_status),
            expected_section_for(*expected),
            "classify_status disagrees with retrieval ranker section for {expected:?}"
        );
    }
}

#[test]
fn test_verification_status_enums_agree_variant_for_variant() {
    let pairs: Vec<(MemoryVerificationStatus, VerificationStatus)> = vec![
        (
            MemoryVerificationStatus::Verified,
            VerificationStatus::Verified,
        ),
        (
            MemoryVerificationStatus::Unverified,
            VerificationStatus::Unverified,
        ),
        (
            MemoryVerificationStatus::InReview,
            VerificationStatus::InReview,
        ),
        (MemoryVerificationStatus::Stale, VerificationStatus::Stale),
        (
            MemoryVerificationStatus::Contradicted,
            VerificationStatus::Contradicted,
        ),
        (
            MemoryVerificationStatus::Superseded,
            VerificationStatus::Superseded,
        ),
        (
            MemoryVerificationStatus::Expired,
            VerificationStatus::Expired,
        ),
        (
            MemoryVerificationStatus::Invalidated,
            VerificationStatus::Invalidated,
        ),
    ];

    for (memory_status, verifier_status) in &pairs {
        assert_eq!(
            memory_status.as_str(),
            verifier_status.as_str(),
            "memory and verifier enum wire names diverge: {memory_status:?} vs {verifier_status:?}"
        );

        let round_tripped = MemoryVerificationStatus::from_str(verifier_status.as_str());
        assert_eq!(
            round_tripped,
            *memory_status,
            "verifier wire name `{}` did not decode back to the same memory variant",
            verifier_status.as_str()
        );
    }
}

// ---------------------------------------------------------------------------
// CONTRACT 2 -- ScopeFilter round-trip (deny-by-default, every scope kind)
// ---------------------------------------------------------------------------

#[test]
fn test_scope_filter_round_trip_for_all_scope_kinds() {
    let fixture = ContractFixture::new();
    let session_id = fixture.seed_session_memory("scope-session-leak");
    let branch_id = fixture.seed_branch_memory("scope-branch-leak");
    let repo_id = fixture.seed_repo_memory("scope-repo-leak");
    let organization_id = fixture.seed_organization_memory("scope-org-leak");

    let starting_event_count = scope_filtered_event_count(&fixture.store);
    let attempts: Vec<(&str, MemoryScope, ScopeFilter)> = vec![
        (
            session_id.as_str(),
            MemoryScope::Session,
            ScopeFilter::new(
                WORKSPACE.to_string(),
                Some(BranchRef {
                    name: BRANCH.to_string(),
                }),
                None,
            )
            .for_session(NON_MATCHING_SESSION),
        ),
        (
            branch_id.as_str(),
            MemoryScope::Branch,
            ScopeFilter::new(
                WORKSPACE.to_string(),
                Some(BranchRef {
                    name: NON_MATCHING_BRANCH.to_string(),
                }),
                None,
            ),
        ),
        (
            repo_id.as_str(),
            MemoryScope::Repo,
            ScopeFilter::new(
                NON_MATCHING_WORKSPACE.to_string(),
                Some(BranchRef {
                    name: BRANCH.to_string(),
                }),
                None,
            ),
        ),
        (
            organization_id.as_str(),
            MemoryScope::Organization,
            ScopeFilter::new(
                WORKSPACE.to_string(),
                Some(BranchRef {
                    name: BRANCH.to_string(),
                }),
                Some(NON_MATCHING_ORG.to_string()),
            ),
        ),
    ];

    for (memory_id, scope_kind, filter) in &attempts {
        let leaked = fixture
            .store
            .get_by_id_scoped(memory_id, filter)
            .expect("scope-enforced lookup succeeds");
        assert!(
            leaked.is_none(),
            "memory `{memory_id}` ({scope_kind:?}) leaked under non-matching ScopeFilter"
        );
        expect_scope_event_for(&fixture.store, memory_id, scope_kind, filter);
    }

    let ending_event_count = scope_filtered_event_count(&fixture.store);
    assert_eq!(
        ending_event_count - starting_event_count,
        attempts.len(),
        "scope filter must emit one MemoryScopeFilteredEvent per blocked row"
    );
}

// ---------------------------------------------------------------------------
// CONTRACT 3 -- Stale-label round-trip into SurfacedBundle
// ---------------------------------------------------------------------------

#[test]
fn test_stale_label_round_trip_retrieval_to_surfaced_bundle() {
    let fixture = ContractFixture::new();
    let untrusted_statuses = [
        MemoryVerificationStatus::Stale,
        MemoryVerificationStatus::Contradicted,
        MemoryVerificationStatus::Superseded,
        MemoryVerificationStatus::Expired,
        MemoryVerificationStatus::Invalidated,
    ];

    let mut seeded_ids: Vec<(String, MemoryVerificationStatus)> = Vec::new();
    for status in untrusted_statuses {
        let memory_id =
            seed_status_memory(&fixture, status, &format!("untrusted-{}", status.as_str()));
        seeded_ids.push((memory_id, status));
    }

    let scope = fixture.matching_repo_filter();
    let loaded = fixture
        .store
        .list_all_scoped(&scope)
        .expect("scoped list succeeds");
    let memories = seeded_ids
        .iter()
        .map(|(id, _)| {
            loaded
                .iter()
                .find(|memory| memory.id == *id)
                .cloned()
                .unwrap_or_else(|| panic!("memory `{id}` missing from retrieval"))
        })
        .collect::<Vec<Memory>>();

    let bundle: SurfacedBundle = SurfacingPipeline::partition(memories);
    let trusted_ids: HashSet<String> = bundle
        .trusted
        .iter()
        .map(|memory| memory.memory_id.ulid.clone())
        .collect();

    for (memory_id, status) in &seeded_ids {
        assert!(
            !trusted_ids.contains(memory_id),
            "untrusted memory `{memory_id}` ({status:?}) leaked into SurfacedBundle.trusted"
        );

        let advisory = bundle
            .advisory
            .iter()
            .find(|memory| memory.memory_id.ulid == *memory_id)
            .unwrap_or_else(|| panic!("memory `{memory_id}` missing from advisory partition"));
        let expected_section = expected_section_for(*status);
        assert_eq!(
            advisory.section, expected_section,
            "memory `{memory_id}` ({status:?}) labeled with wrong BundleSection"
        );
        assert!(
            !advisory.label_reason.trim().is_empty(),
            "memory `{memory_id}` ({status:?}) advisory entry missing label_reason"
        );
    }
}

// ---------------------------------------------------------------------------
// CONTRACT 4 -- Retrieval API requires a ScopeFilter argument
// ---------------------------------------------------------------------------

/// This test pins the static signature of every public scope-enforced
/// retrieval entry point on `memory::MemoryStore`. Because `lattice-core`
/// does not depend on `trybuild`, the task's fallback applies: audit the
/// list at compile time via function-pointer type bindings.
///
/// If any of these signatures stops accepting `&ScopeFilter`, this module
/// fails to compile and the gate trips before Phase 8 work can begin.
#[test]
fn test_retrieval_api_requires_scope_filter_argument() {
    use crate::error::LatticeError;

    let _query: fn(
        &MemoryStore,
        Option<&str>,
        usize,
        &ScopeFilter,
    ) -> Result<Vec<Memory>, LatticeError> = MemoryStore::query;
    let _list_all_scoped: fn(&MemoryStore, &ScopeFilter) -> Result<Vec<Memory>, LatticeError> =
        MemoryStore::list_all_scoped;
    let _get_by_id_scoped: fn(
        &MemoryStore,
        &str,
        &ScopeFilter,
    ) -> Result<Option<Memory>, LatticeError> = MemoryStore::get_by_id_scoped;
    let _search_by_keyword_scoped: fn(
        &MemoryStore,
        &str,
        &ScopeFilter,
    ) -> Result<Vec<Memory>, LatticeError> = MemoryStore::search_by_keyword_scoped;

    let fixture = ContractFixture::new();
    let memory_id = fixture.seed_repo_memory("audit-scope-required");

    let invalid_filter = ScopeFilter::new(String::new(), None, None);
    let invalid_result = fixture.store.list_all_scoped(&invalid_filter);
    assert!(
        invalid_result.is_err(),
        "ScopeFilter without a workspace id must be rejected at the retrieval boundary"
    );

    let valid = fixture
        .store
        .get_by_id_scoped(&memory_id, &fixture.matching_repo_filter())
        .expect("scope-enforced lookup succeeds")
        .expect("matching scope returns the memory");
    assert_eq!(valid.id, memory_id);
    assert_eq!(valid.workspace_id.as_deref(), Some(WORKSPACE));
}

// ---------------------------------------------------------------------------
// END-TO-END -- every status, every scope, surfacing in one round-trip
// ---------------------------------------------------------------------------

#[test]
fn end_to_end_memory_verification_retrieval_round_trip_holds_for_all_statuses_and_scopes() {
    let fixture = ContractFixture::new();
    let seeded = seeded_memory_ids_for_status_round_trip(&fixture);
    let scope = fixture.matching_repo_filter();

    let memories = fixture
        .store
        .list_all_scoped(&scope)
        .expect("scoped list succeeds");
    assert!(memories.len() >= seeded.len());

    let bundle = SurfacingPipeline::partition(memories);
    let mut classified_statuses: Vec<MemoryVerificationStatus> = Vec::new();
    for entry in bundle.trusted.iter().chain(bundle.advisory.iter()) {
        let memory = fixture
            .store
            .get_by_id(&entry.memory_id.ulid)
            .expect("memory loads")
            .expect("memory exists");
        if !classified_statuses.contains(&memory.verification_status) {
            classified_statuses.push(memory.verification_status);
        }
        let expected = expected_section_for(memory.verification_status);
        assert_eq!(
            entry.section, expected,
            "memory `{}` ({:?}) ended in `{:?}` instead of `{:?}`",
            entry.memory_id.ulid, memory.verification_status, entry.section, expected
        );
    }

    for (_, status) in &seeded {
        assert!(
            classified_statuses.iter().any(|seen| seen == status),
            "no surfaced row carried verification status `{status:?}`"
        );
    }
}

// ---------------------------------------------------------------------------
// Shared classification table used by every contract above.
// ---------------------------------------------------------------------------

fn expected_section_for(status: MemoryVerificationStatus) -> BundleSection {
    match status {
        MemoryVerificationStatus::Verified => BundleSection::Trusted,
        MemoryVerificationStatus::Unverified => BundleSection::Unverified,
        MemoryVerificationStatus::InReview => BundleSection::InReview,
        MemoryVerificationStatus::Stale => BundleSection::Stale,
        MemoryVerificationStatus::Contradicted => BundleSection::Contradicted,
        MemoryVerificationStatus::Superseded => BundleSection::Superseded,
        MemoryVerificationStatus::Expired => BundleSection::Expired,
        MemoryVerificationStatus::Invalidated => BundleSection::Invalidated,
    }
}
