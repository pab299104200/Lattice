use std::collections::BTreeMap;

use crate::identity::{Identity, MemoryId};
use crate::memory::{Memory, MemoryScope, MemoryType, MemoryVerificationStatus};
use crate::retrieval_v1::{
    Candidate, CandidateSource, MemoryScoringMetadata, RankedCandidate, ScoringContext,
};
use crate::verification::surfacing::{
    classify_status, BundleSection, LabeledMemory, SurfacingPipeline,
};

#[test]
fn verified_memories_land_in_trusted_and_all_other_states_are_advisory() {
    let memories = all_statuses()
        .into_iter()
        .map(|status| memory_for_status(status, status.as_str()))
        .collect::<Vec<_>>();

    let bundle = SurfacingPipeline::partition(memories);

    assert_eq!(bundle.trusted.len(), 1);
    assert_eq!(bundle.trusted[0].section, BundleSection::Trusted);

    let advisory_sections = bundle
        .advisory
        .iter()
        .map(|memory| memory.section)
        .collect::<Vec<_>>();
    assert_eq!(
        advisory_sections,
        vec![
            BundleSection::Unverified,
            BundleSection::InReview,
            BundleSection::Stale,
            BundleSection::Contradicted,
            BundleSection::Superseded,
            BundleSection::Expired,
            BundleSection::Invalidated,
        ]
    );
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "stale_in_trusted_bundle")]
fn debug_assertion_fires_when_non_trusted_memory_is_injected_into_trusted() {
    let mut trusted = vec![LabeledMemory {
        memory_id: MemoryId {
            workspace_id: "workspace".to_string(),
            ulid: "stale-memory".to_string(),
        },
        section: BundleSection::Stale,
        label_reason: "stale verifier label".to_string(),
        content: "stale guidance".to_string(),
    }];

    SurfacingPipeline::enforce_trusted_invariant_for_test(&mut trusted, module_path!());
}

#[test]
fn label_reason_is_non_empty_and_uses_verifier_reason_scope_file_and_expiry() {
    let memory = memory_for_status(
        MemoryVerificationStatus::Expired,
        "file `src/foo.rs` line 42 changed; expiry 2026-04-01",
    );

    let bundle = SurfacingPipeline::partition(vec![memory]);
    let reason = &bundle.advisory[0].label_reason;

    assert!(reason.contains("file `src/foo.rs` line 42 changed"));
    assert!(reason.contains("expiry 2026-04-01"));
    assert!(reason.contains("scope branch `main`"));
    assert!(reason.contains("file `src/foo.rs`"));
}

#[test]
fn ranker_top_n_excludes_every_non_verified_memory_regardless_of_score() {
    let verified = ranked_memory("verified", 1.0);
    let stale = ranked_memory("stale", 100.0);
    let contradicted = ranked_memory("contradicted", 90.0);
    let ctx = scoring_context([
        ("verified", MemoryVerificationStatus::Verified),
        ("stale", MemoryVerificationStatus::Stale),
        ("contradicted", MemoryVerificationStatus::Contradicted),
    ]);

    let trusted =
        SurfacingPipeline::trusted_ranked_top_n(vec![stale, contradicted, verified], &ctx, 10);

    assert_eq!(trusted.len(), 1);
    assert_eq!(trusted[0].candidate.identity, memory_identity("verified"));
}

#[test]
fn classify_covers_all_verification_status_variants() {
    let sections = all_statuses()
        .into_iter()
        .map(|status| classify_status(&status))
        .collect::<Vec<_>>();

    assert_eq!(
        sections,
        vec![
            BundleSection::Trusted,
            BundleSection::Unverified,
            BundleSection::InReview,
            BundleSection::Stale,
            BundleSection::Contradicted,
            BundleSection::Superseded,
            BundleSection::Expired,
            BundleSection::Invalidated,
        ]
    );
}

fn all_statuses() -> Vec<MemoryVerificationStatus> {
    vec![
        MemoryVerificationStatus::Verified,
        MemoryVerificationStatus::Unverified,
        MemoryVerificationStatus::InReview,
        MemoryVerificationStatus::Stale,
        MemoryVerificationStatus::Contradicted,
        MemoryVerificationStatus::Superseded,
        MemoryVerificationStatus::Expired,
        MemoryVerificationStatus::Invalidated,
    ]
}

fn memory_for_status(status: MemoryVerificationStatus, reason: &str) -> Memory {
    Memory {
        id: status.as_str().to_string(),
        session_id: "session".to_string(),
        content: format!("{} guidance", status.as_str()),
        memory_type: MemoryType::Observation,
        scope: MemoryScope::Branch,
        confidence: 0.9,
        linked_symbols: Vec::new(),
        linked_files: vec!["src/foo.rs".to_string()],
        workspace_id: Some("workspace".to_string()),
        branch: Some("main".to_string()),
        scope_organization_id: None,
        refresh_key: None,
        source_query: None,
        created_at: 1,
        last_accessed: 2,
        access_count: 0,
        is_stale: status != MemoryVerificationStatus::Verified,
        stale_reason: Some(reason.to_string()),
        verification_status: status,
    }
}

fn ranked_memory(id: &str, score: f32) -> RankedCandidate {
    RankedCandidate {
        candidate: Candidate {
            identity: memory_identity(id),
            source: CandidateSource::MemoryLinks,
            seed_anchor: None,
            raw_score: score as f64,
            preliminary_inclusion_reason: format!("memory {id}"),
            expansion_handle_hint: None,
            traversal_path: Vec::new(),
            budget_exhausted: false,
        },
        total_score: score,
        signal_scores: Vec::new(),
        inclusion_reason: format!("ranked memory {id}"),
    }
}

fn scoring_context(
    statuses: impl IntoIterator<Item = (&'static str, MemoryVerificationStatus)>,
) -> ScoringContext {
    let mut memory_metadata = BTreeMap::new();
    for (id, status) in statuses {
        memory_metadata.insert(
            memory_identity(id).to_string(),
            MemoryScoringMetadata {
                verification_status: status,
                scope: MemoryScope::Repo,
                confidence: 1.0,
                evidence_count: 1,
                created_at: Some(1),
                last_accessed: Some(2),
                access_count: 0,
                is_stale: status != MemoryVerificationStatus::Verified,
                superseded_by_memory_id: None,
                contradicted_by_memory_ids: Vec::new(),
            },
        );
    }
    ScoringContext {
        memory_metadata,
        ..ScoringContext::default()
    }
}

fn memory_identity(id: &str) -> Identity {
    Identity::Memory(MemoryId {
        workspace_id: "workspace".to_string(),
        ulid: id.to_string(),
    })
}
