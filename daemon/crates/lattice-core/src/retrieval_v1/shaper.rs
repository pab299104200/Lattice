//! Retrieval V1 compact response shaper.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 7. Retrieval Engine` and `## Phase 4: Retrieval V1` require the
//! retrieval pipeline to deduplicate and compress ranked candidates, then emit
//! a compact bundle where every result carries an inclusion reason and an
//! expansion handle.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Serialize};

use crate::identity::{decode_identity, encode_identity, Identity, IdentityKind};

use super::inclusion_reasons::compact_inclusion_reason;
use super::{
    CandidateSource, DiagnosticMode, RankedCandidate, RankingDiagnostics, SignalKind, SignalScore,
};

const CHARS_PER_TOKEN_ESTIMATE: usize = 4;
const MIN_SNIPPET_CHARS: usize = 24;
const SNIPPET_TRIM_STEP: usize = 16;

/// `## 5. Working Memory` and `## Phase 5: Working Memory` require the bundle
/// schema to remain stable so working-memory checkpoints can store selected and
/// excluded retrieval items without rewriting the Retrieval V1 result shape.
pub mod schema {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct BudgetReport {
        pub token_budget: usize,
        pub estimated_tokens: usize,
        pub trimmed_snippets: usize,
        pub dropped_results: usize,
        pub truncated: bool,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct BundleResult {
        pub identity: Identity,
        pub kind: IdentityKind,
        pub headline: String,
        pub snippet: String,
        pub inclusion_reason: String,
        pub expansion_handle: String,
        pub source: Vec<CandidateSource>,
        pub score: f32,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct RetrievalBundle {
        pub task_id: String,
        pub intent_summary: String,
        pub anchors_summary: String,
        pub results: Vec<BundleResult>,
        pub diagnostics: Option<RankingDiagnostics>,
        pub budget_report: BudgetReport,
    }
}

pub use schema::{BudgetReport, BundleResult, RetrievalBundle};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaperBudget {
    pub max_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaperContext {
    pub pins: HashSet<Identity>,
    pub budget: ShaperBudget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompressionOutcome {
    pub results: Vec<BundleResult>,
    pub dropped_results: Vec<BundleResult>,
    pub budget_report: BudgetReport,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShapedRetrievalBundle {
    pub bundle: RetrievalBundle,
    pub dropped_results: Vec<BundleResult>,
}

#[derive(Debug, Clone)]
struct MergedCandidate {
    best: RankedCandidate,
    sources: BTreeSet<CandidateSource>,
    signal_scores: BTreeMap<SignalKind, SignalScore>,
}

impl Default for ShaperBudget {
    fn default() -> Self {
        Self { max_tokens: 320 }
    }
}

pub fn shape_retrieval_bundle(
    task_id: impl Into<String>,
    intent_summary: impl Into<String>,
    anchors_summary: impl Into<String>,
    ranked: Vec<RankedCandidate>,
    mode: DiagnosticMode,
    diagnostics: Option<RankingDiagnostics>,
    ctx: &ShaperContext,
) -> RetrievalBundle {
    shape_retrieval_bundle_detailed(
        task_id,
        intent_summary,
        anchors_summary,
        ranked,
        mode,
        diagnostics,
        ctx,
    )
    .bundle
}

pub fn shape_retrieval_bundle_detailed(
    task_id: impl Into<String>,
    intent_summary: impl Into<String>,
    anchors_summary: impl Into<String>,
    ranked: Vec<RankedCandidate>,
    mode: DiagnosticMode,
    diagnostics: Option<RankingDiagnostics>,
    ctx: &ShaperContext,
) -> ShapedRetrievalBundle {
    let deduped = deduplicate_ranked_candidates(ranked);
    let results = deduped
        .into_iter()
        .map(bundle_result_from_merged)
        .collect::<Vec<_>>();
    let compression = compress_bundle_results(results, ctx);
    ShapedRetrievalBundle {
        bundle: RetrievalBundle {
            task_id: task_id.into(),
            intent_summary: intent_summary.into(),
            anchors_summary: anchors_summary.into(),
            results: compression.results.clone(),
            diagnostics: diagnostics.filter(|_| matches!(mode, DiagnosticMode::Diagnostic)),
            budget_report: compression.budget_report.clone(),
        },
        dropped_results: compression.dropped_results,
    }
}

fn deduplicate_ranked_candidates(ranked: Vec<RankedCandidate>) -> Vec<MergedCandidate> {
    let mut merged = BTreeMap::<String, MergedCandidate>::new();
    let mut order = Vec::<String>::new();

    for ranked_candidate in ranked {
        let key = ranked_candidate.candidate.identity.to_string();
        let entry = merged.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            MergedCandidate {
                best: ranked_candidate.clone(),
                sources: BTreeSet::new(),
                signal_scores: BTreeMap::new(),
            }
        });

        if ranked_candidate.total_score > entry.best.total_score {
            entry.best = ranked_candidate.clone();
        }
        entry.sources.insert(ranked_candidate.candidate.source);
        merge_signal_scores(&mut entry.signal_scores, &ranked_candidate.signal_scores);
    }

    order
        .into_iter()
        .filter_map(|key| merged.remove(&key))
        .collect()
}

fn merge_signal_scores(merged: &mut BTreeMap<SignalKind, SignalScore>, scores: &[SignalScore]) {
    for score in scores {
        let replace = merged
            .get(&score.signal)
            .map(|current| score.raw > current.raw)
            .unwrap_or(true);
        if replace {
            merged.insert(score.signal, score.clone());
        }
    }
}

fn bundle_result_from_merged(mut merged: MergedCandidate) -> BundleResult {
    merged.best.signal_scores = merged.signal_scores.into_values().collect();
    let identity = merged.best.candidate.identity.clone();
    let expansion_handle = expansion_handle_for_identity(&identity);
    let inclusion_reason = compact_inclusion_reason(&merged.best);
    BundleResult {
        kind: identity.kind(),
        headline: headline_for_identity(&identity),
        snippet: snippet_for_candidate(&merged.best),
        inclusion_reason,
        expansion_handle,
        source: merged.sources.into_iter().collect(),
        score: merged.best.total_score,
        identity,
    }
}

fn headline_for_identity(identity: &Identity) -> String {
    match identity {
        Identity::File(file) => file.repo_relative_path.clone(),
        Identity::Symbol(symbol) => symbol.qualified_name.clone(),
        Identity::Doc(doc) => doc.repo_relative_path.clone(),
        Identity::Section(section) => format!(
            "{}#{}",
            section.doc.repo_relative_path,
            section.heading_path.join(" > ")
        ),
        Identity::Event(event) => format!("event {}", event.ulid),
        Identity::Memory(memory) => format!("memory {}", memory.ulid),
        Identity::ContextHandle(handle) => format!("handle {}", handle.ulid),
    }
}

fn snippet_for_candidate(candidate: &RankedCandidate) -> String {
    let detail = candidate
        .signal_scores
        .iter()
        .filter(|score| score.raw > 0.0)
        .max_by(|left, right| {
            left.weighted
                .partial_cmp(&right.weighted)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|score| score.reason.as_str())
        .unwrap_or(candidate.candidate.preliminary_inclusion_reason.as_str());
    detail.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn expansion_handle_for_identity(identity: &Identity) -> String {
    let handle = encode_identity(identity);
    debug_assert_eq!(
        decode_identity(&handle).expect("retrieval bundle expansion handle must decode"),
        identity.clone(),
        "retrieval bundle expansion handle must round-trip to the same identity"
    );
    handle
}

pub fn compress_bundle_results(
    mut results: Vec<BundleResult>,
    ctx: &ShaperContext,
) -> CompressionOutcome {
    let initial_lengths = results
        .iter()
        .map(|result| result.snippet.len())
        .collect::<Vec<_>>();
    trim_snippets_to_budget(results.as_mut_slice(), ctx.budget.max_tokens);

    let mut dropped_results = 0;
    let mut excluded = Vec::new();
    while estimate_bundle_tokens(results.as_slice()) > ctx.budget.max_tokens {
        let Some(index) = droppable_trailing_index(results.as_slice(), &ctx.pins) else {
            break;
        };
        excluded.push(results.remove(index));
        dropped_results += 1;
    }
    excluded.reverse();

    let trimmed_snippets = results
        .iter()
        .zip(initial_lengths.iter())
        .filter(|(result, original)| result.snippet.len() < **original)
        .count();
    let estimated_tokens = estimate_bundle_tokens(results.as_slice());
    CompressionOutcome {
        results,
        dropped_results: excluded,
        budget_report: BudgetReport {
            token_budget: ctx.budget.max_tokens,
            estimated_tokens,
            trimmed_snippets,
            dropped_results,
            truncated: trimmed_snippets > 0
                || dropped_results > 0
                || estimated_tokens > ctx.budget.max_tokens,
        },
    }
}

fn trim_snippets_to_budget(results: &mut [BundleResult], max_tokens: usize) {
    while estimate_bundle_tokens(results) > max_tokens {
        let Some((index, snippet_len)) = longest_trim_candidate(results) else {
            break;
        };
        if snippet_len <= MIN_SNIPPET_CHARS {
            break;
        }
        let new_len = snippet_len
            .saturating_sub(SNIPPET_TRIM_STEP)
            .max(MIN_SNIPPET_CHARS);
        results[index].snippet = truncate_chars(&results[index].snippet, new_len);
    }
}

fn longest_trim_candidate(results: &[BundleResult]) -> Option<(usize, usize)> {
    results
        .iter()
        .enumerate()
        .map(|(index, result)| (index, result.snippet.chars().count()))
        .max_by_key(|(_, len)| *len)
}

fn droppable_trailing_index(results: &[BundleResult], pins: &HashSet<Identity>) -> Option<usize> {
    results
        .iter()
        .enumerate()
        .rev()
        .find(|(_, result)| !pins.contains(&result.identity))
        .map(|(index, _)| index)
}

fn estimate_bundle_tokens(results: &[BundleResult]) -> usize {
    let chars = results
        .iter()
        .map(|result| {
            result.headline.chars().count()
                + result.snippet.chars().count()
                + result.inclusion_reason.chars().count()
                + result.expansion_handle.chars().count()
                + result.source.len() * 12
                + 24
        })
        .sum::<usize>();
    chars.div_ceil(CHARS_PER_TOKEN_ESTIMATE)
}

fn truncate_chars(value: &str, limit: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= limit {
        return value.to_string();
    }
    if limit == 0 {
        return String::new();
    }
    if limit <= 1 {
        return "…".to_string();
    }
    let prefix = value.chars().take(limit - 1).collect::<String>();
    format!("{prefix}…")
}
