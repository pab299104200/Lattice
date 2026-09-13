//! H3.3: `RiskRecommendation` is banded by the health engine, not by a
//! downstream count.
//!
//! The thresholds these replace (`>= 6` high, `>= 3` medium) are deleted, so
//! the tests below assert the new behaviour directly: the level is a calibrated
//! band, the evidence rides with it, and the structural gate that was worth
//! keeping still holds.

use crate::git_intelligence::{FileHistorySignal, GitIntelligenceSnapshot};
use crate::graph::model::{CodeGraph, EdgeKind};
use crate::health::graph_facts::fixtures::{add_edge, add_symbol, symbol};
use crate::health::graph_facts::GraphFactProducer;
use crate::health::scoring::{Band, FactAvailability, FactKind, HealthFactIndex};
use crate::intelligence::{impact_from_diff, BundleMode, RulesDetector};

/// `src/core.rs` is depended on by two files and depends on two more;
/// `src/lonely.rs` is isolated and unexported.
fn fixture_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();
    let core = symbol("src/core.rs", "run");
    let helper = symbol("src/helper.rs", "help");
    let leaf = symbol("src/leaf.rs", "leaf");
    let caller_one = symbol("src/one.rs", "one");
    let caller_two = symbol("src/two.rs", "two");
    let lonely = symbol("src/lonely.rs", "lonely");

    for (id, exported) in [
        (&core, true),
        (&helper, true),
        (&leaf, true),
        (&caller_one, false),
        (&caller_two, false),
        (&lonely, false),
    ] {
        add_symbol(&mut graph, id, exported, 10);
    }

    add_edge(&mut graph, &core, &helper, EdgeKind::Calls);
    add_edge(&mut graph, &core, &leaf, EdgeKind::Calls);
    add_edge(&mut graph, &caller_one, &core, EdgeKind::Calls);
    add_edge(&mut graph, &caller_two, &core, EdgeKind::Calls);
    graph
}

fn diff_for(file: &str) -> String {
    format!(
        "diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n@@ -10,3 +10,4 @@\n-let before = 1;\n+let after = 2;\n"
    )
}

fn rules() -> Vec<crate::intelligence::ProjectRule> {
    RulesDetector::new().detect_rules(&["src/core.rs".to_string()])
}

fn report_for(
    graph: &CodeGraph,
    file: &str,
    health: Option<&HealthFactIndex>,
) -> crate::intelligence::DiffImpactReport {
    impact_from_diff(
        graph,
        &diff_for(file),
        &[],
        &[],
        &rules(),
        BundleMode::Full,
        2,
        health,
    )
}

/// A history where `src/core.rs` is a churning, bug-fixed hotspot.
fn hot_history() -> GitIntelligenceSnapshot {
    let mut snapshot = GitIntelligenceSnapshot::empty();
    snapshot.processed_commits = vec!["abc1234def".to_string()];
    snapshot.report.sampled_commits = 1;
    snapshot.report.included_commits = 1;
    snapshot.files = vec![
        FileHistorySignal {
            path: "src/core.rs".to_string(),
            hotspot_score: 90,
            bug_fix_commits: 30,
            bug_fix_density_per_mille: 333,
            author_count: 6,
            top_author_share_per_mille: Some(800),
            bus_factor: Some(1),
            lines_added: 4000,
            lines_deleted: 2000,
            line_churn: 6000,
        },
        FileHistorySignal {
            path: "src/helper.rs".to_string(),
            hotspot_score: 1,
            bug_fix_commits: 0,
            bug_fix_density_per_mille: 0,
            author_count: 1,
            top_author_share_per_mille: Some(1000),
            bus_factor: Some(1),
            lines_added: 3,
            lines_deleted: 1,
            line_churn: 4,
        },
    ];
    snapshot
}

fn index_with_history(graph: &CodeGraph) -> HealthFactIndex {
    HealthFactIndex::builder()
        .with_graph_facts(GraphFactProducer::default().produce(graph, true))
        .with_git_intelligence(hot_history())
        .build()
}

