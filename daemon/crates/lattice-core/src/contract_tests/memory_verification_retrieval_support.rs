//! Shared fixtures for the R55 cross-layer contract tests in
//! `memory_verification_retrieval.rs`.
//!
//! These helpers exist so the contract test file can stay focused on the
//! per-surface assertions named in the gate. The fixture seeds memories
//! spanning every `MemoryVerificationStatus` variant and every
//! `MemoryScope` kind so the contracts have material to assert on.
//!
//! Spec anchors (see file-level docs in `memory_verification_retrieval.rs`
//! for the full quotes):
//!
//! - `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//!   `## 4. Memory Graph` -- required `verification status` field.
//! - `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//!   `## 7. Retrieval Engine` -- `scope` and `verification status` ranking
//!   signals.
//! - `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//!   `## 8. Verification Engine` -- the eight verification outputs.
//! - `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//!   `## Risks -- Scope Leakage` -- "scope-aware queries, enforced filters
//!   in store APIs, and negative tests."

use crate::events::BranchRef;
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType, MemoryVerificationStatus};
use crate::verification::ScopeFilter;

pub(super) const WORKSPACE: &str = "workspace-r55";
pub(super) const BRANCH: &str = "main";
pub(super) const SESSION_ID: &str = "session-r55";
pub(super) const ORG_ID: &str = "org-r55";

pub(super) const NON_MATCHING_WORKSPACE: &str = "workspace-other";
pub(super) const NON_MATCHING_BRANCH: &str = "feat/other";
pub(super) const NON_MATCHING_SESSION: &str = "session-other";
pub(super) const NON_MATCHING_ORG: &str = "org-other";

/// In-process harness wiring a fresh `MemoryStore` for each contract test.
///
/// The retrieval pipeline (`crate::retrieval_v1`) reads through the legacy
/// `memory::MemoryStore`, so the contract tests exercise the same store the
/// production retrieval path uses. The store carries the canonical
/// `memory::MemoryVerificationStatus` column the verifier writes to and the
/// `ScopeFilter`-enforced query surface the retrieval ranker calls.
pub(super) struct ContractFixture {
    pub(super) store: MemoryStore,
}

impl ContractFixture {
    pub(super) fn new() -> Self {
        Self {
            store: MemoryStore::open_in_memory().expect("memory store opens"),
        }
    }

    /// `ScopeFilter` that matches the fixture's repo / branch defaults.
    pub(super) fn matching_repo_filter(&self) -> ScopeFilter {
        ScopeFilter::new(
            WORKSPACE.to_string(),
            Some(BranchRef {
                name: BRANCH.to_string(),
            }),
            Some(ORG_ID.to_string()),
        )
        .for_session(SESSION_ID)
    }

    pub(super) fn seed_session_memory(&self, id: &str) -> String {
        self.store_memory(base_memory(id, MemoryScope::Session))
    }

    pub(super) fn seed_branch_memory(&self, id: &str) -> String {
        self.store_memory(base_memory(id, MemoryScope::Branch))
    }

    pub(super) fn seed_repo_memory(&self, id: &str) -> String {
        self.store_memory(base_memory(id, MemoryScope::Repo))
    }

    pub(super) fn seed_organization_memory(&self, id: &str) -> String {
        self.store_memory(base_memory(id, MemoryScope::Organization))
    }

    fn store_memory(&self, memory: Memory) -> String {
        self.store.store(memory).expect("memory stores")
    }
}

/// Seed a memory in repo scope and stamp it with the requested verifier
/// status. The verifier writes `verification_status` via
/// `MemoryStore::update_structured_fields`, so this helper mirrors the
/// Phase 7 verifier path.
pub(super) fn seed_status_memory(
    fixture: &ContractFixture,
    status: MemoryVerificationStatus,
    id: &str,
) -> String {
    let mut memory = base_memory(id, MemoryScope::Repo);
    memory.verification_status = status;
    memory.is_stale = matches!(status, MemoryVerificationStatus::Stale);
    let stored_id = fixture.store.store(memory).expect("memory stores");

    let mut fields = fixture
        .store
        .get_structured_fields(&stored_id)
        .expect("structured fields load")
        .expect("structured fields exist after store");
    fields.verification_status = status;
    fixture
        .store
        .update_structured_fields(&stored_id, &fields)
        .expect("structured fields update");
    stored_id
}

