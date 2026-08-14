//! The `git2` replay adapter: the harness's only impure component.
//!
//! # What it does
//!
//! For each cut-point commit `T` it reconstructs what Lattice would have known
//! at `T` and what actually happened afterwards:
//!
//! 1. **Pre-`T` git facts** — [`crate::git_intelligence::GitHistoryMiner`] over
//!    a revwalk pushed at `T`, which by construction reaches only `T`'s
//!    ancestors.
//! 2. **Repository content at `T`** — read from the tree object at `T` through
//!    `git2`, parsed in memory by [`crate::parser::parse_file`]. Graph facts
//!    come from a [`crate::graph::CodeGraph`] built over those parses;
//!    complexity facts from the same blob text.
//! 3. **The post-`T` horizon** — first-parent descendants of `T`, bounded by
//!    [`super::labels::HorizonLimits`], used only to produce labels.
//!
//! # What it never does
//!
//! It performs **no working-tree mutation of any kind**: no checkout, no
//! index write, no temporary materialization of repository content on disk.
//! Every byte it reads comes from the object database. The caller's checkout is
//! opened read-only and left exactly as it was found, which is what makes the
//! harness safe to run against a repository somebody is working in.
//!
//! # Bounds
//!
//! Every walk is clamped, exactly as the git adapter is, and every clamp that
//! actually bites is counted in [`ReplayReport`] so a report can never present
//! a truncated measurement as a complete one: the first-parent spine, the
//! pre-`T` commit window, the horizon, the number of files read from the tree,
//! and the size of any single blob.

use std::collections::BTreeMap;
use std::path::Path;

use git2::{Oid, Repository, TreeWalkMode, TreeWalkResult};
use serde::{Deserialize, Serialize};

use crate::git_intelligence::{
    canonical_repository_path, GitHistoryMiner, GitIntelligenceSnapshot, GitMiningLimits,
};
use crate::git_intelligence_adapter::{commit_changes, GitHistoryAdapter, GitHistoryAdapterError};
use crate::graph::builder::GraphBuilder;
use crate::health::complexity_facts::{compute_file_complexity_facts, FileComplexityFacts};
use crate::health::graph_facts::{GraphFactProducer, GraphFactsSnapshot};
use crate::parser::parse_file;
use crate::symbols::ParsedFile;

use super::features::{FeatureKind, FileFeatureVector};
use super::labels::{
    cut_point_indices, label_defects, select_horizon, DefectLabels, HorizonCommit, HorizonLimits,
};

/// Default number of cut points per repository (spec H1.1).
pub const DEFAULT_CUT_POINTS: usize = 6;
/// Hard ceiling on cut points per repository.
pub const MAX_CUT_POINTS: usize = 64;
/// Default cap on how far back the first-parent spine is walked.
pub const DEFAULT_SPINE_LIMIT: usize = 5_000;
/// Hard ceiling on the first-parent spine walk.
pub const MAX_SPINE_LIMIT: usize = 100_000;
/// Default cap on indexable files read from the tree at a cut point.
pub const DEFAULT_MAX_TREE_FILES: usize = 20_000;
/// Default cap on the size of a single blob read from the tree, in bytes.
pub const DEFAULT_MAX_BLOB_BYTES: usize = 1_048_576;
/// Commits reserved at the root end of the spine so every cut point has some
/// history behind it to mine.
pub const DEFAULT_MINING_RESERVE: usize = 100;

/// Bounds for one replay run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayLimits {
    /// Bounds on the pre-cut-point commit window.
    pub git: GitMiningLimits,
    /// Bounds on the post-cut-point horizon.
    pub horizon: HorizonLimits,
    /// How many cut points to place.
    pub cut_points: usize,
    /// How far back to walk the first-parent spine from `HEAD`.
    pub spine_limit: usize,
    /// Cap on indexable files read from the tree at a cut point.
    pub max_tree_files: usize,
    /// Cap on the size of a single blob read from the tree.
    pub max_blob_bytes: usize,
    /// Commits reserved at the root end of the spine.
    pub mining_reserve: usize,
}

