//! Feature-based scoring for Retrieval V1 candidates.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 7. Retrieval Engine` defines the ranking signal contract:
//!
//! | Signal | Required input |
//! | --- | --- |
//! | task-type compatibility | `IntentClassification` and candidate source/kind |
//! | graph proximity to anchors | resolved anchors, seed anchor, traversal path |
//! | exact identifier match | resolved anchors and identity text |
//! | semantic similarity | embedding or caller-provided similarity score |
//! | verification status | memory verification metadata |
//! | freshness | memory/event recency metadata |
//! | scope | memory scope metadata |
//! | evidence strength | memory confidence and evidence metadata |
//! | contradiction/supersession state | structured memory contradiction/supersession metadata |
//! | past usefulness | memory access or retrieval event history |
//! | recent successful reuse | workflow success history |
//! | user preference compatibility | observed preference metadata |
//! | token cost | token estimate metadata |

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::memory::{MemoryScope, MemoryVerificationStatus};

use super::scoring_support::*;
use super::{Candidate, CandidateSource, IntentClassification, ResolvedAnchor};

const FRESHNESS_WINDOW_SECONDS: u64 = 60 * 60 * 24 * 30;
const HIGH_TOKEN_COST: u32 = 4_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SignalKind {
    TaskTypeCompatibility,
    GraphProximity,
    ExactIdentifierMatch,
    SemanticSimilarity,
    VerificationStatus,
    Freshness,
    Scope,
    EvidenceStrength,
    ContradictionOrSupersessionState,
    PastUsefulness,
    RecentSuccessfulReuse,
    UserPreferenceCompatibility,
    TokenCost,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalScore {
    pub signal: SignalKind,
    pub raw: f32,
    pub weighted: f32,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankedCandidate {
    pub candidate: Candidate,
    pub total_score: f32,
    pub signal_scores: Vec<SignalScore>,
    pub inclusion_reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticMode {
    Compact,
    Diagnostic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetrievalProfile {
    Balanced,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoringWeights {
    pub task_type_compatibility: f32,
    pub graph_proximity: f32,
    pub exact_identifier_match: f32,
    pub semantic_similarity: f32,
    pub verification_status: f32,
    pub freshness: f32,
    pub scope: f32,
    pub evidence_strength: f32,
    pub contradiction_or_supersession_state: f32,
    pub past_usefulness: f32,
    pub recent_successful_reuse: f32,
    pub user_preference_compatibility: f32,
    pub token_cost: f32,
    /// Hard demotion required by `## Stale Memory Leakage` so stale,
    /// contradicted, and superseded memories cannot outrank trusted peers.
    pub stale_memory_hard_penalty: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryScoringMetadata {
    pub verification_status: MemoryVerificationStatus,
    pub scope: MemoryScope,
    pub confidence: f32,
    pub evidence_count: usize,
    pub created_at: Option<u64>,
    pub last_accessed: Option<u64>,
    pub access_count: u32,
    pub is_stale: bool,
    pub superseded_by_memory_id: Option<String>,
    pub contradicted_by_memory_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventScoringMetadata {
    pub past_usefulness_count: u32,
    pub recent_successful_reuse_count: u32,
    pub occurred_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserPreferenceSignal {
    pub preference_key: String,
    pub preferred_terms: Vec<String>,
    pub conflicting_terms: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScoringContext {
    pub now_unix_seconds: Option<u64>,
    pub semantic_similarity: BTreeMap<String, f32>,
    pub memory_metadata: BTreeMap<String, MemoryScoringMetadata>,
    pub event_history: BTreeMap<String, EventScoringMetadata>,
    pub user_preferences: Vec<UserPreferenceSignal>,
    pub token_estimates: BTreeMap<String, u32>,
    pub allow_untrusted_memory_in_diagnostics: bool,
}

impl Default for ScoringWeights {
    fn default() -> Self {
        Self::for_profile(RetrievalProfile::Balanced)
    }
}

impl ScoringWeights {
    pub fn for_profile(profile: RetrievalProfile) -> Self {
        match profile {
            RetrievalProfile::Balanced => Self {
                task_type_compatibility: 1.0,
                graph_proximity: 1.1,
                exact_identifier_match: 1.4,
                semantic_similarity: 1.0,
                verification_status: 1.0,
                freshness: 0.55,
                scope: 0.65,
                evidence_strength: 0.75,
                contradiction_or_supersession_state: 1.2,
                past_usefulness: 0.45,
                recent_successful_reuse: 0.6,
                user_preference_compatibility: 0.5,
                token_cost: 0.5,
                stale_memory_hard_penalty: -10.0,
            },
        }
    }

    pub(super) fn weight_for(&self, signal: SignalKind) -> f32 {
        match signal {
            SignalKind::TaskTypeCompatibility => self.task_type_compatibility,
            SignalKind::GraphProximity => self.graph_proximity,
            SignalKind::ExactIdentifierMatch => self.exact_identifier_match,
            SignalKind::SemanticSimilarity => self.semantic_similarity,
            SignalKind::VerificationStatus => self.verification_status,
            SignalKind::Freshness => self.freshness,
            SignalKind::Scope => self.scope,
            SignalKind::EvidenceStrength => self.evidence_strength,
            SignalKind::ContradictionOrSupersessionState => {
                self.contradiction_or_supersession_state
            }
            SignalKind::PastUsefulness => self.past_usefulness,
            SignalKind::RecentSuccessfulReuse => self.recent_successful_reuse,
            SignalKind::UserPreferenceCompatibility => self.user_preference_compatibility,
            SignalKind::TokenCost => self.token_cost,
        }
    }
}

pub fn score_candidates(
    candidates: Vec<Candidate>,
    intent: &IntentClassification,
    anchors: &[ResolvedAnchor],
    weights: &ScoringWeights,
    mode: DiagnosticMode,
    ctx: &ScoringContext,
) -> Vec<RankedCandidate> {
    let mut ranked = candidates
        .into_iter()
        .map(|candidate| score_candidate(candidate, intent, anchors, weights, mode, ctx))
        .collect::<Vec<_>>();
    ranked.sort_by(compare_ranked_candidates);
    ranked
}

pub fn task_type_compatibility(
    candidate: &Candidate,
    intent: &IntentClassification,
    weights: &ScoringWeights,
) -> SignalScore {
    let raw = source_intent_score(candidate.source, intent.primary_label);
    let reason = if raw > 0.0 {
        format!(
            "{:?} candidate is compatible with {:?} intent",
            candidate.source, intent.primary_label
        )
    } else {
        "no task-type compatibility signal available; signal skipped".to_string()
    };
    signal(SignalKind::TaskTypeCompatibility, raw, weights, reason)
}

pub fn graph_proximity(
    candidate: &Candidate,
    anchors: &[ResolvedAnchor],
    weights: &ScoringWeights,
) -> SignalScore {
    if candidate.seed_anchor.is_none() && candidate.traversal_path.is_empty() {
        return skipped(SignalKind::GraphProximity, weights, "graph proximity");
    }
    let resolved = resolved_anchor_identities(anchors);
    let raw = graph_proximity_raw(candidate, &resolved);
    let reason = format!(
        "graph distance uses {} traversal step(s) from a resolved anchor",
        candidate.traversal_path.len()
    );
    signal(SignalKind::GraphProximity, raw, weights, reason)
}

pub fn exact_identifier_match(
    candidate: &Candidate,
    anchors: &[ResolvedAnchor],
    weights: &ScoringWeights,
) -> SignalScore {
    let identity_text = candidate.identity.to_string();
    for anchor in anchors {
        if anchor_matches_identity(anchor, &candidate.identity, &identity_text) {
            return signal(
                SignalKind::ExactIdentifierMatch,
                1.0,
                weights,
                format!("candidate identity matches anchor `{}`", anchor.anchor_text),
            );
        }
    }
    skipped(
        SignalKind::ExactIdentifierMatch,
        weights,
        "exact identifier match",
    )
}

pub fn semantic_similarity(
    candidate: &Candidate,
    weights: &ScoringWeights,
    ctx: &ScoringContext,
) -> SignalScore {
    let key = candidate.identity.to_string();
    let raw = ctx.semantic_similarity.get(&key).copied().or_else(|| {
        (candidate.source == CandidateSource::Embeddings).then_some(candidate.raw_score as f32)
    });
    match raw {
        Some(score) => signal(
            SignalKind::SemanticSimilarity,
            score.clamp(0.0, 1.0),
            weights,
            format!("semantic similarity {:.3} supplied for candidate", score),
        ),
        None => skipped(
            SignalKind::SemanticSimilarity,
            weights,
            "semantic similarity",
        ),
    }
}

pub fn verification_status(
    candidate: &Candidate,
    weights: &ScoringWeights,
    mode: DiagnosticMode,
    ctx: &ScoringContext,
) -> SignalScore {
    let Some(metadata) = memory_metadata(candidate, ctx) else {
        return skipped(
            SignalKind::VerificationStatus,
            weights,
            "verification status",
        );
    };
    let raw = verification_raw(metadata);
    let mut score = signal(
        SignalKind::VerificationStatus,
        raw,
        weights,
        format!(
            "memory verification status is {:?}",
            metadata.verification_status
        ),
    );
    apply_untrusted_penalty(&mut score, candidate, metadata, weights, mode, ctx);
    score
}

pub fn freshness(
    candidate: &Candidate,
    weights: &ScoringWeights,
    ctx: &ScoringContext,
) -> SignalScore {
    let Some(now) = ctx.now_unix_seconds else {
        return skipped(SignalKind::Freshness, weights, "freshness timestamp");
    };
    let Some(timestamp) = freshness_timestamp(candidate, ctx) else {
        return skipped(SignalKind::Freshness, weights, "freshness timestamp");
    };
    let age = now.saturating_sub(timestamp);
    let raw = 1.0 - ((age.min(FRESHNESS_WINDOW_SECONDS) as f32) / FRESHNESS_WINDOW_SECONDS as f32);
    signal(
        SignalKind::Freshness,
        raw,
        weights,
        format!("candidate age is {} second(s)", age),
    )
}

pub fn scope(candidate: &Candidate, weights: &ScoringWeights, ctx: &ScoringContext) -> SignalScore {
    let Some(metadata) = memory_metadata(candidate, ctx) else {
        return skipped(SignalKind::Scope, weights, "scope");
    };
    let raw = match metadata.scope {
        MemoryScope::Repo => 1.0,
        MemoryScope::Branch => 0.8,
        MemoryScope::Session => 0.45,
        MemoryScope::Organization => 0.7,
    };
    signal(
        SignalKind::Scope,
        raw,
        weights,
        format!("memory scope is {:?}", metadata.scope),
    )
}

pub fn evidence_strength(
    candidate: &Candidate,
    weights: &ScoringWeights,
    ctx: &ScoringContext,
) -> SignalScore {
    let Some(metadata) = memory_metadata(candidate, ctx) else {
        return skipped(SignalKind::EvidenceStrength, weights, "evidence strength");
    };
    let evidence = (metadata.evidence_count.min(5) as f32) / 5.0;
    let raw = ((metadata.confidence.clamp(0.0, 1.0) * 0.7) + (evidence * 0.3)).clamp(0.0, 1.0);
    signal(
        SignalKind::EvidenceStrength,
        raw,
        weights,
        format!(
            "confidence {:.2} with {} evidence item(s)",
            metadata.confidence, metadata.evidence_count
        ),
    )
}

pub fn contradiction_or_supersession_state(
    candidate: &Candidate,
    weights: &ScoringWeights,
    mode: DiagnosticMode,
    ctx: &ScoringContext,
) -> SignalScore {
    let Some(metadata) = memory_metadata(candidate, ctx) else {
        return skipped(
            SignalKind::ContradictionOrSupersessionState,
            weights,
            "contradiction or supersession state",
        );
    };
    let raw = contradiction_raw(metadata);
    let mut score = signal(
        SignalKind::ContradictionOrSupersessionState,
        raw,
        weights,
        contradiction_reason(metadata),
    );
    apply_untrusted_penalty(&mut score, candidate, metadata, weights, mode, ctx);
    score
}

pub fn past_usefulness(
    candidate: &Candidate,
    weights: &ScoringWeights,
    ctx: &ScoringContext,
) -> SignalScore {
    let key = candidate.identity.to_string();
    let count = ctx
        .event_history
        .get(&key)
        .map(|history| history.past_usefulness_count)
        .or_else(|| memory_metadata(candidate, ctx).map(|metadata| metadata.access_count));
    match count {
        Some(value) => signal(
            SignalKind::PastUsefulness,
            count_raw(value),
            weights,
            format!("candidate has {} prior useful retrieval(s)", value),
        ),
        None => skipped(SignalKind::PastUsefulness, weights, "past usefulness"),
    }
}

pub fn recent_successful_reuse(
    candidate: &Candidate,
    weights: &ScoringWeights,
    ctx: &ScoringContext,
) -> SignalScore {
    let key = candidate.identity.to_string();
    let Some(history) = ctx.event_history.get(&key) else {
        return skipped(
            SignalKind::RecentSuccessfulReuse,
            weights,
            "recent successful reuse",
        );
    };
    signal(
        SignalKind::RecentSuccessfulReuse,
        count_raw(history.recent_successful_reuse_count),
        weights,
        format!(
            "candidate appears in {} recent successful workflow(s)",
            history.recent_successful_reuse_count
        ),
    )
}

pub fn user_preference_compatibility(
    candidate: &Candidate,
    weights: &ScoringWeights,
    ctx: &ScoringContext,
) -> SignalScore {
    if ctx.user_preferences.is_empty() {
        return skipped(
            SignalKind::UserPreferenceCompatibility,
            weights,
            "user preference compatibility",
        );
    }
    let text = candidate_text(candidate).to_lowercase();
    let raw = preference_raw(&text, &ctx.user_preferences);
    let reason = if raw > 0.0 {
        "candidate matches observed user preference".to_string()
    } else if raw < 0.0 {
        "candidate conflicts with observed user preference".to_string()
    } else {
        "no matching user preference terms found; signal skipped".to_string()
    };
    signal(
        SignalKind::UserPreferenceCompatibility,
        raw,
        weights,
        reason,
    )
}

pub fn token_cost(
    candidate: &Candidate,
    weights: &ScoringWeights,
    ctx: &ScoringContext,
) -> SignalScore {
    let key = candidate.identity.to_string();
    let Some(tokens) = ctx.token_estimates.get(&key).copied() else {
        return skipped(SignalKind::TokenCost, weights, "token cost");
    };
    let raw = 1.0 - ((tokens.min(HIGH_TOKEN_COST) as f32) / HIGH_TOKEN_COST as f32);
    signal(
        SignalKind::TokenCost,
        raw,
        weights,
        format!("candidate token estimate is {}", tokens),
    )
}

fn score_candidate(
    candidate: Candidate,
    intent: &IntentClassification,
    anchors: &[ResolvedAnchor],
    weights: &ScoringWeights,
    mode: DiagnosticMode,
    ctx: &ScoringContext,
) -> RankedCandidate {
    let signal_scores = evaluate_signals(&candidate, intent, anchors, weights, mode, ctx);
    let total_score = signal_scores.iter().map(|score| score.weighted).sum();
    let inclusion_reason = inclusion_reason(&candidate, &signal_scores, mode);
    RankedCandidate {
        candidate,
        total_score,
        signal_scores,
        inclusion_reason,
    }
}

fn evaluate_signals(
    candidate: &Candidate,
    intent: &IntentClassification,
    anchors: &[ResolvedAnchor],
    weights: &ScoringWeights,
    mode: DiagnosticMode,
    ctx: &ScoringContext,
) -> Vec<SignalScore> {
    vec![
        task_type_compatibility(candidate, intent, weights),
        graph_proximity(candidate, anchors, weights),
        exact_identifier_match(candidate, anchors, weights),
        semantic_similarity(candidate, weights, ctx),
        verification_status(candidate, weights, mode, ctx),
        freshness(candidate, weights, ctx),
        scope(candidate, weights, ctx),
        evidence_strength(candidate, weights, ctx),
        contradiction_or_supersession_state(candidate, weights, mode, ctx),
        past_usefulness(candidate, weights, ctx),
        recent_successful_reuse(candidate, weights, ctx),
        user_preference_compatibility(candidate, weights, ctx),
        token_cost(candidate, weights, ctx),
    ]
}
