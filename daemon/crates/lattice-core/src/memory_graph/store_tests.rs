use crate::events::{
    EventPayload, MemoryCreatedPayload, MemoryInvalidatedPayload, MemoryUpdatedPayload,
};
use crate::memory_graph::store::MemoryScopeState;
use crate::memory_graph::tests_common::{idem, memory_evidence, row_count, TestHarness};
use crate::memory_graph::{
    get_evidence_for, transition_status, MemoryClass, MemoryPatch, MemoryScope,
    MemoryTransitionError, VerificationStatus,
};

#[test]
fn create_and_get_round_trip_persists_initial_evidence_and_event() {
    let harness = TestHarness::new();
    let created = harness
        .store
        .create(
            harness
                .draft("round-trip")
                .with_initial_evidence(memory_evidence("seed"))
                .into(),
            idem("create-round-trip"),
        )
        .unwrap();

    let loaded = harness
        .store
        .get(&created.memory_id, &harness.branch_scope())
        .unwrap();
    assert_eq!(loaded.content, "memory round-trip");
    assert_eq!(loaded.class, MemoryClass::Observation);
    assert_eq!(loaded.verification_status, VerificationStatus::Unverified);
    assert_eq!(loaded.memory_id, created.memory_id);
    assert_eq!(loaded.provenance_event_ids.len(), 1);
    assert!(loaded
        .evidence_references
        .iter()
        .any(|reference| matches!(reference.target, crate::events::StableRef::EventRef(_))));

    let evidence = get_evidence_for(&harness.conn(), &created.memory_id).unwrap();
    assert_eq!(evidence.len(), 2);
    assert!(evidence
        .iter()
        .any(|row| row.evidence_id.as_str() == "seed"));
    assert!(matches!(
        harness.latest_event_payload(),
        EventPayload::MemoryCreated(MemoryCreatedPayload { memory_id, class, stream, scope, idempotency_key, .. })
            if memory_id == created.memory_id
                && class == "observation"
                && stream == "code_topology"
                && scope == "branch"
                && idempotency_key == "create-round-trip"
    ));
}

#[test]
fn create_with_same_idempotency_key_returns_existing_record() {
    let harness = TestHarness::new();
    let first = harness
        .store
        .create(harness.draft("dedupe-a").into(), idem("dedupe-create"))
        .unwrap();
    let second = harness
        .store
        .create(harness.draft("dedupe-b").into(), idem("dedupe-create"))
        .unwrap();

    assert_eq!(first.memory_id, second.memory_id);
    assert_eq!(row_count(&harness.conn(), "memories"), 1);
    assert_eq!(row_count(&harness.conn(), "memory_idempotency"), 1);
}

#[test]
fn update_preserves_immutable_fields() {
    let harness = TestHarness::new();
    let created = harness
        .store
        .create(harness.draft("patch-me").into(), idem("update-create"))
        .unwrap();

    let updated = harness
        .store
        .update(
            &created.memory_id,
            MemoryPatch {
                content: Some("memory updated".to_string()),
                confidence: Some(0.9),
                freshness_policy: None,
                validity_conditions: None,
                invalidation_triggers: None,
                scope: Some(MemoryScopeState {
                    scope: MemoryScope::Repo,
                    scope_session_id: None,
                    scope_branch: None,
                    scope_workspace_id: Some("workspace".to_string()),
                    scope_user_id: None,
                    scope_org_id: None,
                }),
            },
            Some(memory_evidence("update-evidence")),
            idem("update-once"),
        )
        .unwrap();

    assert_eq!(updated.memory_id, created.memory_id);
    assert_eq!(updated.created_at, created.created_at);
    assert_eq!(updated.created_by, created.created_by);
    assert_eq!(updated.verification_status, created.verification_status);
    assert_eq!(updated.content, "memory updated");
    assert_eq!(updated.confidence, 0.9);
    assert_eq!(updated.scope, MemoryScope::Repo);

    let evidence = get_evidence_for(&harness.conn(), &created.memory_id).unwrap();
    assert!(evidence
        .iter()
        .any(|row| row.evidence_id.as_str() == "update-evidence"));
    assert!(matches!(
        harness.latest_event_payload(),
        EventPayload::MemoryUpdated(MemoryUpdatedPayload { memory_id, update_summary, .. })
            if memory_id == created.memory_id && update_summary == "memory updated"
    ));
}