impl Default for ReplayLimits {
    fn default() -> Self {
        Self {
            git: GitMiningLimits::default(),
            horizon: HorizonLimits::default(),
            cut_points: DEFAULT_CUT_POINTS,
            spine_limit: DEFAULT_SPINE_LIMIT,
            max_tree_files: DEFAULT_MAX_TREE_FILES,
            max_blob_bytes: DEFAULT_MAX_BLOB_BYTES,
            mining_reserve: DEFAULT_MINING_RESERVE,
        }
    }
}

impl ReplayLimits {
    /// Clamp caller-supplied bounds to the hard ceilings.
    pub fn bounded(self) -> Self {
        Self {
            git: GitHistoryMiner::with_limits(self.git).limits(),
            horizon: self.horizon.bounded(),
            cut_points: self.cut_points.clamp(1, MAX_CUT_POINTS),
            spine_limit: self.spine_limit.clamp(1, MAX_SPINE_LIMIT),
            max_tree_files: self.max_tree_files.max(1),
            max_blob_bytes: self.max_blob_bytes.max(1),
            mining_reserve: self.mining_reserve,
            ..self
        }
    }
}

/// Failure while replaying a repository's history.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// The repository could not be opened.
    #[error("could not open Git repository at {path}: {source}")]
    Open {
        /// Path that could not be opened.
        path: String,
        /// Underlying `git2` failure.
        #[source]
        source: git2::Error,
    },
    /// The repository has no commits to replay.
    #[error("repository at {path} has no commit history to replay")]
    EmptyHistory {
        /// Path of the empty repository.
        path: String,
    },
    /// The first-parent spine is too short to place a cut point that has both
    /// history to mine and a horizon to label.
    #[error(
        "repository at {path} has only {spine_length} first-parent commits, too few to place a cut point with history behind it and a horizon ahead of it"
    )]
    HistoryTooShort {
        /// Path of the repository.
        path: String,
        /// Length of the first-parent spine that was found.
        spine_length: usize,
    },
    /// A `git2` operation failed while walking history.
    #[error("could not walk history of {path}: {source}")]
    Walk {
        /// Path of the repository.
        path: String,
        /// Underlying `git2` failure.
        #[source]
        source: git2::Error,
    },
    /// An explicitly requested cut point is not on the first-parent spine.
    #[error("commit {commit} in {path} is not on the first-parent spine reachable from HEAD")]
    CutPointOffSpine {
        /// Path of the repository.
        path: String,
        /// The commit that was requested.
        commit: String,
    },
    /// The shared history adapter failed.
    ///
    /// Carries the adapter's rendered message rather than its error type: the
    /// adapter is a crate-private transport seam and must not become part of
    /// this crate's public surface just because the harness calls it.
    #[error("could not read history of {path}: {message}")]
    History {
        /// Path of the repository.
        path: String,
        /// Rendered adapter failure.
        message: String,
    },
}

impl ReplayError {
    /// Wrap an adapter failure against the repository being replayed.
    fn history(path: &str, source: GitHistoryAdapterError) -> Self {
        Self::History {
            path: path.to_owned(),
            message: source.to_string(),
        }
    }
}

/// What a replay read, clamped, and could not use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayReport {
    /// Bounds actually applied, after clamping.
    pub limits: ReplayLimits,
    /// Commits found on the first-parent spine.
    pub spine_length: u32,
    /// The spine walk stopped at `spine_limit` rather than at the root.
    pub spine_truncated: bool,
    /// Cut points actually placed.
    pub cut_points_placed: u32,
    /// The reserves had to be reduced below the horizon and mining defaults
    /// because the history was too short to honour them. Cut points placed
    /// under reduced reserves have shorter horizons or thinner mining windows,
    /// which is a weaker measurement and is reported as such.
    pub reserves_reduced: bool,
    /// Commits reserved at the `HEAD` end, after any reduction.
    pub horizon_reserve: u32,
    /// Commits reserved at the root end, after any reduction.
    pub mining_reserve: u32,
}

