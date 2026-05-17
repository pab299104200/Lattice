use crate::identity::Identity;
use crate::memory::MemoryVerificationStatus;

use super::{
    AnchorResolution, Candidate, CandidateSource, DiagnosticMode, IntentLabel,
    MemoryScoringMetadata, RankedCandidate, ResolvedAnchor, ScoringContext, ScoringWeights,
    SignalKind, SignalScore, UserPreferenceSignal,
};

pub(super) fn signal(
    signal: SignalKind,
    raw: f32,
    weights: &ScoringWeights,
    reason: String,
) -> SignalScore {
    SignalScore {
        signal,
        raw,
        weighted: raw * weights.weight_for(signal),
        reason,
    }
}

pub(super) fn skipped(signal: SignalKind, weights: &ScoringWeights, field: &str) -> SignalScore {
    SignalScore {
        signal,
        raw: 0.0,
        weighted: 0.0 * weights.weight_for(signal),
        reason: format!("no {field} available; signal skipped"),
    }
}

pub(super) fn source_intent_score(source: CandidateSource, label: IntentLabel) -> f32 {
    match (label, source) {
        (_, CandidateSource::ExactPathSymbolLookup) => 1.0,
        (IntentLabel::Debug, CandidateSource::EventSimilarity) => 0.9,
        (IntentLabel::Debug, CandidateSource::CodeGraphTraversal) => 0.85,
        (IntentLabel::UpdateDocs, CandidateSource::DocBacklinksOutgoingLinks) => 1.0,
        (IntentLabel::Explain, CandidateSource::DocBacklinksOutgoingLinks) => 0.9,
        (IntentLabel::Refactor, CandidateSource::CodeGraphTraversal) => 1.0,
        (
            IntentLabel::AddFeature | IntentLabel::ModifyFeature,
            CandidateSource::CodeGraphTraversal,
        ) => 0.85,
        (IntentLabel::AddTest, CandidateSource::WorkflowSimilarity) => 0.8,
        (IntentLabel::Performance, CandidateSource::Embeddings) => 0.8,
        (IntentLabel::Review, CandidateSource::MemoryLinks) => 0.8,
        (_, CandidateSource::RecentActiveWorkingMemory) => 0.65,
        (_, CandidateSource::Fts | CandidateSource::Embeddings) => 0.55,
        (_, CandidateSource::MemoryLinks | CandidateSource::WorkflowSimilarity) => 0.5,
        _ => 0.35,
    }
}

pub(super) fn graph_proximity_raw(candidate: &Candidate, anchors: &[Identity]) -> f32 {
    if candidate
        .seed_anchor
        .as_ref()
        .is_some_and(|seed| anchors.contains(seed))
    {
        return 1.0;
    }
    if candidate.traversal_path.is_empty() {
        return 0.5;
    }
    (1.0 / (candidate.traversal_path.len() as f32 + 1.0)).clamp(0.0, 1.0)
}

