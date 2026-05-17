use chrono::Utc;

use crate::events::{Actor, EventPayload, MemoryUpdatedPayload};
use crate::memory_graph::tests_common::{
    event_id, idem, memory_evidence, stable_ulid, TestHarness,
};
use crate::memory_graph::{
    get_links_from, get_links_to, insert_link, transition_status, AssertionType, MemoryClass,
    MemoryLink, MemoryLinkId, MemoryLinkTarget, MemoryLinkType, VerificationStatus,
};

#[test]
fn contradiction_link_contradicts_target_and_emits_memory_updated_event() {
    let harness = TestHarness::new();
    let contradicted = verified_memory(&harness, "a");
    let contradictor = created_memory(&harness, "b");
    let link = contradiction_link(&contradictor.memory_id, &contradicted.memory_id, "b-a");

    insert_link(&harness.conn(), &link).expect("contradiction link inserts");
    let updated = transition_status(
        &harness.store,
        contradicted.memory_id.clone(),
        VerificationStatus::Contradicted,
        "contradicted by newer evidence",
        Some(memory_evidence("contradiction-proof")),
    )
    .expect("contradiction transition succeeds");

    assert_eq!(
        updated.verification_status,
        VerificationStatus::Contradicted
    );
    assert_eq!(
        get_links_from(&harness.conn(), &contradictor.memory_id).unwrap(),
        vec![link.clone()]
    );
    assert_eq!(
        get_links_to(
            &harness.conn(),
            &MemoryLinkTarget::Memory(contradicted.memory_id.clone())
        )
        .unwrap(),
        vec![link.clone()]
    );
    assert!(matches!(
        harness.latest_event_payload(),
        EventPayload::MemoryUpdated(MemoryUpdatedPayload { memory_id, update_summary, .. })
            if memory_id == contradicted.memory_id
                && update_summary == "verified -> contradicted: contradicted by newer evidence"
    ));
    assert_eq!(link.evidence_event_id, Some(event_id("contradiction-b-a")));
}

#[test]
fn counter_memory_is_first_class_record_and_can_be_superseded_independently() {
    let harness = TestHarness::new();
    let target = verified_memory(&harness, "target");
    let counter = harness
        .store
        .create(
            harness
                .draft("counter")
                .with_class(MemoryClass::CounterMemory, AssertionType::Counter)
                .with_initial_evidence(memory_evidence("counter-seed"))
                .into(),
            idem("create-counter"),
        )
        .expect("counter memory creates");
    insert_link(
        &harness.conn(),
        &contradiction_link(&counter.memory_id, &target.memory_id, "counter-target"),
    )
    .expect("counter contradiction inserts");
    transition_status(
        &harness.store,
        target.memory_id.clone(),
        VerificationStatus::Contradicted,
        "counter memory contradicts prior claim",
        Some(memory_evidence("counter-proof")),
    )
    .expect("target contradicts");

    let loaded_counter = harness
        .store
        .get(&counter.memory_id, &harness.branch_scope())
        .expect("counter memory loads");
    assert_eq!(loaded_counter.class, MemoryClass::CounterMemory);
    assert_eq!(loaded_counter.scope, counter.scope);
    assert_eq!(
        loaded_counter.verification_status,
        VerificationStatus::Unverified
    );

    let replacement = created_memory(&harness, "replacement");
    insert_link(
        &harness.conn(),
        &supersedes_link(
            &replacement.memory_id,
            &counter.memory_id,
            "replacement-counter",
        ),
    )
    .expect("replacement link inserts");
    let mut superseded_counter = loaded_counter.clone();
    superseded_counter
        .supersession_links
        .push(crate::memory_graph::MemoryLinkReference {
            memory_id: replacement.memory_id.clone(),
            reason: "replacement counter memory".to_string(),
        });
    superseded_counter.superseded_by = Some(replacement.memory_id.clone());
    superseded_counter.verification_status = VerificationStatus::Superseded;
    let transitioned = harness
        .store
        .write_transition(
            &loaded_counter,
            superseded_counter,
            "replacement counter supersedes prior counter",
            Some(memory_evidence("counter-superseded")),
        )
        .expect("counter memory supersedes");

    assert_eq!(
        transitioned.verification_status,
        VerificationStatus::Superseded
    );
    assert_eq!(transitioned.superseded_by, Some(replacement.memory_id));
    assert_eq!(
        harness
            .store
            .get(&target.memory_id, &harness.branch_scope())
            .expect("target reloads")
            .verification_status,
        VerificationStatus::Contradicted
    );
}