/// Accounting for reading one cut point's tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeReadReport {
    /// Indexable files read from the tree.
    pub files_read: u32,
    /// Files that parsed into symbols.
    pub files_parsed: u32,
    /// Files whose parse failed; they contribute no graph or complexity facts.
    pub parse_failures: u32,
    /// Files skipped for exceeding `max_blob_bytes`.
    pub oversized_blobs: u32,
    /// Files skipped for not being valid UTF-8.
    pub non_utf8_blobs: u32,
    /// Files skipped because the tree file cap was reached; when non-zero the
    /// graph is incomplete and the graph facts say so.
    pub files_over_cap: u32,
    /// Blobs `git2` could not read.
    pub unreadable_blobs: u32,
}

impl TreeReadReport {
    /// Whether the tree was read in full.
    pub fn is_complete(&self) -> bool {
        self.parse_failures == 0
            && self.oversized_blobs == 0
            && self.non_utf8_blobs == 0
            && self.files_over_cap == 0
            && self.unreadable_blobs == 0
    }
}

/// One file's features and ground-truth label at one cut point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileObservation {
    /// Canonical repository-relative path.
    pub path: String,
    /// Raw feature values as of the cut point.
    pub features: FileFeatureVector,
    /// Whether a fix-shaped commit touched the file in the horizon.
    pub label: bool,
    /// How many fix-shaped horizon commits touched it.
    pub defect_commits: u32,
}

/// Everything the replay produced for one cut point.
#[derive(Debug, Clone)]
pub struct CutPointReplay {
    /// Position of the cut point on the spine; 0 is `HEAD`.
    pub spine_index: usize,
    /// Cut-point commit id.
    pub commit_id: String,
    /// Cut-point committer timestamp, seconds since the epoch.
    pub committed_at_seconds: i64,
    /// Cut-point commit summary.
    pub subject: String,
    /// Git facts mined from the window before the cut point.
    pub git: GitIntelligenceSnapshot,
    /// Graph facts from the tree at the cut point.
    pub graph: GraphFactsSnapshot,
    /// Complexity facts from the tree at the cut point, by canonical path.
    pub complexity: BTreeMap<String, FileComplexityFacts>,
    /// Ground truth from the horizon after the cut point.
    pub labels: DefectLabels,
    /// Accounting for the tree read.
    pub tree: TreeReadReport,
    /// Per-file features and labels.
    pub observations: Vec<FileObservation>,
    /// Every commit in the pre-cut-point window, for the H1.2 label audit.
    pub window_commits: Vec<AuditCommit>,
}

/// A commit in the pre-cut-point window, reduced to what the label audit needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditCommit {
    /// Commit object id.
    pub id: String,
    /// Commit summary line.
    pub subject: String,
    /// Repository-relative paths the commit touched.
    pub paths: Vec<String>,
}

/// A whole repository's replay across every cut point.
#[derive(Debug, Clone)]
pub struct RepositoryReplay {
    /// Display name of the repository (its directory name).
    pub name: String,
    /// Commit id of `HEAD` at replay time, so a rerun can be tied to the same
    /// history.
    pub head_commit_id: String,
    /// One entry per cut point, ordered from newest to oldest.
    pub cut_points: Vec<CutPointReplay>,
    /// What the replay read and clamped.
    pub report: ReplayReport,
}

/// One commit on the first-parent spine.
struct SpineCommit {
    oid: Oid,
    committed_at_seconds: i64,
    subject: String,
}

/// A replayed repository's identity and accounting, without its cut points.
///
/// Returned by [`replay_repository_streaming`] so a caller that consumes cut
/// points as they are produced still receives everything describing the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaySummary {
    /// Display name of the repository (its directory name).
    pub name: String,
    /// Commit id of `HEAD` at replay time.
    pub head_commit_id: String,
    /// What the replay read and clamped.
    pub report: ReplayReport,
}

