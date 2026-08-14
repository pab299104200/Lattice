//! Tests for report construction and rendering, including the spec's
//! determinism requirement: identical output for replayed input.

use std::fs;
use std::path::Path;

use git2::{IndexAddOption, Repository, Signature, Time};
use tempfile::TempDir;

use super::super::labels::HorizonLimits;
use super::super::replay::{replay_repository, ReplayLimits};
use super::*;

const BASE_TIME: i64 = 1_600_000_000;
const SECONDS_PER_DAY: i64 = 86_400;
/// Commits in the synthetic history. Enough to place several cut points, each
/// with a window behind it and a horizon ahead of it.
const HISTORY_LENGTH: usize = 60;
/// Files in the synthetic repository.
const FILE_COUNT: usize = 8;

/// Build a synthetic repository whose defect history is deliberately
/// structured: the lower-numbered files are churned and fixed far more often
/// than the higher-numbered ones, so the facts have something real to find.
fn build_fixture(directory: &Path) {
    let repository = Repository::init(directory).expect("initialize fixture repository");
    let mut parent: Option<git2::Oid> = None;

    for step in 0..HISTORY_LENGTH {
        // Weight the churn toward the first few files.
        let file_index = (step * step) % FILE_COUNT;
        let name = format!("src/module{file_index}.rs");
        let body = format!(
            "pub fn module{file_index}(value: u32) -> u32 {{\n    if value > {step} {{\n        return value + {step};\n    }}\n    match value {{\n        0 => {step},\n        _ => value,\n    }}\n}}\n"
        );
        let path = directory.join(&name);
        fs::create_dir_all(path.parent().expect("parent")).expect("create fixture directory");
        fs::write(&path, body).expect("write fixture file");

        // Fixes cluster on the busiest files, which is the pattern the harness
        // is supposed to be able to detect.
        let subject = if file_index < 3 && step % 3 == 0 {
            format!("fix: repair module{file_index} at step {step}")
        } else {
            format!("feat: extend module{file_index} at step {step}")
        };

        let mut index = repository.index().expect("fixture index");
        index
            .add_all(["*"], IndexAddOption::DEFAULT, None)
            .expect("stage fixture files");
        index.write().expect("write fixture index");
        let tree_id = index.write_tree().expect("write fixture tree");
        let tree = repository.find_tree(tree_id).expect("read fixture tree");

        let when = Time::new(BASE_TIME + step as i64 * SECONDS_PER_DAY, 0);
        let signature = Signature::new(
            if step % 4 == 0 {
                "Author One"
            } else {
                "Author Two"
            },
            "fixture@example.test",
            &when,
        )
        .expect("fixture signature");
        let parents: Vec<git2::Commit<'_>> = parent
            .into_iter()
            .map(|oid| repository.find_commit(oid).expect("parent commit"))
            .collect();
        let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
        parent = Some(
            repository
                .commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    &subject,
                    &tree,
                    &parent_refs,
                )
                .expect("create fixture commit"),
        );
    }
}

fn fixture_limits() -> ReplayLimits {
    ReplayLimits {
        cut_points: 4,
        mining_reserve: 5,
        horizon: HorizonLimits {
            max_commits: 10,
            max_days: 90,
        },
        ..ReplayLimits::default()
    }
}

fn fixture_report() -> (TempDir, BacktestReport) {
    let directory = tempfile::tempdir().expect("temp directory");
    build_fixture(directory.path());
    let replay =
        replay_repository(directory.path(), fixture_limits()).expect("replay the fixture");
    let report = build_report(std::slice::from_ref(&replay));
    (directory, report)
}

#[test]
fn replaying_the_same_input_twice_produces_a_byte_identical_report() {
    let directory = tempfile::tempdir().expect("temp directory");
    build_fixture(directory.path());

    let first = build_report(&[replay_repository(directory.path(), fixture_limits())
        .expect("first replay")]);
    let second = build_report(&[replay_repository(directory.path(), fixture_limits())
        .expect("second replay")]);

    assert_eq!(
        first.render_markdown(),
        second.render_markdown(),
        "the markdown render must be byte-identical for replayed input"
    );
    assert_eq!(
        first.render_json().expect("json"),
        second.render_json().expect("json"),
        "the JSON render must be byte-identical for replayed input"
    );
    assert_eq!(first, second);
}

#[test]
fn rendering_the_same_report_twice_is_stable() {
    let (_directory, report) = fixture_report();
    assert_eq!(report.render_markdown(), report.render_markdown());
    assert_eq!(
        report.render_json().expect("json"),
        report.render_json().expect("json")
    );
}

#[test]
fn the_report_carries_no_timestamp_that_would_break_determinism() {
    let (_directory, report) = fixture_report();
    let markdown = report.render_markdown();
    // The fixture commits are dated from BASE_TIME; the report must not print
    // a wall-clock reading of its own anywhere.
    assert!(markdown.contains("no generation timestamp"));
    for banned in ["generated at", "Generated:", "timestamp:"] {
        assert!(
            !markdown.contains(banned),
            "report leaked a clock reading: {banned}"
        );
    }
}

