//! Pure, bounded consumers of the health fact index (Phase H4 of
//! `docs/plans/2026-08-13-health-engine.md`).
//!
//! This module is to [`crate::health::scoring`] what
//! [`crate::git_intelligence_consumers`] is to the git miner: it knows nothing
//! about storage, graph traversal, daemon freshness, or response rendering. A
//! caller supplies an index (or `None`) and a bounded set of paths; every
//! consumer here returns a deterministic, capped, serializable answer.
//!
//! # Why the verbs share one module
//!
//! `impact`, `context`, `prepare_change` and `diagnose` all need the same two
//! things: an ordering key and a renderable evidence bundle. Expressing them
//! once means the four verbs cannot drift into four different notions of what a
//! band means or how many facts an entry cites.
//!
//! # Evidence, never prediction
//!
//! Every string this module produces is descriptive. The backtest that derived
//! the weights is correlational and its ground truth is a heuristic with a
//! measured recall gap (`docs/reports/health-backtest/2026-08-14.md`, § "What
//! H3 may and may not conclude"), so a bundle states what was measured and how
//! it ranks — never that a file will fail. `predictive_language_tests` asserts
//! the forbidden vocabulary stays absent from this module and its callers.
//!
//! # Unknown is never zero
//!
//! A file the index has no facts for is *unknown*, not healthy. Ordering keeps
//! unscored candidates in a trailing group ordered by the caller's own stable
//! key rather than mixing them in as though they had scored low, and every
//! rendered entry carries its availability, its floor/ceiling and the inputs
//! that were missing (spec design decision 4).

use serde::{Deserialize, Serialize};

use crate::health::scoring::{
    Axis, AxisScore, FactAvailability, FactContribution, FactKind, FactSourceRange, FactWindow,
    HealthFactIndex,
};

/// Facts cited per ranked entry. Spec H4.2/H4.3 both say "top 3".
pub const HEALTH_EVIDENCE_FACT_LIMIT: usize = 3;

/// Files carried by one `context`/`prepare_change` health section (spec H4.3
/// default cap).
pub const MAX_HEALTH_SECTION_FILES: usize = 10;

/// Untested-change entries carried by one `impact` response.
pub const MAX_UNTESTED_CHANGE_ENTRIES: usize = 10;

/// One fact as a response renders it.
///
/// `description` is the reader-facing form and is the only field a markdown
/// render needs; the numeric fields let a JSON consumer re-derive the ranking
/// without re-reading the fact producers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthFactEvidence {
    /// Stable fact identifier, e.g. `fan_out`.
    pub kind: String,
    /// Producing family, e.g. `graph`.
    pub family: String,
    /// The producer's raw value, as published.
    pub value: u64,
    /// The fact as a reader should see it, e.g. `fan-out 12 (p880)`.
    pub description: String,
    /// Rank percentile of the raw value within the repository, per-mille.
    pub percentile_per_mille: u32,
    /// Weight this fact carries on the axis, per-mille.
    pub weight_per_mille: u32,
    /// Per-mille of the score this fact accounts for.
    pub contribution_per_mille: u32,
    /// How complete the producing family's facts were.
    pub availability: String,
    /// Whether a committed backtest report measured this fact's weight. A
    /// `false` here is not a defect: it marks a documented provisional weight
    /// so a reader can tell measured evidence from provisional evidence.
    pub backtested: bool,
    /// Where the fact was observed, when it has a location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_range: Option<FactSourceRange>,
    /// The history window, for git-derived facts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<FactWindow>,
}

impl HealthFactEvidence {
    fn from_contribution(contribution: &FactContribution) -> Self {
        Self {
            kind: contribution.kind.as_str().to_string(),
            family: contribution.family.as_str().to_string(),
            value: contribution.value,
            description: contribution.describe(),
            percentile_per_mille: contribution.percentile_per_mille,
            weight_per_mille: contribution.weight_per_mille,
            contribution_per_mille: contribution.contribution_per_mille,
            availability: contribution.availability.as_str().to_string(),
            backtested: contribution.backtested,
            source_range: contribution.source_range.clone(),
            window: contribution.window.clone(),
        }
    }
}