/// Replay a repository's history, handing each cut point to `on_cut_point`.
///
/// A [`CutPointReplay`] carries the complete fact snapshots for one moment in
/// a repository's history — every graph symbol, every function's complexity,
/// the whole mined window. For a large repository that is hundreds of
/// megabytes, and holding all six at once was measured at over four gigabytes
/// on one of the Cadres repositories. Handing each cut point to a callback
/// lets a caller extract what it needs and drop the rest, which caps peak
/// memory at a single cut point.
///
/// [`replay_repository`] is the collecting form, kept for callers that
/// genuinely want every cut point at once.
pub fn replay_repository_streaming<F>(
    repository_path: &Path,
    limits: ReplayLimits,
    mut on_cut_point: F,
) -> Result<ReplaySummary, ReplayError>
where
    F: FnMut(CutPointReplay),
{
    let limits = limits.bounded();
    let path_label = repository_path.display().to_string();
    let repository = Repository::open(repository_path).map_err(|source| ReplayError::Open {
        path: path_label.clone(),
        source,
    })?;

    let spine = first_parent_spine(&repository, &path_label, limits.spine_limit)?;
    if spine.is_empty() {
        return Err(ReplayError::EmptyHistory { path: path_label });
    }
    let spine_truncated = spine.len() >= limits.spine_limit;

    // A cut point needs a horizon ahead of it and history behind it. When the
    // repository is too short to honour both defaults, the reserves shrink to a
    // quarter of the spine each rather than the run failing outright — a
    // shorter horizon is a weaker measurement, not an invalid one, and the
    // report says so.
    let mut horizon_reserve = limits.horizon.max_commits;
    let mut mining_reserve = limits.mining_reserve;
    let mut reserves_reduced = false;
    if spine.len() <= horizon_reserve + mining_reserve {
        // Never below one commit on either side: a cut point at `HEAD` has no
        // horizon and would label every file clean, and a cut point at the root
        // has no history to mine. Either would be a measurement of nothing
        // dressed up as a result.
        horizon_reserve = (spine.len() / 4).max(1);
        mining_reserve = (spine.len() / 4).max(1);
        reserves_reduced = true;
    }

    let indices = cut_point_indices(
        spine.len(),
        limits.cut_points,
        horizon_reserve,
        mining_reserve,
    );
    if indices.is_empty() {
        return Err(ReplayError::HistoryTooShort {
            path: path_label,
            spine_length: spine.len(),
        });
    }

    let adapter = GitHistoryAdapter::new(limits.git);
    for index in &indices {
        // Handed over and dropped by the caller before the next one is built.
        on_cut_point(replay_cut_point(
            &repository,
            &path_label,
            &spine,
            *index,
            &adapter,
            limits,
        )?);
    }

    Ok(ReplaySummary {
        name: repository_name(repository_path),
        head_commit_id: spine[0].oid.to_string(),
        report: ReplayReport {
            limits,
            spine_length: spine.len() as u32,
            spine_truncated,
            cut_points_placed: indices.len() as u32,
            reserves_reduced,
            horizon_reserve: horizon_reserve as u32,
            mining_reserve: mining_reserve as u32,
        },
    })
}

/// Replay a repository's history at evenly spaced cut points, collecting them.
///
/// Convenience over [`replay_repository_streaming`]. A caller replaying a
/// large repository should prefer the streaming form: this one holds every cut
/// point's full fact snapshots at once.
pub fn replay_repository(
    repository_path: &Path,
    limits: ReplayLimits,
) -> Result<RepositoryReplay, ReplayError> {
    let mut cut_points = Vec::new();
    let summary = replay_repository_streaming(repository_path, limits, |cut_point| {
        cut_points.push(cut_point)
    })?;
    Ok(RepositoryReplay {
        name: summary.name,
        head_commit_id: summary.head_commit_id,
        cut_points,
        report: summary.report,
    })
}

/// Replay a repository at one explicitly chosen cut-point commit.
///
/// `commit_id` must name a commit on the first-parent spine reachable from
/// `HEAD`; a commit off the spine has no well-defined horizon, so it is
/// rejected rather than silently given the horizon of some nearby commit.
pub fn replay_at_commit(
    repository_path: &Path,
    commit_id: &str,
    limits: ReplayLimits,
) -> Result<CutPointReplay, ReplayError> {
    let limits = limits.bounded();
    let path_label = repository_path.display().to_string();
    let repository = Repository::open(repository_path).map_err(|source| ReplayError::Open {
        path: path_label.clone(),
        source,
    })?;
    let target = repository
        .revparse_single(commit_id)
        .and_then(|object| object.peel_to_commit())
        .map_err(|source| ReplayError::Walk {
            path: path_label.clone(),
            source,
        })?
        .id();

    let spine = first_parent_spine(&repository, &path_label, limits.spine_limit)?;
    if spine.is_empty() {
        return Err(ReplayError::EmptyHistory { path: path_label });
    }
    let index = spine
        .iter()
        .position(|commit| commit.oid == target)
        .ok_or_else(|| ReplayError::CutPointOffSpine {
            path: path_label.clone(),
            commit: target.to_string(),
        })?;

    let adapter = GitHistoryAdapter::new(limits.git);
    replay_cut_point(&repository, &path_label, &spine, index, &adapter, limits)
}

