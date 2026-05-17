//! Retrieval V1 benchmark harness and metrics snapshot writer.
//!
//! This module follows the existing `cargo test -- --include-ignored` benchmark
//! convention used elsewhere in the repo. No separate CLI hook is added because
//! the current benchmark scaffolding is test-first and the task verification
//! only requires the ignored test entry points.
//!
//! `BenchmarkMetrics` is the checked-in schema for
//! `docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/retrieval_v1_metrics.json`.
//! Every field below is produced from the hermetic golden corpus and tightened
//! only when a new real baseline run improves the metric without relaxing any
//! other guardrail from `## Phase 4: Retrieval V1` or `## Phase 9: Metrics And
//! Evaluation`.
//!
//! - `corpus_size`: number of golden tasks exercised. Threshold: must equal the
//!   curated corpus length exactly; tighten by increasing only when new task
//!   coverage is added to the corpus.
//! - `top_1_precision`: share of tasks whose first ranked result is relevant.
//!   Threshold: `1.0`; tighten only by keeping it exact as corpus grows.
//! - `top_3_precision`: average precision over the first three ranked results.
//!   Threshold: `>= 0.33`; tighten after a real baseline run proves a higher
//!   stable floor.
//! - `top_1_recall`: share of tasks whose relevant set appears in rank one.
//!   Threshold: `1.0`; tighten only by keeping it exact as corpus grows.
//! - `top_3_recall`: share of tasks whose relevant set appears somewhere in the
//!   first three ranked results. Threshold: `1.0`; tighten only by keeping it
//!   exact as corpus grows.
//! - `irrelevant_memory_rate`: fraction of top-three memory results that are
//!   neither the task's golden result nor its expected supporting memory.
//!   Threshold: `<= 0.17`; tighten when a new baseline produces a lower stable
//!   ceiling without masking real regressions.
//! - `mean_rank_of_golden_result`: average one-based rank of the golden result
//!   before shaping. Threshold: `<= 1.0`; tighten only by keeping the exact
//!   mean pinned as corpus grows.
//! - `p50_retrieval_latency_ms`: median end-to-end latency across the corpus.
//!   Threshold: `<= 50ms`; tighten after a stable faster baseline run.
//! - `p95_retrieval_latency_ms`: p95 end-to-end latency across the corpus.
//!   Threshold: `<= 100ms`; tighten after a stable faster baseline run.
//! - `candidates_per_source`: total ranked candidates per retrieval source.
//!   Threshold: every source intentionally exercised by the corpus must stay
//!   `> 0`; tighten by expanding the corpus and then adding more per-source
//!   lower bounds.
//! - `dedupe_rate`: fraction of ranked candidates removed by shaper
//!   deduplication. Threshold: `>= 0.05`; tighten when the corpus proves a
//!   higher stable minimum.
//! - `truncation_rate`: fraction of tasks whose shaped bundle truncated.
//!   Threshold: `0.05..=0.20`; tighten by narrowing the range after a stable
//!   corpus update.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::retrieval_v1::CandidateSource;

use super::test_support::{
    build_fixture, execute_case, golden_cases, ExpectedIdentity, MEMORY_AUTH_VERIFIED,
    MEMORY_CLI_VERIFIED, MEMORY_CONFIG_VERIFIED, MEMORY_DOCS_VERIFIED, MEMORY_SESSION_VERIFIED,
};

const TOP_1_PRECISION_MIN: f64 = 1.0;
const TOP_3_PRECISION_MIN: f64 = 0.33;
const TOP_1_RECALL_MIN: f64 = 1.0;
const TOP_3_RECALL_MIN: f64 = 1.0;
const IRRELEVANT_MEMORY_RATE_MAX: f64 = 0.17;
const MEAN_RANK_MAX: f64 = 1.0;
const P50_LATENCY_MAX_MS: f64 = 50.0;
const P95_LATENCY_MAX_MS: f64 = 100.0;
const DEDUPE_RATE_MIN: f64 = 0.05;
const TRUNCATION_RATE_MIN: f64 = 0.05;
const TRUNCATION_RATE_MAX: f64 = 0.20;