/// One file's score on one axis, in the shape a response carries it.
///
/// Spec design decision 3: never a bare number. `summary` alone is sufficient
/// for a markdown render, and the structured fields carry the same evidence for
/// a JSON consumer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthEvidence {
    /// Workspace-relative path the score describes.
    pub path: String,
    /// Which axis was scored, e.g. `defect_risk`.
    pub axis: String,
    /// Weighted mean of the available facts' percentiles, per-mille.
    pub score_per_mille: u16,
    /// The band, or a band range like `moderate-critical` when inputs were
    /// missing, or `unknown` when there were none.
    pub band: String,
    /// Score with every missing input at its lowest possible value.
    pub score_floor_per_mille: u16,
    /// Score with every missing input at its highest possible value.
    pub score_ceiling_per_mille: u16,
    /// Whether the missing inputs could not change the band.
    pub exact: bool,
    /// Worst availability of any family the score drew on.
    pub availability: String,
    /// The heaviest contributing facts, most first.
    pub facts: Vec<HealthFactEvidence>,
    /// Inputs to this axis that had no value, in canonical fact order.
    pub inputs_missing: Vec<String>,
    /// One line a reader can act on without reading the structured fields.
    pub summary: String,
    /// Version of the weight table the score was computed under.
    pub weights_version: u32,
    /// Version of the health config the facts were produced under.
    pub config_version: u32,
}

impl HealthEvidence {
    /// Render an [`AxisScore`] for one path, citing at most `fact_limit` facts.
    pub fn from_score(path: &str, score: &AxisScore, fact_limit: usize) -> Self {
        Self {
            path: path.to_string(),
            axis: score.axis.as_str().to_string(),
            score_per_mille: score.score_per_mille,
            band: score.band_label(),
            score_floor_per_mille: score.score_floor_per_mille,
            score_ceiling_per_mille: score.score_ceiling_per_mille,
            exact: score.is_exact(),
            availability: score.availability.as_str().to_string(),
            facts: score
                .top_facts(fact_limit)
                .iter()
                .map(HealthFactEvidence::from_contribution)
                .collect(),
            inputs_missing: score
                .inputs_missing
                .iter()
                .map(|kind| kind.as_str().to_string())
                .collect(),
            summary: score.summary(fact_limit),
            weights_version: score.weights_version,
            config_version: score.config_version,
        }
    }

    /// Whether the score drew on no facts at all.
    pub fn is_unknown(&self) -> bool {
        self.availability == FactAvailability::Unavailable.as_str()
    }
}

/// Score one path on one axis, or `None` when there is no index at all.
///
/// A present index always answers: a path it never saw scores `unavailable`,
/// which is a fact about the index and is reported as one.
pub fn axis_evidence(
    health: Option<&HealthFactIndex>,
    path: &str,
    axis: Axis,
    fact_limit: usize,
) -> Option<HealthEvidence> {
    let health = health?;
    Some(HealthEvidence::from_score(
        path,
        &health.score(path, axis),
        fact_limit,
    ))
}

/// Score one path's `defect_risk`, citing the top three facts.
pub fn defect_risk_evidence(
    health: Option<&HealthFactIndex>,
    path: &str,
) -> Option<HealthEvidence> {
    axis_evidence(
        health,
        path,
        Axis::DefectRisk,
        HEALTH_EVIDENCE_FACT_LIMIT,
    )
}

/// A graph-derived impact candidate plus the stable keys required to order it.
///
/// Mirrors [`crate::git_intelligence_consumers::ImpactCandidate`]: call this
/// only within one caller-defined graph-distance tier, and `stable_key` is the
/// caller's existing deterministic final tie-break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthImpactCandidate<T> {
    pub candidate: T,
    pub stable_key: String,
    pub file_path: Option<String>,
}