pub(super) fn resolved_anchor_identities(anchors: &[ResolvedAnchor]) -> Vec<Identity> {
    anchors
        .iter()
        .filter_map(|anchor| match &anchor.resolution {
            AnchorResolution::Resolved(identity) => Some(identity.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn anchor_matches_identity(
    anchor: &ResolvedAnchor,
    identity: &Identity,
    identity_text: &str,
) -> bool {
    if matches!(&anchor.resolution, AnchorResolution::Resolved(resolved) if resolved == identity) {
        return true;
    }
    identity_terms(identity)
        .iter()
        .any(|term| term == &anchor.anchor_text || identity_text.contains(&anchor.anchor_text))
}

pub(super) fn memory_metadata<'a>(
    candidate: &Candidate,
    ctx: &'a ScoringContext,
) -> Option<&'a MemoryScoringMetadata> {
    ctx.memory_metadata.get(&candidate.identity.to_string())
}

pub(super) fn verification_raw(metadata: &MemoryScoringMetadata) -> f32 {
    match metadata.verification_status {
        MemoryVerificationStatus::Verified => 1.0,
        MemoryVerificationStatus::InReview => 0.55,
        MemoryVerificationStatus::Unverified => 0.3,
        MemoryVerificationStatus::Stale
        | MemoryVerificationStatus::Contradicted
        | MemoryVerificationStatus::Superseded
        | MemoryVerificationStatus::Expired
        | MemoryVerificationStatus::Invalidated => -1.0,
    }
}

pub(super) fn contradiction_raw(metadata: &MemoryScoringMetadata) -> f32 {
    if metadata.superseded_by_memory_id.is_some() || !metadata.contradicted_by_memory_ids.is_empty()
    {
        return -1.0;
    }
    if matches!(
        metadata.verification_status,
        MemoryVerificationStatus::Contradicted | MemoryVerificationStatus::Superseded
    ) {
        return -1.0;
    }
    1.0
}

pub(super) fn contradiction_reason(metadata: &MemoryScoringMetadata) -> String {
    if metadata.superseded_by_memory_id.is_some() {
        return "memory has been superseded by a newer assertion".to_string();
    }
    if !metadata.contradicted_by_memory_ids.is_empty() {
        return "memory has contradicting assertion(s)".to_string();
    }
    "memory has no contradiction or supersession marker".to_string()
}

pub(super) fn apply_untrusted_penalty(
    score: &mut SignalScore,
    candidate: &Candidate,
    metadata: &MemoryScoringMetadata,
    weights: &ScoringWeights,
    mode: DiagnosticMode,
    ctx: &ScoringContext,
) {
    if !is_untrusted_memory(metadata) || diagnostics_allows_untrusted(mode, ctx) {
        return;
    }
    score.weighted += weights.stale_memory_hard_penalty;
    score.reason = format!(
        "{}; hard penalty applied to {}",
        score.reason, candidate.identity
    );
}

pub(super) fn freshness_timestamp(candidate: &Candidate, ctx: &ScoringContext) -> Option<u64> {
    memory_metadata(candidate, ctx)
        .and_then(|metadata| metadata.last_accessed.or(metadata.created_at))
        .or_else(|| {
            ctx.event_history
                .get(&candidate.identity.to_string())
                .and_then(|history| history.occurred_at)
        })
}

pub(super) fn count_raw(count: u32) -> f32 {
    if count == 0 {
        0.0
    } else {
        ((count.min(10) as f32) / 10.0).max(0.1)
    }
}

pub(super) fn preference_raw(candidate_text: &str, preferences: &[UserPreferenceSignal]) -> f32 {
    let mut raw: f32 = 0.0;
    for preference in preferences {
        if has_term(candidate_text, &preference.preferred_terms) {
            raw += 0.5;
        }
        if has_term(candidate_text, &preference.conflicting_terms) {
            raw -= 0.5;
        }
    }
    raw.clamp(-1.0, 1.0)
}

pub(super) fn candidate_text(candidate: &Candidate) -> String {
    format!(
        "{} {} {:?}",
        candidate.identity, candidate.preliminary_inclusion_reason, candidate.source
    )
}

pub(super) fn inclusion_reason(
    candidate: &Candidate,
    signal_scores: &[SignalScore],
    mode: DiagnosticMode,
) -> String {
    match mode {
        DiagnosticMode::Compact => compact_reason(candidate, signal_scores),
        DiagnosticMode::Diagnostic => diagnostic_reason(signal_scores),
    }
}

pub(super) fn compare_ranked_candidates(
    left: &RankedCandidate,
    right: &RankedCandidate,
) -> std::cmp::Ordering {
    right
        .total_score
        .partial_cmp(&left.total_score)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| {
            left.candidate
                .identity
                .to_string()
                .cmp(&right.candidate.identity.to_string())
        })
}

fn identity_terms(identity: &Identity) -> Vec<String> {
    match identity {
        Identity::File(file) => vec![file.repo_relative_path.clone()],
        Identity::Symbol(symbol) => vec![
            symbol.qualified_name.clone(),
            symbol.file.repo_relative_path.clone(),
        ],
        Identity::Doc(doc) => vec![doc.repo_relative_path.clone()],
        Identity::Section(section) => section.heading_path.clone(),
        Identity::Memory(memory) => vec![memory.ulid.clone()],
        Identity::Event(event) => vec![event.ulid.clone()],
        Identity::ContextHandle(handle) => vec![handle.ulid.clone()],
    }
}

fn is_untrusted_memory(metadata: &MemoryScoringMetadata) -> bool {
    metadata.is_stale
        || matches!(
            metadata.verification_status,
            MemoryVerificationStatus::Stale
                | MemoryVerificationStatus::Contradicted
                | MemoryVerificationStatus::Superseded
        )
        || metadata.superseded_by_memory_id.is_some()
        || !metadata.contradicted_by_memory_ids.is_empty()
}

fn diagnostics_allows_untrusted(mode: DiagnosticMode, ctx: &ScoringContext) -> bool {
    mode == DiagnosticMode::Diagnostic && ctx.allow_untrusted_memory_in_diagnostics
}

fn has_term(candidate_text: &str, terms: &[String]) -> bool {
    terms
        .iter()
        .any(|term| candidate_text.contains(&term.to_lowercase()))
}

fn compact_reason(candidate: &Candidate, signal_scores: &[SignalScore]) -> String {
    let mut positive = signal_scores
        .iter()
        .filter(|score| score.raw > 0.0)
        .map(|score| format!("{:?}", score.signal))
        .take(3)
        .collect::<Vec<_>>();
    if positive.is_empty() {
        positive.push("retrieval candidate".to_string());
    }
    format!(
        "{}; ranked by {}",
        candidate.preliminary_inclusion_reason,
        positive.join(", ")
    )
}

fn diagnostic_reason(signal_scores: &[SignalScore]) -> String {
    signal_scores
        .iter()
        .map(|score| {
            format!(
                "{:?}: raw {:.3}, weighted {:.3}, {}",
                score.signal, score.raw, score.weighted, score.reason
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
}