#[test]
fn counter_memory_can_be_invalidated_without_reopening_its_target() {
    let harness = TestHarness::new();
    let target = verified_memory(&harness, "invalid-target");
    let counter = harness
        .store
        .create(
            harness
                .draft("invalid-counter")
                .with_class(MemoryClass::CounterMemory, AssertionType::Counter)
                .into(),
            idem("create-invalid-counter"),
        )
        .expect("counter memory creates");
    insert_link(
        &harness.conn(),
        &contradiction_link(
            &counter.memory_id,
            &target.memory_id,
            "invalid-counter-target",
        ),
    )
    .expect("counter contradiction inserts");
    transition_status(
        &harness.store,
        target.memory_id.clone(),
        VerificationStatus::Contradicted,
        "counter claim wins",
        Some(memory_evidence("target-countered")),
    )
    .expect("target contradicts");

    harness
        .store
        .delete(&counter.memory_id, "counter memory evidence retracted")
        .expect("counter invalidates");

    assert_eq!(
        harness
            .store
            .get(&counter.memory_id, &harness.branch_scope())
            .expect("counter reloads")
            .verification_status,
        VerificationStatus::Invalidated
    );
    assert_eq!(
        harness
            .store
            .get(&target.memory_id, &harness.branch_scope())
            .expect("target reloads")
            .verification_status,
        VerificationStatus::Contradicted
    );
}

#[test]
fn low_confidence_contradiction_records_link_without_flipping_high_confidence_target() {
    let harness = TestHarness::new();
    let target = verified_memory(&harness, "high-confidence");
    let weak_counter = harness
        .store
        .create(
            harness.draft("low-confidence").with_confidence(0.2).into(),
            idem("create-low-confidence"),
        )
        .expect("weak contradiction creates");
    let link = contradiction_link(&weak_counter.memory_id, &target.memory_id, "weak-target");

    insert_link(&harness.conn(), &link).expect("weak contradiction link inserts");
    let source = harness
        .store
        .get(&weak_counter.memory_id, &harness.branch_scope())
        .expect("weak source loads");
    harness
        .store
        .write_transition(
            &source,
            source.clone(),
            "policy: contradiction requires source confidence >= 0.6 and target confidence < source confidence",
            Some(memory_evidence("weak-policy")),
        )
        .expect("source event emits");

    assert_eq!(
        harness
            .store
            .get(&target.memory_id, &harness.branch_scope())
            .expect("target reloads")
            .verification_status,
        VerificationStatus::Verified
    );
    assert_eq!(
        get_links_from(&harness.conn(), &weak_counter.memory_id).unwrap(),
        vec![link]
    );
}

