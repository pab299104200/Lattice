use crate::identity::{FileId, Identity, MemoryId};
use crate::memory::{MemoryScope, MemoryVerificationStatus};
use crate::retrieval_v1::{
    classify_intent, score_candidates, AnchorKind, AnchorResolution, Candidate, CandidateSource,
    DiagnosticMode, EventScoringMetadata, MemoryScoringMetadata, RankingDiagnostics,
    ResolvedAnchor, ScoringContext, ScoringWeights, SignalKind, SourceSpan, UserPreferenceSignal,
};

const WORKSPACE: &str = "workspace-main";

#[test]
fn representative_candidate_evaluates_all_thirteen_signals() {
    let candidate = memory_candidate(
        "memory-auth",
        CandidateSource::MemoryLinks,
        "auth preferred",
    );
    let anchors = vec![resolved_anchor(candidate.identity.clone(), "memory-auth")];
    let intent = classify_intent("review auth memory");
    let ctx = rich_context(&candidate.identity, MemoryVerificationStatus::Verified);

    let ranked = score_candidates(
        vec![candidate],
        &intent,
        &anchors,
        &ScoringWeights::default(),
        DiagnosticMode::Diagnostic,
        &ctx,
    );
    let signal_scores = &ranked[0].signal_scores;

    assert_eq!(signal_scores.len(), 13);
    for kind in all_signal_kinds() {
        assert!(
            signal_scores.iter().any(|score| score.signal == kind),
            "missing signal {kind:?}"
        );
    }
    assert!(signal_scores.iter().all(|score| !score.reason.is_empty()));
}

#[test]
fn missing_signal_data_scores_zero_with_documented_reason() {
    let candidate = file_candidate("src/plain.rs", CandidateSource::Fts, "plain result");
    let intent = classify_intent("explain plain result");
    let ranked = score_candidates(
        vec![candidate],
        &intent,
        &[],
        &ScoringWeights::default(),
        DiagnosticMode::Diagnostic,
        &ScoringContext::default(),
    );

    for kind in [
        SignalKind::GraphProximity,
        SignalKind::ExactIdentifierMatch,
        SignalKind::SemanticSimilarity,
        SignalKind::VerificationStatus,
        SignalKind::Freshness,
        SignalKind::Scope,
        SignalKind::EvidenceStrength,
        SignalKind::ContradictionOrSupersessionState,
        SignalKind::RecentSuccessfulReuse,
        SignalKind::TokenCost,
    ] {
        let score = find_signal(&ranked[0], kind);
        assert_eq!(score.raw, 0.0);
        assert!(score.reason.contains("signal skipped"));
    }
}

#[test]
fn stale_and_contradicted_memories_rank_below_trusted_peers() {
    let trusted = memory_candidate("trusted", CandidateSource::MemoryLinks, "same raw match");
    let stale = memory_candidate("stale", CandidateSource::MemoryLinks, "same raw match");
    let contradicted = memory_candidate(
        "contradicted",
        CandidateSource::MemoryLinks,
        "same raw match",
    );
    let mut ctx = ScoringContext::default();
    insert_memory(
        &mut ctx,
        &trusted.identity,
        MemoryVerificationStatus::Verified,
    );
    insert_memory(&mut ctx, &stale.identity, MemoryVerificationStatus::Stale);
    insert_memory(
        &mut ctx,
        &contradicted.identity,
        MemoryVerificationStatus::Contradicted,
    );

    let ranked = score_candidates(
        vec![stale.clone(), contradicted.clone(), trusted.clone()],
        &classify_intent("review memory"),
        &[],
        &ScoringWeights::default(),
        DiagnosticMode::Compact,
        &ctx,
    );

    assert_eq!(ranked[0].candidate.identity, trusted.identity);
    assert!(ranked[0].total_score > ranked[1].total_score);
    assert!(ranked[0].total_score > ranked[2].total_score);
}

