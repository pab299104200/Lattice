//! Building and rendering the H1.3 backtest report.
//!
//! # Determinism
//!
//! The report deliberately carries **no clock reading**. Nothing in it depends
//! on when it was generated: it identifies itself by harness version, health
//! config version, and the `HEAD` commit of every repository replayed. That is
//! what makes "same input produces a byte-identical report" a property a test
//! can assert rather than an aspiration, and what lets a reader confirm a
//! committed report by rerunning it.
//!
//! # What is compared, and how the weights are derived
//!
//! Three nested family sets are ranked against ground truth: graph-only,
//! graph+git, and graph+git+complexity. Each is evaluated twice.
//!
//! * **Uniform weights** — every feature counts equally. No parameter is
//!   chosen by looking at the labels, so this number cannot be overfitted. It
//!   is the honest answer to "do these facts carry signal at all".
//! * **Derived weights, held out** — a feature's weight comes from its
//!   measured univariate discrimination, but for each cut point the weights
//!   are derived from *the other* cut points only. A cut point never
//!   contributes to the weights used to score it, so this is an out-of-sample
//!   estimate rather than a description of the data it was fitted to.
//!
//! The per-feature univariate table is what H3 should read for its weights.
//! The held-out family result is what H3 should expect those weights to
//! achieve. Reporting the in-sample number alone would overstate the engine's
//! accuracy, which is the exact failure the spec's "backtest before claim"
//! rule exists to prevent.

use serde::{Deserialize, Serialize};

use crate::health::arithmetic::per_mille;
use crate::health::config::HEALTH_CONFIG_VERSION;

use super::audit::{audit_labels, AuditInput, LabelAudit, DEFAULT_SAMPLE_SIZE};
use super::features::{
    normalize, score, FactFamily, FamilySet, FeatureKind, FeatureWeights, NormalizedFeatureVector,
    ALL_FAMILY_SETS, ALL_FEATURES, FEATURE_COUNT,
};
use super::metrics::{evaluate, CalibrationBucket, Evaluation, EvaluationUnavailable, ScoredObservation};
use super::replay::{CutPointReplay, ReplayReport, ReplaySummary, RepositoryReplay, TreeReadReport};
use super::BACKTEST_HARNESS_VERSION;

/// How a family's features were weighted for one evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Weighting {
    /// Every feature counts equally; nothing was fitted.
    Uniform,
    /// Weights derived from univariate discrimination measured on the *other*
    /// cut points, so each cut point is scored out of sample.
    DerivedHeldOut,
}

impl Weighting {
    /// Human-readable name for report headings.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Uniform => "uniform",
            Self::DerivedHeldOut => "derived (held out)",
        }
    }
}

/// One family set's evaluation under one weighting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyResult {
    /// Which fact families were drawn on.
    pub family: FamilySet,
    /// How the features were weighted.
    pub weighting: Weighting,
    /// The measured evaluation, or why one could not be produced.
    pub evaluation: Option<Evaluation>,
    /// Present instead of `evaluation` when the measurement was impossible.
    pub unavailable: Option<String>,
}

impl FamilyResult {
    fn new(
        family: FamilySet,
        weighting: Weighting,
        evaluation: Result<Evaluation, EvaluationUnavailable>,
    ) -> Self {
        match evaluation {
            Ok(evaluation) => Self {
                family,
                weighting,
                evaluation: Some(evaluation),
                unavailable: None,
            },
            Err(reason) => Self {
                family,
                weighting,
                evaluation: None,
                unavailable: Some(reason.reason().to_owned()),
            },
        }
    }
}

/// One feature's standalone discriminative power.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureResult {
    /// The feature.
    pub feature: FeatureKind,
    /// Its producing family.
    pub family: FactFamily,
    /// Files for which the feature had a value.
    pub observations: u32,
    /// Share of all scored files for which it had a value, per-mille.
    pub coverage_per_mille: u32,
    /// Univariate ROC-AUC, per-mille; absent when it could not be measured.
    pub roc_auc_per_mille: Option<u32>,
    /// The weight this measurement implies for H3.
    pub derived_weight: u32,
}

/// One cut point's contribution, as reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CutPointReport {
    /// Position on the first-parent spine; 0 is `HEAD`.
    pub spine_index: usize,
    /// Abbreviated cut-point commit id.
    pub commit: String,
    /// Cut-point commit summary.
    pub subject: String,
    /// Commits mined from the window before the cut point.
    pub window_commits: u32,
    /// Files scored at this cut point.
    pub files_scored: u32,
    /// Files labeled defective by the horizon.
    pub defective_files: u32,
    /// Commits admitted to the horizon.
    pub horizon_commits: u32,
    /// Fix-shaped commits among them.
    pub horizon_fix_commits: u32,
    /// The horizon stopped at the commit cap.
    pub horizon_truncated_by_commit_cap: bool,
    /// The horizon stopped at the day cap.
    pub horizon_truncated_by_day_cap: bool,
    /// Commits excluded from labeling for exceeding the path cap.
    pub horizon_path_overflow_commits: u32,
    /// Commits excluded from the mining window for exceeding the path cap.
    pub window_path_overflow_commits: u32,
    /// Accounting for the tree read at this cut point.
    pub tree: TreeReadReport,
}

