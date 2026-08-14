//! Tests for the replay adapter, built on real synthetic repositories created
//! commit by commit with `git2`.
//!
//! The load-bearing test in this file is [`no_post_cut_point_commit_can_reach
//! _the_facts`], which proves the harness's zero-leakage property by
//! construction: a repository truncated at the cut point must produce facts
//! identical to the full repository replayed at that cut point. Any leakage of
//! any kind — a wider walk, a stale tree read, an off-by-one in the horizon —
//! changes the first and breaks the equality.

use std::fs;
use std::path::{Path, PathBuf};

use git2::{IndexAddOption, Repository, Signature, Time};
use tempfile::TempDir;

use super::*;

/// Fixed epoch for fixture commits, so timestamps are deterministic.
const BASE_TIME: i64 = 1_600_000_000;
const SECONDS_PER_DAY: i64 = 86_400;

/// One fixture commit.
struct CommitSpec {
    subject: &'static str,
    /// Files written by this commit.
    files: &'static [(&'static str, &'static str)],
    /// Files deleted by this commit.
    removals: &'static [&'static str],
    /// Days after [`BASE_TIME`] at which the commit is made.
    day: i64,
}

impl CommitSpec {
    const fn new(day: i64, subject: &'static str, files: &'static [(&'static str, &'static str)]) -> Self {
        Self {
            subject,
            files,
            removals: &[],
            day,
        }
    }

    const fn removing(
        day: i64,
        subject: &'static str,
        files: &'static [(&'static str, &'static str)],
        removals: &'static [&'static str],
    ) -> Self {
        Self {
            subject,
            files,
            removals,
            day,
        }
    }
}

/// A file body long and branchy enough to yield non-trivial complexity facts.
const BUSY_BODY: &str = "pub fn work(value: u32) -> u32 {\n    if value > 10 {\n        if value > 20 {\n            return value * 2;\n        }\n        return value + 1;\n    }\n    match value {\n        0 => 0,\n        1 => 1,\n        _ => value - 1,\n    }\n}\n";
const QUIET_BODY: &str = "pub fn quiet() -> u32 {\n    7\n}\n";

/// Create a repository and apply `specs` in order, returning the commit ids in
/// the same order.
fn build_repository(directory: &Path, specs: &[CommitSpec]) -> Vec<git2::Oid> {
    let repository = Repository::init(directory).expect("initialize fixture repository");
    let mut commits = Vec::new();
    let mut parent: Option<git2::Oid> = None;

    for spec in specs {
        for (name, contents) in spec.files {
            let path = directory.join(name);
            if let Some(folder) = path.parent() {
                fs::create_dir_all(folder).expect("create fixture parent directory");
            }
            fs::write(path, contents).expect("write fixture file");
        }
        for name in spec.removals {
            let path = directory.join(name);
            if path.exists() {
                fs::remove_file(path).expect("remove fixture file");
            }
        }

        let mut index = repository.index().expect("fixture index");
        index
            .add_all(["*"], IndexAddOption::DEFAULT, None)
            .expect("stage fixture files");
        // `add_all` does not notice deletions; this reconciles them.
        index
            .update_all(["*"], None)
            .expect("reconcile fixture deletions");
        index.write().expect("write fixture index");
        let tree_id = index.write_tree().expect("write fixture tree");
        let tree = repository.find_tree(tree_id).expect("read fixture tree");

        let when = Time::new(BASE_TIME + spec.day * SECONDS_PER_DAY, 0);
        let signature = Signature::new("Fixture Author", "fixture@example.test", &when)
            .expect("fixture signature");
        let parents: Vec<git2::Commit<'_>> = parent
            .into_iter()
            .map(|oid| repository.find_commit(oid).expect("parent commit"))
            .collect();
        let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
        let oid = repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                spec.subject,
                &tree,
                &parent_refs,
            )
            .expect("create fixture commit");
        commits.push(oid);
        parent = Some(oid);
    }

    commits
}