/// Display name for a repository path: its directory name.
///
/// Public so a streaming caller can label a repository before its replay
/// finishes, which is when [`ReplaySummary`] would otherwise supply the name.
pub fn repository_name(repository_path: &Path) -> String {
    repository_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repository")
        .to_owned()
}

/// Walk the first-parent spine from `HEAD` toward the root.
///
/// First-parent is the spec's definition of "the repository's history": it is
/// the sequence of changes that landed on the branch, with each merged
/// side-branch collapsed to the single commit that integrated it.
fn first_parent_spine(
    repository: &Repository,
    path_label: &str,
    spine_limit: usize,
) -> Result<Vec<SpineCommit>, ReplayError> {
    let walk_error = |source: git2::Error| ReplayError::Walk {
        path: path_label.to_owned(),
        source,
    };
    if repository.is_empty().map_err(walk_error)? {
        return Ok(Vec::new());
    }
    let head = repository
        .head()
        .map_err(walk_error)?
        .peel_to_commit()
        .map_err(walk_error)?;

    let mut spine = Vec::new();
    let mut current = Some(head);
    while let Some(commit) = current {
        if spine.len() >= spine_limit {
            break;
        }
        spine.push(SpineCommit {
            oid: commit.id(),
            committed_at_seconds: commit.time().seconds(),
            subject: commit.summary().unwrap_or_default().to_owned(),
        });
        current = commit.parent(0).ok();
    }
    Ok(spine)
}

/// Replay one cut point.
fn replay_cut_point(
    repository: &Repository,
    path_label: &str,
    spine: &[SpineCommit],
    index: usize,
    adapter: &GitHistoryAdapter,
    limits: ReplayLimits,
) -> Result<CutPointReplay, ReplayError> {
    let cut_point = &spine[index];
    let commit_id = cut_point.oid.to_string();

    // --- Everything below the cut point -------------------------------------
    // A revwalk pushed at the cut point reaches only its ancestors, so no
    // post-cut-point commit can enter this window.
    let samples = adapter
        .collect_from(repository, cut_point.oid)
        .map_err(|source| ReplayError::history(path_label, source))?;
    let window_commits: Vec<AuditCommit> = samples
        .iter()
        .map(|sample| AuditCommit {
            id: sample.id.clone(),
            subject: sample.subject.clone(),
            paths: sample
                .changes
                .iter()
                .map(|change| change.path.clone())
                .collect(),
        })
        .collect();
    let git = GitHistoryMiner::with_limits(limits.git).mine(samples);

    // Repository content is read from the tree object at the cut point. A later
    // commit's blobs are not reachable from it.
    let (parsed, complexity, tree) = read_tree(repository, path_label, cut_point.oid, limits)?;
    let graph_snapshot = {
        // The graph owns its own copy of every symbol body, so the parsed files
        // are dead weight the moment it is built. Dropping them before
        // producing facts keeps one copy of a historical codebase in memory
        // rather than two — worth several gigabytes on the larger Cadres
        // repositories.
        let graph = GraphBuilder::build_from_files(parsed.iter());
        drop(parsed);
        GraphFactProducer::default().produce(&graph, tree.is_complete())
    };

    // --- Everything above the cut point -------------------------------------
    // Ordered by increasing distance from the cut point: spine[index - 1] is
    // its immediate first-parent child.
    let mut candidates = Vec::new();
    for offset in (0..index).rev() {
        let descendant = &spine[offset];
        let commit = repository
            .find_commit(descendant.oid)
            .map_err(|source| ReplayError::Walk {
                path: path_label.to_owned(),
                source,
            })?;
        let descendant_id = descendant.oid.to_string();
        let changes = commit_changes(repository, &commit, &descendant_id, limits.git)
            .map_err(|source| ReplayError::history(path_label, source))?;
        candidates.push(HorizonCommit {
            id: descendant_id,
            committed_at_seconds: descendant.committed_at_seconds,
            subject: descendant.subject.clone(),
            changed_paths: changes.iter().map(|change| change.path.clone()).collect(),
            path_overflow: changes.len() > limits.git.paths_per_commit,
        });
    }
    let selection = select_horizon(
        cut_point.committed_at_seconds,
        candidates,
        limits.horizon,
    );
    let labels = label_defects(&selection);

    let observations = build_observations(&git, &graph_snapshot, &complexity, &labels);

    Ok(CutPointReplay {
        spine_index: index,
        commit_id,
        committed_at_seconds: cut_point.committed_at_seconds,
        subject: cut_point.subject.clone(),
        git,
        graph: graph_snapshot,
        complexity,
        labels,
        tree,
        observations,
        window_commits,
    })
}