/// One repository's contribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryReport {
    /// Repository directory name.
    pub name: String,
    /// Abbreviated `HEAD` commit id at replay time.
    pub head_commit: String,
    /// What the replay read and clamped.
    pub replay: ReplayReport,
    /// Per-cut-point detail.
    pub cut_points: Vec<CutPointReport>,
    /// Files scored across every cut point.
    pub observations: u32,
    /// Of those, how many were labeled defective.
    pub positives: u32,
    /// Family comparison for this repository alone, uniform weights.
    pub families: Vec<FamilyResult>,
    /// Standalone discriminative power per feature for this repository alone.
    ///
    /// Present so a reader can see whether a fact generalises or merely
    /// reflects one codebase's habits. A fact that ranks well on one corpus and
    /// at chance on the others is a fact about that corpus, not about
    /// software, and must not be weighted as though it were the latter.
    pub features: Vec<FeatureResult>,
}

/// The complete backtest report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BacktestReport {
    /// Version of the harness that produced this report.
    pub harness_version: u32,
    /// Version of the health configuration table in force.
    pub health_config_version: u32,
    /// Per-repository results.
    pub repositories: Vec<RepositoryReport>,
    /// Files scored across every repository and cut point.
    pub pooled_observations: u32,
    /// Of those, how many were labeled defective.
    pub pooled_positives: u32,
    /// Cut points pooled.
    pub pooled_cut_points: u32,
    /// Family comparison pooled across repositories, uniform weights.
    pub pooled_families: Vec<FamilyResult>,
    /// Family comparison pooled across repositories, held-out derived weights.
    pub held_out_families: Vec<FamilyResult>,
    /// Standalone discriminative power per feature, pooled.
    pub features: Vec<FeatureResult>,
    /// The weights the per-feature table implies for H3.
    pub derived_weights: FeatureWeights,
    /// The H1.2 label-quality audit, pooled across every mined window.
    pub audit: LabelAudit,
}

/// One cut point's normalized, labeled files.
struct CutPointFrame {
    repository: String,
    spine_index: usize,
    files: Vec<FrameFile>,
}

struct FrameFile {
    key: String,
    vector: NormalizedFeatureVector,
    label: bool,
}

/// Accumulates repositories one at a time into a report.
///
/// Pooling needs every repository's *normalized* observations, but not their
/// replays: a `RepositoryReplay` carries the full fact snapshots for every cut
/// point, which for a large repository is hundreds of megabytes. Feeding
/// replays in one at a time and dropping each once its frames are extracted
/// caps peak memory at a single repository rather than the whole corpus, which
/// is what makes pooling across many repositories practical.
#[derive(Default)]
pub struct ReportBuilder {
    frames: Vec<CutPointFrame>,
    audit_inputs: Vec<AuditInput>,
    repositories: Vec<RepositoryReport>,
    pending: Option<PendingRepository>,
}

/// A repository whose cut points are still arriving.
struct PendingRepository {
    name: String,
    first_frame: usize,
    cut_points: Vec<CutPointReport>,
}

impl ReportBuilder {
    /// A builder with no repositories yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one fully replayed repository in.
    ///
    /// The replay may be dropped immediately afterwards. For a large
    /// repository, prefer the streaming trio
    /// ([`ReportBuilder::start_repository`],
    /// [`ReportBuilder::push_cut_point`],
    /// [`ReportBuilder::finish_repository`]) so cut points can be dropped one
    /// at a time.
    pub fn push(&mut self, replay: &RepositoryReplay) {
        self.start_repository(&replay.name);
        for cut_point in &replay.cut_points {
            self.push_cut_point(cut_point);
        }
        self.finish_repository(&ReplaySummary {
            name: replay.name.clone(),
            head_commit_id: replay.head_commit_id.clone(),
            report: replay.report.clone(),
        });
    }

    /// Begin a repository; every cut point pushed next belongs to it.
    pub fn start_repository(&mut self, name: &str) {
        self.pending = Some(PendingRepository {
            name: name.to_owned(),
            first_frame: self.frames.len(),
            cut_points: Vec::new(),
        });
    }

    /// Fold one cut point in. It may be dropped immediately afterwards.
    ///
    /// Everything the report needs is extracted here: the normalized feature
    /// vectors, the labels, the audit inputs, and the accounting. The fact
    /// snapshots themselves are not retained.
    pub fn push_cut_point(&mut self, cut_point: &CutPointReplay) {
        let Some(pending) = self.pending.as_mut() else {
            debug_assert!(false, "push_cut_point called outside start_repository");
            return;
        };
        let (frame, report) = extract_cut_point(&pending.name, cut_point, &mut self.audit_inputs);
        pending.cut_points.push(report);
        self.frames.push(frame);
    }