#[test]
fn diagnostic_mode_round_trips_signal_scores_and_reasons_through_serde() {
    let candidate = memory_candidate(
        "serde-memory",
        CandidateSource::MemoryLinks,
        "serde preferred",
    );
    let intent = classify_intent("review serde memory");
    let anchors = vec![resolved_anchor(candidate.identity.clone(), "serde-memory")];
    let weights = ScoringWeights::default();
    let ranked = score_candidates(
        vec![candidate],
        &intent,
        &anchors,
        &weights,
        DiagnosticMode::Diagnostic,
        &rich_context(
            &memory_identity("serde-memory"),
            MemoryVerificationStatus::Verified,
        ),
    );
    let diagnostics = RankingDiagnostics::new(intent, anchors, ranked, weights);

    let encoded = serde_json::to_string(&diagnostics).expect("serialize diagnostics");
    let decoded: RankingDiagnostics =
        serde_json::from_str(&encoded).expect("deserialize diagnostics");

    assert_eq!(decoded.ranked.len(), 1);
    assert_eq!(decoded.ranked[0].signal_scores.len(), 13);
    assert!(decoded.ranked[0].inclusion_reason.contains("raw"));
}

#[test]
fn compact_mode_produces_non_empty_single_line_reason() {
    let candidate = file_candidate(
        "src/compact.rs",
        CandidateSource::ExactPathSymbolLookup,
        "exact",
    );
    let anchors = vec![resolved_anchor(
        candidate.identity.clone(),
        "src/compact.rs",
    )];
    let ranked = score_candidates(
        vec![candidate],
        &classify_intent("explain src/compact.rs"),
        &anchors,
        &ScoringWeights::default(),
        DiagnosticMode::Compact,
        &ScoringContext::default(),
    );

    assert!(!ranked[0].inclusion_reason.is_empty());
    assert!(!ranked[0].inclusion_reason.contains('\n'));
}

#[test]
fn weight_overrides_change_ranking_deterministically() {
    let exact = file_candidate(
        "src/exact.rs",
        CandidateSource::ExactPathSymbolLookup,
        "exact",
    );
    let semantic = file_candidate("src/semantic.rs", CandidateSource::Embeddings, "semantic");
    let anchors = vec![resolved_anchor(exact.identity.clone(), "src/exact.rs")];
    let mut ctx = ScoringContext::default();
    ctx.semantic_similarity
        .insert(semantic.identity.to_string(), 1.0);
    let intent = classify_intent("explain src/exact.rs");

    let default_ranked = score_candidates(
        vec![semantic.clone(), exact.clone()],
        &intent,
        &anchors,
        &ScoringWeights::default(),
        DiagnosticMode::Compact,
        &ctx,
    );
    let semantic_weights = ScoringWeights {
        exact_identifier_match: 0.0,
        semantic_similarity: 5.0,
        ..ScoringWeights::default()
    };
    let overridden = score_candidates(
        vec![semantic.clone(), exact.clone()],
        &intent,
        &anchors,
        &semantic_weights,
        DiagnosticMode::Compact,
        &ctx,
    );

    assert_eq!(default_ranked[0].candidate.identity, exact.identity);
    assert_eq!(overridden[0].candidate.identity, semantic.identity);
}

#[test]
fn user_preference_compatibility_raises_matches_and_lowers_conflicts() {
    let preferred = file_candidate("src/boring.rs", CandidateSource::Fts, "uses boring helper");
    let conflicting = file_candidate(
        "src/clever.rs",
        CandidateSource::Fts,
        "uses clever shortcut",
    );
    let mut ctx = ScoringContext::default();
    ctx.user_preferences.push(UserPreferenceSignal {
        preference_key: "style".to_string(),
        preferred_terms: vec!["boring helper".to_string()],
        conflicting_terms: vec!["clever shortcut".to_string()],
    });

    let ranked = score_candidates(
        vec![conflicting.clone(), preferred.clone()],
        &classify_intent("review helper style"),
        &[],
        &ScoringWeights::default(),
        DiagnosticMode::Diagnostic,
        &ctx,
    );
    let preferred_score = preference_score(&ranked, &preferred.identity);
    let conflicting_score = preference_score(&ranked, &conflicting.identity);

    assert!(preferred_score > 0.0);
    assert!(conflicting_score < 0.0);
    assert_eq!(ranked[0].candidate.identity, preferred.identity);
}

fn all_signal_kinds() -> [SignalKind; 13] {
    [
        SignalKind::TaskTypeCompatibility,
        SignalKind::GraphProximity,
        SignalKind::ExactIdentifierMatch,
        SignalKind::SemanticSimilarity,
        SignalKind::VerificationStatus,
        SignalKind::Freshness,
        SignalKind::Scope,
        SignalKind::EvidenceStrength,
        SignalKind::ContradictionOrSupersessionState,
        SignalKind::PastUsefulness,
        SignalKind::RecentSuccessfulReuse,
        SignalKind::UserPreferenceCompatibility,
        SignalKind::TokenCost,
    ]
}

