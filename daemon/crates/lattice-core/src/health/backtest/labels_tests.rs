//! Tests for horizon selection and defect labeling, with explicit coverage of
//! both edges of both horizon bounds (spec H1 acceptance: "horizon boundary
//! handling").

use super::*;

const CUT_POINT_TIME: i64 = 1_700_000_000;
const NINETY_DAYS: i64 = 90 * SECONDS_PER_DAY;

fn commit(id: &str, offset_seconds: i64, subject: &str, paths: &[&str]) -> HorizonCommit {
    HorizonCommit {
        id: id.to_owned(),
        committed_at_seconds: CUT_POINT_TIME + offset_seconds,
        subject: subject.to_owned(),
        changed_paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        path_overflow: false,
    }
}

fn select(candidates: Vec<HorizonCommit>) -> HorizonSelection {
    select_horizon(CUT_POINT_TIME, candidates, HorizonLimits::default())
}

#[test]
fn a_commit_exactly_on_the_day_boundary_is_admitted() {
    let selection = select(vec![commit(
        "a",
        NINETY_DAYS,
        "fix: boundary",
        &["src/lib.rs"],
    )]);
    assert_eq!(
        selection.report.deadline_seconds,
        CUT_POINT_TIME + NINETY_DAYS
    );
    assert_eq!(selection.report.selected_commits, 1);
    assert!(!selection.report.truncated_by_day_cap);
    assert!(label_defects(&selection).is_defective("src/lib.rs"));
}

#[test]
fn a_commit_one_second_past_the_day_boundary_is_excluded() {
    let selection = select(vec![commit(
        "a",
        NINETY_DAYS + 1,
        "fix: just past the boundary",
        &["src/lib.rs"],
    )]);
    assert_eq!(selection.report.selected_commits, 0);
    assert!(selection.report.truncated_by_day_cap);
    assert!(!selection.report.truncated_by_commit_cap);
    assert!(!label_defects(&selection).is_defective("src/lib.rs"));
}

#[test]
fn a_commit_exactly_on_the_commit_cap_is_admitted_and_the_next_is_not() {
    // 201 commits, one second apart so the day cap can never be the binding
    // constraint. Only the 200th touches `on_the_cap`; the 201st touches
    // `past_the_cap`.
    let mut candidates: Vec<HorizonCommit> = (1..=199)
        .map(|index| {
            commit(
                &format!("c{index:03}"),
                index,
                "chore: filler",
                &["src/filler.rs"],
            )
        })
        .collect();
    candidates.push(commit(
        "c200",
        200,
        "fix: on the cap",
        &["src/on_the_cap.rs"],
    ));
    candidates.push(commit(
        "c201",
        201,
        "fix: past the cap",
        &["src/past_the_cap.rs"],
    ));
    assert_eq!(candidates.len(), 201);

    let selection = select(candidates);
    assert_eq!(selection.report.selected_commits, 200);
    assert!(selection.report.truncated_by_commit_cap);
    assert!(!selection.report.truncated_by_day_cap);

    let labels = label_defects(&selection);
    assert!(labels.is_defective("src/on_the_cap.rs"));
    assert!(!labels.is_defective("src/past_the_cap.rs"));
}

#[test]
fn whichever_bound_trips_first_wins() {
    // Ten commits spread far beyond the day cap: the day cap binds even though
    // the commit cap is nowhere near.
    let candidates: Vec<HorizonCommit> = (1..=10)
        .map(|index| {
            commit(
                &format!("c{index}"),
                index * 30 * SECONDS_PER_DAY,
                "fix: spread out",
                &["src/lib.rs"],
            )
        })
        .collect();
    let selection = select(candidates);
    // Days 30, 60 and 90 are inside; day 120 stops the walk.
    assert_eq!(selection.report.selected_commits, 3);
    assert!(selection.report.truncated_by_day_cap);
    assert!(!selection.report.truncated_by_commit_cap);
}

#[test]
fn an_exhausted_horizon_reports_neither_truncation() {
    let selection = select(vec![
        commit("a", 60, "fix: one", &["src/a.rs"]),
        commit("b", 120, "feat: two", &["src/b.rs"]),
    ]);
    assert_eq!(selection.report.candidates_available, 2);
    assert_eq!(selection.report.selected_commits, 2);
    assert!(selection.report.is_exhaustive());
}

#[test]
fn only_fix_shaped_commits_label_the_files_they_touch() {
    let selection = select(vec![
        commit(
            "a",
            60,
            "fix: null deref",
            &["src/broken.rs", "src/also.rs"],
        ),
        commit("b", 120, "feat: add a thing", &["src/feature.rs"]),
        commit("c", 180, "refactor: tidy", &["src/broken.rs"]),
    ]);
    let labels = label_defects(&selection);
    assert!(labels.is_defective("src/broken.rs"));
    assert!(labels.is_defective("src/also.rs"));
    // Touched only by non-fix commits: clean.
    assert!(!labels.is_defective("src/feature.rs"));
    assert_eq!(selection.report.fix_shaped_commits, 1);
    assert_eq!(labels.defective_paths().len(), 2);
}

