pub mod anchors;
pub mod candidates;
pub mod diagnostic;
pub mod inclusion_reasons;
pub mod intent;
pub mod scoring;
mod scoring_support;
pub mod shaper;

#[cfg(test)]
mod anchors_tests;
#[cfg(test)]
mod benchmark;
#[cfg(test)]
mod candidates_tests;
#[cfg(test)]
mod golden_tests;
#[cfg(test)]
mod intent_tests;
#[cfg(test)]
mod scoring_tests;
#[cfg(test)]
mod shaper_tests;
#[cfg(test)]
mod test_support;

pub mod schema {
    pub use super::shaper::schema::{BudgetReport, BundleResult, RetrievalBundle};
}

pub use anchors::{
    extract_anchors, resolve_anchors, AnchorDiagnostic, AnchorKind, AnchorResolution, RawAnchor,
    ResolvedAnchor, SourceSpan,
};
pub use candidates::{
    retrieve_candidates, Candidate, CandidateSource, RetrievalBudget, RetrievalBudgetConfig,
    RetrievalContext,
};
pub use diagnostic::{BudgetExhaustionFlags, RankingDiagnostics};
pub use intent::{
    classify_intent, IntentClassification, IntentDiagnostic, IntentFeature, IntentFeatureKind,
    IntentLabel, MAX_INTENT_TASK_CHARS,
};
pub use scoring::{
    score_candidates, DiagnosticMode, EventScoringMetadata, MemoryScoringMetadata, RankedCandidate,
    RetrievalProfile, ScoringContext, ScoringWeights, SignalKind, SignalScore,
    UserPreferenceSignal,
};
pub use shaper::{shape_retrieval_bundle, ShaperBudget, ShaperContext};
