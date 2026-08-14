//! Per-file line-churn health facts (H2.5,
//! `docs/plans/2026-08-13-health-engine.md`).
//!
//! This module wraps [`crate::git_intelligence::GitIntelligenceSnapshot`]'s
//! `lines_added`/`lines_deleted`/`line_churn` file signals — themselves
//! produced by the bounded, deterministic `git2` adapter extension in
//! [`crate::git_intelligence_adapter`] — into a fact type that carries its
//! own availability, per design decision 4 of the health-engine plan
//! ("unknown is never zero"): a file the git window never observed, or a
//! window that had to exclude an over-wide commit, is never reported as a
//! silent zero. It is flagged `degraded` and the reason travels with it.
//!
//! This module is pure: it takes an already-mined snapshot and a list of
//! paths, and returns facts. It performs no I/O, git access, or persistence
//! of its own — that remains the adapter's and the store's job.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::git_intelligence::{canonical_repository_path, GitIntelligenceSnapshot, GitMiningReport};

/// Availability of one persisted health fact.
///
/// Extends the git-intelligence rule ("unknown history is never scored as
/// zero risk") to every fact family, per design decision 4 of
/// `docs/plans/2026-08-13-health-engine.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FactAvailability {
    /// The sampled window fully covers this file; the recorded values are
    /// the complete signal for that window (not necessarily the file's
    /// entire history, which is bounded by the miner's history window).
    Available,
    /// The window is degraded (an over-wide commit had to be excluded from
    /// churn accounting) or this file has no observed commits in the
    /// window. The recorded counts may understate true churn and must not
    /// be read as "no churn happened".
    Degraded,
}

/// Line-churn fact for one repository-relative file.
///
/// `lines_added`, `lines_deleted`, and `line_churn` are raw window-scoped
/// counts, matching the git-intelligence convention of representing hotspot
/// counts as plain integers (`FileHistorySignal::hotspot_score`) rather than
/// a normalized per-mille ratio. All arithmetic that produced these values is
/// integer and deterministic; nothing here uses or persists a float.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineChurnFact {
    /// Canonical, repository-relative, slash-separated path.
    pub path: String,
    pub availability: FactAvailability,
    pub lines_added: u64,
    pub lines_deleted: u64,
    /// `lines_added + lines_deleted`, saturating.
    pub line_churn: u64,
    /// Distinct sampled commits that touched this file in the window. `0`
    /// with `availability: Degraded` means "not observed", which is a
    /// distinct claim from "observed and touched by zero commits" — the
    /// latter cannot happen here since a file only has a signal at all once
    /// at least one commit in the window touches it.
    pub hotspot_score: u32,
    /// Explains why `availability` is `Degraded`. Always `None` when
    /// `availability` is `Available`, always `Some` otherwise.
    pub reason: Option<String>,
}

/// Produces one line-churn fact per requested path, in request order with
/// duplicates (after canonicalization) collapsed to their first occurrence.
///
/// A path that does not canonicalize (see
/// [`crate::git_intelligence::canonical_repository_path`]) cannot correspond
/// to any commit observation and is silently skipped: it is not this
/// module's fact to report, and the caller's canonicalization is the
/// authority on what counts as a valid repository path.
pub fn line_churn_facts<'a, I>(snapshot: &GitIntelligenceSnapshot, paths: I) -> Vec<LineChurnFact>
where
    I: IntoIterator<Item = &'a str>,
{
    let window_reason = churn_window_degradation_reason(&snapshot.report);
    let mut seen = BTreeSet::new();
    let mut facts = Vec::new();
    for path in paths {
        let Some(canonical) = canonical_repository_path(path) else {
            continue;
        };
        if !seen.insert(canonical.clone()) {
            continue;
        }
        facts.push(line_churn_fact(snapshot, &canonical, window_reason.as_deref()));
    }
    facts
}

/// Produces the line-churn fact for exactly one already-canonical path.
///
/// Exposed separately from [`line_churn_facts`] so a caller with a single
/// path of interest (e.g. one diff entry in `impact`) is not forced to
/// allocate a collection, and so the window-degradation reason can be
/// computed once by a caller iterating many snapshots' worth of paths.
pub fn line_churn_fact(
    snapshot: &GitIntelligenceSnapshot,
    canonical_path: &str,
    window_degraded_reason: Option<&str>,
) -> LineChurnFact {
    match snapshot.file(canonical_path) {
        Some(signal) => LineChurnFact {
            path: canonical_path.to_owned(),
            availability: match window_degraded_reason {
                Some(_) => FactAvailability::Degraded,
                None => FactAvailability::Available,
            },
            lines_added: signal.lines_added,
            lines_deleted: signal.lines_deleted,
            line_churn: signal.line_churn,
            hotspot_score: signal.hotspot_score,
            reason: window_degraded_reason.map(str::to_owned),
        },
        None => LineChurnFact {
            path: canonical_path.to_owned(),
            availability: FactAvailability::Degraded,
            lines_added: 0,
            lines_deleted: 0,
            line_churn: 0,
            hotspot_score: 0,
            reason: Some(window_degraded_reason.map(str::to_owned).unwrap_or_else(|| {
                "file has no observed commits in the sampled Git window".to_owned()
            })),
        },
    }
}