#[test]
fn repeated_fixes_to_one_file_accumulate_a_count() {
    let selection = select(vec![
        commit("a", 60, "fix: first", &["src/fragile.rs"]),
        commit("b", 120, "fix: second", &["src/fragile.rs"]),
        commit("c", 180, "bug: third", &["src/fragile.rs"]),
    ]);
    let labels = label_defects(&selection);
    assert_eq!(labels.defect_commits("src/fragile.rs"), 3);
    assert_eq!(labels.defect_commits("src/never_touched.rs"), 0);
}

#[test]
fn a_path_repeated_inside_one_commit_counts_once() {
    // A rename records the old and new path on the same delta; without the
    // per-commit dedupe a rename would inflate the count.
    let selection = select(vec![commit(
        "a",
        60,
        "fix: rename during a fix",
        &["src/same.rs", "./src/same.rs", "src/same.rs"],
    )]);
    assert_eq!(label_defects(&selection).defect_commits("src/same.rs"), 1);
}

#[test]
fn path_overflow_commits_label_nothing_and_are_counted() {
    let mut overflowing = commit("a", 60, "fix: enormous sweep", &["src/everything.rs"]);
    overflowing.path_overflow = true;
    let selection = select(vec![overflowing]);
    assert_eq!(selection.report.path_overflow_commits, 1);
    let labels = label_defects(&selection);
    assert!(!labels.is_defective("src/everything.rs"));
    assert!(labels.report.is_degraded());
}

#[test]
fn uncanonicalizable_paths_are_counted_rather_than_silently_dropped() {
    let selection = select(vec![commit(
        "a",
        60,
        "fix: escape attempt",
        &["../outside.rs", "/absolute.rs", "src/legit.rs"],
    )]);
    let labels = label_defects(&selection);
    assert_eq!(labels.report.invalid_path_entries, 2);
    assert!(labels.is_defective("src/legit.rs"));
    assert_eq!(labels.defective_paths().len(), 1);
}

#[test]
fn backwards_timestamps_are_admitted_but_reported_as_skew() {
    let selection = select(vec![
        commit("a", -3600, "fix: rewritten history", &["src/a.rs"]),
        commit("b", 60, "fix: normal", &["src/b.rs"]),
    ]);
    assert_eq!(selection.report.selected_commits, 2);
    assert_eq!(selection.report.clock_skew_commits, 1);
    assert!(selection.report.is_degraded());
    // Skew degrades the measurement's provenance but does not discard the
    // label: the commit really does descend from the cut point.
    assert!(label_defects(&selection).is_defective("src/a.rs"));
}

#[test]
fn horizon_limits_are_clamped_to_the_hard_ceilings() {
    let bounded = HorizonLimits {
        max_commits: usize::MAX,
        max_days: u32::MAX,
    }
    .bounded();
    assert_eq!(bounded.max_commits, MAX_HORIZON_COMMITS);
    assert_eq!(bounded.max_days, MAX_HORIZON_DAYS);

    // A zero bound would label every file clean; it is raised, never honored.
    let raised = HorizonLimits {
        max_commits: 0,
        max_days: 0,
    }
    .bounded();
    assert_eq!(raised.max_commits, 1);
    assert_eq!(raised.max_days, 1);
}

#[test]
fn an_empty_horizon_labels_nothing() {
    let selection = select(Vec::new());
    let labels = label_defects(&selection);
    assert_eq!(selection.report.selected_commits, 0);
    assert!(selection.report.is_exhaustive());
    assert!(labels.defect_commits_by_path.is_empty());
}

#[test]
fn cut_points_are_evenly_spaced_distinct_and_inside_the_reserved_band() {
    let indices = cut_point_indices(1000, 6, 100, 100);
    assert_eq!(indices.len(), 6);
    for index in &indices {
        assert!(*index >= 100, "{index} intrudes on the HEAD reserve");
        assert!(*index <= 899, "{index} intrudes on the root reserve");
    }
    // Ascending and strictly increasing.
    for window in indices.windows(2) {
        assert!(window[0] < window[1]);
    }
    // Evenly spaced: integer placement makes gaps differ by at most one
    // commit, which is the tightest guarantee available without floats.
    let gaps: Vec<usize> = indices.windows(2).map(|pair| pair[1] - pair[0]).collect();
    let smallest = *gaps.iter().min().expect("gaps");
    let largest = *gaps.iter().max().expect("gaps");
    assert!(largest - smallest <= 1, "{gaps:?}");
}

#[test]
fn a_history_too_short_for_the_reserves_yields_no_cut_points() {
    assert!(cut_point_indices(150, 6, 100, 100).is_empty());
    assert!(cut_point_indices(0, 6, 0, 0).is_empty());
    assert!(cut_point_indices(1000, 0, 100, 100).is_empty());
}

#[test]
fn a_narrow_band_yields_fewer_points_rather_than_duplicates() {
    let indices = cut_point_indices(206, 6, 100, 100);
    assert!(!indices.is_empty());
    assert!(indices.len() <= 6);
    let mut deduped = indices.clone();
    deduped.dedup();
    assert_eq!(indices, deduped);
}

#[test]
fn a_single_cut_point_lands_in_the_middle_of_the_band() {
    assert_eq!(cut_point_indices(1000, 1, 100, 100), vec![499]);
}