#[test]
fn a_risk_level_is_a_calibrated_band_and_never_the_old_vocabulary() {
    let graph = fixture_graph();
    let report = report_for(&graph, "src/core.rs", None);

    assert!(!report.risks.is_empty(), "a central symbol carries risk");
    for risk in &report.risks {
        assert_ne!(risk.level, "medium", "the old vocabulary is deleted");
        for part in risk.level.split('-') {
            assert!(
                Band::from_code(part).is_some() || part == "unknown",
                "unexpected level {}",
                risk.level
            );
        }
    }
}

#[test]
fn a_risk_carries_the_evidence_that_banded_it() {
    let graph = fixture_graph();
    let report = report_for(&graph, "src/core.rs", None);
    let risk = report.risks.first().expect("a risk was reported");

    assert!(
        !risk.defect_risk.facts.is_empty(),
        "a score must ship its evidence"
    );
    assert_eq!(risk.defect_risk.weights_version, 1);
    assert!(risk
        .defect_risk
        .facts
        .iter()
        .any(|fact| fact.kind == FactKind::FanIn));
    assert!(
        risk.reason.contains("defect risk"),
        "the reason states the band: {}",
        risk.reason
    );
}

#[test]
fn a_graph_only_risk_says_which_families_it_lacked() {
    let graph = fixture_graph();
    let report = report_for(&graph, "src/core.rs", None);
    let risk = report.risks.first().expect("a risk was reported");

    assert_eq!(risk.defect_risk.availability, FactAvailability::Degraded);
    assert!(risk
        .defect_risk
        .inputs_missing
        .contains(&FactKind::HotspotScore));
    assert!(risk.reason.contains("scored without"));
}

#[test]
fn history_evidence_changes_the_band_without_changing_the_graph() {
    let graph = fixture_graph();
    let without = report_for(&graph, "src/core.rs", None);
    let index = index_with_history(&graph);
    let with = report_for(&graph, "src/core.rs", Some(&index));

    let plain = without.risks.first().expect("a risk without history");
    let scored = with.risks.first().expect("a risk with history");

    assert_eq!(plain.impact_count, scored.impact_count, "same graph");
    assert!(
        scored
            .defect_risk
            .facts
            .iter()
            .any(|fact| fact.kind == FactKind::HotspotScore),
        "history facts now contribute"
    );
    assert!(
        scored.defect_risk.inputs_missing.len() < plain.defect_risk.inputs_missing.len(),
        "supplying a family removes it from the missing list"
    );
    assert!(
        scored.defect_risk.score_ceiling_per_mille - scored.defect_risk.score_floor_per_mille
            < plain.defect_risk.score_ceiling_per_mille - plain.defect_risk.score_floor_per_mille,
        "more evidence narrows the interval"
    );
}

#[test]
fn a_symbol_nothing_reaches_is_still_not_a_change_risk() {
    let graph = fixture_graph();
    let report = report_for(&graph, "src/lonely.rs", None);

    assert!(
        report.risks.iter().all(|risk| risk.symbol != "lonely"),
        "an unexported symbol with no dependents is not a change risk"
    );
}

#[test]
fn risks_are_ordered_by_evidence_rather_than_by_downstream_count() {
    let graph = fixture_graph();
    let index = index_with_history(&graph);
    let mut diff = diff_for("src/core.rs");
    diff.push_str(&diff_for("src/helper.rs"));
    let report = impact_from_diff(
        &graph,
        &diff,
        &[],
        &[],
        &rules(),
        BundleMode::Full,
        2,
        Some(&index),
    );

    let ceilings: Vec<Band> = report
        .risks
        .iter()
        .map(|risk| risk.defect_risk.band_range.ceiling)
        .collect();
    let mut descending = ceilings.clone();
    descending.sort_by(|left, right| right.cmp(left));

    assert_eq!(ceilings, descending, "worst band first");
}
