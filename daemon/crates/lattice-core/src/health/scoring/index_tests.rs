use std::collections::BTreeMap;

use super::*;

use crate::git_intelligence::{FileHistorySignal, GitIntelligenceSnapshot};
use crate::graph::{CodeGraph, EdgeKind};
use crate::health::complexity_facts::compute_file_complexity_facts;
use crate::health::dead_symbol_facts::{DeadSymbolExclusionInputs, DeadSymbolFactProducer};
use crate::health::graph_facts::fixtures::{add_edge, add_symbol, symbol};
use crate::health::graph_facts::GraphFactProducer;
use crate::health::scoring::facts::{FactAvailability, FactKind};
use crate::health::scoring::weights::Axis;
use crate::health::test_proximity_facts::TestProximityFactProducer;

/// A graph where `core.rs` is central and cyclic and `leaf.rs` is neither.
fn fixture_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();
    let core = symbol("src/core.rs", "run");
    let helper = symbol("src/helper.rs", "help");
    let leaf = symbol("src/leaf.rs", "leaf");
    let caller_one = symbol("src/one.rs", "one");
    let caller_two = symbol("src/two.rs", "two");

    for (id, exported) in [
        (&core, true),
        (&helper, true),
        (&leaf, true),
        (&caller_one, false),
        (&caller_two, false),
    ] {
        add_symbol(&mut graph, id, exported, 10);
    }

    // A two-file cycle, plus two callers of the core.
    add_edge(&mut graph, &core, &helper, EdgeKind::Calls);
    add_edge(&mut graph, &helper, &core, EdgeKind::Calls);
    add_edge(&mut graph, &caller_one, &core, EdgeKind::Calls);
    add_edge(&mut graph, &caller_two, &core, EdgeKind::Calls);
    add_edge(&mut graph, &core, &leaf, EdgeKind::Calls);
    graph
}

fn git_snapshot() -> GitIntelligenceSnapshot {
    let mut snapshot = GitIntelligenceSnapshot::empty();
    snapshot.processed_commits = vec!["abc1234def".to_string(), "0000000000".to_string()];
    snapshot.report.sampled_commits = 2;
    snapshot.report.included_commits = 2;
    snapshot.files = vec![
        FileHistorySignal {
            path: "src/core.rs".to_string(),
            hotspot_score: 40,
            bug_fix_commits: 12,
            bug_fix_density_per_mille: 300,
            author_count: 5,
            top_author_share_per_mille: Some(700),
            bus_factor: Some(2),
            lines_added: 900,
            lines_deleted: 300,
            line_churn: 1200,
        },
        FileHistorySignal {
            path: "src/helper.rs".to_string(),
            hotspot_score: 4,
            bug_fix_commits: 0,
            bug_fix_density_per_mille: 0,
            author_count: 1,
            top_author_share_per_mille: Some(1000),
            bus_factor: Some(1),
            lines_added: 20,
            lines_deleted: 5,
            line_churn: 25,
        },
    ];
    snapshot
}

fn complexity_facts() -> BTreeMap<String, crate::health::complexity_facts::FileComplexityFacts> {
    let mut facts = BTreeMap::new();
    facts.insert(
        "src/core.rs".to_string(),
        compute_file_complexity_facts(
            "src/core.rs",
            "fn run(a: u32) -> u32 {\n    if a > 1 {\n        for i in 0..a {\n            if i % 2 == 0 {\n                return i;\n            }\n        }\n    }\n    a\n}\n",
        ),
    );
    facts.insert(
        "src/leaf.rs".to_string(),
        compute_file_complexity_facts("src/leaf.rs", "fn leaf() -> u32 {\n    1\n}\n"),
    );
    facts
}

fn full_index() -> HealthFactIndex {
    let graph = fixture_graph();
    HealthFactIndex::builder()
        .with_graph_facts(GraphFactProducer::default().produce(&graph, true))
        .with_git_intelligence(git_snapshot())
        .with_complexity_facts(complexity_facts())
        .with_test_proximity_facts(TestProximityFactProducer::default().produce(&graph, true))
        .with_dead_symbol_facts(DeadSymbolFactProducer::default().produce(
            &graph,
            &DeadSymbolExclusionInputs::default(),
            true,
        ))
        .build()
}