/// Orders candidates inside one caller-defined graph-distance tier by
/// `defect_risk`, highest first.
///
/// This supersedes the hotspot tie-break for `impact`
/// (`docs/architecture/2026-08-13-git-intelligence.md`, § "Consumer contracts →
/// impact"): history-derived facts may now contribute to the primary ordering,
/// but only through the score bundle, which always carries its evidence.
///
/// Candidates the index has no facts for keep the caller's stable-key order in
/// a trailing group. They are unknown, not low-risk, and the response says so
/// per entry. With no index, or an index that scores nothing in this tier, the
/// ordering reduces exactly to `stable_key` and no signal is invented.
pub fn order_impact_within_tier_by_defect_risk<T>(
    health: Option<&HealthFactIndex>,
    candidates: &mut [HealthImpactCandidate<T>],
) {
    let Some(health) = health else {
        candidates.sort_by(|left, right| left.stable_key.cmp(&right.stable_key));
        return;
    };

    // Score once per candidate rather than once per comparison: `sort_by` calls
    // its comparator O(n log n) times and a score walks every input weight.
    let ranks: Vec<Option<u16>> = candidates
        .iter()
        .map(|candidate| {
            let path = candidate.file_path.as_deref()?;
            let score = health.score(path, Axis::DefectRisk);
            if score.availability == FactAvailability::Unavailable {
                None
            } else {
                Some(score.score_per_mille)
            }
        })
        .collect();

    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by(|&left, &right| {
        match (ranks[left], ranks[right]) {
            (Some(left_score), Some(right_score)) => right_score.cmp(&left_score),
            // Scored before unscored: an entry with evidence is ranked, an
            // entry without keeps its place rather than claiming a low score.
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| candidates[left].stable_key.cmp(&candidates[right].stable_key))
    });

    apply_permutation(candidates, order);
}

/// Reorders `items` so that `items[i]` becomes the element at `order[i]`.
fn apply_permutation<T>(items: &mut [T], order: Vec<usize>) {
    let mut position: Vec<usize> = vec![0; order.len()];
    for (target, &source) in order.iter().enumerate() {
        position[source] = target;
    }
    for index in 0..items.len() {
        while position[index] != index {
            let destination = position[index];
            items.swap(index, destination);
            position.swap(index, destination);
        }
    }
}

/// A bounded per-file health section for `context` and `prepare_change`.
///
/// Spec H4.3: the files in the proposed change set or subsystem only, never
/// every file the response happens to touch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthSection {
    /// Scored files, most defect-risk evidence first, capped at `considered`.
    pub files: Vec<HealthFileHealth>,
    /// Distinct paths the caller offered, before the cap.
    pub considered: usize,
    /// How many of those were dropped by the cap.
    pub truncated: usize,
    /// Worst availability of any family the index holds, or `unavailable` when
    /// there is no index.
    pub availability: String,
    /// Why the section carries no files, when it carries none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    /// Version of the weight table the scores were computed under.
    pub weights_version: u32,
}

impl HealthSection {
    /// Whether the section has anything worth rendering.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Both axes for one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthFileHealth {
    /// Workspace-relative path.
    pub path: String,
    /// Evidence-bearing `defect_risk` score.
    pub defect_risk: HealthEvidence,
    /// Evidence-bearing `maintainability` score. Not backtested: its weights
    /// are a documented editorial judgment (`crate::health::scoring`, module
    /// header), so no reader may cite the backtest for it.
    pub maintainability: HealthEvidence,
}