    /// Close the repository opened by [`ReportBuilder::start_repository`].
    pub fn finish_repository(&mut self, summary: &ReplaySummary) {
        let Some(pending) = self.pending.take() else {
            debug_assert!(false, "finish_repository called without start_repository");
            return;
        };
        let repository_frames = &self.frames[pending.first_frame..];
        let observations: u32 = repository_frames
            .iter()
            .map(|frame| frame.files.len() as u32)
            .sum();
        let positives: u32 = repository_frames
            .iter()
            .map(|frame| frame.files.iter().filter(|file| file.label).count() as u32)
            .sum();

        let uniform = FeatureWeights::uniform();
        let families = ALL_FAMILY_SETS
            .iter()
            .map(|set| {
                FamilyResult::new(
                    *set,
                    Weighting::Uniform,
                    evaluate(scored(repository_frames, *set, &uniform)),
                )
            })
            .collect();

        let (univariate, coverage) = univariate_discrimination(repository_frames);
        let features = feature_results(
            &univariate,
            &coverage,
            observations,
            &FeatureWeights::from_univariate_roc(&univariate),
        );

        self.repositories.push(RepositoryReport {
            name: summary.name.clone(),
            head_commit: abbreviate(&summary.head_commit_id),
            replay: summary.report.clone(),
            cut_points: pending.cut_points,
            observations,
            positives,
            families,
            features,
        });
    }

    /// Finish the report.
    pub fn finish(self) -> BacktestReport {
        debug_assert!(
            self.pending.is_none(),
            "finish called with a repository still open"
        );
        finish_report(self.frames, self.audit_inputs, self.repositories)
    }
}

/// Extract everything the report needs from one cut point.
fn extract_cut_point(
    repository: &str,
    cut_point: &CutPointReplay,
    audit_inputs: &mut Vec<AuditInput>,
) -> (CutPointFrame, CutPointReport) {
    // Normalization is per cut point: a file's percentile describes its
    // standing among the files that existed alongside it, not among files from
    // a different era or repository.
    let vectors: Vec<_> = cut_point
        .observations
        .iter()
        .map(|observation| observation.features.clone())
        .collect();
    let normalized = normalize(&vectors);
    let files: Vec<FrameFile> = normalized
        .into_iter()
        .zip(cut_point.observations.iter())
        .map(|(vector, observation)| FrameFile {
            key: format!(
                "{}#{:06}:{}",
                repository, cut_point.spine_index, observation.path
            ),
            vector,
            label: observation.label,
        })
        .collect();

    let defective_files = files.iter().filter(|file| file.label).count() as u32;
    let report = CutPointReport {
        spine_index: cut_point.spine_index,
        commit: abbreviate(&cut_point.commit_id),
        subject: cut_point.subject.clone(),
        window_commits: cut_point.git.processed_commits.len() as u32,
        files_scored: files.len() as u32,
        defective_files,
        horizon_commits: cut_point.labels.report.selected_commits,
        horizon_fix_commits: cut_point.labels.report.fix_shaped_commits,
        horizon_truncated_by_commit_cap: cut_point.labels.report.truncated_by_commit_cap,
        horizon_truncated_by_day_cap: cut_point.labels.report.truncated_by_day_cap,
        horizon_path_overflow_commits: cut_point.labels.report.path_overflow_commits,
        window_path_overflow_commits: cut_point.git.report.path_overflow_commits,
        tree: cut_point.tree.clone(),
    };

    for commit in &cut_point.window_commits {
        audit_inputs.push(AuditInput {
            id: commit.id.clone(),
            subject: commit.subject.clone(),
            paths: commit.paths.clone(),
        });
    }

    (
        CutPointFrame {
            repository: repository.to_owned(),
            spine_index: cut_point.spine_index,
            files,
        },
        report,
    )
}

/// Build the report from one or more replayed repositories.
///
/// Convenience over [`ReportBuilder`] for callers that already hold every
/// replay; a caller replaying many large repositories should use the builder
/// so it can drop each replay as it goes.
pub fn build_report(replays: &[RepositoryReplay]) -> BacktestReport {
    let mut builder = ReportBuilder::new();
    for replay in replays {
        builder.push(replay);
    }
    builder.finish()
}


/// Assemble the per-feature result rows from a univariate measurement.
fn feature_results(
    univariate: &[Option<u32>; FEATURE_COUNT],
    coverage: &[u32; FEATURE_COUNT],
    observations: u32,
    weights: &FeatureWeights,
) -> Vec<FeatureResult> {
    ALL_FEATURES
        .iter()
        .map(|feature| FeatureResult {
            feature: *feature,
            family: feature.family(),
            observations: coverage[feature.index()],
            coverage_per_mille: per_mille(
                u64::from(coverage[feature.index()]),
                u64::from(observations),
            ),
            roc_auc_per_mille: univariate[feature.index()],
            derived_weight: weights.get(*feature),
        })
        .collect()
}