/// A history designed so that leakage in either direction flips a label.
///
/// * `src/late_break.rs` is quiet before the cut point and is fixed twice
///   after it. Its pre-cut-point facts must show no bug-fix history at all,
///   yet its label must be positive.
/// * `src/early_fix.rs` is fixed *before* the cut point and never touched
///   after it. Its pre-cut-point facts must show that bug-fix history, yet its
///   label must be negative. If the labeler's window reached backwards past the
///   cut point, this label would flip to positive.
///
/// Commit index 5 (day 50) is the cut point.
const LEAKAGE_HISTORY: &[CommitSpec] = &[
    CommitSpec::new(
        0,
        "Initial implementation",
        &[
            ("src/late_break.rs", QUIET_BODY),
            ("src/early_fix.rs", BUSY_BODY),
            ("src/steady.rs", BUSY_BODY),
        ],
    ),
    CommitSpec::new(10, "fix: correct the early defect", &[("src/early_fix.rs", "pub fn early() -> u32 {\n    1\n}\n")]),
    CommitSpec::new(20, "feat: extend steady", &[("src/steady.rs", "pub fn steady() -> u32 {\n    2\n}\n")]),
    CommitSpec::new(30, "fix: correct the early defect again", &[("src/early_fix.rs", "pub fn early() -> u32 {\n    3\n}\n")]),
    CommitSpec::new(40, "feat: extend steady further", &[("src/steady.rs", "pub fn steady() -> u32 {\n    4\n}\n")]),
    // --- cut point: index 5, day 50 -------------------------------------
    CommitSpec::new(50, "chore: the cut point", &[("src/steady.rs", "pub fn steady() -> u32 {\n    5\n}\n")]),
    // --- horizon --------------------------------------------------------
    CommitSpec::new(60, "fix: the late break surfaces", &[("src/late_break.rs", "pub fn quiet() -> u32 {\n    60\n}\n")]),
    CommitSpec::new(70, "fix: the late break again", &[("src/late_break.rs", "pub fn quiet() -> u32 {\n    70\n}\n")]),
    CommitSpec::new(80, "feat: unrelated work", &[("src/steady.rs", "pub fn steady() -> u32 {\n    8\n}\n")]),
];

/// Index of the cut point in [`LEAKAGE_HISTORY`].
const CUT_POINT_INDEX: usize = 5;

/// Test limits: a single cut point, small enough windows to be quick, but the
/// real default horizon so the boundary logic under test is the shipped one.
fn test_limits() -> ReplayLimits {
    ReplayLimits {
        cut_points: 1,
        mining_reserve: 1,
        ..ReplayLimits::default()
    }
}

fn fixture(specs: &[CommitSpec]) -> (TempDir, Vec<git2::Oid>) {
    let directory = tempfile::tempdir().expect("temp directory");
    let commits = build_repository(directory.path(), specs);
    (directory, commits)
}

fn feature(replay: &CutPointReplay, path: &str, kind: FeatureKind) -> Option<u64> {
    replay
        .observations
        .iter()
        .find(|observation| observation.path == path)
        .unwrap_or_else(|| panic!("no observation for {path}"))
        .features
        .get(kind)
}

fn label(replay: &CutPointReplay, path: &str) -> bool {
    replay
        .observations
        .iter()
        .find(|observation| observation.path == path)
        .unwrap_or_else(|| panic!("no observation for {path}"))
        .label
}

#[test]
fn labels_come_only_from_the_horizon_and_facts_only_from_before_it() {
    let (directory, commits) = fixture(LEAKAGE_HISTORY);
    let replay = replay_at_commit(
        directory.path(),
        &commits[CUT_POINT_INDEX].to_string(),
        test_limits(),
    )
    .expect("replay at the cut point");

    // The file that breaks *after* the cut point: no bug-fix history is
    // visible at the cut point, yet the horizon labels it defective. A fact
    // pipeline that reached forward would show bug-fix commits here.
    assert_eq!(
        feature(&replay, "src/late_break.rs", FeatureKind::BugFixCommits),
        Some(0),
        "post-cut-point fixes must be invisible to the facts"
    );
    assert!(
        label(&replay, "src/late_break.rs"),
        "the horizon must label a file its fixes touched"
    );

    // The file fixed *before* the cut point: the history is visible as a fact,
    // and the label is negative. A labeler whose window reached backwards would
    // flip this label to positive.
    assert_eq!(
        feature(&replay, "src/early_fix.rs", FeatureKind::BugFixCommits),
        Some(2),
        "pre-cut-point fixes must be visible to the facts"
    );
    assert!(
        !label(&replay, "src/early_fix.rs"),
        "a fix before the cut point is history, not ground truth"
    );

    // Touched on both sides, but never by a fix-shaped commit.
    assert!(!label(&replay, "src/steady.rs"));
    assert_eq!(replay.labels.defective_paths().len(), 1);
}

