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
    let mut candidates = vec![candidate("src/leaf.rs", "a"), candidate("src/core.rs", "z")];
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

/// A population with a real spread: `f00` depends on every other file and `f14`
/// on none, so the files hold fifteen distinct fan-in/fan-out ranks and the
/// decile cutoff means something. `f00` is the riskiest because fan-out carries
/// the heavier measured weight of the two.
fn warning_fixture_index() -> HealthFactIndex {
    const FILES: usize = 15;
    let mut graph = CodeGraph::new();
    let ids: Vec<_> = (0..FILES)
        .map(|index| symbol(&format!("src/f{index:02}.rs"), &format!("f{index:02}")))
        .collect();
    for id in &ids {
        add_symbol(&mut graph, id, true, 10);
    }
    for target in 0..FILES {
        for source in 0..target {
            add_edge(&mut graph, &ids[source], &ids[target], EdgeKind::Calls);
        }
    }
    HealthFactIndex::from_graph(&graph, true)
}

/// The riskiest file in the fixture population.
const RISKIEST_FIXTURE_FILE: &str = "src/f00.rs";

#[test]
fn an_edited_file_in_the_riskiest_decile_earns_one_warning_citing_its_top_fact() {
    let index = warning_fixture_index();
    let warnings = select_health_warnings(Some(&index), [RISKIEST_FIXTURE_FILE]);

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let warning = &warnings[0];
    assert_eq!(warning.path, RISKIEST_FIXTURE_FILE);
    assert!(!warning.band.is_empty());
    assert!(
        warning.top_fact.contains("fan-in") || warning.top_fact.contains("fan-out"),
        "the warning must cite the fact that drove it, not a bare count: {}",
        warning.top_fact
    );
    assert!(warning.top_fact.contains("(p"), "{}", warning.top_fact);
    assert_eq!(warning.population, 15);
}

#[test]
fn a_file_outside_the_riskiest_decile_earns_no_warning() {
    let index = warning_fixture_index();
    assert!(
        select_health_warnings(Some(&index), ["src/f14.rs"]).is_empty(),
        "a low-ranked file must stay silent"
    );
}

#[test]
fn warnings_are_deduplicated_and_capped_at_five_files() {
    let index = warning_fixture_index();
    // Every path in the fixture, twice over.
    let mut paths: Vec<String> = index.paths().map(str::to_string).collect();
    paths.extend(paths.clone());

    let warnings = select_health_warnings(Some(&index), &paths);
    assert!(warnings.len() <= MAX_HEALTH_WARNINGS, "{warnings:?}");

    let mut seen: Vec<&str> = warnings.iter().map(|w| w.path.as_str()).collect();
    let before = seen.len();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), before, "warnings must be deduplicated");

    // Heaviest first.
    for pair in warnings.windows(2) {
        assert!(pair[0].score_per_mille >= pair[1].score_per_mille);
    }
}

#[test]
fn the_warning_is_silent_whenever_the_evidence_is_missing() {
    let index = warning_fixture_index();

    assert!(
        select_health_warnings(None, [RISKIEST_FIXTURE_FILE]).is_empty(),
        "no index means no warning"
    );
    assert!(
        select_health_warnings(Some(&HealthFactIndex::empty()), [RISKIEST_FIXTURE_FILE]).is_empty(),
        "an index with no facts means no warning"
    );
    assert!(
        select_health_warnings(Some(&index), ["src/never-indexed.rs"]).is_empty(),
        "an unmeasured path is unknown, not risky"
    );
    assert!(
        select_health_warnings(Some(&index), Vec::<String>::new()).is_empty(),
        "no edits means no warning"
    );
    // A population too small to have a meaningful decile stays silent rather
    // than warning about the least-good file in a handful.
    assert!(
        select_health_warnings(Some(&full_index()), ["src/core.rs"]).is_empty(),
        "a tiny population has no decile worth reporting"
    );
}

#[test]
fn warning_selection_is_deterministic() {
    let index = warning_fixture_index();
    let paths: Vec<String> = index.paths().map(str::to_string).collect();
    let mut reversed = paths.clone();
    reversed.reverse();

    assert_eq!(
        select_health_warnings(Some(&index), &paths),
        select_health_warnings(Some(&index), &reversed)
    );
}

fn healthy_status_inputs() -> HealthStatusInputs {
    HealthStatusInputs {
        index_complete: true,
        parse_failures: 0,
        git_availability: "available".to_string(),
        git_window_commits: 500,
        git_generation: Some(7),
    }
}

