//! H1.2: auditing the quality of the harness's ground-truth labels.
//!
//! Every precision and recall figure the harness reports rests on
//! [`crate::git_intelligence::looks_like_bug_fix`] — a subject-prefix
//! heuristic that had never been measured. Before those figures can be
//! believed, the classifier itself has to be checked, so this module does two
//! things the spec asks for:
//!
//! 1. **A labeled sample for human spot-review** (default 50 subjects, half
//!    classified fix and half not), emitted into the report so a reader can
//!    judge the vocabulary directly instead of trusting a statistic about it.
//! 2. **Agreement against an independent signal**: commits whose diff touches
//!    both an existing test file and a production file.
//!
//! # Reading the agreement figures honestly
//!
//! The two signals do not measure the same thing, and a reader who treats
//! them as two raters of one label will draw the wrong conclusion. Plenty of
//! real fixes ship without touching a test, and plenty of features touch both
//! a test and production code. Raw agreement and Cohen's kappa between them
//! are therefore expected to be low even when the classifier is working, and
//! **a low kappa here is not by itself evidence that the vocabulary is bad**.
//!
//! The figure that does carry evidence is *enrichment*: whether commits the
//! classifier calls fixes are more likely to co-modify tests and production
//! code than commits in general. If fix-shaped commits are no more likely than
//! any other commit to look like repair work by this independent measure, the
//! classifier is not tracking anything real and the vocabulary needs
//! tightening. If they are meaningfully enriched, two unrelated signals are
//! pointing the same way, which is what corroboration means.
//!
//! [`AuditVerdict`] encodes exactly that bar, and all of raw agreement, kappa,
//! and enrichment are reported so a reader can disagree with the bar and
//! recompute their own.

use serde::{Deserialize, Serialize};

use crate::git_intelligence::looks_like_bug_fix;
use crate::query::engine::is_test_file;

/// Default number of subjects emitted for human spot-review.
pub const DEFAULT_SAMPLE_SIZE: usize = 50;

/// Enrichment at or below which the classifier is judged uncorroborated: its
/// fix-shaped commits are no more likely to co-modify tests than any commit.
pub const UNCORROBORATED_ENRICHMENT_PER_MILLE: u32 = 1_000;

/// Enrichment at or above which the classifier is judged corroborated: its
/// fix-shaped commits co-modify tests at least 20% more often than the base
/// rate. Twenty percent is a deliberately modest bar — the independent signal
/// is a weak proxy, so demanding a large effect from it would be demanding
/// evidence it cannot supply.
pub const CORROBORATED_ENRICHMENT_PER_MILLE: u32 = 1_200;

/// One commit reduced to what the audit needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditInput {
    /// Commit object id.
    pub id: String,
    /// Commit summary line.
    pub subject: String,
    /// Repository-relative paths the commit touched.
    pub paths: Vec<String>,
}

/// One sampled commit, for human spot-review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditSample {
    /// Abbreviated commit id.
    pub commit: String,
    /// The subject the classifier judged.
    pub subject: String,
    /// What `looks_like_bug_fix` decided.
    pub classified_fix: bool,
    /// Whether the commit touched both a test file and a production file.
    pub touches_test_and_production: bool,
}

/// The audit's judgement on whether the classifier is corroborated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditVerdict {
    /// Fix-shaped commits co-modify tests and production materially more often
    /// than commits in general. Two unrelated signals agree.
    Corroborated,
    /// Enriched, but only slightly. The classifier is tracking something, but
    /// the corroboration is thin enough that the report says so.
    Weak,
    /// No enrichment at all: fix-shaped commits look no different from any
    /// other commit by the independent measure. The vocabulary needs work.
    Uncorroborated,
    /// The audit could not run — too few commits, or no commit of one class.
    Unavailable,
}

impl AuditVerdict {
    /// Stable identifier for report text and JSON.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Corroborated => "corroborated",
            Self::Weak => "weak",
            Self::Uncorroborated => "uncorroborated",
            Self::Unavailable => "unavailable",
        }
    }

    /// Whether the measurement authorizes tightening the classifier's
    /// vocabulary in this change (spec H1.2).
    pub fn warrants_tightening(&self) -> bool {
        matches!(self, Self::Uncorroborated)
    }
}