#[test]
fn the_report_compares_graph_only_against_graph_plus_git() {
    let (_directory, report) = fixture_report();

    assert_eq!(report.pooled_families.len(), 3);
    let families: Vec<FamilySet> = report
        .pooled_families
        .iter()
        .map(|result| result.family)
        .collect();
    assert_eq!(
        families,
        vec![FamilySet::GraphOnly, FamilySet::GraphGit, FamilySet::All]
    );

    let markdown = report.render_markdown();
    assert!(markdown.contains("graph-only"));
    assert!(markdown.contains("graph+git"));
    assert!(markdown.contains("graph+git+complexity"));
}

#[test]
fn the_fixture_history_produces_measurable_results_rather_than_empty_tables() {
    let (_directory, report) = fixture_report();

    assert!(report.pooled_cut_points > 1);
    assert!(report.pooled_observations > 0);
    assert!(
        report.pooled_positives > 0,
        "the fixture must produce defect labels for the metrics to mean anything"
    );
    assert!(report.pooled_positives < report.pooled_observations);

    for result in &report.pooled_families {
        let evaluation = result
            .evaluation
            .as_ref()
            .unwrap_or_else(|| panic!("{} was unavailable", result.family.label()));
        assert_eq!(evaluation.calibration.len(), 10);
        assert!(evaluation.roc_auc_per_mille <= 1000);
    }
}

#[test]
fn the_git_family_measures_facts_the_graph_family_cannot_see() {
    let (_directory, report) = fixture_report();
    // The fixture concentrates fixes on the busiest files, so the bug-fix and
    // hotspot facts must carry above-chance signal. This is the assertion that
    // the whole pipeline — mining, labeling, normalizing, ranking — is wired
    // to something real rather than producing well-formed noise.
    let hotspot = report
        .features
        .iter()
        .find(|feature| feature.feature == FeatureKind::HotspotScore)
        .expect("hotspot feature");
    assert!(hotspot.observations > 0);
    assert!(
        hotspot.roc_auc_per_mille.expect("hotspot auc") > 500,
        "hotspot measured at or below chance on a fixture built to reward it"
    );
    assert!(hotspot.derived_weight > 0);
}

#[test]
fn every_feature_appears_in_the_derivation_table_even_when_unmeasurable() {
    let (_directory, report) = fixture_report();
    assert_eq!(report.features.len(), FEATURE_COUNT);
    for (result, feature) in report.features.iter().zip(ALL_FEATURES.iter()) {
        assert_eq!(result.feature, *feature);
        assert_eq!(result.family, feature.family());
        // A fact nobody could measure reports no AUC and no weight, rather than
        // a default that would read as a measurement.
        if result.roc_auc_per_mille.is_none() {
            assert_eq!(result.derived_weight, 0);
        }
    }
}

#[test]
fn a_feature_at_or_below_chance_receives_no_weight() {
    let (_directory, report) = fixture_report();
    for feature in &report.features {
        if let Some(auc) = feature.roc_auc_per_mille {
            if auc <= 500 {
                assert_eq!(
                    feature.derived_weight, 0,
                    "{} measured at or below chance but carries weight",
                    feature.feature.as_str()
                );
            } else {
                assert_eq!(feature.derived_weight, (auc - 500) * 2);
            }
        }
    }
}

#[test]
fn the_json_render_is_parseable_and_carries_the_headline_numbers() {
    let (_directory, report) = fixture_report();
    let json = report.render_json().expect("json");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse json");

    assert_eq!(parsed["harness_version"], BACKTEST_HARNESS_VERSION);
    assert_eq!(parsed["health_config_version"], HEALTH_CONFIG_VERSION);
    assert_eq!(parsed["pooled_observations"], report.pooled_observations);
    assert!(parsed["pooled_families"].as_array().expect("families").len() == 3);
    assert!(parsed["features"].as_array().expect("features").len() == FEATURE_COUNT);
    assert!(parsed["audit"]["verdict"].is_string());
}

#[test]
fn the_markdown_render_contains_every_required_section() {
    let (_directory, report) = fixture_report();
    let markdown = report.render_markdown();
    for section in [
        "# Health backtest report",
        "## Provenance",
        "## Headline",
        "## Per repository",
        "## Calibration by decile",
        "## Per-fact discrimination",
        "## H1.2 label-quality audit",
        "## Windows, cut points, and exclusion counters",
        "## What H3 may and may not conclude",
    ] {
        assert!(markdown.contains(section), "missing section: {section}");
    }
}

#[test]
fn the_report_states_the_limits_of_its_own_evidence() {
    let (_directory, report) = fixture_report();
    let markdown = report.render_markdown();
    // The spec forbids predictive language without evidence; the report must
    // say plainly what it cannot support.
    assert!(markdown.contains("correlational"));
    assert!(markdown.contains("Recall is measured against detected fixes"));
    assert!(markdown.contains("small sample"));
}