#[test]
fn a_percentile_is_the_share_of_files_a_value_strictly_exceeds() {
    let index = HealthFactIndex::from_graph(&fixture_graph(), true);
    let core = index.facts("src/core.rs").expect("core is indexed");
    let leaf = index.facts("src/leaf.rs").expect("leaf is indexed");

    let core_fan_in = core.get(FactKind::FanIn).expect("fan-in");
    let leaf_fan_in = leaf.get(FactKind::FanIn).expect("fan-in");

    assert!(core_fan_in.value > leaf_fan_in.value);
    assert!(core_fan_in.percentile_per_mille > leaf_fan_in.percentile_per_mille);
}

#[test]
fn tied_values_receive_tied_ranks() {
    let index = HealthFactIndex::from_graph(&fixture_graph(), true);
    let one = index
        .facts("src/one.rs")
        .and_then(|facts| facts.get(FactKind::FanIn))
        .expect("one is indexed");
    let two = index
        .facts("src/two.rs")
        .and_then(|facts| facts.get(FactKind::FanIn))
        .expect("two is indexed");

    assert_eq!(one.value, two.value);
    assert_eq!(one.percentile_per_mille, two.percentile_per_mille);
}

#[test]
fn a_graph_only_index_scores_and_reports_every_family_it_lacked() {
    let index = HealthFactIndex::from_graph(&fixture_graph(), true);
    let score = index.score("src/core.rs", Axis::DefectRisk);

    assert_eq!(score.availability, FactAvailability::Degraded);
    assert!(!score.facts.is_empty());
    assert!(score.inputs_missing.contains(&FactKind::HotspotScore));
    assert!(score.inputs_missing.contains(&FactKind::FunctionCount));
    assert!(!score.is_exact());
}

#[test]
fn an_incomplete_index_degrades_every_fact_it_produced() {
    let index = HealthFactIndex::from_graph(&fixture_graph(), false);

    assert_eq!(index.availability(), FactAvailability::Degraded);
    let score = index.score("src/core.rs", Axis::DefectRisk);
    for fact in &score.facts {
        assert_eq!(fact.availability, FactAvailability::Degraded);
    }
}

#[test]
fn a_file_the_git_window_never_touched_has_zero_counts_and_unknown_ratios() {
    let index = full_index();
    let leaf = index.facts("src/leaf.rs").expect("leaf is indexed");

    assert_eq!(leaf.get(FactKind::HotspotScore).map(|f| f.value), Some(0));
    assert_eq!(leaf.get(FactKind::BugFixCommits).map(|f| f.value), Some(0));
    assert_eq!(leaf.get(FactKind::LineChurn).map(|f| f.value), Some(0));
    assert!(
        !leaf.has(FactKind::BugFixDensity),
        "a ratio with no denominator stays unknown"
    );
    assert!(!leaf.has(FactKind::BusFactor));
    assert!(!leaf.has(FactKind::TopAuthorShare));
}

#[test]
fn git_facts_carry_the_window_they_were_mined_over() {
    let index = full_index();
    let churn = index
        .facts("src/core.rs")
        .and_then(|facts| facts.get(FactKind::LineChurn))
        .expect("core has churn");
    let window = churn.window.as_ref().expect("window is recorded");

    assert_eq!(window.included_commits, 2);
    assert_eq!(window.head_commit.as_deref(), Some("abc1234def"));
}

#[test]
fn a_file_with_no_dead_exports_records_a_measured_zero() {
    let index = full_index();
    let core = index.facts("src/core.rs").expect("core is indexed");

    assert_eq!(
        core.get(FactKind::DeadExportedSymbols).map(|f| f.value),
        Some(0),
        "the producer examined the file and found nothing"
    );
}