/// The complete label-quality audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelAudit {
    /// Commits examined.
    pub commits_considered: u32,
    /// Commits `looks_like_bug_fix` classified as fixes.
    pub classified_fix: u32,
    /// Commits touching both a test file and a production file.
    pub touches_test_and_production: u32,
    /// Classified fix *and* co-modifying.
    pub both: u32,
    /// Classified fix but not co-modifying.
    pub fix_only: u32,
    /// Co-modifying but not classified fix.
    pub signal_only: u32,
    /// Neither.
    pub neither: u32,
    /// Share of commits on which the two signals agree, per-mille.
    pub percent_agreement_per_mille: u32,
    /// Cohen's kappa in per-mille; negative when agreement is worse than
    /// chance.
    pub cohen_kappa_per_mille: i32,
    /// Share of commits classified fix, per-mille.
    pub fix_rate_per_mille: u32,
    /// Share of commits co-modifying tests and production, per-mille.
    pub co_modification_rate_per_mille: u32,
    /// Co-modification rate among fix-shaped commits, per-mille.
    pub co_modification_given_fix_per_mille: u32,
    /// Co-modification rate among all other commits, per-mille.
    pub co_modification_given_non_fix_per_mille: u32,
    /// `co_modification_given_fix / co_modification_rate`, per-mille: 1000
    /// means fix-shaped commits are indistinguishable from any other commit by
    /// the independent signal.
    pub enrichment_per_mille: u32,
    /// The verdict, and with it whether tightening is warranted.
    pub verdict: AuditVerdict,
    /// Subjects for human spot-review.
    pub sample: Vec<AuditSample>,
}

/// Round `numerator / denominator` to the nearest integer, halves up.
fn round_div(numerator: u128, denominator: u128) -> u128 {
    if denominator == 0 {
        return 0;
    }
    (numerator * 2 + denominator) / (denominator * 2)
}

/// `numerator / denominator` in per-mille, rounded half up.
fn per_mille(numerator: u64, denominator: u64) -> u32 {
    if denominator == 0 {
        return 0;
    }
    round_div(u128::from(numerator) * 1000, u128::from(denominator)) as u32
}

/// Whether a commit touched both an existing test file and a production file.
///
/// This is the independent signal: it reads only the diff's paths, shares no
/// input with the subject-line classifier, and so can corroborate it or fail
/// to. A commit that touches only tests, or only production code, is not
/// evidence either way and counts as "not co-modifying".
pub fn touches_test_and_production(paths: &[String]) -> bool {
    let mut saw_test = false;
    let mut saw_production = false;
    for path in paths {
        if is_test_file(path) {
            saw_test = true;
        } else {
            saw_production = true;
        }
        if saw_test && saw_production {
            return true;
        }
    }
    false
}