/// Turn accumulated frames into the finished report.
fn finish_report(
    frames: Vec<CutPointFrame>,
    audit_inputs: Vec<AuditInput>,
    repositories: Vec<RepositoryReport>,
) -> BacktestReport {
    let uniform = FeatureWeights::uniform();
    let pooled_families = ALL_FAMILY_SETS
        .iter()
        .map(|set| {
            FamilyResult::new(
                *set,
                Weighting::Uniform,
                evaluate(scored(&frames, *set, &uniform)),
            )
        })
        .collect();

    let (univariate, coverage) = univariate_discrimination(&frames);
    let derived_weights = FeatureWeights::from_univariate_roc(&univariate);
    let held_out_families = ALL_FAMILY_SETS
        .iter()
        .map(|set| {
            FamilyResult::new(
                *set,
                Weighting::DerivedHeldOut,
                evaluate(held_out_scored(&frames, *set)),
            )
        })
        .collect();

    let pooled_observations: u32 = frames.iter().map(|frame| frame.files.len() as u32).sum();
    let pooled_positives: u32 = frames
        .iter()
        .map(|frame| frame.files.iter().filter(|file| file.label).count() as u32)
        .sum();

    let features = feature_results(
        &univariate,
        &coverage,
        pooled_observations,
        &derived_weights,
    );

    BacktestReport {
        harness_version: BACKTEST_HARNESS_VERSION,
        health_config_version: HEALTH_CONFIG_VERSION,
        repositories,
        pooled_observations,
        pooled_positives,
        pooled_cut_points: frames.len() as u32,
        pooled_families,
        held_out_families,
        features,
        derived_weights,
        audit: audit_labels(&audit_inputs, DEFAULT_SAMPLE_SIZE),
    }
}

/// Score every file in `frames` under one family set and weighting.
fn scored(
    frames: &[CutPointFrame],
    set: FamilySet,
    weights: &FeatureWeights,
) -> Vec<ScoredObservation> {
    let mut observations = Vec::new();
    for frame in frames {
        for file in &frame.files {
            if let Ok(value) = score(&file.vector, set, weights) {
                observations.push(ScoredObservation {
                    key: file.key.clone(),
                    score_per_mille: value,
                    label: file.label,
                });
            }
        }
    }
    observations
}

/// Score every cut point with weights derived from the *other* cut points.
///
/// This is the report's out-of-sample estimate. A cut point never contributes
/// to the weights used to score it, so the result cannot be a description of
/// data the weights were fitted to.
fn held_out_scored(frames: &[CutPointFrame], set: FamilySet) -> Vec<ScoredObservation> {
    let mut observations = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        let others: Vec<&CutPointFrame> = frames
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .map(|(_, other)| other)
            .collect();
        if others.is_empty() {
            // With a single cut point there is nothing to hold out against, so
            // no out-of-sample claim can be made at all.
            return Vec::new();
        }
        let (univariate, _) = univariate_discrimination_refs(&others);
        let weights = FeatureWeights::from_univariate_roc(&univariate);
        if !weights.has_signal(set) {
            continue;
        }
        for file in &frame.files {
            if let Ok(value) = score(&file.vector, set, &weights) {
                observations.push(ScoredObservation {
                    key: file.key.clone(),
                    score_per_mille: value,
                    label: file.label,
                });
            }
        }
    }
    observations
}

/// Univariate ROC-AUC per feature, plus how many files carried each one.
fn univariate_discrimination(
    frames: &[CutPointFrame],
) -> ([Option<u32>; FEATURE_COUNT], [u32; FEATURE_COUNT]) {
    let refs: Vec<&CutPointFrame> = frames.iter().collect();
    univariate_discrimination_refs(&refs)
}

fn univariate_discrimination_refs(
    frames: &[&CutPointFrame],
) -> ([Option<u32>; FEATURE_COUNT], [u32; FEATURE_COUNT]) {
    let mut roc = [None; FEATURE_COUNT];
    let mut coverage = [0u32; FEATURE_COUNT];

    for feature in ALL_FEATURES {
        let mut observations = Vec::new();
        for frame in frames {
            for file in &frame.files {
                let Some(percentile) = file.vector.get(feature) else {
                    continue;
                };
                observations.push(ScoredObservation {
                    key: file.key.clone(),
                    score_per_mille: percentile,
                    label: file.label,
                });
            }
        }
        coverage[feature.index()] = observations.len() as u32;
        if let Ok(evaluation) = evaluate(observations) {
            roc[feature.index()] = Some(evaluation.roc_auc_per_mille);
        }
    }

    (roc, coverage)
}

/// Shorten a commit id for report text.
fn abbreviate(id: &str) -> String {
    id.chars().take(10).collect()
}

/// Render a per-mille value as a fixed three-decimal string.
///
/// Formatted from integers so the output has no platform-dependent float
/// rounding anywhere in it.
fn decimal(value: u32) -> String {
    format!("{}.{:03}", value / 1000, value % 1000)
}

/// Render a signed per-mille value as a fixed three-decimal string.
fn signed_decimal(value: i32) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let magnitude = value.unsigned_abs();
    format!("{}{}.{:03}", sign, magnitude / 1000, magnitude % 1000)
}