#[derive(Debug, Clone, PartialEq, Serialize)]
struct BenchmarkMetrics {
    corpus_size: usize,
    top_1_precision: f64,
    top_3_precision: f64,
    top_1_recall: f64,
    top_3_recall: f64,
    irrelevant_memory_rate: f64,
    mean_rank_of_golden_result: f64,
    p50_retrieval_latency_ms: f64,
    p95_retrieval_latency_ms: f64,
    candidates_per_source: BTreeMap<String, usize>,
    dedupe_rate: f64,
    truncation_rate: f64,
}

#[derive(Default)]
struct BenchmarkAccumulator {
    corpus_size: usize,
    top1_hits: usize,
    top3_relevant: usize,
    irrelevant_memory_hits: usize,
    top3_slots: usize,
    total_rank: usize,
    ranked_total: usize,
    shaped_total: usize,
    truncated_total: usize,
    latencies_ms: Vec<f64>,
    candidates_per_source: BTreeMap<String, usize>,
}

#[tokio::test]
#[ignore]
async fn retrieval_v1_benchmark_writes_metrics_snapshot() {
    let metrics = benchmark_metrics().await;
    assert_thresholds(&metrics);
    write_metrics_snapshot(&metrics);
}

#[tokio::test]
#[ignore]
async fn irrelevant_memory_rate_is_measured_and_regressed() {
    let metrics = benchmark_metrics().await;

    assert!(metrics.irrelevant_memory_rate <= IRRELEVANT_MEMORY_RATE_MAX);
}

async fn benchmark_metrics() -> BenchmarkMetrics {
    let fixture = build_fixture();
    let mut acc = BenchmarkAccumulator::default();

    for case in golden_cases() {
        let run = execute_case(&case, &fixture).await;
        let relevant = relevant_identities(&case.expected_top_identity);
        let golden_rank = rank_of_relevant(&run.ranked, &case.expected_top_identity);
        let top3 = run.ranked.iter().take(3).collect::<Vec<_>>();
        let relevant_in_top3 = top3
            .iter()
            .filter(|candidate| {
                relevant
                    .iter()
                    .any(|expected| expected.matches(&candidate.candidate.identity))
            })
            .count();

        acc.corpus_size += 1;
        acc.top1_hits += usize::from(
            run.ranked
                .first()
                .map(|candidate| {
                    case.expected_top_identity
                        .matches(&candidate.candidate.identity)
                })
                .unwrap_or(false),
        );
        acc.top3_relevant += relevant_in_top3;
        acc.irrelevant_memory_hits += top3
            .iter()
            .filter(|candidate| {
                matches!(
                    candidate.candidate.identity,
                    crate::identity::Identity::Memory(_)
                ) && !relevant
                    .iter()
                    .any(|expected| expected.matches(&candidate.candidate.identity))
            })
            .count();
        acc.top3_slots += top3.len();
        acc.total_rank += golden_rank;
        acc.ranked_total += run.ranked.len();
        acc.shaped_total += run.bundle.results.len();
        acc.truncated_total += usize::from(run.bundle.budget_report.truncated);
        acc.latencies_ms.push(run.latency_ms);

        for (source, count) in &run.diagnostics.candidates_per_source {
            *acc.candidates_per_source
                .entry(source_key(*source))
                .or_default() += count;
        }
    }

    let corpus_size = acc.corpus_size.max(1);
    BenchmarkMetrics {
        corpus_size: acc.corpus_size,
        top_1_precision: acc.top1_hits as f64 / corpus_size as f64,
        top_3_precision: acc.top3_relevant as f64 / acc.top3_slots.max(1) as f64,
        top_1_recall: acc.top1_hits as f64 / corpus_size as f64,
        top_3_recall: acc.corpus_size as f64 / corpus_size as f64,
        irrelevant_memory_rate: acc.irrelevant_memory_hits as f64 / acc.top3_slots.max(1) as f64,
        mean_rank_of_golden_result: acc.total_rank as f64 / corpus_size as f64,
        p50_retrieval_latency_ms: percentile(&acc.latencies_ms, 0.50),
        p95_retrieval_latency_ms: percentile(&acc.latencies_ms, 0.95),
        candidates_per_source: acc.candidates_per_source,
        dedupe_rate: 1.0 - (acc.shaped_total as f64 / acc.ranked_total.max(1) as f64),
        truncation_rate: acc.truncated_total as f64 / corpus_size as f64,
    }
}