#[test]
fn soft_delete_marks_memory_invalidated_and_keeps_row_readable() {
    let harness = TestHarness::new();
    let created = harness
        .store
        .create(harness.draft("delete-me").into(), idem("delete-create"))
        .unwrap();

    harness
        .store
        .delete(&created.memory_id, "user request")
        .unwrap();

    let loaded = harness
        .store
        .get(&created.memory_id, &harness.branch_scope())
        .unwrap();
    assert_eq!(loaded.verification_status, VerificationStatus::Invalidated);
    assert_eq!(row_count(&harness.conn(), "memory_tombstones"), 1);
    assert!(matches!(
        harness.latest_event_payload(),
        EventPayload::MemoryInvalidated(MemoryInvalidatedPayload { memory_id, reason, .. })
            if memory_id == created.memory_id && reason == "user request"
    ));
}

#[test]
fn legal_transitions_cover_full_allowed_matrix() {
    assert_transition_chain(
        "u_to_review",
        &[VerificationStatus::InReview],
        VerificationStatus::InReview,
    );
    assert_transition_chain(
        "u_to_verified",
        &[VerificationStatus::Verified],
        VerificationStatus::Verified,
    );
    assert_transition_chain(
        "review_to_verified",
        &[VerificationStatus::InReview, VerificationStatus::Verified],
        VerificationStatus::Verified,
    );
    assert_transition_chain(
        "review_to_stale",
        &[VerificationStatus::InReview, VerificationStatus::Stale],
        VerificationStatus::Stale,
    );
    assert_transition_chain(
        "review_to_contradicted",
        &[
            VerificationStatus::InReview,
            VerificationStatus::Contradicted,
        ],
        VerificationStatus::Contradicted,
    );
    assert_transition_chain(
        "review_to_expired",
        &[VerificationStatus::InReview, VerificationStatus::Expired],
        VerificationStatus::Expired,
    );
    assert_transition_chain(
        "review_to_invalidated",
        &[
            VerificationStatus::InReview,
            VerificationStatus::Invalidated,
        ],
        VerificationStatus::Invalidated,
    );
    assert_transition_chain(
        "verified_to_stale",
        &[VerificationStatus::Verified, VerificationStatus::Stale],
        VerificationStatus::Stale,
    );
    assert_transition_chain(
        "verified_to_contradicted",
        &[
            VerificationStatus::Verified,
            VerificationStatus::Contradicted,
        ],
        VerificationStatus::Contradicted,
    );
    assert_transition_chain(
        "verified_to_superseded",
        &[VerificationStatus::Verified, VerificationStatus::Superseded],
        VerificationStatus::Superseded,
    );
    assert_transition_chain(
        "verified_to_expired",
        &[VerificationStatus::Verified, VerificationStatus::Expired],
        VerificationStatus::Expired,
    );
    assert_transition_chain(
        "verified_to_invalidated",
        &[
            VerificationStatus::Verified,
            VerificationStatus::Invalidated,
        ],
        VerificationStatus::Invalidated,
    );
    assert_transition_chain(
        "stale_to_verified",
        &[
            VerificationStatus::Verified,
            VerificationStatus::Stale,
            VerificationStatus::Verified,
        ],
        VerificationStatus::Verified,
    );
    assert_transition_chain(
        "stale_to_invalidated",
        &[
            VerificationStatus::Verified,
            VerificationStatus::Stale,
            VerificationStatus::Invalidated,
        ],
        VerificationStatus::Invalidated,
    );
}