/// Read every indexable file from the tree at a commit, parse it in memory, and
/// compute its complexity facts.
///
/// Nothing is written to disk. `git2` yields blob contents straight from the
/// object database and [`crate::parser::parse_file`] takes source text, so the
/// whole reconstruction of a historical repository state happens in memory.
fn read_tree(
    repository: &Repository,
    path_label: &str,
    commit_oid: Oid,
    limits: ReplayLimits,
) -> Result<
    (
        Vec<ParsedFile>,
        BTreeMap<String, FileComplexityFacts>,
        TreeReadReport,
    ),
    ReplayError,
> {
    let walk_error = |source: git2::Error| ReplayError::Walk {
        path: path_label.to_owned(),
        source,
    };
    let tree = repository
        .find_commit(commit_oid)
        .map_err(walk_error)?
        .tree()
        .map_err(walk_error)?;

    let mut report = TreeReadReport {
        files_read: 0,
        files_parsed: 0,
        parse_failures: 0,
        oversized_blobs: 0,
        non_utf8_blobs: 0,
        files_over_cap: 0,
        unreadable_blobs: 0,
    };
    let mut sources: BTreeMap<String, String> = BTreeMap::new();

    tree.walk(TreeWalkMode::PreOrder, |root, entry| {
        let Some(name) = entry.name() else {
            // A non-UTF-8 path cannot be a canonical repository path.
            return TreeWalkResult::Ok;
        };
        if entry.kind() == Some(git2::ObjectType::Tree) {
            // Prune whole excluded subtrees rather than filtering their files
            // one at a time, matching the indexer's directory exclusions.
            return if crate::watcher::is_excluded_dir(name) {
                TreeWalkResult::Skip
            } else {
                TreeWalkResult::Ok
            };
        }
        if entry.kind() != Some(git2::ObjectType::Blob) {
            // Submodule links and anything else that is not a blob.
            return TreeWalkResult::Ok;
        }

        let relative_path = format!("{root}{name}");
        if !crate::watcher::should_index_file(&relative_path) {
            return TreeWalkResult::Ok;
        }
        let Some(canonical) = canonical_repository_path(&relative_path) else {
            return TreeWalkResult::Ok;
        };
        if sources.len() >= limits.max_tree_files {
            report.files_over_cap += 1;
            return TreeWalkResult::Ok;
        }

        let Ok(object) = entry.to_object(repository) else {
            report.unreadable_blobs += 1;
            return TreeWalkResult::Ok;
        };
        let Some(blob) = object.as_blob() else {
            report.unreadable_blobs += 1;
            return TreeWalkResult::Ok;
        };
        if blob.size() > limits.max_blob_bytes {
            report.oversized_blobs += 1;
            return TreeWalkResult::Ok;
        }
        match std::str::from_utf8(blob.content()) {
            Ok(text) => {
                report.files_read += 1;
                sources.insert(canonical, text.to_owned());
            }
            Err(_) => report.non_utf8_blobs += 1,
        }
        TreeWalkResult::Ok
    })
    .map_err(walk_error)?;

    let mut parsed = Vec::with_capacity(sources.len());
    let mut complexity = BTreeMap::new();
    // Consume the source map so each file's text is freed as soon as it has
    // been parsed and measured, instead of holding the whole historical
    // codebase alongside the parses of it.
    for (path, source) in std::mem::take(&mut sources) {
        match parse_file(&path, &source) {
            Ok(file) => {
                report.files_parsed += 1;
                parsed.push(file);
            }
            Err(_) => report.parse_failures += 1,
        }
        complexity.insert(path.clone(), compute_file_complexity_facts(&path, &source));
        drop(source);
    }

    Ok((parsed, complexity, report))
}