#[test]
fn no_post_cut_point_commit_can_reach_the_facts() {
    // The full history, replayed at the cut point.
    let (full_directory, commits) = fixture(LEAKAGE_HISTORY);
    let replayed = replay_at_commit(
        full_directory.path(),
        &commits[CUT_POINT_INDEX].to_string(),
        test_limits(),
    )
    .expect("replay the full history at the cut point");

    // The same history physically truncated at the cut point: this repository
    // contains no post-cut-point object at all, so whatever facts it yields are
    // leakage-free by construction.
    let (truncated_directory, truncated_commits) = fixture(&LEAKAGE_HISTORY[..=CUT_POINT_INDEX]);
    let truncated = replay_at_commit(
        truncated_directory.path(),
        &truncated_commits[CUT_POINT_INDEX].to_string(),
        test_limits(),
    )
    .expect("replay the truncated history at its head");

    // Every fact family must match exactly. This is the general proof: any
    // post-cut-point information reaching any producer would show up here.
    assert_eq!(
        replayed.git.files, truncated.git.files,
        "git facts differ, so a post-cut-point commit reached the miner"
    );
    assert_eq!(
        replayed.git.co_changes, truncated.git.co_changes,
        "co-change facts differ, so a post-cut-point commit reached the miner"
    );
    assert_eq!(
        replayed.git.processed_commits, truncated.git.processed_commits,
        "the mined commit window differs across the cut point"
    );
    assert_eq!(
        replayed.graph.files, truncated.graph.files,
        "graph facts differ, so a post-cut-point tree reached the graph"
    );
    assert_eq!(
        replayed.complexity, truncated.complexity,
        "complexity facts differ, so a post-cut-point blob was read"
    );
    assert_eq!(
        replayed.tree, truncated.tree,
        "the tree read differs across the cut point"
    );

    // The features fed to scoring are therefore identical too, even though the
    // labels are not: the truncated repository has no horizon.
    let replayed_features: Vec<_> = replayed
        .observations
        .iter()
        .map(|observation| (&observation.path, &observation.features))
        .collect();
    let truncated_features: Vec<_> = truncated
        .observations
        .iter()
        .map(|observation| (&observation.path, &observation.features))
        .collect();
    assert_eq!(replayed_features, truncated_features);
    assert!(truncated.labels.defect_commits_by_path.is_empty());
    assert!(!replayed.labels.defect_commits_by_path.is_empty());
}

#[test]
fn the_tree_read_at_the_cut_point_ignores_later_content() {
    const HISTORY: &[CommitSpec] = &[
        CommitSpec::new(0, "Initial implementation", &[("src/original.rs", QUIET_BODY)]),
        CommitSpec::new(10, "feat: still here later", &[("src/doomed.rs", QUIET_BODY)]),
        // --- cut point: index 2 ------------------------------------------
        CommitSpec::new(20, "chore: the cut point", &[("src/original.rs", BUSY_BODY)]),
        CommitSpec::new(30, "feat: added after the cut point", &[("src/newcomer.rs", QUIET_BODY)]),
        CommitSpec::removing(40, "chore: delete after the cut point", &[], &["src/doomed.rs"]),
    ];
    let (directory, commits) = fixture(HISTORY);
    let replay = replay_at_commit(directory.path(), &commits[2].to_string(), test_limits())
        .expect("replay at the cut point");

    let observed: Vec<&str> = replay
        .observations
        .iter()
        .map(|observation| observation.path.as_str())
        .collect();
    // Present at the cut point, deleted later: must still be observed.
    assert!(observed.contains(&"src/doomed.rs"));
    assert!(observed.contains(&"src/original.rs"));
    // Created after the cut point: must be invisible.
    assert!(
        !observed.contains(&"src/newcomer.rs"),
        "a file created after the cut point leaked into the tree read"
    );
}

