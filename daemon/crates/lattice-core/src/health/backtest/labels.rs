//! Pure horizon selection and defect labeling for the backtest harness.
//!
//! Ground truth for a cut point `T` is: *a file is defective if a fix-shaped
//! commit touched it in the bounded horizon after `T`*. This module owns the
//! two decisions that definition hides — which commits are in the horizon, and
//! which of them count as fix-shaped — and owns them as pure functions over
//! already-read commit records so the boundary rules are testable without a
//! repository.
//!
//! The classifier itself is [`crate::git_intelligence::looks_like_bug_fix`],
//! deliberately reused rather than reimplemented: the harness must audit the
//! same function production uses, or its precision numbers describe something
//! that never runs (see [`super::audit`]).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::git_intelligence::{canonical_repository_path, looks_like_bug_fix};

/// Seconds in a day, for horizon deadline arithmetic.
const SECONDS_PER_DAY: i64 = 86_400;

/// Default horizon: the next 90 days or 200 commits, whichever comes first
/// (spec H1.1).
pub const DEFAULT_HORIZON_DAYS: u32 = 90;
/// Default horizon commit cap (spec H1.1).
pub const DEFAULT_HORIZON_COMMITS: usize = 200;

/// Hard ceiling on the horizon commit cap, mirroring the git adapter's
/// discipline of clamping every caller-supplied bound.
pub const MAX_HORIZON_COMMITS: usize = 2_000;
/// Hard ceiling on the horizon day cap.
pub const MAX_HORIZON_DAYS: u32 = 3_650;

/// One commit on the first-parent spine, as read by the replay adapter.
///
/// Ordering is by increasing distance from the cut point: `distance == 1` is
/// the first commit after `T`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HorizonCommit {
    /// Commit object id.
    pub id: String,
    /// Committer timestamp in seconds since the epoch. Committer time, not
    /// author time: it reflects when the change entered this history, which is
    /// what a horizon measures.
    pub committed_at_seconds: i64,
    /// Commit summary line.
    pub subject: String,
    /// Repository-relative paths the commit touched, first-parent diff only.
    pub changed_paths: Vec<String>,
    /// The commit exceeded the per-commit path cap and must be excluded from
    /// labeling exactly as the miner excludes it from hotspots.
    pub path_overflow: bool,
}

/// Bounds on the post-cut-point horizon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HorizonLimits {
    /// Maximum number of commits admitted after the cut point.
    pub max_commits: usize,
    /// Maximum age, in days, of an admitted commit relative to the cut point.
    pub max_days: u32,
}

impl Default for HorizonLimits {
    fn default() -> Self {
        Self {
            max_commits: DEFAULT_HORIZON_COMMITS,
            max_days: DEFAULT_HORIZON_DAYS,
        }
    }
}

impl HorizonLimits {
    /// Clamp caller-supplied bounds to the hard ceilings.
    ///
    /// A zero cap is raised to one rather than accepted: a horizon of nothing
    /// would silently label every file clean, which is exactly the "unknown
    /// scored as zero" failure the spec forbids.
    pub fn bounded(self) -> Self {
        Self {
            max_commits: self.max_commits.clamp(1, MAX_HORIZON_COMMITS),
            max_days: self.max_days.clamp(1, MAX_HORIZON_DAYS),
        }
    }
}