/// Render a per-mille value as a percentage with one decimal place.
fn percentage(value: u32) -> String {
    format!("{}.{}%", value / 10, value % 10)
}

/// Escape a commit subject for inclusion in a markdown table cell.
fn escape_cell(text: &str) -> String {
    text.replace('\\', "\\\\").replace('|', "\\|")
}

impl BacktestReport {
    /// Render the report as JSON.
    pub fn render_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Render the report as markdown.
    pub fn render_markdown(&self) -> String {
        let mut out = String::new();
        self.write_header(&mut out);
        self.write_headline(&mut out);
        self.write_per_repository(&mut out);
        self.write_calibration(&mut out);
        self.write_features(&mut out);
        self.write_feature_generalization(&mut out);
        self.write_audit(&mut out);
        self.write_windows(&mut out);
        self.write_conclusions(&mut out);
        out
    }

    fn write_header(&self, out: &mut String) {
        out.push_str("# Health backtest report\n\n");
        out.push_str(
            "Produced by `lattice health-backtest` (Phase H1 of\n`docs/plans/2026-08-13-health-engine.md`). This report is the only authority\npermitted to set the H3 scoring weights.\n\n",
        );
        out.push_str("This document contains no generation timestamp by design: nothing in it\ndepends on when it was produced, so rerunning the harness against the same\ncommits reproduces it byte for byte.\n\n");
        out.push_str("## Provenance\n\n");
        out.push_str("| Field | Value |\n| --- | --- |\n");
        out.push_str(&format!(
            "| Harness version | {} |\n",
            self.harness_version
        ));
        out.push_str(&format!(
            "| Health config version | {} |\n",
            self.health_config_version
        ));
        out.push_str(&format!(
            "| Repositories replayed | {} |\n",
            self.repositories.len()
        ));
        out.push_str(&format!("| Cut points | {} |\n", self.pooled_cut_points));
        out.push_str(&format!(
            "| Files scored | {} |\n",
            self.pooled_observations
        ));
        out.push_str(&format!(
            "| Files labeled defective | {} ({}) |\n",
            self.pooled_positives,
            percentage(per_mille(
                u64::from(self.pooled_positives),
                u64::from(self.pooled_observations)
            ))
        ));
        for repository in &self.repositories {
            out.push_str(&format!(
                "| Repository `{}` | HEAD `{}`, {} first-parent commits |\n",
                repository.name, repository.head_commit, repository.replay.spine_length
            ));
        }
        out.push('\n');
    }

    fn write_headline(&self, out: &mut String) {
        out.push_str("## Headline: do the fact families predict defects?\n\n");
        out.push_str(
            "A file is labeled defective when a fix-shaped commit touched it inside the\nbounded horizon after a cut point. Every figure below ranks files by facts\nknown *at* the cut point only.\n\n",
        );
        out.push_str(
            "`PR-AUC` is only interpretable against the prevalence baseline in the same\nrow: a random ranker scores exactly the prevalence. `ROC-AUC` is the\nprobability that a defect-labeled file outranks a clean one, where 0.500 is\nchance. `lift` is precision at the stated threshold divided by prevalence.\n\n",
        );

        out.push_str("### Pooled, uniform weights (nothing fitted)\n\n");
        self.write_family_table(out, &self.pooled_families);
        out.push_str(
            "\nNo parameter above was chosen by looking at the labels, so none of it can be\noverfitted.\n\n",
        );

        out.push_str("### Pooled, derived weights, held out\n\n");
        out.push_str(
            "Weights come from measured univariate discrimination, but each cut point is\nscored with weights derived from the *other* cut points only. This is the\nout-of-sample estimate: it is what H3 should expect the derived weights to\nachieve, and it is the number to quote.\n\n",
        );
        self.write_family_table(out, &self.held_out_families);
        out.push('\n');
    }

    fn write_family_table(&self, out: &mut String, results: &[FamilyResult]) {
        out.push_str("| Fact families | Files | Prevalence | PR-AUC | PR lift | ROC-AUC | P@top-decile | R@top-decile | Lift@top-decile |\n");
        out.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for result in results {
            match &result.evaluation {
                Some(evaluation) => {
                    let top = evaluation
                        .operating_points
                        .iter()
                        .find(|point| point.threshold_per_mille == 900);
                    let (precision, recall, lift) = match top {
                        Some(point) if point.selected > 0 => (
                            decimal(point.precision_per_mille),
                            decimal(point.recall_per_mille),
                            format!("{}x", decimal(point.lift_per_mille)),
                        ),
                        _ => ("n/a".to_owned(), "n/a".to_owned(), "n/a".to_owned()),
                    };
                    out.push_str(&format!(
                        "| {} | {} | {} | {} | {}x | {} | {} | {} | {} |\n",
                        result.family.label(),
                        evaluation.observations,
                        decimal(evaluation.prevalence_per_mille),
                        decimal(evaluation.pr_auc_per_mille),
                        decimal(evaluation.pr_auc_lift_per_mille),
                        decimal(evaluation.roc_auc_per_mille),
                        precision,
                        recall,
                        lift,
                    ));
                }
                None => {
                    out.push_str(&format!(
                        "| {} | _unavailable: {}_ | | | | | | | |\n",
                        result.family.label(),
                        result.unavailable.as_deref().unwrap_or("unknown")
                    ));
                }
            }
        }
    }