/// Build the bounded health section for a change set or subsystem.
///
/// `paths` is deduplicated in first-seen order; scored files sort by descending
/// `defect_risk` and then by path, so the cap keeps the best-evidenced entries.
/// Files with no facts at all are reported only when nothing scored, so that a
/// section never fills its ten slots with `unknown`.
pub fn health_section<I, S>(
    health: Option<&HealthFactIndex>,
    paths: I,
    limit: usize,
) -> HealthSection
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut unique: Vec<String> = Vec::new();
    for path in paths {
        let path = path.as_ref();
        if path.is_empty() || unique.iter().any(|seen| seen == path) {
            continue;
        }
        unique.push(path.to_string());
    }
    let considered = unique.len();

    let Some(health) = health else {
        return HealthSection {
            files: Vec::new(),
            considered,
            truncated: considered,
            availability: FactAvailability::Unavailable.as_str().to_string(),
            unavailable_reason: Some(
                "no health facts have been produced for this workspace".to_string(),
            ),
            weights_version: 0,
        };
    };

    let weights_version = health.weights().version;
    let mut scored: Vec<HealthFileHealth> = unique
        .iter()
        .map(|path| HealthFileHealth {
            path: path.clone(),
            defect_risk: HealthEvidence::from_score(
                path,
                &health.score(path, Axis::DefectRisk),
                HEALTH_EVIDENCE_FACT_LIMIT,
            ),
            maintainability: HealthEvidence::from_score(
                path,
                &health.score(path, Axis::Maintainability),
                HEALTH_EVIDENCE_FACT_LIMIT,
            ),
        })
        .collect();

    scored.retain(|file| !file.defect_risk.is_unknown());
    scored.sort_by(|left, right| {
        right
            .defect_risk
            .score_per_mille
            .cmp(&left.defect_risk.score_per_mille)
            .then_with(|| left.path.cmp(&right.path))
    });

    let truncated = considered.saturating_sub(scored.len().min(limit));
    scored.truncate(limit);

    let unavailable_reason = if scored.is_empty() && considered > 0 {
        Some("no health facts cover these files".to_string())
    } else {
        None
    };

    HealthSection {
        files: scored,
        considered,
        truncated,
        availability: health.availability().as_str().to_string(),
        unavailable_reason,
        weights_version,
    }
}

/// A changed file with no edge-linked test (spec H4.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UntestedChangeEntry {
    /// Workspace-relative path of the changed file.
    pub path: String,
    /// Always zero — the entry exists because the count is zero. Carried so a
    /// JSON reader need not infer it.
    pub linked_test_count: u32,
    /// How complete the test-proximity facts were for this file.
    pub availability: String,
    /// The reader-facing statement of the fact.
    pub detail: String,
}

/// Changed files whose test-proximity facts record zero edge-linked tests.
///
/// Only files the producer actually measured appear: a file with no
/// test-proximity fact is unknown, and reporting it as untested would be
/// exactly the "unknown scored as zero" this engine forbids.
pub fn untested_changes<I, S>(health: Option<&HealthFactIndex>, paths: I) -> Vec<UntestedChangeEntry>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let Some(health) = health else {
        return Vec::new();
    };
    let mut entries: Vec<UntestedChangeEntry> = Vec::new();
    for path in paths {
        let path = path.as_ref();
        if entries.iter().any(|entry| entry.path == path) {
            continue;
        }
        let Some(facts) = health.facts(path) else {
            continue;
        };
        let Some(value) = facts.get(FactKind::UntestedChange) else {
            continue;
        };
        if value.value == 0 {
            continue;
        }
        entries.push(UntestedChangeEntry {
            path: path.to_string(),
            linked_test_count: 0,
            availability: value.availability.as_str().to_string(),
            detail: format!(
                "{} has no edge-linked tests in the published index",
                path
            ),
        });
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    entries.truncate(MAX_UNTESTED_CHANGE_ENTRIES);
    entries
}

/// The committed report the `defect_risk` weights were derived from.
///
/// Echoed in `status` so an agent can read the evidence, and its limits,
/// without being told where to look.
pub const BACKTEST_REPORT_PATH: &str = "docs/reports/health-backtest/2026-08-14.md";

/// Files named per axis in a `status` report.
pub const MAX_HEALTH_STATUS_TOP_FILES: usize = 5;

/// What the caller knows about the analysis behind the facts.
///
/// Kept as caller-supplied input so this module stays pure: parse failures and
/// history freshness are the indexer's and the git runtime's truth, not
/// something a fact index can discover about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthStatusInputs {
    /// Whether the index behind the graph was whole.
    pub index_complete: bool,
    /// Files the parser could not read, from index health.
    pub parse_failures: u32,
    /// The git view's own availability word, e.g. `available` or `stale`.
    pub git_availability: String,
    /// Commits the published history window actually covered.
    pub git_window_commits: u32,
    /// Generation of the published history snapshot, when there is one.
    pub git_generation: Option<i64>,
}

