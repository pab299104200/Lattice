use std::collections::BTreeMap;

use super::*;

use crate::git_intelligence::{FileHistorySignal, GitIntelligenceSnapshot};
use crate::graph::{CodeGraph, EdgeKind};
use crate::health::complexity_facts::{compute_file_complexity_facts, FileComplexityFacts};
use crate::health::dead_symbol_facts::{DeadSymbolExclusionInputs, DeadSymbolFactProducer};
use crate::health::graph_facts::fixtures::{add_edge, add_symbol, symbol};
use crate::health::graph_facts::GraphFactProducer;
use crate::health::test_proximity_facts::TestProximityFactProducer;

/// `src/core.rs` is central and cyclic; `src/leaf.rs` is neither. `one`/`two`
/// are callers so that the population has more than two members and a
/// percentile means something.
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

fn complexity_facts() -> BTreeMap<String, FileComplexityFacts> {
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

fn candidate(path: &str, stable_key: &str) -> HealthImpactCandidate<String> {
    HealthImpactCandidate {
        candidate: path.to_string(),
        stable_key: stable_key.to_string(),
        file_path: Some(path.to_string()),
    }
}

fn ordered(candidates: &[HealthImpactCandidate<String>]) -> Vec<&str> {
    candidates
        .iter()
        .map(|item| item.candidate.as_str())
        .collect()
}

#[test]
fn evidence_is_a_bundle_and_never_a_bare_number() {
    let index = full_index();
    let evidence = defect_risk_evidence(Some(&index), "src/core.rs").expect("an index answers");

    assert_eq!(evidence.path, "src/core.rs");
    assert_eq!(evidence.axis, "defect_risk");
    assert!(
        !evidence.facts.is_empty(),
        "a score must ship the facts it was computed from"
    );
    assert!(evidence.facts.len() <= HEALTH_EVIDENCE_FACT_LIMIT);
    for fact in &evidence.facts {
        assert!(
            !fact.description.is_empty(),
            "every cited fact must be readable on its own: {fact:?}"
        );
        assert!(fact.description.contains('('), "{}", fact.description);
    }
    // The one-line form a markdown render prints must name the band, the score,
    // and at least one fact.
    assert!(evidence.summary.starts_with("defect risk "));
    assert!(evidence.summary.contains("/1000)"));
    assert!(evidence.summary.contains(&evidence.facts[0].description));
    assert_eq!(evidence.weights_version, index.weights().version);
}

#[test]
fn a_path_the_index_never_saw_is_unknown_rather_than_low_risk() {
    let index = full_index();
    let evidence = defect_risk_evidence(Some(&index), "src/never-indexed.rs").expect("answers");

    assert!(evidence.is_unknown());
    assert_eq!(evidence.band, "unknown");
    assert_eq!(evidence.score_floor_per_mille, 0);
    assert_eq!(evidence.score_ceiling_per_mille, 1000);
    assert!(
        !evidence.inputs_missing.is_empty(),
        "an unknown file must name the inputs it lacked"
    );
    assert!(evidence.summary.contains("no facts available"));
}

#[test]
fn no_index_at_all_yields_no_evidence_rather_than_an_empty_score() {
    assert!(defect_risk_evidence(None, "src/core.rs").is_none());
}

#[test]
fn impact_ordering_ranks_by_defect_risk_within_the_supplied_tier() {
    let index = full_index();
    let core = index.score("src/core.rs", Axis::DefectRisk);
    let leaf = index.score("src/leaf.rs", Axis::DefectRisk);
    assert!(
        core.score_per_mille > leaf.score_per_mille,
        "fixture must make core the riskier file: {} vs {}",
        core.score_per_mille,
        leaf.score_per_mille
    );

    // Stable keys are deliberately in the opposite order, so alphabetical
    // ordering alone cannot produce the expected result.
    let mut candidates = vec![
        candidate("src/leaf.rs", "a"),
        candidate("src/core.rs", "z"),
    ];
    order_impact_within_tier_by_defect_risk(Some(&index), &mut candidates);

    assert_eq!(ordered(&candidates), vec!["src/core.rs", "src/leaf.rs"]);
}

#[test]
fn unscored_candidates_trail_scored_ones_and_keep_their_stable_order() {
    let index = full_index();
    let mut candidates = vec![
        candidate("src/zzz-unknown.rs", "b"),
        candidate("src/aaa-unknown.rs", "a"),
        candidate("src/core.rs", "z"),
    ];
    order_impact_within_tier_by_defect_risk(Some(&index), &mut candidates);

    // The scored file leads despite the worst stable key; the two unknowns keep
    // the caller's deterministic order rather than claiming a low score.
    assert_eq!(
        ordered(&candidates),
        vec!["src/core.rs", "src/aaa-unknown.rs", "src/zzz-unknown.rs"]
    );
}

#[test]
fn ordering_without_an_index_reduces_to_the_callers_stable_key() {
    let mut candidates = vec![
        candidate("src/core.rs", "z"),
        candidate("src/leaf.rs", "a"),
        candidate("src/helper.rs", "m"),
    ];
    order_impact_within_tier_by_defect_risk(None, &mut candidates);

    assert_eq!(
        ordered(&candidates),
        vec!["src/leaf.rs", "src/helper.rs", "src/core.rs"],
        "with no facts the ordering must invent no signal"
    );
}

#[test]
fn ordering_is_deterministic_regardless_of_input_order() {
    let index = full_index();
    let paths = ["src/core.rs", "src/leaf.rs", "src/helper.rs", "src/one.rs"];

    let mut forward: Vec<_> = paths.iter().map(|path| candidate(path, path)).collect();
    let mut backward: Vec<_> = paths
        .iter()
        .rev()
        .map(|path| candidate(path, path))
        .collect();
    order_impact_within_tier_by_defect_risk(Some(&index), &mut forward);
    order_impact_within_tier_by_defect_risk(Some(&index), &mut backward);

    assert_eq!(ordered(&forward), ordered(&backward));
}

#[test]
fn ordering_permutes_and_never_admits_or_drops_a_candidate() {
    let index = full_index();
    let paths = [
        "src/core.rs",
        "src/leaf.rs",
        "src/helper.rs",
        "src/never-seen.rs",
    ];
    let mut candidates: Vec<_> = paths.iter().map(|path| candidate(path, path)).collect();
    order_impact_within_tier_by_defect_risk(Some(&index), &mut candidates);

    let mut got = ordered(&candidates);
    got.sort_unstable();
    let mut want: Vec<&str> = paths.to_vec();
    want.sort_unstable();
    assert_eq!(got, want);
}

#[test]
fn the_health_section_is_capped_and_reports_what_it_dropped() {
    let index = full_index();
    let paths = [
        "src/core.rs",
        "src/leaf.rs",
        "src/helper.rs",
        "src/one.rs",
        "src/two.rs",
    ];
    let section = health_section(Some(&index), paths, 2);

    assert_eq!(section.considered, 5);
    assert_eq!(section.files.len(), 2);
    assert_eq!(section.truncated, 3);
    assert!(section.unavailable_reason.is_none());
    assert_eq!(section.weights_version, index.weights().version);
    // Highest defect risk first.
    assert!(
        section.files[0].defect_risk.score_per_mille
            >= section.files[1].defect_risk.score_per_mille
    );
    // Both axes ship, each with its own evidence.
    assert_eq!(section.files[0].maintainability.axis, "maintainability");
    assert!(!section.files[0].maintainability.facts.is_empty());
}

#[test]
fn the_health_section_deduplicates_and_ignores_empty_paths() {
    let index = full_index();
    let section = health_section(
        Some(&index),
        ["src/core.rs", "src/core.rs", "", "src/leaf.rs"],
        MAX_HEALTH_SECTION_FILES,
    );

    assert_eq!(section.considered, 2);
    assert_eq!(section.files.len(), 2);
}

#[test]
fn the_health_section_omits_files_with_no_facts_rather_than_banding_them() {
    let index = full_index();
    let section = health_section(
        Some(&index),
        ["src/core.rs", "src/not-indexed.rs"],
        MAX_HEALTH_SECTION_FILES,
    );

    assert_eq!(section.considered, 2);
    assert_eq!(section.files.len(), 1);
    assert_eq!(section.files[0].path, "src/core.rs");
    assert_eq!(section.truncated, 1);
}

#[test]
fn the_health_section_states_why_it_is_empty() {
    let index = full_index();
    let section = health_section(
        Some(&index),
        ["src/not-indexed.rs"],
        MAX_HEALTH_SECTION_FILES,
    );
    assert!(section.is_empty());
    assert_eq!(
        section.unavailable_reason.as_deref(),
        Some("no health facts cover these files")
    );

    let none = health_section(None, ["src/core.rs"], MAX_HEALTH_SECTION_FILES);
    assert!(none.is_empty());
    assert_eq!(none.availability, "unavailable");
    assert_eq!(
        none.unavailable_reason.as_deref(),
        Some("no health facts have been produced for this workspace")
    );
}

#[test]
fn untested_changes_report_only_measured_files_with_zero_edge_linked_tests() {
    let index = full_index();
    // The fixture graph contains no test files at all, so every production file
    // the test-proximity producer measured is untested.
    let entries = untested_changes(Some(&index), ["src/core.rs", "src/never-indexed.rs"]);

    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].path, "src/core.rs");
    assert_eq!(entries[0].linked_test_count, 0);
    assert!(entries[0].detail.contains("no edge-linked tests"));

    assert!(
        untested_changes(None, ["src/core.rs"]).is_empty(),
        "no index means no claim, not a claim of zero tests"
    );
}