    fn write_per_repository(&self, out: &mut String) {
        out.push_str("## Per repository (uniform weights)\n\n");
        if self.repositories.len() == 1 {
            out.push_str(
                "Only one repository was available in this environment. Multi-repository\npooling is exercised by the same code path and is reported below whenever\nmore than one repository is replayed.\n\n",
            );
        }
        for repository in &self.repositories {
            out.push_str(&format!(
                "### `{}` — {} files scored, {} defective ({})\n\n",
                repository.name,
                repository.observations,
                repository.positives,
                percentage(per_mille(
                    u64::from(repository.positives),
                    u64::from(repository.observations)
                ))
            ));
            self.write_family_table(out, &repository.families);
            out.push('\n');
        }
    }

    fn write_calibration(&self, out: &mut String) {
        out.push_str("## Calibration by decile\n\n");
        out.push_str(
            "Observed defect rate per score decile, pooled, under held-out derived\nweights. A well-calibrated score has a rate that rises monotonically down the\ntable.\n\n",
        );
        for result in &self.held_out_families {
            let Some(evaluation) = &result.evaluation else {
                continue;
            };
            out.push_str(&format!("### {}\n\n", result.family.label()));
            out.push_str("| Score band | Files | Defective | Observed rate |\n");
            out.push_str("| --- | ---: | ---: | ---: |\n");
            for bucket in evaluation.calibration.iter().rev() {
                out.push_str(&format!(
                    "| {} – {} | {} | {} | {} |\n",
                    decimal(bucket.lower_per_mille),
                    decimal(bucket.upper_per_mille.saturating_sub(1)),
                    bucket.observations,
                    bucket.positives,
                    calibration_rate(bucket),
                ));
            }
            out.push('\n');
        }
    }

    fn write_features(&self, out: &mut String) {
        out.push_str("## Per-fact discrimination — the weight derivation for H3\n\n");
        out.push_str(
            "Each fact ranked on its own against the same ground truth. `ROC-AUC` of\n0.500 is chance. The derived weight is `max(0, roc - 0.500) * 2`, so a fact\nat or below chance carries no weight; a fact measured *below* chance is\ndropped rather than inverted, because flipping its sign would invent a signal\nthe producer never claimed.\n\n",
        );
        out.push_str("`Coverage` is the share of scored files for which the fact had any value at\nall. A low-coverage fact with a strong AUC is a narrow signal, not a strong\none.\n\n");
        out.push_str("| Fact | Family | Coverage | ROC-AUC | Derived weight |\n");
        out.push_str("| --- | --- | ---: | ---: | ---: |\n");
        for feature in &self.features {
            let auc = match feature.roc_auc_per_mille {
                Some(value) => decimal(value),
                None => "n/a".to_owned(),
            };
            out.push_str(&format!(
                "| `{}` | {} | {} | {} | {} |\n",
                feature.feature.as_str(),
                feature.family.as_str(),
                percentage(feature.coverage_per_mille),
                auc,
                decimal(feature.derived_weight),
            ));
        }
        out.push('\n');
    }

    /// Cross-repository view of each fact's standalone discrimination.
    ///
    /// The pooled column above can be carried by a single large repository.
    /// This table exists to catch that: a fact worth weighting should beat
    /// chance in most corpora, not in one.
    fn write_feature_generalization(&self, out: &mut String) {
        if self.repositories.len() < 2 {
            return;
        }
        out.push_str("### Does each fact generalise across repositories?\n\n");
        out.push_str(
            "Univariate ROC-AUC per repository. A fact that ranks well in one corpus and\nat chance in the others is a fact about that corpus, not about software, and\nH3 should not weight it as though it were the latter. `consistent` counts the\nrepositories where the fact beat chance.\n\n",
        );

        out.push_str("| Fact | Pooled |");
        for repository in &self.repositories {
            out.push_str(&format!(" {} |", repository.name));
        }
        out.push_str(" Consistent |\n| --- | ---: |");
        for _ in &self.repositories {
            out.push_str(" ---: |");
        }
        out.push_str(" ---: |\n");

        for (index, feature) in ALL_FEATURES.iter().enumerate() {
            let pooled = match self.features[index].roc_auc_per_mille {
                Some(value) => decimal(value),
                None => "n/a".to_owned(),
            };
            out.push_str(&format!("| `{}` | {} |", feature.as_str(), pooled));
            let mut above_chance = 0usize;
            let mut measured = 0usize;
            for repository in &self.repositories {
                match repository.features[index].roc_auc_per_mille {
                    Some(value) => {
                        measured += 1;
                        if value > 500 {
                            above_chance += 1;
                        }
                        out.push_str(&format!(" {} |", decimal(value)));
                    }
                    None => out.push_str(" n/a |"),
                }
            }
            out.push_str(&format!(" {above_chance}/{measured} |\n"));
        }
        out.push('\n');
    }