/// How much of one fact family reached the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthFamilyStatus {
    /// Producing family, e.g. `complexity`.
    pub family: String,
    /// Worst availability of any fact this family contributed.
    pub availability: String,
    /// Files for which the family contributed at least one fact.
    pub files_covered: usize,
    /// Share of the index's files the family covered, per-mille.
    pub coverage_per_mille: u32,
    /// Whether a committed backtest measured this family's weights.
    pub backtested: bool,
}

/// How many files fall in each band on one axis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthBandHistogram {
    /// Which axis was scored.
    pub axis: String,
    /// Band name to file count, weakest band first.
    pub bands: Vec<(String, usize)>,
    /// Files the axis could not score at all.
    pub unknown: usize,
    /// Files whose missing inputs left the band a range rather than a point.
    pub inexact: usize,
    /// The heaviest-scoring files, with the evidence behind them.
    pub top_files: Vec<HealthEvidence>,
}

/// Repository-wide health state, for `status{scope:"health"}` (spec H4.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthStatusReport {
    /// Worst availability of any family the index holds.
    pub availability: String,
    /// Files the index holds facts for.
    pub files_scored: usize,
    /// Version of the weight table scores are computed under.
    pub weights_version: u32,
    /// Version of the health config the facts were produced under.
    pub config_version: u32,
    /// Per-family freshness and coverage.
    pub families: Vec<HealthFamilyStatus>,
    /// Band histogram and top files, per axis.
    pub axes: Vec<HealthBandHistogram>,
    /// Everything known to be missing or partial, in plain words.
    ///
    /// Present so an agent can distrust the numbers correctly rather than
    /// having to infer degradation from a silence.
    pub incomplete_analysis: Vec<String>,
    /// Where the weights came from, and where their limits are recorded.
    pub backtest_report: String,
}