#[test]
fn replaying_never_touches_the_callers_checkout() {
    let (directory, commits) = fixture(LEAKAGE_HISTORY);
    let repository = Repository::open(directory.path()).expect("open fixture");
    let head_before = repository.head().unwrap().target().unwrap();
    let files_before = working_tree_listing(directory.path());
    let dirty_before = repository
        .statuses(None)
        .expect("statuses")
        .iter()
        .count();

    replay_at_commit(
        directory.path(),
        &commits[CUT_POINT_INDEX].to_string(),
        test_limits(),
    )
    .expect("replay");

    // Reading history at a cut point must never check anything out, write an
    // index, or materialize historical content on disk.
    assert_eq!(repository.head().unwrap().target().unwrap(), head_before);
    assert_eq!(working_tree_listing(directory.path()), files_before);
    assert_eq!(
        repository.statuses(None).expect("statuses").iter().count(),
        dirty_before
    );
}

/// Sorted listing of every file in the working tree, excluding `.git`.
fn working_tree_listing(root: &Path) -> Vec<String> {
    fn walk(root: &Path, directory: &Path, into: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
            if name == ".git" {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, into);
            } else {
                let relative: PathBuf = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                into.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut listing = Vec::new();
    walk(root, root, &mut listing);
    listing.sort();
    listing
}

#[test]
fn replaying_the_same_history_twice_produces_identical_output() {
    let (directory, commits) = fixture(LEAKAGE_HISTORY);
    let first = replay_at_commit(
        directory.path(),
        &commits[CUT_POINT_INDEX].to_string(),
        test_limits(),
    )
    .expect("first replay");
    let second = replay_at_commit(
        directory.path(),
        &commits[CUT_POINT_INDEX].to_string(),
        test_limits(),
    )
    .expect("second replay");

    assert_eq!(first.git, second.git);
    assert_eq!(first.graph, second.graph);
    assert_eq!(first.complexity, second.complexity);
    assert_eq!(first.labels, second.labels);
    assert_eq!(first.tree, second.tree);
    assert_eq!(first.observations, second.observations);
}

#[test]
fn observations_are_ordered_by_path_so_output_is_stable() {
    let (directory, commits) = fixture(LEAKAGE_HISTORY);
    let replay = replay_at_commit(
        directory.path(),
        &commits[CUT_POINT_INDEX].to_string(),
        test_limits(),
    )
    .expect("replay");
    let paths: Vec<&str> = replay
        .observations
        .iter()
        .map(|observation| observation.path.as_str())
        .collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(paths, sorted);
}

#[test]
fn the_horizon_commit_cap_cuts_a_real_history_at_the_documented_edge() {
    // Two commits after the cut point; a cap of one admits only the nearer.
    const HISTORY: &[CommitSpec] = &[
        CommitSpec::new(0, "Initial implementation", &[("src/a.rs", QUIET_BODY), ("src/b.rs", QUIET_BODY)]),
        CommitSpec::new(10, "chore: the cut point", &[("src/a.rs", BUSY_BODY)]),
        CommitSpec::new(20, "fix: inside the cap", &[("src/a.rs", QUIET_BODY)]),
        CommitSpec::new(30, "fix: outside the cap", &[("src/b.rs", BUSY_BODY)]),
    ];
    let (directory, commits) = fixture(HISTORY);
    let limits = ReplayLimits {
        cut_points: 1,
        mining_reserve: 1,
        horizon: HorizonLimits {
            max_commits: 1,
            max_days: 90,
        },
        ..ReplayLimits::default()
    };
    let replay =
        replay_at_commit(directory.path(), &commits[1].to_string(), limits).expect("replay");

    assert_eq!(replay.labels.report.selected_commits, 1);
    assert!(replay.labels.report.truncated_by_commit_cap);
    assert!(label(&replay, "src/a.rs"));
    assert!(!label(&replay, "src/b.rs"));
}

#[test]
fn the_horizon_day_cap_cuts_a_real_history_at_the_documented_edge() {
    // The fix lands 91 days after the cut point, one day past the bound.
    const HISTORY: &[CommitSpec] = &[
        CommitSpec::new(0, "Initial implementation", &[("src/a.rs", QUIET_BODY), ("src/b.rs", QUIET_BODY)]),
        CommitSpec::new(10, "chore: the cut point", &[("src/a.rs", BUSY_BODY)]),
        CommitSpec::new(100, "fix: exactly on the ninety day bound", &[("src/a.rs", QUIET_BODY)]),
        CommitSpec::new(101, "fix: one day past the bound", &[("src/b.rs", BUSY_BODY)]),
    ];
    let (directory, commits) = fixture(HISTORY);
    let replay = replay_at_commit(directory.path(), &commits[1].to_string(), test_limits())
        .expect("replay");

    assert_eq!(replay.labels.report.selected_commits, 1);
    assert!(replay.labels.report.truncated_by_day_cap);
    assert!(label(&replay, "src/a.rs"), "the commit on the bound counts");
    assert!(
        !label(&replay, "src/b.rs"),
        "the commit past the bound does not"
    );
}

#[test]
fn multiple_cut_points_are_placed_across_a_real_history() {
    let (directory, _) = fixture(LEAKAGE_HISTORY);
    let replay = replay_repository(
        directory.path(),
        ReplayLimits {
            cut_points: 3,
            mining_reserve: 1,
            horizon: HorizonLimits {
                max_commits: 2,
                max_days: 90,
            },
            ..ReplayLimits::default()
        },
    )
    .expect("replay the repository");

    assert!(replay.cut_points.len() > 1);
    assert_eq!(
        replay.report.cut_points_placed as usize,
        replay.cut_points.len()
    );
    assert_eq!(replay.report.spine_length, LEAKAGE_HISTORY.len() as u32);
    // Distinct commits, ordered from newest to oldest.
    let indices: Vec<usize> = replay
        .cut_points
        .iter()
        .map(|cut_point| cut_point.spine_index)
        .collect();
    let mut sorted = indices.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(indices, sorted);
    assert_eq!(replay.name, directory.path().file_name().unwrap().to_str().unwrap());
}

#[test]
fn a_history_too_short_to_place_a_cut_point_is_refused_rather_than_guessed() {
    const HISTORY: &[CommitSpec] = &[CommitSpec::new(
        0,
        "Initial implementation",
        &[("src/a.rs", QUIET_BODY)],
    )];
    let (directory, _) = fixture(HISTORY);
    let error = replay_repository(
        directory.path(),
        ReplayLimits {
            cut_points: 6,
            mining_reserve: 10,
            ..ReplayLimits::default()
        },
    )
    .expect_err("a one-commit history cannot host a cut point");
    assert!(matches!(error, ReplayError::HistoryTooShort { .. }), "{error}");
}

#[test]
fn an_empty_repository_is_refused() {
    let directory = tempfile::tempdir().expect("temp directory");
    Repository::init(directory.path()).expect("initialize empty repository");
    let error = replay_repository(directory.path(), test_limits())
        .expect_err("an empty repository cannot be replayed");
    assert!(matches!(error, ReplayError::EmptyHistory { .. }), "{error}");
}

#[test]
fn a_commit_off_the_first_parent_spine_is_refused() {
    let (directory, commits) = fixture(LEAKAGE_HISTORY);
    let repository = Repository::open(directory.path()).expect("open fixture");
    // A commit on a side branch, never merged, so it is not on the spine.
    let parent = repository
        .find_commit(commits[0])
        .expect("root commit");
    let signature = Signature::new(
        "Fixture Author",
        "fixture@example.test",
        &Time::new(BASE_TIME, 0),
    )
    .expect("signature");
    let side = repository
        .commit(
            None,
            &signature,
            &signature,
            "chore: off the spine",
            &parent.tree().expect("tree"),
            &[&parent],
        )
        .expect("side commit");

    let error = replay_at_commit(directory.path(), &side.to_string(), test_limits())
        .expect_err("a commit off the spine has no well-defined horizon");
    assert!(matches!(error, ReplayError::CutPointOffSpine { .. }), "{error}");
}

#[test]
fn a_file_present_but_untouched_in_the_window_reports_known_zeros_not_unknowns() {
    let (directory, commits) = fixture(LEAKAGE_HISTORY);
    let replay = replay_at_commit(
        directory.path(),
        &commits[CUT_POINT_INDEX].to_string(),
        test_limits(),
    )
    .expect("replay");

    // `late_break.rs` exists at the cut point and was touched once, at the root
    // commit, so it has real counts.
    assert_eq!(
        feature(&replay, "src/late_break.rs", FeatureKind::HotspotScore),
        Some(1)
    );
    // Ratio features have no denominator for a file with no fix history, and
    // must stay unknown rather than being reported as a measured zero.
    assert_eq!(
        feature(&replay, "src/late_break.rs", FeatureKind::BugFixDensity),
        Some(0),
        "density over a real commit window is a measured zero"
    );
}

#[test]
fn tree_read_accounting_reports_a_complete_read_for_a_clean_fixture() {
    let (directory, commits) = fixture(LEAKAGE_HISTORY);
    let replay = replay_at_commit(
        directory.path(),
        &commits[CUT_POINT_INDEX].to_string(),
        test_limits(),
    )
    .expect("replay");
    assert!(replay.tree.is_complete(), "{:?}", replay.tree);
    assert_eq!(replay.tree.files_read, 3);
    assert_eq!(replay.tree.files_parsed, 3);
    assert!(replay.graph.report.index_complete);
}

#[test]
fn an_oversized_blob_is_excluded_and_counted_rather_than_silently_dropped() {
    let (directory, commits) = fixture(LEAKAGE_HISTORY);
    let replay = replay_at_commit(
        directory.path(),
        &commits[CUT_POINT_INDEX].to_string(),
        ReplayLimits {
            max_blob_bytes: 10,
            ..test_limits()
        },
    )
    .expect("replay");
    assert!(replay.tree.oversized_blobs > 0);
    assert!(!replay.tree.is_complete());
    // An incomplete tree read must be visible in the graph facts, not hidden.
    assert!(!replay.graph.report.index_complete);
}

#[test]
fn the_tree_file_cap_is_enforced_and_counted() {
    let (directory, commits) = fixture(LEAKAGE_HISTORY);
    let replay = replay_at_commit(
        directory.path(),
        &commits[CUT_POINT_INDEX].to_string(),
        ReplayLimits {
            max_tree_files: 1,
            ..test_limits()
        },
    )
    .expect("replay");
    assert_eq!(replay.tree.files_read, 1);
    assert_eq!(replay.tree.files_over_cap, 2);
    assert!(!replay.tree.is_complete());
}

#[test]
fn limits_are_clamped_to_their_ceilings() {
    let bounded = ReplayLimits {
        cut_points: usize::MAX,
        spine_limit: usize::MAX,
        max_tree_files: 0,
        max_blob_bytes: 0,
        ..ReplayLimits::default()
    }
    .bounded();
    assert_eq!(bounded.cut_points, MAX_CUT_POINTS);
    assert_eq!(bounded.spine_limit, MAX_SPINE_LIMIT);
    assert_eq!(bounded.max_tree_files, 1);
    assert_eq!(bounded.max_blob_bytes, 1);
}

#[test]
fn a_short_history_reduces_its_reserves_and_says_so() {
    let (directory, _) = fixture(LEAKAGE_HISTORY);
    // The default 200-commit horizon reserve cannot fit in a nine-commit spine.
    let replay = replay_repository(directory.path(), ReplayLimits::default())
        .expect("replay a short history");
    assert!(replay.report.reserves_reduced);
    assert!(replay.report.horizon_reserve < super::super::labels::DEFAULT_HORIZON_COMMITS as u32);
    assert!(!replay.cut_points.is_empty());
}