/// Join the three fact families and the labels into per-file observations.
///
/// The population is every indexable file present in the tree at the cut
/// point, which is the population a health engine would actually score — not
/// only the files that happened to change, which would bias the measurement
/// toward files already known to be active.
fn build_observations(
    git: &GitIntelligenceSnapshot,
    graph: &GraphFactsSnapshot,
    complexity: &BTreeMap<String, FileComplexityFacts>,
    labels: &DefectLabels,
) -> Vec<FileObservation> {
    let mut observations = Vec::with_capacity(complexity.len());
    for path in complexity.keys() {
        let mut features = FileFeatureVector::new(path.clone());

        if let Some(facts) = graph.file(path) {
            features.set(FeatureKind::FanIn, u64::from(facts.fan_in));
            features.set(FeatureKind::FanOut, u64::from(facts.fan_out));
            features.set(FeatureKind::SccSize, u64::from(facts.scc_size));
            features.set(FeatureKind::CycleMember, u64::from(facts.cycle_member));
            features.set_optional(
                FeatureKind::Instability,
                facts.instability_per_mille.map(u64::from),
            );
        }

        match git.file(path) {
            Some(facts) => {
                features.set(FeatureKind::HotspotScore, u64::from(facts.hotspot_score));
                features.set(FeatureKind::BugFixCommits, u64::from(facts.bug_fix_commits));
                features.set(
                    FeatureKind::BugFixDensity,
                    u64::from(facts.bug_fix_density_per_mille),
                );
                features.set(FeatureKind::LineChurn, facts.line_churn);
                features.set(FeatureKind::AuthorCount, u64::from(facts.author_count));
                features.set_optional(
                    FeatureKind::TopAuthorShare,
                    facts.top_author_share_per_mille.map(u64::from),
                );
                features.set_optional(FeatureKind::BusFactor, facts.bus_factor.map(u64::from));
            }
            None => {
                // The file exists at the cut point but no commit in the sampled
                // window touched it. Counts are genuinely zero *for this
                // window*; the ratios have no denominator and stay unknown
                // rather than being invented.
                features.set(FeatureKind::HotspotScore, 0);
                features.set(FeatureKind::BugFixCommits, 0);
                features.set(FeatureKind::LineChurn, 0);
            }
        }

        if let Some(facts) = complexity.get(path) {
            if let Some(rollup) = &facts.rollup {
                features.set_optional(
                    FeatureKind::MaxCyclomaticComplexity,
                    rollup.max_cyclomatic_complexity.map(u64::from),
                );
                features.set_optional(
                    FeatureKind::P90CyclomaticComplexity,
                    rollup.p90_cyclomatic_complexity.map(u64::from),
                );
                features.set_optional(
                    FeatureKind::MaxFunctionLength,
                    rollup.max_function_length.map(u64::from),
                );
                features.set_optional(
                    FeatureKind::MaxNestingDepth,
                    rollup.max_nesting_depth.map(u64::from),
                );
                features.set(
                    FeatureKind::OverThresholdShare,
                    u64::from(rollup.over_threshold_share_per_mille),
                );
                features.set(FeatureKind::FunctionCount, u64::from(rollup.function_count));
            }
        }

        observations.push(FileObservation {
            path: path.clone(),
            features,
            label: labels.is_defective(path),
            defect_commits: labels.defect_commits(path),
        });
    }
    observations
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;