/// Build the repository-wide health status.
pub fn health_status(
    health: Option<&HealthFactIndex>,
    inputs: &HealthStatusInputs,
    top_files: usize,
) -> HealthStatusReport {
    use crate::health::config::HEALTH_CONFIG_VERSION;
    use crate::health::scoring::{ALL_AXES, ALL_BANDS, ALL_FACT_KINDS};

    let mut incomplete_analysis: Vec<String> = Vec::new();
    if !inputs.index_complete {
        incomplete_analysis.push(
            "the index behind these facts was incomplete, so every graph-derived fact is degraded"
                .to_string(),
        );
    }
    if inputs.parse_failures > 0 {
        incomplete_analysis.push(format!(
            "{} file(s) failed to parse, so their symbols and complexity are absent rather than zero",
            inputs.parse_failures
        ));
    }
    if inputs.git_availability != "available" {
        incomplete_analysis.push(format!(
            "history is {}, so no git-derived fact contributed to any score",
            inputs.git_availability
        ));
    } else if inputs.git_window_commits == 0 {
        incomplete_analysis.push(
            "the history window covered no commits, so git-derived facts are absent".to_string(),
        );
    }

    let Some(health) = health else {
        incomplete_analysis.push(
            "no health facts have been produced for this workspace; nothing here is scored"
                .to_string(),
        );
        return HealthStatusReport {
            availability: FactAvailability::Unavailable.as_str().to_string(),
            files_scored: 0,
            weights_version: 0,
            config_version: HEALTH_CONFIG_VERSION,
            families: Vec::new(),
            axes: Vec::new(),
            incomplete_analysis,
            backtest_report: BACKTEST_REPORT_PATH.to_string(),
        };
    };

    let files_scored = health.file_count();

    // Family coverage, walked once over the index rather than once per family.
    let mut family_files: std::collections::BTreeMap<&'static str, usize> = Default::default();
    let mut family_availability: std::collections::BTreeMap<&'static str, FactAvailability> =
        Default::default();
    for path in health.paths() {
        let Some(facts) = health.facts(path) else {
            continue;
        };
        let mut seen: Vec<&'static str> = Vec::new();
        for value in &facts.values {
            let family = value.kind.family().as_str();
            if !seen.contains(&family) {
                seen.push(family);
                *family_files.entry(family).or_insert(0) += 1;
            }
            let entry = family_availability
                .entry(family)
                .or_insert(FactAvailability::Available);
            *entry = entry.worst(value.availability);
        }
    }

    // Every family is listed, including the ones that contributed nothing: a
    // family absent from the report would read as a family with no problems.
    let mut families: Vec<HealthFamilyStatus> = Vec::new();
    let mut listed: Vec<&'static str> = Vec::new();
    for kind in ALL_FACT_KINDS {
        let family = kind.family().as_str();
        if listed.contains(&family) {
            continue;
        }
        listed.push(family);
        let covered = family_files.get(family).copied().unwrap_or(0);
        families.push(HealthFamilyStatus {
            family: family.to_string(),
            availability: family_availability
                .get(family)
                .copied()
                .unwrap_or(FactAvailability::Unavailable)
                .as_str()
                .to_string(),
            files_covered: covered,
            coverage_per_mille: if files_scored == 0 {
                0
            } else {
                ((covered as u64 * 1000) / files_scored as u64) as u32
            },
            backtested: kind.is_backtested(),
        });
    }
    for family in &families {
        if family.availability == FactAvailability::Unavailable.as_str() {
            incomplete_analysis.push(format!(
                "no {} facts reached the index, so every score names them as missing inputs",
                family.family
            ));
        }
    }

    let mut axes: Vec<HealthBandHistogram> = Vec::new();
    for axis in ALL_AXES {
        let mut counts: Vec<(String, usize)> = ALL_BANDS
            .iter()
            .map(|band| (band.as_str().to_string(), 0))
            .collect();
        let mut unknown = 0;
        let mut inexact = 0;
        let mut ranked: Vec<(u16, String)> = Vec::new();

        for path in health.paths() {
            let score = health.score(path, axis);
            if score.availability == FactAvailability::Unavailable {
                unknown += 1;
                continue;
            }
            if !score.is_exact() {
                inexact += 1;
            }
            if let Some(entry) = counts
                .iter_mut()
                .find(|(name, _)| name == score.band.as_str())
            {
                entry.1 += 1;
            }
            ranked.push((score.score_per_mille, path.to_string()));
        }

        // Heaviest first, path-ordered on ties, so the report is byte-stable.
        ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        ranked.truncate(top_files);
        let top = ranked
            .into_iter()
            .map(|(_, path)| {
                HealthEvidence::from_score(
                    &path,
                    &health.score(&path, axis),
                    HEALTH_EVIDENCE_FACT_LIMIT,
                )
            })
            .collect();

        axes.push(HealthBandHistogram {
            axis: axis.as_str().to_string(),
            bands: counts,
            unknown,
            inexact,
            top_files: top,
        });
    }

    if files_scored == 0 {
        incomplete_analysis
            .push("the index holds facts for no file at all; nothing here is scored".to_string());
    }
    incomplete_analysis.dedup();

    HealthStatusReport {
        availability: health.availability().as_str().to_string(),
        files_scored,
        weights_version: health.weights().version,
        config_version: HEALTH_CONFIG_VERSION,
        families,
        axes,
        incomplete_analysis,
        backtest_report: BACKTEST_REPORT_PATH.to_string(),
    }
}

/// `defect_risk` as a strictly secondary ranking key for `diagnose` (spec
/// H4.4).
///
/// The caller has already selected candidates from trace and graph proximity.
/// This reorders *within* one caller-defined primary tier and returns the
/// evidence for the explanation text. It can never admit a candidate the trace
/// evidence did not already select, because it only ever permutes the slice it
/// is given.
pub fn rank_fault_candidates_by_defect_risk<T>(
    health: Option<&HealthFactIndex>,
    candidates: &mut [HealthImpactCandidate<T>],
) {
    order_impact_within_tier_by_defect_risk(health, candidates);
}

#[cfg(test)]
#[path = "health_consumers_tests.rs"]
mod tests;