#[test]
fn a_single_cut_point_cannot_support_a_held_out_claim() {
    let directory = tempfile::tempdir().expect("temp directory");
    build_fixture(directory.path());
    let replay = replay_repository(
        directory.path(),
        ReplayLimits {
            cut_points: 1,
            ..fixture_limits()
        },
    )
    .expect("replay");
    let report = build_report(&[replay]);

    assert_eq!(report.pooled_cut_points, 1);
    for result in &report.held_out_families {
        assert!(
            result.evaluation.is_none(),
            "{} claimed a held-out result with nothing to hold out",
            result.family.label()
        );
        assert!(result.unavailable.is_some());
    }
    // The uniform comparison is still available: it needs no holdout.
    assert!(report
        .pooled_families
        .iter()
        .any(|result| result.evaluation.is_some()));
}

#[test]
fn pooling_across_repositories_sums_their_observations() {
    let first = tempfile::tempdir().expect("temp directory");
    build_fixture(first.path());
    let second = tempfile::tempdir().expect("temp directory");
    build_fixture(second.path());

    let replays = vec![
        replay_repository(first.path(), fixture_limits()).expect("first replay"),
        replay_repository(second.path(), fixture_limits()).expect("second replay"),
    ];
    let report = build_report(&replays);

    assert_eq!(report.repositories.len(), 2);
    let summed: u32 = report
        .repositories
        .iter()
        .map(|repository| repository.observations)
        .sum();
    assert_eq!(report.pooled_observations, summed);
    let summed_positives: u32 = report
        .repositories
        .iter()
        .map(|repository| repository.positives)
        .sum();
    assert_eq!(report.pooled_positives, summed_positives);
    // Two repositories replayed identically must contribute identical cut
    // point counts, and the pooled count is their sum.
    assert_eq!(
        report.pooled_cut_points,
        report
            .repositories
            .iter()
            .map(|repository| repository.cut_points.len() as u32)
            .sum::<u32>()
    );
}

#[test]
fn streaming_repositories_in_one_at_a_time_matches_building_them_together() {
    let first = tempfile::tempdir().expect("temp directory");
    build_fixture(first.path());
    let second = tempfile::tempdir().expect("temp directory");
    build_fixture(second.path());

    let replays = vec![
        replay_repository(first.path(), fixture_limits()).expect("first replay"),
        replay_repository(second.path(), fixture_limits()).expect("second replay"),
    ];
    let batched = build_report(&replays);

    // The streaming path exists so a caller can drop each replay as it goes;
    // it must not thereby produce a different report.
    let mut builder = ReportBuilder::new();
    for replay in replays {
        builder.push(&replay);
        drop(replay);
    }
    assert_eq!(builder.finish(), batched);
}

#[test]
fn the_generalization_table_appears_only_when_there_is_something_to_compare() {
    let (_directory, single) = fixture_report();
    // One repository cannot show whether a fact generalises, so the section is
    // omitted rather than printed with a single column that implies it does.
    assert!(!single
        .render_markdown()
        .contains("Does each fact generalise"));

    let first = tempfile::tempdir().expect("temp directory");
    build_fixture(first.path());
    let second = tempfile::tempdir().expect("temp directory");
    build_fixture(second.path());
    let pooled = build_report(&[
        replay_repository(first.path(), fixture_limits()).expect("first replay"),
        replay_repository(second.path(), fixture_limits()).expect("second replay"),
    ]);
    let markdown = pooled.render_markdown();
    assert!(markdown.contains("Does each fact generalise"));
    for repository in &pooled.repositories {
        assert_eq!(repository.features.len(), FEATURE_COUNT);
    }
}

#[test]
fn per_mille_values_render_without_floating_point_drift() {
    assert_eq!(decimal(0), "0.000");
    assert_eq!(decimal(1), "0.001");
    assert_eq!(decimal(500), "0.500");
    assert_eq!(decimal(1000), "1.000");
    assert_eq!(decimal(2345), "2.345");
    assert_eq!(signed_decimal(-1), "-0.001");
    assert_eq!(signed_decimal(-1000), "-1.000");
    assert_eq!(signed_decimal(0), "0.000");
    assert_eq!(percentage(1000), "100.0%");
    assert_eq!(percentage(55), "5.5%");
    assert_eq!(percentage(0), "0.0%");
}

#[test]
fn commit_subjects_containing_table_syntax_are_escaped() {
    assert_eq!(escape_cell("fix: a | b"), "fix: a \\| b");
    assert_eq!(escape_cell("fix: a \\ b"), "fix: a \\\\ b");
}

#[test]
fn the_audit_pools_every_mined_window_across_repositories() {
    let (_directory, report) = fixture_report();
    // Windows overlap heavily across cut points; the audit dedupes by commit
    // id, so it can never report more commits than the history contains.
    assert!(report.audit.commits_considered > 0);
    assert!(report.audit.commits_considered <= HISTORY_LENGTH as u32);
    assert!(report.audit.classified_fix > 0);
}