#[test]
fn every_link_type_inserts_reads_deletes_and_emits_events() {
    let harness = TestHarness::new();
    for link_type in all_link_types() {
        let source = created_memory(&harness, &format!("source-{}", link_type.as_str()));
        let target = created_memory(&harness, &format!("target-{}", link_type.as_str()));
        let link = typed_link(link_type, &source.memory_id, &target.memory_id);

        insert_link(&harness.conn(), &link).expect("link inserts");
        let source_before = harness
            .store
            .get(&source.memory_id, &harness.branch_scope())
            .expect("source loads");
        harness
            .store
            .write_transition(
                &source_before,
                source_before.clone(),
                &format!("{} link inserted", link_type.as_str()),
                Some(memory_evidence(&format!("insert-{}", link_type.as_str()))),
            )
            .expect("insert event emits");

        assert_eq!(
            get_links_from(&harness.conn(), &source.memory_id).unwrap(),
            vec![link.clone()]
        );
        assert_eq!(
            get_links_to(
                &harness.conn(),
                &MemoryLinkTarget::Memory(target.memory_id.clone())
            )
            .unwrap(),
            vec![link.clone()]
        );

        harness
            .conn()
            .execute(
                "DELETE FROM memory_links WHERE link_id = ?1",
                rusqlite::params![link.link_id.as_str()],
            )
            .expect("link deletes");
        let source_after = harness
            .store
            .get(&source.memory_id, &harness.branch_scope())
            .expect("source reloads");
        harness
            .store
            .write_transition(
                &source_after,
                source_after.clone(),
                &format!("{} link deleted", link_type.as_str()),
                Some(memory_evidence(&format!("delete-{}", link_type.as_str()))),
            )
            .expect("delete event emits");

        assert!(get_links_from(&harness.conn(), &source.memory_id)
            .unwrap()
            .is_empty());
    }

    let update_count = harness
        .tail_event_payloads(all_link_types().len() * 4)
        .into_iter()
        .filter(|payload| matches!(payload, EventPayload::MemoryUpdated(_)))
        .count();
    assert_eq!(update_count, all_link_types().len() * 2);
}

fn all_link_types() -> [MemoryLinkType; 11] {
    [
        MemoryLinkType::Supports,
        MemoryLinkType::Contradicts,
        MemoryLinkType::Supersedes,
        MemoryLinkType::Refines,
        MemoryLinkType::Generalizes,
        MemoryLinkType::Specializes,
        MemoryLinkType::CoOccursWith,
        MemoryLinkType::DerivedFrom,
        MemoryLinkType::AppliesTo,
        MemoryLinkType::ValidatedBy,
        MemoryLinkType::InvalidatedBy,
    ]
}

fn verified_memory(harness: &TestHarness, seed: &str) -> crate::memory_graph::MemoryRecord {
    let created = created_memory(harness, seed);
    transition_status(
        &harness.store,
        created.memory_id.clone(),
        VerificationStatus::Verified,
        "seed verified",
        Some(memory_evidence(&format!("verify-{seed}"))),
    )
    .expect("seed verifies")
}

fn created_memory(harness: &TestHarness, seed: &str) -> crate::memory_graph::MemoryRecord {
    harness
        .store
        .create(
            harness
                .draft(seed)
                .with_initial_evidence(memory_evidence(&format!("seed-{seed}")))
                .into(),
            idem(&format!("create-{seed}")),
        )
        .expect("memory creates")
}

fn contradiction_link(
    source: &crate::identity::MemoryId,
    target: &crate::identity::MemoryId,
    seed: &str,
) -> MemoryLink {
    typed_link_with(
        MemoryLinkType::Contradicts,
        source,
        target,
        seed,
        0.85,
        Some(event_id(&format!("contradiction-{seed}"))),
    )
}

fn supersedes_link(
    source: &crate::identity::MemoryId,
    target: &crate::identity::MemoryId,
    seed: &str,
) -> MemoryLink {
    typed_link_with(
        MemoryLinkType::Supersedes,
        source,
        target,
        seed,
        0.9,
        Some(event_id(&format!("supersedes-{seed}"))),
    )
}

fn typed_link(
    link_type: MemoryLinkType,
    source: &crate::identity::MemoryId,
    target: &crate::identity::MemoryId,
) -> MemoryLink {
    typed_link_with(link_type, source, target, link_type.as_str(), 0.6, None)
}

fn typed_link_with(
    link_type: MemoryLinkType,
    source: &crate::identity::MemoryId,
    target: &crate::identity::MemoryId,
    seed: &str,
    strength: f32,
    evidence_event_id: Option<crate::identity::EventId>,
) -> MemoryLink {
    MemoryLink::try_new(
        MemoryLinkId(format!("link-{}-{}", link_type.as_str(), stable_ulid(seed))),
        source.clone(),
        MemoryLinkTarget::Memory(target.clone()),
        link_type,
        strength,
        format!("{} relation", link_type.as_str()),
        evidence_event_id,
        Actor::Assistant {
            model: "gpt-test".to_string(),
        },
        Utc::now(),
        VerificationStatus::Verified,
    )
    .expect("link validates")
}