    fn write_audit(&self, out: &mut String) {
        let audit = &self.audit;
        out.push_str("## H1.2 label-quality audit\n\n");
        out.push_str(
            "The ground truth above rests entirely on `looks_like_bug_fix`, a\nsubject-prefix heuristic. It is audited here against an independent signal:\ncommits whose diff touches both an existing test file and a production file.\n\n",
        );
        out.push_str(
            "**These two signals do not measure the same thing.** Real fixes often ship\nwithout touching a test, and features often ship with one. Raw agreement and\nCohen's kappa are therefore expected to be low even when the classifier works\ncorrectly, and a low kappa here is *not* evidence against the vocabulary. The\nfigure that carries the most evidence is enrichment: whether commits the\nclassifier calls fixes co-modify tests and production more often than commits\nin general.\n\n",
        );
        out.push_str(
            "Enrichment below 1.000 is genuinely ambiguous. It can mean the vocabulary\nadmits commits that are not fixes — or it can mean the proxy is\n*anti-correlated* with fix-ness in this corpus, which is what happens wherever\nfeature work ships with tests while bug fixes are one-line repairs. Only the\nspot-review sample below separates those two readings, which is why the spec\nrequires one. Read them together.\n\n",
        );
        out.push_str("| Measure | Value |\n| --- | ---: |\n");
        out.push_str(&format!(
            "| Commits audited | {} |\n",
            audit.commits_considered
        ));
        out.push_str(&format!(
            "| Classified fix-shaped | {} ({}) |\n",
            audit.classified_fix,
            percentage(audit.fix_rate_per_mille)
        ));
        out.push_str(&format!(
            "| Touching test + production | {} ({}) |\n",
            audit.touches_test_and_production,
            percentage(audit.co_modification_rate_per_mille)
        ));
        out.push_str(&format!(
            "| Co-modification rate, fix-shaped commits | {} |\n",
            percentage(audit.co_modification_given_fix_per_mille)
        ));
        out.push_str(&format!(
            "| Co-modification rate, all other commits | {} |\n",
            percentage(audit.co_modification_given_non_fix_per_mille)
        ));
        out.push_str(&format!(
            "| **Enrichment** | **{}x** |\n",
            decimal(audit.enrichment_per_mille)
        ));
        out.push_str(&format!(
            "| Raw agreement | {} |\n",
            percentage(audit.percent_agreement_per_mille)
        ));
        out.push_str(&format!(
            "| Cohen's kappa | {} |\n",
            signed_decimal(audit.cohen_kappa_per_mille)
        ));
        out.push_str(&format!(
            "| **Verdict (independent signal)** | **{}** |\n",
            audit.verdict.as_str()
        ));
        out.push('\n');
        out.push_str("### Recall: the fixes the classifier does not see\n\n");
        out.push_str(
            "Precision is not the only way a labeler fails. `looks_like_bug_fix` inspects\nonly the *prefix* of a subject, so a commit that announces repair work\nanywhere else goes unlabeled and its files are recorded as clean. The count\nbelow is every rejected commit whose subject names repair work as a whole word\n(`fix`, `bug`, `hotfix`, `regression`, `revert`, …), which is a lower bound on\nwhat the prefix rule gives up.\n\n",
        );
        out.push_str("| Measure | Value |\n| --- | ---: |\n");
        out.push_str(&format!(
            "| Recognised as fixes | {} |\n",
            audit.classified_fix
        ));
        out.push_str(&format!(
            "| Rejected but naming repair work | {} |\n",
            audit.unclassified_with_fix_vocabulary
        ));
        out.push_str(&format!(
            "| **Recall gap** | **{}** |\n",
            percentage(audit.recall_gap_per_mille)
        ));
        out.push_str(
            "\nA large recall gap means the reported prevalence understates the true defect\nrate and that files repaired by unrecognised commits are counted as clean.\nThat depresses measured precision — it does not inflate it — so the accuracy\nfigures in this report are conservative with respect to this failure mode.\n\n",
        );
        out.push_str("2x2 table: fix-shaped and co-modifying ");
        out.push_str(&format!(
            "{}; fix-shaped only {}; co-modifying only {}; neither {}.\n\n",
            audit.both, audit.fix_only, audit.signal_only, audit.neither
        ));

        if !audit.sample.is_empty() {
            out.push_str("### Sample for human spot-review\n\n");
            out.push_str(
                "Half classified fix-shaped, half not, spaced evenly across each class.\n\n",
            );
            out.push_str("| Commit | Classified | Test+prod | Subject |\n");
            out.push_str("| --- | --- | --- | --- |\n");
            for entry in &audit.sample {
                out.push_str(&format!(
                    "| `{}` | {} | {} | {} |\n",
                    entry.commit,
                    if entry.classified_fix { "fix" } else { "not fix" },
                    if entry.touches_test_and_production {
                        "yes"
                    } else {
                        "no"
                    },
                    escape_cell(&entry.subject),
                ));
            }
            out.push('\n');
        }
    }