fn rich_context(identity: &Identity, status: MemoryVerificationStatus) -> ScoringContext {
    let mut ctx = ScoringContext {
        now_unix_seconds: Some(2_000),
        ..ScoringContext::default()
    };
    insert_memory(&mut ctx, identity, status);
    ctx.semantic_similarity.insert(identity.to_string(), 0.82);
    ctx.event_history.insert(
        identity.to_string(),
        EventScoringMetadata {
            past_usefulness_count: 5,
            recent_successful_reuse_count: 3,
            occurred_at: Some(1_980),
        },
    );
    ctx.user_preferences.push(UserPreferenceSignal {
        preference_key: "style".to_string(),
        preferred_terms: vec!["preferred".to_string()],
        conflicting_terms: vec!["avoid".to_string()],
    });
    ctx.token_estimates.insert(identity.to_string(), 800);
    ctx
}

fn insert_memory(ctx: &mut ScoringContext, identity: &Identity, status: MemoryVerificationStatus) {
    let is_stale = status == MemoryVerificationStatus::Stale;
    let superseded =
        (status == MemoryVerificationStatus::Superseded).then_some("newer".to_string());
    let contradicted = if status == MemoryVerificationStatus::Contradicted {
        vec!["other".to_string()]
    } else {
        Vec::new()
    };
    ctx.memory_metadata.insert(
        identity.to_string(),
        MemoryScoringMetadata {
            verification_status: status,
            scope: MemoryScope::Repo,
            confidence: 0.95,
            evidence_count: 3,
            created_at: Some(1_900),
            last_accessed: Some(1_990),
            access_count: 4,
            is_stale,
            superseded_by_memory_id: superseded,
            contradicted_by_memory_ids: contradicted,
        },
    );
}

fn preference_score(ranked: &[crate::retrieval_v1::RankedCandidate], identity: &Identity) -> f32 {
    ranked
        .iter()
        .find(|candidate| &candidate.candidate.identity == identity)
        .map(|candidate| find_signal(candidate, SignalKind::UserPreferenceCompatibility).raw)
        .expect("ranked candidate")
}

fn find_signal(
    ranked: &crate::retrieval_v1::RankedCandidate,
    kind: SignalKind,
) -> &crate::retrieval_v1::SignalScore {
    ranked
        .signal_scores
        .iter()
        .find(|score| score.signal == kind)
        .expect("signal score")
}

fn resolved_anchor(identity: Identity, anchor_text: &str) -> ResolvedAnchor {
    ResolvedAnchor {
        kind: AnchorKind::Path,
        anchor_text: anchor_text.to_string(),
        source_span: SourceSpan {
            start: 0,
            end: anchor_text.len(),
        },
        resolution: AnchorResolution::Resolved(identity),
    }
}

fn file_candidate(path: &str, source: CandidateSource, reason: &str) -> Candidate {
    Candidate {
        identity: Identity::File(file_id(path)),
        source,
        seed_anchor: None,
        raw_score: 0.65,
        preliminary_inclusion_reason: reason.to_string(),
        expansion_handle_hint: None,
        traversal_path: Vec::new(),
        budget_exhausted: false,
    }
}

fn memory_candidate(id: &str, source: CandidateSource, reason: &str) -> Candidate {
    let identity = memory_identity(id);
    Candidate {
        identity: identity.clone(),
        source,
        seed_anchor: Some(identity.clone()),
        raw_score: 0.65,
        preliminary_inclusion_reason: reason.to_string(),
        expansion_handle_hint: None,
        traversal_path: vec![identity.clone()],
        budget_exhausted: false,
    }
}

fn memory_identity(id: &str) -> Identity {
    Identity::Memory(MemoryId {
        workspace_id: WORKSPACE.to_string(),
        ulid: id.to_string(),
    })
}

fn file_id(path: &str) -> FileId {
    FileId {
        workspace_id: WORKSPACE.to_string(),
        repo_relative_path: path.to_string(),
        content_hash: "hash".to_string(),
    }
}
