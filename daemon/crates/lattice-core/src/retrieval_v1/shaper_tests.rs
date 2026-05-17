use std::collections::{BTreeMap, HashSet};

use crate::identity::{decode_identity, FileId, Identity, MemoryId, SymbolId};
use crate::retrieval_v1::{
    classify_intent, schema::RetrievalBundle, shape_retrieval_bundle, Candidate, CandidateSource,
    DiagnosticMode, RankedCandidate, RankingDiagnostics, ResolvedAnchor, ScoringWeights,
    ShaperBudget, ShaperContext, SignalKind, SignalScore,
};

const WORKSPACE: &str = "workspace-main";

#[test]
fn every_bundle_result_has_compact_inclusion_reason() {
    let ranked = vec![file_ranked("src/auth.rs", 9.0, CandidateSource::Fts)];
    let bundle = shape_retrieval_bundle(
        "task-31",
        "review auth",
        "auth anchors",
        ranked,
        DiagnosticMode::Compact,
        None,
        &context(320, []),
    );

    assert_eq!(bundle.results.len(), 1);
    let reason = &bundle.results[0].inclusion_reason;
    assert!(!reason.is_empty());
    assert!(reason.len() <= 120);
}

#[test]
fn every_expansion_handle_resolves_back_to_original_identity() {
    let ranked = vec![
        file_ranked("src/auth.rs", 8.0, CandidateSource::Fts),
        symbol_ranked("src/auth.rs", "auth::login", 64, 7.5),
        memory_ranked("01ARZ3NDEKTSV4RRFFQ69G5FAV", 7.0),
    ];
    let bundle = shape_retrieval_bundle(
        "task-31",
        "review auth",
        "auth anchors",
        ranked,
        DiagnosticMode::Compact,
        None,
        &context(400, []),
    );

    for result in bundle.results {
        let decoded = decode_identity(&result.expansion_handle).expect("decode expansion handle");
        assert_eq!(decoded, result.identity);
    }
}

#[test]
fn deduplication_merges_signal_scores_and_sources() {
    let identity = file_identity("src/auth.rs");
    let ranked = vec![
        ranked_candidate(
            identity.clone(),
            CandidateSource::Fts,
            5.0,
            vec![
                signal(SignalKind::TaskTypeCompatibility, 0.4, 0.4, "task"),
                signal(SignalKind::SemanticSimilarity, 0.3, 0.3, "semantic low"),
            ],
            "fts candidate",
        ),
        ranked_candidate(
            identity,
            CandidateSource::Embeddings,
            7.0,
            vec![
                signal(SignalKind::TaskTypeCompatibility, 0.8, 0.8, "task high"),
                signal(SignalKind::SemanticSimilarity, 0.9, 0.9, "semantic high"),
            ],
            "embedding candidate",
        ),
    ];

    let bundle = shape_retrieval_bundle(
        "task-31",
        "review auth",
        "auth anchors",
        ranked,
        DiagnosticMode::Compact,
        None,
        &context(320, []),
    );

    assert_eq!(bundle.results.len(), 1);
    assert_eq!(
        bundle.results[0].source,
        vec![CandidateSource::Fts, CandidateSource::Embeddings]
    );
    assert_eq!(bundle.results[0].score, 7.0);
}

#[test]
fn compression_respects_budget_without_dropping_pinned_results() {
    let pinned = file_identity("src/pinned.rs");
    let ranked = vec![
        ranked_with_long_snippet(pinned.clone(), CandidateSource::Fts, 9.0),
        ranked_with_long_snippet(
            file_identity("src/other-a.rs"),
            CandidateSource::Embeddings,
            5.0,
        ),
        ranked_with_long_snippet(
            file_identity("src/other-b.rs"),
            CandidateSource::MemoryLinks,
            4.0,
        ),
    ];
    let bundle = shape_retrieval_bundle(
        "task-31",
        "review auth",
        "auth anchors",
        ranked,
        DiagnosticMode::Compact,
        None,
        &ShaperContext {
            pins: HashSet::from([pinned.clone()]),
            budget: ShaperBudget { max_tokens: 75 },
        },
    );

    assert!(bundle
        .results
        .iter()
        .any(|result| result.identity == pinned));
    assert!(bundle.budget_report.truncated);
    assert!(
        bundle.budget_report.dropped_results >= 1 || bundle.budget_report.trimmed_snippets >= 1
    );
}

#[test]
fn diagnostic_mode_populates_diagnostics_and_compact_mode_does_not() {
    let ranked = vec![file_ranked("src/auth.rs", 9.0, CandidateSource::Fts)];
    let diagnostics = sample_diagnostics(ranked.clone());

    let compact = shape_retrieval_bundle(
        "task-31",
        "review auth",
        "auth anchors",
        ranked.clone(),
        DiagnosticMode::Compact,
        Some(diagnostics.clone()),
        &context(320, []),
    );
    let diagnostic = shape_retrieval_bundle(
        "task-31",
        "review auth",
        "auth anchors",
        ranked,
        DiagnosticMode::Diagnostic,
        Some(diagnostics),
        &context(320, []),
    );

    assert!(compact.diagnostics.is_none());
    assert!(diagnostic.diagnostics.is_some());
}

