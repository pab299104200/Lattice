use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{
    CandidateSource, IntentClassification, RankedCandidate, ResolvedAnchor, ScoringWeights,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetExhaustionFlags {
    pub any_budget_exhausted: bool,
    pub exhausted_sources: Vec<CandidateSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankingDiagnostics {
    pub intent: IntentClassification,
    pub anchors: Vec<ResolvedAnchor>,
    pub candidates_per_source: BTreeMap<CandidateSource, usize>,
    pub ranked: Vec<RankedCandidate>,
    pub weights: ScoringWeights,
    pub budget_exhaustion_flags: BudgetExhaustionFlags,
}

impl RankingDiagnostics {
    pub fn new(
        intent: IntentClassification,
        anchors: Vec<ResolvedAnchor>,
        ranked: Vec<RankedCandidate>,
        weights: ScoringWeights,
    ) -> Self {
        let candidates_per_source = candidates_per_source(&ranked);
        let budget_exhaustion_flags = budget_exhaustion_flags(&ranked);
        Self {
            intent,
            anchors,
            candidates_per_source,
            ranked,
            weights,
            budget_exhaustion_flags,
        }
    }
}

fn candidates_per_source(ranked: &[RankedCandidate]) -> BTreeMap<CandidateSource, usize> {
    let mut counts = BTreeMap::new();
    for candidate in ranked {
        let count = counts.entry(candidate.candidate.source).or_insert(0);
        *count += 1;
    }
    counts
}

fn budget_exhaustion_flags(ranked: &[RankedCandidate]) -> BudgetExhaustionFlags {
    let mut exhausted_sources = Vec::new();
    for candidate in ranked {
        if candidate.candidate.budget_exhausted
            && !exhausted_sources.contains(&candidate.candidate.source)
        {
            exhausted_sources.push(candidate.candidate.source);
        }
    }
    BudgetExhaustionFlags {
        any_budget_exhausted: !exhausted_sources.is_empty(),
        exhausted_sources,
    }
}