#[test]
fn illegal_transitions_return_disallowed_transition() {
    assert_illegal_transition(
        "illegal_u_to_stale",
        &[],
        VerificationStatus::Stale,
        VerificationStatus::Unverified,
    );
    assert_illegal_transition(
        "illegal_verified_to_review",
        &[VerificationStatus::Verified],
        VerificationStatus::InReview,
        VerificationStatus::Verified,
    );
    assert_illegal_transition(
        "illegal_invalidated_to_verified",
        &[
            VerificationStatus::Verified,
            VerificationStatus::Invalidated,
        ],
        VerificationStatus::Verified,
        VerificationStatus::Invalidated,
    );
}

#[test]
fn transitions_emit_expected_lifecycle_events() {
    let harness = TestHarness::new();
    let created = harness
        .store
        .create(
            harness.draft("transition-events").into(),
            idem("transition-events-create"),
        )
        .unwrap();

    let verified = transition_status(
        &harness.store,
        created.memory_id.clone(),
        VerificationStatus::Verified,
        "deterministic check passed",
        Some(memory_evidence("verify-evidence")),
    )
    .unwrap();
    assert_eq!(verified.verification_status, VerificationStatus::Verified);
    assert!(matches!(
        harness.latest_event_payload(),
        EventPayload::MemoryUpdated(MemoryUpdatedPayload { memory_id, update_summary, .. })
            if memory_id == created.memory_id
                && update_summary == "unverified -> verified: deterministic check passed"
    ));

    let invalidated = transition_status(
        &harness.store,
        created.memory_id.clone(),
        VerificationStatus::Invalidated,
        "manual invalidation",
        Some(memory_evidence("invalidate-evidence")),
    )
    .unwrap();
    assert_eq!(
        invalidated.verification_status,
        VerificationStatus::Invalidated
    );
    assert!(matches!(
        harness.latest_event_payload(),
        EventPayload::MemoryInvalidated(MemoryInvalidatedPayload { memory_id, reason, .. })
            if memory_id == created.memory_id && reason == "manual invalidation"
    ));
}

fn assert_transition_chain(
    slug: &str,
    transitions: &[VerificationStatus],
    expected: VerificationStatus,
) {
    let harness = TestHarness::new();
    let created = harness
        .store
        .create(harness.draft(slug).into(), idem(&format!("{slug}-create")))
        .unwrap();

    let mut current = created;
    for (index, status) in transitions.iter().copied().enumerate() {
        current = transition_status(
            &harness.store,
            current.memory_id.clone(),
            status,
            &format!("{slug}-{index}"),
            Some(memory_evidence(&format!("{slug}-evidence-{index}"))),
        )
        .unwrap();
    }

    assert_eq!(current.verification_status, expected);
    let latest = harness.latest_event_payload();
    match expected {
        VerificationStatus::Invalidated => {
            assert!(matches!(latest, EventPayload::MemoryInvalidated(_)));
        }
        _ => {
            assert!(matches!(latest, EventPayload::MemoryUpdated(_)));
        }
    }
}

fn assert_illegal_transition(
    slug: &str,
    setup: &[VerificationStatus],
    target: VerificationStatus,
    expected_from: VerificationStatus,
) {
    let harness = TestHarness::new();
    let created = harness
        .store
        .create(harness.draft(slug).into(), idem(&format!("{slug}-create")))
        .unwrap();

    let mut current = created;
    for (index, status) in setup.iter().copied().enumerate() {
        current = transition_status(
            &harness.store,
            current.memory_id.clone(),
            status,
            &format!("{slug}-setup-{index}"),
            Some(memory_evidence(&format!("{slug}-setup-evidence-{index}"))),
        )
        .unwrap();
    }

    let error = transition_status(
        &harness.store,
        current.memory_id.clone(),
        target,
        &format!("{slug}-illegal"),
        Some(memory_evidence(&format!("{slug}-illegal-evidence"))),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        MemoryTransitionError::DisallowedTransition { from, to }
            if from == expected_from && to == target
    ));
}