#[test]
fn complexity_evidence_points_at_the_function_that_earned_it() {
    let index = full_index();
    let longest = index
        .facts("src/core.rs")
        .and_then(|facts| facts.get(FactKind::MaxFunctionLength))
        .expect("core has a longest function");
    let range = longest.source_range.as_ref().expect("range is recorded");

    assert_eq!(range.path, "src/core.rs");
    assert!(range.start_line >= 1);
    assert!(range.end_line >= range.start_line);
}

#[test]
fn every_family_present_makes_a_score_exact() {
    let index = full_index();
    let score = index.score("src/core.rs", Axis::DefectRisk);

    assert!(
        score.inputs_missing.is_empty(),
        "unexpectedly missing: {:?}",
        score.inputs_missing
    );
    assert!(score.is_exact());
    assert_eq!(score.availability, FactAvailability::Available);
}

#[test]
fn a_central_cyclic_churning_file_outranks_a_quiet_leaf() {
    let index = full_index();

    let core = index.score("src/core.rs", Axis::DefectRisk);
    let leaf = index.score("src/leaf.rs", Axis::DefectRisk);

    assert!(
        core.score_per_mille > leaf.score_per_mille,
        "core {} should outrank leaf {}",
        core.score_per_mille,
        leaf.score_per_mille
    );
}

#[test]
fn an_unindexed_path_is_unknown_rather_than_healthy() {
    let index = full_index();
    let score = index.score("src/never-seen.rs", Axis::DefectRisk);

    assert_eq!(score.availability, FactAvailability::Unavailable);
    assert_eq!(score.band_label(), "unknown");
    assert!(score.facts.is_empty());
}

#[test]
fn an_index_over_no_snapshots_scores_nothing_available() {
    let index = HealthFactIndex::empty();

    assert_eq!(index.file_count(), 0);
    assert_eq!(index.availability(), FactAvailability::Unavailable);
    assert_eq!(
        index.score("src/core.rs", Axis::DefectRisk).availability,
        FactAvailability::Unavailable
    );
}

#[test]
fn building_the_same_snapshots_twice_produces_identical_scores() {
    let first = full_index();
    let second = full_index();

    for path in first.paths() {
        for axis in crate::health::scoring::weights::ALL_AXES {
            assert_eq!(first.score(path, axis), second.score(path, axis));
        }
    }
}

#[test]
fn the_index_reports_which_files_it_knows() {
    let index = full_index();
    let paths: Vec<&str> = index.paths().collect();

    assert!(paths.contains(&"src/core.rs"));
    assert!(paths.contains(&"src/leaf.rs"));
    assert_eq!(index.file_count(), paths.len());
}

#[test]
fn broad_commit_exclusions_do_not_degrade_valid_file_risk_facts() {
    let mut history = git_snapshot();
    history.report.co_change_width_exclusions = 4;
    let index = HealthFactIndex::builder()
        .with_git_intelligence(history)
        .build();
    let fact = index
        .facts("src/core.rs")
        .unwrap()
        .get(FactKind::LineChurn)
        .unwrap();
    assert_eq!(fact.value, 1200);
    assert_eq!(fact.availability, FactAvailability::Available);
}

#[test]
fn documentation_without_control_flow_does_not_disable_code_health() {
    let mut complexity = complexity_facts();
    complexity.insert(
        "README.md".into(),
        compute_file_complexity_facts("README.md", "# Documentation\n"),
    );
    let index = HealthFactIndex::builder()
        .with_complexity_facts(complexity)
        .build();
    assert_eq!(index.availability(), FactAvailability::Available);
    assert!(index
        .facts("src/core.rs")
        .unwrap()
        .get(FactKind::MaxCyclomaticComplexity)
        .is_some());
    assert!(index.facts("README.md").is_none());
}

#[test]
fn unavailable_code_complexity_degrades_without_fabricating_measurements() {
    let mut complexity = complexity_facts();
    complexity.insert("unsupported.xyz".into(), compute_file_complexity_facts("unsupported.xyz", "opaque"));
    let index = HealthFactIndex::builder().with_complexity_facts(complexity).build();
    assert_eq!(index.availability(), FactAvailability::Degraded);
    assert!(index.facts("src/core.rs").unwrap().get(FactKind::MaxCyclomaticComplexity).is_some());
}