#[test]
fn health_status_reports_coverage_bands_and_provenance() {
    let index = full_index();
    let report = health_status(
        Some(&index),
        &healthy_status_inputs(),
        MAX_HEALTH_STATUS_TOP_FILES,
    );

    assert_eq!(report.files_scored, index.file_count());
    assert_eq!(report.weights_version, index.weights().version);
    assert_eq!(report.backtest_report, BACKTEST_REPORT_PATH);

    // Every family is listed, including ones that contributed nothing: an
    // absent family would read as a family with no problems.
    let families: Vec<&str> = report
        .families
        .iter()
        .map(|family| family.family.as_str())
        .collect();
    for expected in [
        "graph",
        "git",
        "complexity",
        "test_proximity",
        "dead_symbol",
    ] {
        assert!(families.contains(&expected), "{families:?}");
    }
    let graph = report
        .families
        .iter()
        .find(|family| family.family == "graph")
        .expect("graph family");
    assert!(graph.files_covered > 0);
    assert!(graph.coverage_per_mille > 0);
    assert!(graph.backtested);

    let test_proximity = report
        .families
        .iter()
        .find(|family| family.family == "test_proximity")
        .expect("test-proximity family");
    assert!(
        !test_proximity.backtested,
        "a provisional weight must be visible as one"
    );

    // Both axes, each with a full band histogram and its heaviest files.
    let axes: Vec<&str> = report.axes.iter().map(|axis| axis.axis.as_str()).collect();
    assert_eq!(axes, vec!["defect_risk", "maintainability"]);
    let defect_risk = &report.axes[0];
    assert_eq!(defect_risk.bands.len(), 4);
    assert_eq!(
        defect_risk
            .bands
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        vec!["low", "moderate", "high", "critical"]
    );
    let banded: usize = defect_risk.bands.iter().map(|(_, count)| count).sum();
    assert_eq!(banded + defect_risk.unknown, report.files_scored);
    assert!(!defect_risk.top_files.is_empty());
    assert!(!defect_risk.top_files[0].facts.is_empty());
    // Heaviest first.
    for pair in defect_risk.top_files.windows(2) {
        assert!(pair[0].score_per_mille >= pair[1].score_per_mille);
    }
}

#[test]
fn health_status_names_every_missing_input_in_plain_words() {
    let graph_only = HealthFactIndex::from_graph(&fixture_graph(), false);
    let report = health_status(
        Some(&graph_only),
        &HealthStatusInputs {
            index_complete: false,
            parse_failures: 3,
            git_availability: "stale".to_string(),
            git_window_commits: 0,
            git_generation: None,
        },
        MAX_HEALTH_STATUS_TOP_FILES,
    );

    let stated = report.incomplete_analysis.join(" | ");
    assert!(
        stated.contains("index behind these facts was incomplete"),
        "{stated}"
    );
    assert!(stated.contains("3 file(s) failed to parse"), "{stated}");
    assert!(stated.contains("history is stale"), "{stated}");
    // The families that produced nothing are each named.
    assert!(
        stated.contains("no git facts reached the index"),
        "{stated}"
    );
    assert!(
        stated.contains("no complexity facts reached the index"),
        "{stated}"
    );
    assert!(
        stated.contains("no test_proximity facts reached the index"),
        "{stated}"
    );

    let git = report
        .families
        .iter()
        .find(|family| family.family == "git")
        .expect("git family");
    assert_eq!(git.availability, "unavailable");
    assert_eq!(git.files_covered, 0);
    assert_eq!(git.coverage_per_mille, 0);

    // Missing inputs widen bands rather than faking precision.
    assert!(report.axes[0].inexact > 0);
}

#[test]
fn health_status_without_an_index_says_so_rather_than_reporting_zero() {
    let report = health_status(None, &healthy_status_inputs(), MAX_HEALTH_STATUS_TOP_FILES);

    assert_eq!(report.availability, "unavailable");
    assert_eq!(report.files_scored, 0);
    assert!(report.families.is_empty());
    assert!(report.axes.is_empty());
    assert!(report
        .incomplete_analysis
        .iter()
        .any(|line| line.contains("no health facts have been produced")));
    assert_eq!(report.backtest_report, BACKTEST_REPORT_PATH);
}

#[test]
fn health_status_is_deterministic() {
    let index = full_index();
    let inputs = healthy_status_inputs();
    let first = health_status(Some(&index), &inputs, MAX_HEALTH_STATUS_TOP_FILES);
    let second = health_status(Some(&index), &inputs, MAX_HEALTH_STATUS_TOP_FILES);
    assert_eq!(
        serde_json::to_string(&first).expect("serialize"),
        serde_json::to_string(&second).expect("serialize")
    );
}

#[test]
fn the_committed_backtest_report_health_status_cites_exists() {
    // A provenance pointer nobody can follow is not provenance.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join(BACKTEST_REPORT_PATH);
    assert!(
        path.exists(),
        "status cites a backtest report that is not in the repository: {}",
        path.display()
    );
}