/// The window-wide reason line-churn facts must be flagged `degraded`, if
/// any over-wide commit had to be excluded from churn accounting.
///
/// Tied specifically to `path_overflow_commits`: that is the exact counter
/// the miner increments when a commit is excluded from *both* hotspot and
/// churn accounting (H2.5's bound reuses the existing per-commit path cap).
/// Other degradation reasons in [`GitMiningReport::is_degraded`] (symbol or
/// co-change overflow) do not affect file-level line counts and would
/// needlessly widen every file's availability, so they are not included
/// here.
fn churn_window_degradation_reason(report: &GitMiningReport) -> Option<String> {
    if report.path_overflow_commits > 0 {
        Some(format!(
            "{} over-wide commit(s) were excluded from line-churn accounting in this window; \
             recorded counts may understate true churn",
            report.path_overflow_commits
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_intelligence::{CommitSample, GitHistoryMiner, GitMiningLimits, PathChange};

    fn change(path: &str, lines_added: u32, lines_deleted: u32) -> PathChange {
        PathChange {
            path: path.to_owned(),
            symbols: Vec::new(),
            lines_added,
            lines_deleted,
        }
    }

    fn commit(id: &str, changes: Vec<PathChange>) -> CommitSample {
        CommitSample {
            id: id.to_owned(),
            author: Some("author@example.test".to_owned()),
            subject: "feature".to_owned(),
            changes,
        }
    }

    #[test]
    fn available_fact_reports_observed_counts() {
        let snapshot = GitHistoryMiner::default().mine(vec![commit(
            "c1",
            vec![change("src/a.rs", 12, 3)],
        )]);

        let facts = line_churn_facts(&snapshot, ["src/a.rs"]);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].path, "src/a.rs");
        assert_eq!(facts[0].availability, FactAvailability::Available);
        assert_eq!(facts[0].lines_added, 12);
        assert_eq!(facts[0].lines_deleted, 3);
        assert_eq!(facts[0].line_churn, 15);
        assert_eq!(facts[0].hotspot_score, 1);
        assert_eq!(facts[0].reason, None);
    }

    #[test]
    fn file_outside_the_git_window_is_degraded_never_a_silent_zero() {
        let snapshot = GitHistoryMiner::default().mine(vec![commit(
            "c1",
            vec![change("src/a.rs", 5, 1)],
        )]);

        let facts = line_churn_facts(&snapshot, ["src/never-touched.rs"]);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].availability, FactAvailability::Degraded);
        assert_eq!(facts[0].lines_added, 0);
        assert_eq!(facts[0].lines_deleted, 0);
        assert_eq!(facts[0].line_churn, 0);
        assert_eq!(facts[0].hotspot_score, 0);
        assert!(facts[0]
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("no observed commits")));
    }

    #[test]
    fn excluded_over_wide_commit_degrades_every_fact_in_the_window() {
        let miner = GitHistoryMiner::with_limits(GitMiningLimits {
            paths_per_commit: 1,
            ..GitMiningLimits::default()
        });
        // This commit overflows the cap and is excluded from both hotspot
        // and churn accounting; a later commit keeps a normal signal alive
        // for `src/b.rs` so the test can prove degradation applies globally,
        // not just to the excluded commit's own paths.
        let snapshot = miner.mine(vec![
            commit(
                "wide",
                vec![change("src/a.rs", 100, 100), change("src/c.rs", 50, 50)],
            ),
            commit("narrow", vec![change("src/b.rs", 7, 2)]),
        ]);
        assert_eq!(snapshot.report.path_overflow_commits, 1);

        let facts = line_churn_facts(&snapshot, ["src/b.rs", "src/a.rs"]);
        let b = facts.iter().find(|fact| fact.path == "src/b.rs").unwrap();
        assert_eq!(b.availability, FactAvailability::Degraded);
        // The observed counts are still reported (not zeroed out): degraded
        // means "may understate", not "discard the evidence we do have".
        assert_eq!(b.lines_added, 7);
        assert_eq!(b.lines_deleted, 2);
        assert!(b
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("excluded from line-churn accounting")));

        let a = facts.iter().find(|fact| fact.path == "src/a.rs").unwrap();
        assert_eq!(a.availability, FactAvailability::Degraded);
        assert_eq!(a.lines_added, 0, "the excluded commit contributes no signal at all");
    }

    #[test]
    fn requested_paths_deduplicate_after_canonicalization_and_skip_invalid_paths() {
        let snapshot = GitHistoryMiner::default().mine(vec![commit(
            "c1",
            vec![change("src/a.rs", 1, 1)],
        )]);

        let facts = line_churn_facts(&snapshot, ["./src/a.rs", "src\\a.rs", "../outside", "src/a.rs"]);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].path, "src/a.rs");
    }

    #[test]
    fn is_deterministic_across_repeated_calls() {
        let snapshot = GitHistoryMiner::default().mine(vec![commit(
            "c1",
            vec![change("src/a.rs", 3, 3), change("src/b.rs", 4, 0)],
        )]);

        let first = line_churn_facts(&snapshot, ["src/a.rs", "src/b.rs"]);
        let second = line_churn_facts(&snapshot, ["src/a.rs", "src/b.rs"]);
        assert_eq!(first, second);
    }
}