/// Audit the fix classifier against the independent co-modification signal.
///
/// `commits` may contain duplicates across cut-point windows; they are deduped
/// by commit id so a commit appearing in several windows is not counted twice.
pub fn audit_labels(commits: &[AuditInput], sample_size: usize) -> LabelAudit {
    let mut seen: Vec<&str> = Vec::new();
    let mut classified: Vec<(&AuditInput, bool, bool)> = Vec::new();
    for commit in commits {
        if seen.binary_search(&commit.id.as_str()).is_ok() {
            continue;
        }
        let position = seen
            .binary_search(&commit.id.as_str())
            .unwrap_or_else(|slot| slot);
        seen.insert(position, commit.id.as_str());
        classified.push((
            commit,
            looks_like_bug_fix(&commit.subject),
            touches_test_and_production(&commit.paths),
        ));
    }
    // Deterministic order independent of which window yielded a commit first.
    classified.sort_by(|left, right| left.0.id.cmp(&right.0.id));

    let total = classified.len() as u64;
    let mut both = 0u64;
    let mut fix_only = 0u64;
    let mut signal_only = 0u64;
    let mut neither = 0u64;
    for (_, is_fix, co_modifies) in &classified {
        match (is_fix, co_modifies) {
            (true, true) => both += 1,
            (true, false) => fix_only += 1,
            (false, true) => signal_only += 1,
            (false, false) => neither += 1,
        }
    }

    let classified_fix = both + fix_only;
    let co_modifying = both + signal_only;
    let non_fix = total - classified_fix;

    let co_modification_rate_per_mille = per_mille(co_modifying, total);
    let co_modification_given_fix_per_mille = per_mille(both, classified_fix);
    let enrichment_per_mille = per_mille(
        u64::from(co_modification_given_fix_per_mille),
        u64::from(co_modification_rate_per_mille),
    );

    let verdict = if total < 2 || classified_fix == 0 || non_fix == 0 || co_modifying == 0 {
        AuditVerdict::Unavailable
    } else if enrichment_per_mille >= CORROBORATED_ENRICHMENT_PER_MILLE {
        AuditVerdict::Corroborated
    } else if enrichment_per_mille > UNCORROBORATED_ENRICHMENT_PER_MILLE {
        AuditVerdict::Weak
    } else {
        AuditVerdict::Uncorroborated
    };

    LabelAudit {
        commits_considered: total as u32,
        classified_fix: classified_fix as u32,
        touches_test_and_production: co_modifying as u32,
        both: both as u32,
        fix_only: fix_only as u32,
        signal_only: signal_only as u32,
        neither: neither as u32,
        percent_agreement_per_mille: per_mille(both + neither, total),
        cohen_kappa_per_mille: cohen_kappa_per_mille(both, fix_only, signal_only, neither),
        fix_rate_per_mille: per_mille(classified_fix, total),
        co_modification_rate_per_mille,
        co_modification_given_fix_per_mille,
        co_modification_given_non_fix_per_mille: per_mille(signal_only, non_fix),
        enrichment_per_mille,
        verdict,
        sample: build_sample(&classified, sample_size),
    }
}

/// Cohen's kappa over a 2x2 table, in per-mille.
///
/// `kappa = (observed - expected) / (1 - expected)`, computed with integer
/// arithmetic by scaling both terms by the squared population.
fn cohen_kappa_per_mille(both: u64, fix_only: u64, signal_only: u64, neither: u64) -> i32 {
    let total = both + fix_only + signal_only + neither;
    if total == 0 {
        return 0;
    }
    let total = i128::from(total);
    let observed = i128::from(both + neither) * total;
    // Marginals: (fix, co-modifying) and their complements.
    let fix = i128::from(both + fix_only);
    let co_modifying = i128::from(both + signal_only);
    let expected = fix * co_modifying + (total - fix) * (total - co_modifying);
    let denominator = total * total - expected;
    if denominator == 0 {
        // Perfect expected agreement: kappa is undefined, and reporting 1000
        // would claim a measurement that was not made.
        return 0;
    }
    let numerator = (observed - expected) * 1000;
    // Round half away from zero so a negative kappa rounds symmetrically.
    let half = denominator.abs() / 2;
    let adjusted = if numerator >= 0 {
        numerator + half
    } else {
        numerator - half
    };
    (adjusted / denominator) as i32
}

/// Choose the spot-review sample: half classified fix, half not.
///
/// Selection is evenly spaced across each class's commits in commit-id order,
/// so the sample spans the population rather than clustering, and is identical
/// on every run.
fn build_sample(
    classified: &[(&AuditInput, bool, bool)],
    sample_size: usize,
) -> Vec<AuditSample> {
    let wanted_per_class = sample_size / 2;
    let mut sample = Vec::new();
    for class in [true, false] {
        let members: Vec<&(&AuditInput, bool, bool)> = classified
            .iter()
            .filter(|(_, is_fix, _)| *is_fix == class)
            .collect();
        if members.is_empty() || wanted_per_class == 0 {
            continue;
        }
        let take = wanted_per_class.min(members.len());
        for step in 0..take {
            // Evenly spaced positions across the class.
            let position = (step * members.len()) / take;
            let (commit, is_fix, co_modifies) = members[position];
            sample.push(AuditSample {
                commit: abbreviate(&commit.id),
                subject: commit.subject.clone(),
                classified_fix: *is_fix,
                touches_test_and_production: *co_modifies,
            });
        }
    }
    sample
}

/// Shorten a commit id for report text.
fn abbreviate(id: &str) -> String {
    id.chars().take(10).collect()
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