/// Seed one memory per `MemoryVerificationStatus` variant for the
/// status-round-trip and end-to-end tests.
pub(super) fn seeded_memory_ids_for_status_round_trip(
    fixture: &ContractFixture,
) -> Vec<(String, MemoryVerificationStatus)> {
    let statuses = [
        MemoryVerificationStatus::Verified,
        MemoryVerificationStatus::Unverified,
        MemoryVerificationStatus::InReview,
        MemoryVerificationStatus::Stale,
        MemoryVerificationStatus::Contradicted,
        MemoryVerificationStatus::Superseded,
        MemoryVerificationStatus::Expired,
        MemoryVerificationStatus::Invalidated,
    ];

    statuses
        .into_iter()
        .map(|status| {
            let id = format!("status-{}", status.as_str());
            let stored_id = seed_status_memory(fixture, status, &id);
            (stored_id, status)
        })
        .collect()
}

/// Total `MemoryScopeFilteredEvent` rows observed by the store so far.
pub(super) fn scope_filtered_event_count(store: &MemoryStore) -> usize {
    store
        .scope_filter_events()
        .expect("scope filter events load")
        .len()
}

/// Verify that a `MemoryScopeFilteredEvent` was recorded for the leaked
/// memory and that it carries the attempted filter's workspace / branch /
/// scope identifiers.
pub(super) fn expect_scope_event_for(
    store: &MemoryStore,
    memory_id: &str,
    scope_kind: &MemoryScope,
    filter: &ScopeFilter,
) {
    let events = store.scope_filter_events().expect("events load");
    let event = events
        .iter()
        .find(|event| event.memory_id == memory_id)
        .unwrap_or_else(|| {
            panic!(
                "no MemoryScopeFilteredEvent recorded for `{memory_id}` ({scope_kind:?}); \
                 captured events = {:?}",
                events
                    .iter()
                    .map(|e| e.memory_id.as_str())
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(
        event.attempted_workspace_id, filter.workspace_id,
        "scope filter event must carry the attempted workspace id"
    );
    assert_eq!(
        event.attempted_branch,
        filter.branch.as_ref().map(|branch| branch.name.clone()),
        "scope filter event must carry the attempted branch"
    );
    assert_eq!(
        &event.memory_scope, scope_kind,
        "scope filter event must carry the memory's own scope kind"
    );
}

fn base_memory(id: &str, scope: MemoryScope) -> Memory {
    let workspace_id = workspace_id_for_scope(&scope);
    let branch = branch_for_scope(&scope);
    let scope_organization_id = organization_for_scope(&scope);
    Memory {
        id: id.to_string(),
        session_id: SESSION_ID.to_string(),
        content: format!("contract memory {id}"),
        memory_type: MemoryType::Observation,
        scope,
        confidence: 0.9,
        linked_symbols: Vec::new(),
        linked_files: vec!["src/contract.rs".to_string()],
        workspace_id,
        branch,
        scope_organization_id,
        refresh_key: None,
        source_query: None,
        created_at: 100,
        last_accessed: 100,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: MemoryVerificationStatus::Unverified,
    }
}

fn workspace_id_for_scope(scope: &MemoryScope) -> Option<String> {
    match scope {
        MemoryScope::Organization => None,
        _ => Some(WORKSPACE.to_string()),
    }
}

fn branch_for_scope(scope: &MemoryScope) -> Option<String> {
    match scope {
        MemoryScope::Organization => None,
        _ => Some(BRANCH.to_string()),
    }
}

fn organization_for_scope(scope: &MemoryScope) -> Option<String> {
    match scope {
        MemoryScope::Organization => Some(ORG_ID.to_string()),
        _ => None,
    }
}