/// Accounting for what the horizon admitted, excluded, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HorizonReport {
    /// The bounds actually applied, after clamping.
    pub limits: HorizonLimits,
    /// Committer timestamp of the cut-point commit.
    pub cut_point_committed_at_seconds: i64,
    /// Latest committer timestamp an admitted commit may carry, inclusive.
    pub deadline_seconds: i64,
    /// Commits offered to the selector, before any bound was applied.
    pub candidates_available: u32,
    /// Commits admitted to the horizon.
    pub selected_commits: u32,
    /// The commit cap stopped the horizon before the candidates ran out.
    pub truncated_by_commit_cap: bool,
    /// The day cap stopped the horizon before the candidates ran out.
    pub truncated_by_day_cap: bool,
    /// Admitted commits classified fix-shaped by `looks_like_bug_fix`.
    pub fix_shaped_commits: u32,
    /// Admitted commits excluded from labeling for exceeding the path cap.
    pub path_overflow_commits: u32,
    /// Admitted commits whose committer time precedes the cut point's, which
    /// only happens with rewritten or clock-skewed history. Recorded because a
    /// day-bounded horizon over skewed timestamps is a degraded measurement,
    /// not a clean one.
    pub clock_skew_commits: u32,
    /// Path strings rejected by canonicalization while labeling.
    pub invalid_path_entries: u32,
}

impl HorizonReport {
    /// Whether the horizon ran to the full extent of available history without
    /// hitting either bound.
    pub fn is_exhaustive(&self) -> bool {
        !self.truncated_by_commit_cap && !self.truncated_by_day_cap
    }

    /// Whether any accounting counter indicates a degraded measurement.
    pub fn is_degraded(&self) -> bool {
        self.path_overflow_commits > 0
            || self.clock_skew_commits > 0
            || self.invalid_path_entries > 0
    }
}

/// The commits admitted to a horizon, plus the accounting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HorizonSelection {
    /// Admitted commits, ordered by increasing distance from the cut point.
    pub commits: Vec<HorizonCommit>,
    /// What was admitted and excluded.
    pub report: HorizonReport,
}

/// Ground-truth labels derived from a horizon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefectLabels {
    /// Canonical paths touched by at least one fix-shaped horizon commit,
    /// each with the number of such commits.
    pub defect_commits_by_path: BTreeMap<String, u32>,
    /// What the horizon admitted and excluded.
    pub report: HorizonReport,
}

impl DefectLabels {
    /// Whether a path carries a positive label.
    pub fn is_defective(&self, canonical_path: &str) -> bool {
        self.defect_commits_by_path.contains_key(canonical_path)
    }

    /// Number of distinct fix-shaped horizon commits touching a path.
    pub fn defect_commits(&self, canonical_path: &str) -> u32 {
        self.defect_commits_by_path
            .get(canonical_path)
            .copied()
            .unwrap_or(0)
    }

    /// The set of paths carrying a positive label.
    pub fn defective_paths(&self) -> BTreeSet<&str> {
        self.defect_commits_by_path
            .keys()
            .map(String::as_str)
            .collect()
    }
}

/// Select the horizon after a cut point.
///
/// `candidates` must already be ordered by increasing distance from the cut
/// point along the first-parent spine, and must contain only commits that
/// descend from it — the adapter guarantees both by walking the spine.
///
/// # Boundary semantics
///
/// Both bounds are inclusive of the boundary element and exclusive of the one
/// past it:
///
/// * **Commit cap** — commit number `max_commits` is admitted; number
///   `max_commits + 1` is not.
/// * **Day cap** — a commit at exactly `cut_point + max_days` days is
///   admitted; one second later is not.
///
/// "Whichever comes first" is enforced by walking in distance order and
/// stopping at the first bound that trips. The day cap *stops* the walk rather
/// than skipping the offending commit: a horizon with holes in it is not a
/// horizon, and a contiguous window is what the label definition means. When
/// rewritten history makes timestamps non-monotonic this can truncate early,
/// which is why `clock_skew_commits` is reported rather than smoothed over.
pub fn select_horizon(
    cut_point_committed_at_seconds: i64,
    candidates: Vec<HorizonCommit>,
    limits: HorizonLimits,
) -> HorizonSelection {
    let limits = limits.bounded();
    let deadline_seconds =
        cut_point_committed_at_seconds.saturating_add(i64::from(limits.max_days) * SECONDS_PER_DAY);
    let candidates_available = candidates.len() as u32;

    let mut commits = Vec::new();
    let mut truncated_by_commit_cap = false;
    let mut truncated_by_day_cap = false;
    let mut fix_shaped_commits = 0u32;
    let mut path_overflow_commits = 0u32;
    let mut clock_skew_commits = 0u32;

    for candidate in candidates {
        if commits.len() >= limits.max_commits {
            truncated_by_commit_cap = true;
            break;
        }
        if candidate.committed_at_seconds > deadline_seconds {
            truncated_by_day_cap = true;
            break;
        }
        if candidate.committed_at_seconds < cut_point_committed_at_seconds {
            clock_skew_commits += 1;
        }
        if candidate.path_overflow {
            path_overflow_commits += 1;
        }
        if looks_like_bug_fix(&candidate.subject) {
            fix_shaped_commits += 1;
        }
        commits.push(candidate);
    }

    let selected_commits = commits.len() as u32;
    HorizonSelection {
        commits,
        report: HorizonReport {
            limits,
            cut_point_committed_at_seconds,
            deadline_seconds,
            candidates_available,
            selected_commits,
            truncated_by_commit_cap,
            truncated_by_day_cap,
            fix_shaped_commits,
            path_overflow_commits,
            clock_skew_commits,
            invalid_path_entries: 0,
        },
    }
}

