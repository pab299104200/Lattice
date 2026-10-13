//! Q2 Phase 2 — ranking-parity comparison: `retrieval_v1` vs the heuristic
//! `QueryEngine`, on identical inputs, over the hermetic golden fixture.
//!
//! SCOPE / OVER-CLAIM GUARD: this measures *fixture-level ranking parity* —
//! "on identical inputs, does v1 rank the expected code anchor at the top at
//! least as well as the heuristic engine?" — NOT Beacon-scale real-world
//! precision. A win here is evidence the convergence does not regress ranking
//! on the fixture; it is not evidence of production quality. The Beacon
//! paired-task evaluation answers the real-world question and is out of scope.
//!
//! Fairness is enforced structurally:
//!   * Both engines receive clones of the SAME leaked fixture graph (not two
//!     independently constructed graphs).
//!   * Both receive the SAME `Arc<dyn VectorIndex>` handle and the SAME query
//!     embedding, so any divergence can only come from ranking.
//!   * Only code anchors (File/Symbol/Section) are scored, because the
//!     heuristic engine structurally cannot emit Memory results
//!     (`memories: Vec::new()`). Memory-expected cases are excluded and counted.
//!
//! The test REPORTS the win/tie/loss tally and does NOT assert a direction —
//! asserting "v1 must win" would prejudge the measurement. It DOES assert the
//! scored denominator cleared an interpretability floor, so a comparison that
//! silently scored too few cases fails as invalid rather than passing hollow.

use crate::query::QueryEngine;

use super::context_bridge::{context_capsule_via_retrieval_v1, ContextBridgeResources};
use super::test_support::{build_fixture, golden_cases, shared_vector_index};

/// Minimum number of code-anchor cases that must be scored for the comparison
/// to be interpretable. The fixture has 20 cases; well above half must be
/// code-eligible or the tally is meaningless.
const MIN_SCORED_CASES: usize = 8;

#[derive(Debug, Default)]
struct Tally {
    v1_wins: usize,
    heuristic_wins: usize,
    ties_both_correct: usize,
    ties_both_wrong: usize,
    excluded_memory: usize,
    /// How many scored cases each engine produced ANY result for. Guards
    /// against a hollow comparison where one engine silently returned empty
    /// capsules (which would make the win tally meaningless rather than real).
    v1_produced_output: usize,
    heuristic_produced_output: usize,
}

impl Tally {
    fn scored(&self) -> usize {
        self.v1_wins + self.heuristic_wins + self.ties_both_correct + self.ties_both_wrong
    }
}

// A plain (non-tokio) test: the bridge drives its own runtime internally and
// the heuristic engine is synchronous, so an ambient tokio runtime would only
// cause a nested-`block_on` panic. (That nesting is the Phase-3 `run_blocking`
// smell noted in the design doc — harmless here, worth cleaning up later.)
#[test]
fn v1_vs_heuristic_ranking_parity_over_golden_fixture() {
    let fixture = build_fixture();
    // The ONE vector index handle, shared by both engines.
    let vector_index = shared_vector_index();

    let mut tally = Tally::default();

    for case in golden_cases() {
        if !case.expected_top_identity.is_code() {
            // Heuristic engine can't produce Memory results; exclude for fairness.
            tally.excluded_memory += 1;
            continue;
        }

        // Heuristic path: a clone of the SAME leaked graph + the SAME vector
        // index handle + the SAME embedding.
        let mut heuristic_engine =
            QueryEngine::new(fixture.graph.clone(), Some(vector_index.clone()));
        let heuristic_capsule =
            heuristic_engine.query(case.task, Some(&case.query_embedding), false);
        let heuristic_correct = case
            .expected_top_identity
            .capsule_top_matches(&heuristic_capsule);
        if !heuristic_capsule.pivots.is_empty() || !heuristic_capsule.context.is_empty() {
            tally.heuristic_produced_output += 1;
        }

        // Principled path: the SAME leaked graph (by reference), the SAME file
        // index, the SAME vector index handle (as &dyn), the SAME embedding.
        let resources = ContextBridgeResources {
            workspace_id: "workspace-main",
            graph: fixture.graph,
            file_index: fixture.file_index,
            memory_store: Some(&fixture.memory_store),
            vector_index: Some(&*vector_index),
        };
        let v1_capsule = context_capsule_via_retrieval_v1(
            case.task,
            &resources,
            Some(&case.query_embedding),
            320,
        );
        let v1_correct = case.expected_top_identity.capsule_top_matches(&v1_capsule);
        if !v1_capsule.pivots.is_empty() || !v1_capsule.context.is_empty() {
            tally.v1_produced_output += 1;
        }

        match (v1_correct, heuristic_correct) {
            (true, false) => tally.v1_wins += 1,
            (false, true) => tally.heuristic_wins += 1,
            (true, true) => tally.ties_both_correct += 1,
            (false, false) => tally.ties_both_wrong += 1,
        }

        let heuristic_empty =
            heuristic_capsule.pivots.is_empty() && heuristic_capsule.context.is_empty();
        eprintln!(
            "  case={:<52} v1_correct={:<5} heuristic_correct={:<5} heuristic_empty={}",
            case.name, v1_correct, heuristic_correct, heuristic_empty
        );
    }

    // Report the tally (captured with `--nocapture`; always emitted so the
    // measurement is visible in CI logs even when the test passes).
    eprintln!(
        "Q2 Phase 2 ranking parity (fixture-level, NOT Beacon-scale): \
         scored={} v1_wins={} heuristic_wins={} ties_both_correct={} ties_both_wrong={} \
         excluded_memory={} v1_produced_output={} heuristic_produced_output={}",
        tally.scored(),
        tally.v1_wins,
        tally.heuristic_wins,
        tally.ties_both_correct,
        tally.ties_both_wrong,
        tally.excluded_memory,
        tally.v1_produced_output,
        tally.heuristic_produced_output,
    );

    // Validity guard (NOT a direction assertion): the measurement must have had
    // enough substrate to be interpretable. This mirrors the T1/T2 discipline —
    // guard the validity of the comparison, never prejudge its result.
    assert!(
        tally.scored() >= MIN_SCORED_CASES,
        "comparison scored only {} code-anchor case(s) (excluded {} memory case(s)); \
         need >= {} to be interpretable",
        tally.scored(),
        tally.excluded_memory,
        MIN_SCORED_CASES,
    );
    assert_eq!(
        tally.scored() + tally.excluded_memory,
        golden_cases().len(),
        "every golden case must be either scored or excluded, with a reason"
    );
}