/// Vocabulary that would turn measured evidence into a claim about the future.
///
/// The backtest is correlational, its ground truth is a subject-line heuristic
/// with a measured 15.9% recall gap, and 30 cut points across 5 repositories is
/// a small sample (`docs/reports/health-backtest/2026-08-14.md`, § "What H3 may
/// and may not conclude"). Spec design decision 6 therefore forbids a response
/// from saying a file *will* do anything. This list is the enforcement.
const PREDICTIVE_VOCABULARY: &[&str] = &[
    "likely defect",
    "likely bug",
    "likely to fail",
    "likely to break",
    "will fail",
    "will break",
    "will regress",
    "going to fail",
    "expect a defect",
    "expected to fail",
    "predict",
    "prediction",
    "predictive",
    "forecast",
    "probability of",
    "chance of failure",
    "risk of failure",
    "bug-prone",
    "defect-prone",
    "is buggy",
    "unsafe to change",
    "should be rewritten",
];

fn assert_descriptive(text: &str, origin: &str) {
    let lowered = text.to_lowercase();
    for phrase in PREDICTIVE_VOCABULARY {
        assert!(
            !lowered.contains(phrase),
            "{origin} uses predictive language {phrase:?}: {text}"
        );
    }
}

/// Every string the health surface can emit, from a fixture that exercises
/// every fact kind, both axes, both value polarities, and the unknown case.
fn every_emitted_string() -> Vec<(String, String)> {
    use crate::health::scoring::{ALL_AXES, ALL_FACT_KINDS};

    let index = full_index();
    let mut strings: Vec<(String, String)> = Vec::new();

    // Raw fact renderings across each fact's range: a boolean fact reads
    // differently at 0 and 1, and both forms reach a response.
    for kind in ALL_FACT_KINDS {
        for value in [0_u64, 1, 12, 4_200] {
            strings.push((
                format!("FactKind::{}::describe_value({value})", kind.as_str()),
                kind.describe_value(value),
            ));
        }
    }

    // Whole bundles, per axis, for a scored file, an unscored file, and a
    // graph-only index whose bands are widened by missing inputs.
    let graph_only = HealthFactIndex::from_graph(&fixture_graph(), true);
    for axis in ALL_AXES {
        for (label, source) in [("full", &index), ("graph-only", &graph_only)] {
            for path in ["src/core.rs", "src/leaf.rs", "src/never-indexed.rs"] {
                let score = source.score(path, axis);
                strings.push((
                    format!("{label} {} {path} summary", axis.as_str()),
                    score.summary(HEALTH_EVIDENCE_FACT_LIMIT),
                ));
                strings.push((
                    format!("{label} {} {path} band", axis.as_str()),
                    score.band_label(),
                ));
                for fact in score.top_facts(HEALTH_EVIDENCE_FACT_LIMIT) {
                    strings.push((
                        format!("{label} {} {path} fact", axis.as_str()),
                        fact.describe(),
                    ));
                }
            }
        }
    }

    // The consumer surfaces themselves, serialized exactly as a response
    // carries them.
    strings.push((
        "health_section".to_string(),
        serde_json::to_string(&health_section(
            Some(&index),
            ["src/core.rs", "src/leaf.rs", "src/never-indexed.rs"],
            MAX_HEALTH_SECTION_FILES,
        ))
        .expect("serialize section"),
    ));
    strings.push((
        "empty health_section".to_string(),
        serde_json::to_string(&health_section(
            None,
            ["src/core.rs"],
            MAX_HEALTH_SECTION_FILES,
        ))
        .expect("serialize empty section"),
    ));
    strings.push((
        "untested_changes".to_string(),
        serde_json::to_string(&untested_changes(
            Some(&index),
            ["src/core.rs", "src/leaf.rs"],
        ))
        .expect("serialize untested"),
    ));

    strings
}

#[test]
fn no_health_response_text_uses_predictive_language() {
    let emitted = every_emitted_string();
    assert!(
        emitted.len() > 100,
        "the corpus must actually exercise the surface, got {}",
        emitted.len()
    );
    for (origin, text) in emitted {
        assert_descriptive(&text, &origin);
    }
}

#[test]
fn the_predictive_vocabulary_guard_would_catch_a_regression() {
    // A guard nothing can fail is not a guard. This pins that the check itself
    // rejects the phrasing the spec forbids.
    let result = std::panic::catch_unwind(|| {
        assert_descriptive(
            "high defect risk: this file will fail next release",
            "fixture",
        )
    });
    assert!(result.is_err(), "the guard must reject predictive phrasing");
}