#[test]
fn serde_round_trip_preserves_schema_for_phase_five_checkpointing() {
    let ranked = vec![symbol_ranked("src/auth.rs", "auth::login", 64, 7.5)];
    let bundle = shape_retrieval_bundle(
        "task-31",
        "review auth",
        "auth anchors",
        ranked,
        DiagnosticMode::Compact,
        None,
        &context(320, []),
    );

    let encoded = serde_json::to_string(&bundle).expect("serialize bundle");
    let decoded: RetrievalBundle = serde_json::from_str(&encoded).expect("deserialize bundle");

    assert_eq!(decoded, bundle);
}

#[test]
fn truncation_flag_is_set_when_results_are_dropped() {
    let ranked = vec![
        ranked_with_long_snippet(file_identity("src/a.rs"), CandidateSource::Fts, 8.0),
        ranked_with_long_snippet(file_identity("src/b.rs"), CandidateSource::Embeddings, 7.0),
        ranked_with_long_snippet(file_identity("src/c.rs"), CandidateSource::MemoryLinks, 6.0),
    ];

    let bundle = shape_retrieval_bundle(
        "task-31",
        "review auth",
        "auth anchors",
        ranked,
        DiagnosticMode::Compact,
        None,
        &context(60, []),
    );

    assert!(bundle.budget_report.truncated);
    assert!(bundle.budget_report.dropped_results > 0 || bundle.budget_report.trimmed_snippets > 0);
}

fn context<const N: usize>(max_tokens: usize, pins: [Identity; N]) -> ShaperContext {
    ShaperContext {
        pins: HashSet::from(pins),
        budget: ShaperBudget { max_tokens },
    }
}

fn file_ranked(path: &str, total_score: f32, source: CandidateSource) -> RankedCandidate {
    ranked_candidate(
        file_identity(path),
        source,
        total_score,
        vec![
            signal(SignalKind::TaskTypeCompatibility, 1.0, 1.0, "task fit"),
            signal(SignalKind::ExactIdentifierMatch, 0.8, 0.8, "exact id"),
        ],
        "exact path symbol lookup result",
    )
}

fn symbol_ranked(path: &str, name: &str, byte_offset: usize, total_score: f32) -> RankedCandidate {
    ranked_candidate(
        symbol_identity(path, name, byte_offset),
        CandidateSource::CodeGraphTraversal,
        total_score,
        vec![
            signal(SignalKind::GraphProximity, 1.0, 1.1, "graph proximity"),
            signal(SignalKind::TaskTypeCompatibility, 0.8, 0.8, "task fit"),
        ],
        "graph traversal result",
    )
}

fn memory_ranked(ulid: &str, total_score: f32) -> RankedCandidate {
    ranked_candidate(
        Identity::Memory(MemoryId {
            workspace_id: WORKSPACE.to_string(),
            ulid: ulid.to_string(),
        }),
        CandidateSource::MemoryLinks,
        total_score,
        vec![
            signal(SignalKind::EvidenceStrength, 0.8, 0.8, "evidence"),
            signal(SignalKind::VerificationStatus, 1.0, 1.0, "verified"),
        ],
        "memory graph retrieval result",
    )
}

fn ranked_with_long_snippet(
    identity: Identity,
    source: CandidateSource,
    total_score: f32,
) -> RankedCandidate {
    ranked_candidate(
        identity,
        source,
        total_score,
        vec![
            signal(SignalKind::SemanticSimilarity, 1.0, 1.0, "semantic"),
            signal(SignalKind::TaskTypeCompatibility, 0.9, 0.9, "task"),
        ],
        "this is an intentionally long retrieval explanation used to force snippet trimming before result dropping happens in the response shaper",
    )
}

fn ranked_candidate(
    identity: Identity,
    source: CandidateSource,
    total_score: f32,
    signal_scores: Vec<SignalScore>,
    preliminary_reason: &str,
) -> RankedCandidate {
    RankedCandidate {
        candidate: Candidate {
            identity,
            source,
            seed_anchor: None,
            raw_score: total_score as f64,
            preliminary_inclusion_reason: preliminary_reason.to_string(),
            expansion_handle_hint: None,
            traversal_path: Vec::new(),
            budget_exhausted: false,
        },
        total_score,
        signal_scores,
        inclusion_reason: preliminary_reason.to_string(),
    }
}

fn sample_diagnostics(ranked: Vec<RankedCandidate>) -> RankingDiagnostics {
    RankingDiagnostics {
        intent: classify_intent("review auth"),
        anchors: Vec::<ResolvedAnchor>::new(),
        candidates_per_source: BTreeMap::from([(CandidateSource::Fts, ranked.len())]),
        ranked,
        weights: ScoringWeights::default(),
        budget_exhaustion_flags: crate::retrieval_v1::BudgetExhaustionFlags {
            any_budget_exhausted: false,
            exhausted_sources: Vec::new(),
        },
    }
}

fn signal(signal: SignalKind, raw: f32, weighted: f32, reason: &str) -> SignalScore {
    SignalScore {
        signal,
        raw,
        weighted,
        reason: reason.to_string(),
    }
}

fn file_identity(path: &str) -> Identity {
    Identity::File(FileId {
        workspace_id: WORKSPACE.to_string(),
        repo_relative_path: path.to_string(),
        content_hash: "abcdef12".to_string(),
    })
}

fn symbol_identity(path: &str, name: &str, byte_offset: usize) -> Identity {
    Identity::Symbol(SymbolId {
        file: FileId {
            workspace_id: WORKSPACE.to_string(),
            repo_relative_path: path.to_string(),
            content_hash: "abcdef12".to_string(),
        },
        qualified_name: name.to_string(),
        byte_offset,
        kind: "function".to_string(),
    })
}