#[test]
fn untested_changes_are_deduplicated_ordered_and_bounded() {
    let index = full_index();
    let entries = untested_changes(
        Some(&index),
        ["src/leaf.rs", "src/core.rs", "src/core.rs", "src/helper.rs"],
    );

    let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(paths, sorted, "entries must be path-ordered");
    assert!(entries.len() <= MAX_UNTESTED_CHANGE_ENTRIES);
    assert_eq!(
        paths.iter().filter(|path| **path == "src/core.rs").count(),
        1
    );
}

#[test]
fn a_graph_only_index_still_scores_and_names_every_family_it_lacked() {
    let index = HealthFactIndex::from_graph(&fixture_graph(), true);
    let evidence = defect_risk_evidence(Some(&index), "src/core.rs").expect("answers");

    assert!(!evidence.is_unknown());
    assert!(!evidence.exact, "missing families must widen the band");
    assert!(evidence.score_floor_per_mille < evidence.score_ceiling_per_mille);
    assert!(evidence
        .inputs_missing
        .iter()
        .any(|kind| kind == "bug_fix_commits"));
    assert!(evidence.summary.contains("scored without"));
}

#[test]
fn diagnose_ranking_only_permutes_the_candidates_the_trace_selected() {
    let index = full_index();
    // `src/core.rs` outranks everything, but it is not in the slice, so it can
    // never be admitted by the secondary key.
    let mut candidates = vec![candidate("src/leaf.rs", "b"), candidate("src/one.rs", "a")];
    rank_fault_candidates_by_defect_risk(Some(&index), &mut candidates);

    let paths = ordered(&candidates);
    assert_eq!(paths.len(), 2);
    assert!(!paths.contains(&"src/core.rs"));
}