/// Label every path touched by a fix-shaped commit inside the horizon.
///
/// Path-overflow commits contribute nothing, matching
/// [`crate::git_intelligence::GitHistoryMiner::mine`]: a commit too wide to
/// attribute to any file is too wide to blame one.
pub fn label_defects(selection: &HorizonSelection) -> DefectLabels {
    let mut defect_commits_by_path: BTreeMap<String, u32> = BTreeMap::new();
    let mut invalid_path_entries = 0u32;

    for commit in &selection.commits {
        if commit.path_overflow || !looks_like_bug_fix(&commit.subject) {
            continue;
        }
        // Dedupe within a commit so a rename recorded on both sides cannot
        // count the same commit twice against one path.
        let mut counted: BTreeSet<String> = BTreeSet::new();
        for path in &commit.changed_paths {
            match canonical_repository_path(path) {
                Some(canonical) => {
                    counted.insert(canonical);
                }
                None => invalid_path_entries += 1,
            }
        }
        for path in counted {
            *defect_commits_by_path.entry(path).or_insert(0) += 1;
        }
    }

    let mut report = selection.report.clone();
    report.invalid_path_entries = invalid_path_entries;
    DefectLabels {
        defect_commits_by_path,
        report,
    }
}

/// Choose evenly spaced cut points along a first-parent spine.
///
/// `spine_length` is the number of commits on the spine, indexed 0 (HEAD) to
/// `spine_length - 1` (root). A cut point needs history behind it to mine and
/// history ahead of it to label, so the usable band excludes `reserve_before`
/// commits at the root end and `reserve_after` at the HEAD end.
///
/// Returned indices are ascending (nearest HEAD first) and always distinct;
/// when the band cannot accommodate `count` distinct points, fewer are
/// returned rather than duplicates being emitted.
pub fn cut_point_indices(
    spine_length: usize,
    count: usize,
    reserve_after: usize,
    reserve_before: usize,
) -> Vec<usize> {
    if count == 0 || spine_length <= reserve_after + reserve_before {
        return Vec::new();
    }
    let first = reserve_after;
    let last = spine_length - reserve_before - 1;
    if last < first {
        return Vec::new();
    }
    let span = last - first;
    let mut indices = Vec::new();
    for step in 0..count {
        // Place points at the midpoints of `count` equal sub-intervals so the
        // set is symmetric and never collapses onto the band's endpoints.
        let offset = if count == 1 {
            span / 2
        } else {
            (span * (2 * step + 1)) / (2 * count)
        };
        let index = first + offset;
        if !indices.contains(&index) {
            indices.push(index);
        }
    }
    indices
}

#[cfg(test)]
#[path = "labels_tests.rs"]
mod tests;