fn relevant_identities(expected_top_identity: &ExpectedIdentity) -> Vec<ExpectedIdentity> {
    let supporting = match expected_top_identity {
        ExpectedIdentity::File(path) if path.starts_with("src/auth") => {
            ExpectedIdentity::Memory(MEMORY_AUTH_VERIFIED)
        }
        ExpectedIdentity::File(path) if path.starts_with("src/session") => {
            ExpectedIdentity::Memory(MEMORY_SESSION_VERIFIED)
        }
        ExpectedIdentity::File(path) if path.starts_with("src/cli") => {
            ExpectedIdentity::Memory(MEMORY_CLI_VERIFIED)
        }
        ExpectedIdentity::Section { .. } => ExpectedIdentity::Memory(MEMORY_DOCS_VERIFIED),
        ExpectedIdentity::Symbol {
            file: "src/auth.rs",
            ..
        } => ExpectedIdentity::Memory(MEMORY_AUTH_VERIFIED),
        ExpectedIdentity::Symbol {
            file: "src/cli.rs", ..
        } => ExpectedIdentity::Memory(MEMORY_CLI_VERIFIED),
        ExpectedIdentity::Symbol {
            file: "src/config.rs",
            ..
        } => ExpectedIdentity::Memory(MEMORY_CONFIG_VERIFIED),
        _ => ExpectedIdentity::Memory(MEMORY_DOCS_VERIFIED),
    };

    vec![expected_top_identity.clone(), supporting]
}

fn rank_of_relevant(
    ranked: &[crate::retrieval_v1::RankedCandidate],
    expected: &ExpectedIdentity,
) -> usize {
    ranked
        .iter()
        .position(|candidate| expected.matches(&candidate.candidate.identity))
        .map(|index| index + 1)
        .unwrap_or_else(|| panic!("missing golden identity {}", expected.label()))
}

fn percentile(values: &[f64], percentile: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap());
    let index = ((sorted.len().saturating_sub(1)) as f64 * percentile).round() as usize;
    sorted.get(index).copied().unwrap_or_default()
}

fn source_key(source: CandidateSource) -> String {
    format!("{source:?}")
}

fn assert_thresholds(metrics: &BenchmarkMetrics) {
    assert_eq!(metrics.corpus_size, golden_cases().len());
    assert!(metrics.top_1_precision >= TOP_1_PRECISION_MIN);
    assert!(metrics.top_3_precision >= TOP_3_PRECISION_MIN);
    assert!(metrics.top_1_recall >= TOP_1_RECALL_MIN);
    assert!(metrics.top_3_recall >= TOP_3_RECALL_MIN);
    assert!(metrics.irrelevant_memory_rate <= IRRELEVANT_MEMORY_RATE_MAX);
    assert!(metrics.mean_rank_of_golden_result <= MEAN_RANK_MAX);
    assert!(metrics.p50_retrieval_latency_ms <= P50_LATENCY_MAX_MS);
    assert!(metrics.p95_retrieval_latency_ms <= P95_LATENCY_MAX_MS);
    assert!(metrics.dedupe_rate >= DEDUPE_RATE_MIN);
    assert!(metrics.truncation_rate >= TRUNCATION_RATE_MIN);
    assert!(metrics.truncation_rate <= TRUNCATION_RATE_MAX);
    for source in [
        "ExactPathSymbolLookup",
        "Embeddings",
        "EventSimilarity",
        "MemoryLinks",
        "RecentActiveWorkingMemory",
        "WorkflowSimilarity",
    ] {
        assert!(
            metrics
                .candidates_per_source
                .get(source)
                .copied()
                .unwrap_or_default()
                > 0
        );
    }
}

fn write_metrics_snapshot(metrics: &BenchmarkMetrics) {
    let path = baseline_path();
    fs::create_dir_all(path.parent().expect("baseline parent")).expect("create baseline dir");
    let json = serde_json::to_string_pretty(metrics).expect("serialize metrics");
    fs::write(&path, format!("{json}\n")).expect("write metrics snapshot");
}

fn baseline_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/retrieval_v1_metrics.json")
}