    fn write_windows(&self, out: &mut String) {
        out.push_str("## Windows, cut points, and exclusion counters\n\n");
        for repository in &self.repositories {
            let limits = &repository.replay.limits;
            out.push_str(&format!("### `{}`\n\n", repository.name));
            out.push_str(&format!(
                "Mining window {} commits, max {} paths per commit. Horizon {} days or {} commits, whichever first. First-parent spine {} commits{}. Reserves: {} commits at the HEAD end, {} at the root end{}.\n\n",
                limits.git.history_limit,
                limits.git.paths_per_commit,
                limits.horizon.max_days,
                limits.horizon.max_commits,
                repository.replay.spine_length,
                if repository.replay.spine_truncated {
                    " (truncated at the spine limit)"
                } else {
                    ""
                },
                repository.replay.horizon_reserve,
                repository.replay.mining_reserve,
                if repository.replay.reserves_reduced {
                    " — reduced from the defaults because the history is short, so horizons are shorter and mining windows thinner than the defaults intend"
                } else {
                    ""
                },
            ));
            out.push_str("| Cut point | Commit | Window | Files | Defective | Horizon | Fixes | Truncated | Path overflow | Tree exclusions |\n");
            out.push_str("| ---: | --- | ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: |\n");
            for cut_point in &repository.cut_points {
                let truncated = match (
                    cut_point.horizon_truncated_by_commit_cap,
                    cut_point.horizon_truncated_by_day_cap,
                ) {
                    (true, true) => "commits+days",
                    (true, false) => "commits",
                    (false, true) => "days",
                    (false, false) => "no",
                };
                let tree_exclusions = cut_point.tree.parse_failures
                    + cut_point.tree.oversized_blobs
                    + cut_point.tree.non_utf8_blobs
                    + cut_point.tree.files_over_cap
                    + cut_point.tree.unreadable_blobs;
                out.push_str(&format!(
                    "| {} | `{}` | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                    cut_point.spine_index,
                    cut_point.commit,
                    cut_point.window_commits,
                    cut_point.files_scored,
                    cut_point.defective_files,
                    cut_point.horizon_commits,
                    cut_point.horizon_fix_commits,
                    truncated,
                    cut_point.window_path_overflow_commits
                        + cut_point.horizon_path_overflow_commits,
                    tree_exclusions,
                ));
            }
            out.push('\n');
        }
    }

    fn write_conclusions(&self, out: &mut String) {
        out.push_str("## What H3 may and may not conclude\n\n");
        out.push_str("The `Derived weight` column of the per-fact table is the weight table H3\nshould adopt, and the held-out family result is the accuracy it should expect.\nAny fact whose derived weight is zero measured at or below chance on this\nevidence and must not be given a non-zero weight without new evidence.\n\n");

        let all = self
            .held_out_families
            .iter()
            .find(|result| result.family == FamilySet::All)
            .and_then(|result| result.evaluation.as_ref());
        let graph_only = self
            .held_out_families
            .iter()
            .find(|result| result.family == FamilySet::GraphOnly)
            .and_then(|result| result.evaluation.as_ref());
        if let (Some(all), Some(graph_only)) = (all, graph_only) {
            let verdict = if all.roc_auc_per_mille > graph_only.roc_auc_per_mille {
                "adding git history and complexity facts improved ranking over graph facts alone"
            } else {
                "adding git history and complexity facts did **not** improve ranking over graph facts alone"
            };
            out.push_str(&format!(
                "On this evidence, {}: ROC-AUC {} for graph+git+complexity against {} for\ngraph-only, held out.\n\n",
                verdict,
                decimal(all.roc_auc_per_mille),
                decimal(graph_only.roc_auc_per_mille),
            ));
        }

        out.push_str("Limits on what this evidence supports:\n\n");
        out.push_str(&format!(
            "- Ground truth is a subject-line heuristic. The independent signal judged it **{}**, and it leaves a **{} recall gap** — subjects naming repair work that its prefix-only matching rejects. Every accuracy figure inherits that classifier's error.\n",
            self.audit.verdict.as_str(),
            percentage(self.audit.recall_gap_per_mille)
        ));
        out.push_str(
            "- A defect that was never fixed inside the horizon, or fixed with a subject the classifier does not recognise, is labeled clean. Recall is measured against detected fixes, not against defects.\n",
        );
        out.push_str(&format!(
            "- {} cut points across {} repositories is a small sample. Differences between families smaller than the spread across cut points are not evidence.\n",
            self.pooled_cut_points,
            self.repositories.len()
        ));
        out.push_str(
            "- The measurement is correlational. A fact that ranks defect-prone files well is not thereby a cause of defects, and the score must be presented as evidence, never as a prediction of a specific future failure.\n",
        );
    }
}

/// Render a calibration bucket's rate, or `n/a` when it has no observations.
fn calibration_rate(bucket: &CalibrationBucket) -> String {
    if bucket.observations == 0 {
        "n/a".to_owned()
    } else {
        percentage(bucket.observed_rate_per_mille)
    }
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
