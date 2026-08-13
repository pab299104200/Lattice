//! Bounded, deterministic aggregation of repository history.
//!
//! The git adapter is deliberately outside this module.  It turns `git2` commits
//! into [`CommitSample`] values, while this module owns the pure aggregation and
//! its invariants.  Keeping those boundaries separate makes the ranking signal
//! testable without shelling out or depending on the caller's repository state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::git_intelligence_adapter::{GitHistoryAdapter, GitHistoryAdapterError};

/// The maximum history window read during one refresh.
pub const DEFAULT_HISTORY_LIMIT: usize = 500;
/// Hard ceiling for an operator-configured history window.
pub const MAX_HISTORY_LIMIT: usize = 500;
/// A wider commit is excluded rather than partially contributing file signals.
pub const MAX_PATHS_PER_COMMIT: usize = 20_000;
/// A commit exceeding this bound contributes files but no symbol signals.
pub const MAX_SYMBOLS_PER_COMMIT: usize = 4_096;
/// Wider commits contribute file signals but no co-change pairs.
pub const MAX_CO_CHANGE_WIDTH: usize = 256;
/// A generation exceeding this bound publishes no partial co-change view.
pub const MAX_CO_CHANGE_PAIRS: usize = 250_000;
/// Increment when persisted aggregation semantics change.
pub const AGGREGATION_VERSION: u32 = 1;

/// Effective aggregation bounds persisted alongside a snapshot generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitMiningLimits {
    pub history_limit: usize,
    pub paths_per_commit: usize,
    pub symbols_per_commit: usize,
    pub co_change_width: usize,
    pub co_change_pairs: usize,
}

impl Default for GitMiningLimits {
    fn default() -> Self {
        Self {
            history_limit: DEFAULT_HISTORY_LIMIT,
            paths_per_commit: MAX_PATHS_PER_COMMIT,
            symbols_per_commit: MAX_SYMBOLS_PER_COMMIT,
            co_change_width: MAX_CO_CHANGE_WIDTH,
            co_change_pairs: MAX_CO_CHANGE_PAIRS,
        }
    }
}

impl GitMiningLimits {
    fn bounded(self) -> Self {
        Self {
            history_limit: self.history_limit.min(MAX_HISTORY_LIMIT),
            paths_per_commit: self.paths_per_commit.min(MAX_PATHS_PER_COMMIT),
            symbols_per_commit: self.symbols_per_commit.min(MAX_SYMBOLS_PER_COMMIT),
            co_change_width: self.co_change_width.min(MAX_CO_CHANGE_WIDTH),
            co_change_pairs: self.co_change_pairs.min(MAX_CO_CHANGE_PAIRS),
        }
    }
}

/// Completeness evidence for one pure aggregation run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitMiningReport {
    pub aggregation_version: u32,
    pub limits: GitMiningLimits,
    /// Adapter samples examined, including invalid and replayed samples.
    pub samples_seen: u64,
    /// Distinct valid commit ids admitted to the history window.
    pub sampled_commits: u32,
    /// Sampled commits with complete file observations.
    pub included_commits: u32,
    pub duplicate_commits: u64,
    pub invalid_commit_ids: u64,
    pub invalid_path_entries: u64,
    pub path_overflow_commits: u32,
    pub symbol_overflow_commits: u32,
    pub co_change_width_exclusions: u32,
    /// False means all co-change rows were discarded to avoid a biased view.
    pub co_changes_complete: bool,
}

impl GitMiningReport {
    /// Complete snapshots are the only snapshots eligible to affect consumers.
    /// Degraded aggregates remain useful for diagnostics and a later rebuild.
    pub fn is_complete(&self) -> bool {
        !self.is_degraded()
    }

    pub fn is_degraded(&self) -> bool {
        self.sampled_commits
            != self
                .included_commits
                .saturating_add(self.path_overflow_commits)
            || self.invalid_commit_ids > 0
            || self.invalid_path_entries > 0
            || self.path_overflow_commits > 0
            || self.symbol_overflow_commits > 0
            || self.co_change_width_exclusions > 0
            || !self.co_changes_complete
    }
}

/// A bounded observation of one commit, ordered newest-first by the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitSample {
    /// Immutable object id, used by persistence to make refreshes idempotent.
    pub id: String,
    /// Canonical author identity when Git made one available.
    pub author: Option<String>,
    /// Commit subject only; body text is not retained in the intelligence store.
    pub subject: String,
    /// Paths and optionally parser-resolved symbols changed by this commit.
    pub changes: Vec<PathChange>,
}

/// A file changed in a commit and the symbols the caller could resolve in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathChange {
    /// Repository-relative, slash-separated path. Invalid paths are ignored.
    pub path: String,
    /// Stable symbol keys, if a historical blob could be parsed. Empty is valid.
    pub symbols: Vec<String>,
}

/// Stable history-derived signals consumed by ranking and impact presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitIntelligenceSnapshot {
    /// Commit ids actually examined, newest first, capped by `history_limit`.
    pub processed_commits: Vec<String>,
    pub files: Vec<FileHistorySignal>,
    pub symbols: Vec<SymbolHistorySignal>,
    pub co_changes: Vec<CoChangeSignal>,
    pub report: GitMiningReport,
}

impl GitIntelligenceSnapshot {
    /// Empty snapshots are honest: no history was available, not zero risk.
    pub fn empty() -> Self {
        Self {
            processed_commits: Vec::new(),
            files: Vec::new(),
            symbols: Vec::new(),
            co_changes: Vec::new(),
            report: GitMiningReport {
                aggregation_version: AGGREGATION_VERSION,
                limits: GitMiningLimits::default(),
                samples_seen: 0,
                sampled_commits: 0,
                included_commits: 0,
                duplicate_commits: 0,
                invalid_commit_ids: 0,
                invalid_path_entries: 0,
                path_overflow_commits: 0,
                symbol_overflow_commits: 0,
                co_change_width_exclusions: 0,
                co_changes_complete: true,
            },
        }
    }

    /// The newest commit represented by this snapshot.
    pub fn head_commit_id(&self) -> Option<&str> {
        self.processed_commits.first().map(String::as_str)
    }

    /// Looks up a file signal without requiring consumers to rebuild an index.
    pub fn file(&self, path: &str) -> Option<&FileHistorySignal> {
        let path = canonical_repository_path(path)?;
        self.files
            .binary_search_by(|candidate| candidate.path.cmp(&path))
            .ok()
            .map(|index| &self.files[index])
    }

    /// Looks up an exact stable symbol key.
    pub fn symbol(&self, symbol: &str) -> Option<&SymbolHistorySignal> {
        let symbol = symbol.trim();
        if symbol.is_empty() {
            return None;
        }
        self.symbols
            .binary_search_by(|candidate| candidate.symbol.as_str().cmp(symbol))
            .ok()
            .map(|index| &self.symbols[index])
    }

    /// Looks up an unordered co-change pair when that signal is complete.
    pub fn co_change(&self, left_path: &str, right_path: &str) -> Option<&CoChangeSignal> {
        if self.report.is_degraded() {
            return None;
        }
        let left = canonical_repository_path(left_path)?;
        let right = canonical_repository_path(right_path)?;
        if left == right {
            return None;
        }
        let pair = if left < right {
            (left, right)
        } else {
            (right, left)
        };
        self.co_changes
            .binary_search_by(|candidate| {
                (&candidate.left_path, &candidate.right_path).cmp(&(&pair.0, &pair.1))
            })
            .ok()
            .map(|index| &self.co_changes[index])
    }

    /// Returns bounded co-change partners absent from the supplied current diff.
    pub fn missing_co_change_partners<I, S>(
        &self,
        changed_paths: I,
        minimum_commits: u32,
        limit: usize,
    ) -> Vec<MissingCoChangePartner>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        if self.report.is_degraded() || limit == 0 {
            return Vec::new();
        }
        let changed: BTreeSet<String> = changed_paths
            .into_iter()
            .filter_map(|path| canonical_repository_path(path.as_ref()))
            .collect();
        let mut partners = Vec::new();
        for signal in &self.co_changes {
            let (source_path, partner_path) = match (
                changed.contains(&signal.left_path),
                changed.contains(&signal.right_path),
            ) {
                (true, false) => (&signal.left_path, &signal.right_path),
                (false, true) => (&signal.right_path, &signal.left_path),
                _ => continue,
            };
            if signal.commit_count >= minimum_commits {
                partners.push(MissingCoChangePartner {
                    source_path: source_path.clone(),
                    partner_path: partner_path.clone(),
                    commit_count: signal.commit_count,
                });
            }
        }
        partners.sort_unstable_by(|left, right| {
            right
                .commit_count
                .cmp(&left.commit_count)
                .then_with(|| left.source_path.cmp(&right.source_path))
                .then_with(|| left.partner_path.cmp(&right.partner_path))
        });
        partners.truncate(limit);
        partners
    }

    /// Returns the decile cutoff for nonzero file hotness, if one exists.
    ///
    /// A caller should warn only when a file's score is at least this value. The
    /// value is derived from files, never commits, and intentionally excludes
    /// zeroes because no unobserved path belongs in a history-derived warning.
    pub fn top_decile_hotspot_cutoff(&self) -> Option<u32> {
        if self.report.is_degraded() {
            return None;
        }
        let mut histogram = vec![0_usize; self.processed_commits.len().saturating_add(1)];
        let mut nonzero_files = 0_usize;
        for file in &self.files {
            let score = usize::try_from(file.hotspot_score).ok()?;
            if score == 0 {
                continue;
            }
            // Reject corrupt persisted aggregates instead of allocating from or
            // issuing warnings based on an impossible score.
            if score >= histogram.len() {
                return None;
            }
            histogram[score] = histogram[score].saturating_add(1);
            nonzero_files = nonzero_files.saturating_add(1);
        }
        if nonzero_files == 0 {
            return None;
        }
        let target_index = (nonzero_files - 1) / 10;
        let mut seen = 0_usize;
        for score in (1..histogram.len()).rev() {
            seen = seen.saturating_add(histogram[score]);
            if seen > target_index {
                return u32::try_from(score).ok();
            }
        }
        None
    }
}

/// History-derived signal for one repository-relative file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHistorySignal {
    pub path: String,
    /// Number of distinct sampled commits that touched this path.
    pub hotspot_score: u32,
    pub bug_fix_commits: u32,
    /// Per-mille ratio to avoid non-deterministic floating-point persistence.
    pub bug_fix_density_per_mille: u16,
    /// Distinct known authors in the sampled window.
    pub author_count: u32,
    /// Largest author's share, in per-mille. `None` means all authors unknown.
    pub top_author_share_per_mille: Option<u16>,
    /// A low number means history has little redundancy; it is not a personnel claim.
    pub bus_factor: Option<u32>,
}

/// History-derived signal for a stable symbol key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolHistorySignal {
    pub symbol: String,
    pub hotspot_score: u32,
    pub bug_fix_commits: u32,
    pub author_count: u32,
}

/// Two paths changed together in distinct sampled commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoChangeSignal {
    pub left_path: String,
    pub right_path: String,
    pub commit_count: u32,
}

/// An impact advisory linking a changed path to an unchanged history partner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingCoChangePartner {
    pub source_path: String,
    pub partner_path: String,
    pub commit_count: u32,
}

/// Pure miner with a deliberately bounded history window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitHistoryMiner {
    limits: GitMiningLimits,
}

impl Default for GitHistoryMiner {
    fn default() -> Self {
        Self::new(DEFAULT_HISTORY_LIMIT)
    }
}

/// The result of one bounded repository-history read and aggregation pass.
///
/// The snapshot owns the complete report so a caller cannot accidentally pair
/// signals from one generation with completeness evidence from another. This
/// wrapper repeats the report for callers which only need to record or surface
/// refresh health without traversing the signals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepositoryGitMiningResult {
    pub snapshot: GitIntelligenceSnapshot,
    pub report: GitMiningReport,
}

/// Reads and aggregates the bounded history reachable from `repository_path`.
///
/// This is the crate-internal integration seam between the pure miner and the
/// local `git2` adapter. It deliberately performs no storage, scheduling, or
/// ranking work. The configured limits are normalized once before extraction
/// and then used for aggregation, so the returned report always records the
/// effective limits that actually constrained repository traversal.
pub(crate) fn mine_repository(
    repository_path: &Path,
    limits: GitMiningLimits,
) -> Result<RepositoryGitMiningResult, GitHistoryAdapterError> {
    let miner = GitHistoryMiner::with_limits(limits);
    let samples = GitHistoryAdapter::new(miner.limits()).collect(repository_path)?;
    let snapshot = miner.mine(samples);
    let report = snapshot.report.clone();
    Ok(RepositoryGitMiningResult { snapshot, report })
}

impl GitHistoryMiner {
    pub fn new(history_limit: usize) -> Self {
        Self::with_limits(GitMiningLimits {
            history_limit,
            ..GitMiningLimits::default()
        })
    }

    /// Accepts lower test/operator bounds while enforcing every hard ceiling.
    pub fn with_limits(limits: GitMiningLimits) -> Self {
        Self {
            limits: limits.bounded(),
        }
    }

    pub fn limits(&self) -> GitMiningLimits {
        self.limits
    }

    /// Aggregates at most `history_limit` samples in caller-provided order.
    ///
    /// Duplicate paths and symbols inside a commit count once. Duplicate commit
    /// ids are ignored after their first observation, which makes a replayed
    /// adapter page harmless and keeps persisted commit keys truthful.
    pub fn mine<I>(&self, commits: I) -> GitIntelligenceSnapshot
    where
        I: IntoIterator<Item = CommitSample>,
    {
        let mut processed_commits = Vec::new();
        let mut seen_commits = BTreeSet::new();
        let mut files: BTreeMap<String, FileAccumulator> = BTreeMap::new();
        let mut symbols: BTreeMap<String, SymbolAccumulator> = BTreeMap::new();
        let mut co_changes: BTreeMap<(String, String), u32> = BTreeMap::new();
        let mut report = GitMiningReport {
            aggregation_version: AGGREGATION_VERSION,
            limits: self.limits,
            samples_seen: 0,
            sampled_commits: 0,
            included_commits: 0,
            duplicate_commits: 0,
            invalid_commit_ids: 0,
            invalid_path_entries: 0,
            path_overflow_commits: 0,
            symbol_overflow_commits: 0,
            co_change_width_exclusions: 0,
            co_changes_complete: true,
        };

        for commit in commits {
            if processed_commits.len() >= self.limits.history_limit {
                break;
            }
            report.samples_seen = report.samples_seen.saturating_add(1);
            let commit_id = commit.id.trim();
            if commit_id.is_empty() {
                report.invalid_commit_ids = report.invalid_commit_ids.saturating_add(1);
                continue;
            }
            if !seen_commits.insert(commit_id.to_owned()) {
                report.duplicate_commits = report.duplicate_commits.saturating_add(1);
                continue;
            }
            processed_commits.push(commit_id.to_owned());
            report.sampled_commits = report.sampled_commits.saturating_add(1);
            let is_bug_fix = looks_like_bug_fix(&commit.subject);
            let mut commit_paths = BTreeSet::new();
            let mut commit_symbols = BTreeSet::new();
            let mut path_overflow = false;
            let mut symbol_overflow = false;

            for change in commit.changes {
                let Some(path) = canonical_repository_path(&change.path) else {
                    report.invalid_path_entries = report.invalid_path_entries.saturating_add(1);
                    continue;
                };
                commit_paths.insert(path);
                if commit_paths.len() > self.limits.paths_per_commit {
                    path_overflow = true;
                    break;
                }
                for symbol in change.symbols {
                    let symbol = symbol.trim();
                    if !symbol.is_empty() && !symbol_overflow {
                        commit_symbols.insert(symbol.to_owned());
                        if commit_symbols.len() > self.limits.symbols_per_commit {
                            commit_symbols.clear();
                            symbol_overflow = true;
                        }
                    }
                }
            }

            if path_overflow {
                report.path_overflow_commits = report.path_overflow_commits.saturating_add(1);
                continue;
            }
            if symbol_overflow {
                report.symbol_overflow_commits = report.symbol_overflow_commits.saturating_add(1);
            }
            report.included_commits = report.included_commits.saturating_add(1);

            for path in &commit_paths {
                files
                    .entry(path.clone())
                    .or_default()
                    .record(commit.author.as_deref(), is_bug_fix);
            }
            for symbol in &commit_symbols {
                symbols
                    .entry(symbol.clone())
                    .or_default()
                    .record(commit.author.as_deref(), is_bug_fix);
            }
            let paths: Vec<_> = commit_paths.into_iter().collect();
            if paths.len() > self.limits.co_change_width {
                report.co_change_width_exclusions =
                    report.co_change_width_exclusions.saturating_add(1);
                continue;
            }
            if !report.co_changes_complete {
                continue;
            }
            for left_index in 0..paths.len() {
                for right_index in (left_index + 1)..paths.len() {
                    let pair = (paths[left_index].clone(), paths[right_index].clone());
                    if !co_changes.contains_key(&pair)
                        && co_changes.len() >= self.limits.co_change_pairs
                    {
                        co_changes.clear();
                        report.co_changes_complete = false;
                        break;
                    }
                    let count = co_changes.entry(pair).or_default();
                    *count = count.saturating_add(1);
                }
                if !report.co_changes_complete {
                    break;
                }
            }
        }

        GitIntelligenceSnapshot {
            processed_commits,
            files: files
                .into_iter()
                .map(|(path, stats)| stats.into_signal(path))
                .collect(),
            symbols: symbols
                .into_iter()
                .map(|(symbol, stats)| stats.into_signal(symbol))
                .collect(),
            co_changes: co_changes
                .into_iter()
                .map(|((left_path, right_path), commit_count)| CoChangeSignal {
                    left_path,
                    right_path,
                    commit_count,
                })
                .collect(),
            report,
        }
    }
}

#[derive(Default)]
struct FileAccumulator {
    commits: u32,
    bug_fix_commits: u32,
    known_authors: BTreeMap<String, u32>,
    unknown_author_commits: u32,
}

impl FileAccumulator {
    fn record(&mut self, author: Option<&str>, is_bug_fix: bool) {
        self.commits = self.commits.saturating_add(1);
        if is_bug_fix {
            self.bug_fix_commits = self.bug_fix_commits.saturating_add(1);
        }
        if let Some(author) = author.map(str::trim).filter(|author| !author.is_empty()) {
            let count = self.known_authors.entry(author.to_owned()).or_default();
            *count = count.saturating_add(1);
        } else {
            self.unknown_author_commits = self.unknown_author_commits.saturating_add(1);
        }
    }

    fn into_signal(self, path: String) -> FileHistorySignal {
        let author_count = self.known_authors.len() as u32;
        let top_author_commits = self.known_authors.values().copied().max().unwrap_or(0);
        let known_author_commits = self.known_authors.values().copied().sum();
        FileHistorySignal {
            path,
            hotspot_score: self.commits,
            bug_fix_commits: self.bug_fix_commits,
            bug_fix_density_per_mille: ratio_per_mille(self.bug_fix_commits, self.commits),
            author_count,
            top_author_share_per_mille: (author_count > 0)
                .then(|| ratio_per_mille(top_author_commits, known_author_commits)),
            bus_factor: (author_count > 0 && self.unknown_author_commits == 0)
                .then(|| estimated_bus_factor(&self.known_authors)),
        }
    }
}

/// Minimum contributors responsible for a strict majority of known commits.
///
/// This is deliberately absent when one or more commits have an unknown author:
/// inferring a bus factor from incomplete attribution would be a false claim.
fn estimated_bus_factor(authors: &BTreeMap<String, u32>) -> u32 {
    let total: u32 = authors.values().copied().sum();
    let mut contributions: Vec<u32> = authors.values().copied().collect();
    contributions.sort_unstable_by(|left, right| right.cmp(left));
    let target = total / 2 + 1;
    let mut covered = 0;
    for (index, commits) in contributions.into_iter().enumerate() {
        covered += commits;
        if covered >= target {
            return (index + 1) as u32;
        }
    }
    0
}

#[derive(Default)]
struct SymbolAccumulator {
    commits: u32,
    bug_fix_commits: u32,
    known_authors: BTreeSet<String>,
}

impl SymbolAccumulator {
    fn record(&mut self, author: Option<&str>, is_bug_fix: bool) {
        self.commits = self.commits.saturating_add(1);
        if is_bug_fix {
            self.bug_fix_commits = self.bug_fix_commits.saturating_add(1);
        }
        if let Some(author) = author.map(str::trim).filter(|author| !author.is_empty()) {
            self.known_authors.insert(author.to_owned());
        }
    }

    fn into_signal(self, symbol: String) -> SymbolHistorySignal {
        SymbolHistorySignal {
            symbol,
            hotspot_score: self.commits,
            bug_fix_commits: self.bug_fix_commits,
            author_count: self.known_authors.len() as u32,
        }
    }
}

fn ratio_per_mille(numerator: u32, denominator: u32) -> u16 {
    if denominator == 0 {
        return 0;
    }
    ((u64::from(numerator) * 1_000) / u64::from(denominator)).min(u64::from(u16::MAX)) as u16
}

/// Detects conventional fix-shaped subjects without retaining commit bodies.
pub fn looks_like_bug_fix(subject: &str) -> bool {
    let normalized = subject.trim_start().to_ascii_lowercase();
    ["fix", "bug", "hotfix", "patch"].into_iter().any(|label| {
        normalized.strip_prefix(label).is_some_and(|suffix| {
            suffix
                .chars()
                .next()
                .is_none_or(|next| !next.is_alphanumeric())
        })
    }) || normalized
        .split(|character: char| !character.is_alphanumeric())
        .any(|word| word == "regression")
}

/// Canonicalizes a repository-relative path or rejects unsafe/ambiguous input.
pub fn canonical_repository_path(path: &str) -> Option<String> {
    let path = path.trim().replace('\\', "/");
    let has_windows_drive_prefix = path
        .as_bytes()
        .get(..2)
        .is_some_and(|prefix| prefix[0].is_ascii_alphabetic() && prefix[1] == b':');
    if path.is_empty() || path.starts_with('/') || has_windows_drive_prefix || path.contains('\0') {
        return None;
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => continue,
            ".." => return None,
            value => parts.push(value),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use git2::{IndexAddOption, Repository, Signature};
    use tempfile::TempDir;

    use super::*;

    fn repository_fixture() -> (TempDir, Repository) {
        let directory = tempfile::tempdir().expect("create fixture directory");
        let repository = Repository::init(directory.path()).expect("initialize fixture repository");
        (directory, repository)
    }

    fn commit_fixture_file(repository: &Repository, path: &str, contents: &str, subject: &str) {
        let workdir = repository.workdir().expect("fixture has workdir");
        let file = workdir.join(path);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).expect("create fixture parent");
        }
        fs::write(file, contents).expect("write fixture file");

        let mut index = repository.index().expect("open fixture index");
        index
            .add_all([path], IndexAddOption::DEFAULT, None)
            .expect("stage fixture file");
        index.write().expect("write fixture index");
        let tree_id = index.write_tree().expect("write fixture tree");
        let tree = repository.find_tree(tree_id).expect("read fixture tree");
        let signature = Signature::now("Facade Fixture", "facade@example.test")
            .expect("create fixture signature");
        let parents = repository
            .head()
            .ok()
            .and_then(|head| head.target())
            .and_then(|oid| repository.find_commit(oid).ok())
            .into_iter()
            .collect::<Vec<_>>();
        let parent_refs = parents.iter().collect::<Vec<_>>();
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                subject,
                &tree,
                &parent_refs,
            )
            .expect("create fixture commit");
    }

    fn change(path: &str, symbols: &[&str]) -> PathChange {
        PathChange {
            path: path.to_owned(),
            symbols: symbols.iter().map(|symbol| (*symbol).to_owned()).collect(),
        }
    }

    fn commit(
        id: &str,
        author: Option<&str>,
        subject: &str,
        changes: Vec<PathChange>,
    ) -> CommitSample {
        CommitSample {
            id: id.to_owned(),
            author: author.map(str::to_owned),
            subject: subject.to_owned(),
            changes,
        }
    }

    #[test]
    fn repository_facade_uses_effective_limits_and_preserves_adapter_errors() {
        let (directory, repository) = repository_fixture();
        commit_fixture_file(
            &repository,
            "src/first.rs",
            "first",
            "Initial implementation",
        );
        commit_fixture_file(
            &repository,
            "src/second.rs",
            "second",
            "Fix parser regression",
        );

        let result = mine_repository(
            directory.path(),
            GitMiningLimits {
                history_limit: 1,
                paths_per_commit: usize::MAX,
                symbols_per_commit: usize::MAX,
                co_change_width: usize::MAX,
                co_change_pairs: usize::MAX,
            },
        )
        .expect("mine fixture repository");

        assert_eq!(result.report, result.snapshot.report);
        assert_eq!(result.report.limits.history_limit, 1);
        assert_eq!(result.report.limits.paths_per_commit, MAX_PATHS_PER_COMMIT);
        assert_eq!(
            result.report.limits.symbols_per_commit,
            MAX_SYMBOLS_PER_COMMIT
        );
        assert_eq!(result.report.limits.co_change_width, MAX_CO_CHANGE_WIDTH);
        assert_eq!(result.report.limits.co_change_pairs, MAX_CO_CHANGE_PAIRS);
        assert_eq!(result.snapshot.processed_commits.len(), 1);
        assert_eq!(result.snapshot.files.len(), 1);
        assert_eq!(result.snapshot.files[0].path, "src/second.rs");
        assert_eq!(result.snapshot.files[0].bug_fix_commits, 1);

        let missing = directory.path().join("missing-repository");
        let error = mine_repository(&missing, GitMiningLimits::default())
            .expect_err("missing repository must preserve the adapter open error");
        assert!(matches!(error, GitHistoryAdapterError::Open { .. }));
    }

    #[test]
    fn mines_fixture_history_deterministically() {
        let snapshot = GitHistoryMiner::default().mine(vec![
            commit(
                "c3",
                Some("A"),
                "Fix parser regression",
                vec![
                    change("src/a.rs", &["parse"]),
                    change("src/b.rs", &["read"]),
                ],
            ),
            commit(
                "c2",
                Some("B"),
                "feature",
                vec![change("src/a.rs", &["parse"]), change("src/b.rs", &[])],
            ),
            commit(
                "c1",
                Some("A"),
                "refactor",
                vec![change("src/a.rs", &["render"])],
            ),
        ]);

        assert_eq!(snapshot.processed_commits, vec!["c3", "c2", "c1"]);
        assert_eq!(snapshot.files[0].path, "src/a.rs");
        assert_eq!(snapshot.files[0].hotspot_score, 3);
        assert_eq!(snapshot.files[0].bug_fix_density_per_mille, 333);
        assert_eq!(snapshot.files[0].author_count, 2);
        assert_eq!(snapshot.files[0].bus_factor, Some(1));
        assert_eq!(
            snapshot.co_changes,
            vec![CoChangeSignal {
                left_path: "src/a.rs".to_owned(),
                right_path: "src/b.rs".to_owned(),
                commit_count: 2
            }]
        );
        assert_eq!(snapshot.symbols[0].symbol, "parse");
        assert_eq!(snapshot.symbols[0].hotspot_score, 2);
    }

    #[test]
    fn bounds_history_and_deduplicates_within_and_across_commits() {
        let snapshot = GitHistoryMiner::new(2).mine(vec![
            commit(
                "first",
                Some("A"),
                "fix",
                vec![change("src/a.rs", &["a", "a"]), change("src/a.rs", &["a"])],
            ),
            commit("first", Some("B"), "fix", vec![change("src/b.rs", &[])]),
            commit("third", Some("C"), "fix", vec![change("src/c.rs", &[])]),
        ]);
        assert_eq!(snapshot.processed_commits, vec!["first", "third"]);
        assert_eq!(snapshot.report.samples_seen, 3);
        assert_eq!(snapshot.report.sampled_commits, 2);
        assert_eq!(snapshot.report.included_commits, 2);
        assert_eq!(snapshot.report.duplicate_commits, 1);
        assert_eq!(snapshot.files.len(), 2);
        assert_eq!(snapshot.files[0].hotspot_score, 1);
        assert_eq!(snapshot.symbols[0].hotspot_score, 1);
    }

    #[test]
    fn clamps_all_configurable_limits_to_hard_ceilings() {
        let miner = GitHistoryMiner::with_limits(GitMiningLimits {
            history_limit: usize::MAX,
            paths_per_commit: usize::MAX,
            symbols_per_commit: usize::MAX,
            co_change_width: usize::MAX,
            co_change_pairs: usize::MAX,
        });

        assert_eq!(
            miner.limits(),
            GitMiningLimits {
                history_limit: MAX_HISTORY_LIMIT,
                paths_per_commit: MAX_PATHS_PER_COMMIT,
                symbols_per_commit: MAX_SYMBOLS_PER_COMMIT,
                co_change_width: MAX_CO_CHANGE_WIDTH,
                co_change_pairs: MAX_CO_CHANGE_PAIRS,
            }
        );
    }

    #[test]
    fn excludes_an_overwide_commit_without_partial_file_signals() {
        let miner = GitHistoryMiner::with_limits(GitMiningLimits {
            paths_per_commit: 1,
            ..GitMiningLimits::default()
        });
        let snapshot = miner.mine(vec![commit(
            "wide",
            Some("A"),
            "fix",
            vec![change("src/a.rs", &["a"]), change("src/b.rs", &["b"])],
        )]);

        assert_eq!(snapshot.processed_commits, vec!["wide"]);
        assert!(snapshot.files.is_empty());
        assert!(snapshot.symbols.is_empty());
        assert_eq!(snapshot.report.path_overflow_commits, 1);
        assert_eq!(snapshot.report.sampled_commits, 1);
        assert_eq!(snapshot.report.included_commits, 0);
        assert!(snapshot.report.is_degraded());
    }

    #[test]
    fn symbol_overflow_preserves_complete_file_observations() {
        let miner = GitHistoryMiner::with_limits(GitMiningLimits {
            symbols_per_commit: 1,
            ..GitMiningLimits::default()
        });
        let snapshot = miner.mine(vec![commit(
            "symbols",
            Some("A"),
            "feature",
            vec![change("src/a.rs", &["a", "b"])],
        )]);

        assert_eq!(snapshot.files.len(), 1);
        assert!(snapshot.symbols.is_empty());
        assert_eq!(snapshot.report.symbol_overflow_commits, 1);
        assert!(snapshot.report.is_degraded());
    }

    #[test]
    fn co_change_bounds_never_publish_a_biased_partial_set() {
        let width_limited = GitHistoryMiner::with_limits(GitMiningLimits {
            co_change_width: 1,
            ..GitMiningLimits::default()
        })
        .mine(vec![commit(
            "wide-pair",
            Some("A"),
            "feature",
            vec![change("a", &[]), change("b", &[])],
        )]);
        assert_eq!(width_limited.files.len(), 2);
        assert!(width_limited.co_changes.is_empty());
        assert_eq!(width_limited.report.co_change_width_exclusions, 1);
        assert!(width_limited.report.is_degraded());

        let pair_limited = GitHistoryMiner::with_limits(GitMiningLimits {
            co_change_pairs: 1,
            ..GitMiningLimits::default()
        })
        .mine(vec![commit(
            "too-many-pairs",
            Some("A"),
            "feature",
            vec![change("a", &[]), change("b", &[]), change("c", &[])],
        )]);
        assert!(pair_limited.co_changes.is_empty());
        assert!(!pair_limited.report.co_changes_complete);
        assert!(pair_limited.report.is_degraded());
    }

    #[test]
    fn snapshot_lookups_and_missing_partners_are_bounded_and_stable() {
        let snapshot = GitHistoryMiner::default().mine(vec![
            commit(
                "c3",
                Some("A"),
                "feature",
                vec![change("src/a.rs", &["a"]), change("src/c.rs", &[])],
            ),
            commit(
                "c2",
                Some("B"),
                "feature",
                vec![change("src/a.rs", &["a"]), change("src/b.rs", &[])],
            ),
            commit(
                "c1",
                Some("A"),
                "feature",
                vec![change("src/a.rs", &["a"]), change("src/b.rs", &[])],
            ),
        ]);

        assert_eq!(snapshot.head_commit_id(), Some("c3"));
        assert_eq!(snapshot.file("./src\\a.rs").unwrap().hotspot_score, 3);
        assert_eq!(snapshot.symbol(" a ").unwrap().hotspot_score, 3);
        assert_eq!(
            snapshot
                .co_change("src/b.rs", "src/a.rs")
                .unwrap()
                .commit_count,
            2
        );
        assert_eq!(
            snapshot.missing_co_change_partners(["src/a.rs"], 1, 1),
            vec![MissingCoChangePartner {
                source_path: "src/a.rs".to_owned(),
                partner_path: "src/b.rs".to_owned(),
                commit_count: 2,
            }]
        );
    }

    #[test]
    fn replayed_input_produces_identical_snapshots() {
        let history = vec![
            commit("c2", Some("A"), "fix: x", vec![change("b", &["b"])]),
            commit("c1", Some("B"), "feature", vec![change("a", &["a"])]),
        ];
        let miner = GitHistoryMiner::default();

        assert_eq!(miner.mine(history.clone()), miner.mine(history));
    }

    #[test]
    fn hotspot_cutoff_uses_a_bounded_histogram_and_rejects_corrupt_scores() {
        let changes: Vec<PathChange> = (0..11)
            .map(|index| change(&format!("src/{index}.rs"), &[]))
            .collect();
        let mut snapshot = GitHistoryMiner::default().mine(vec![
            commit("c2", Some("A"), "feature", changes),
            commit("c1", Some("A"), "feature", vec![change("src/0.rs", &[])]),
        ]);

        // Two files form the top decile of an eleven-file population.
        assert_eq!(snapshot.top_decile_hotspot_cutoff(), Some(1));
        snapshot.files[0].hotspot_score = 3;
        assert_eq!(snapshot.top_decile_hotspot_cutoff(), None);
    }

    #[test]
    fn rejects_unsafe_paths_and_keeps_unknown_authorship_honest() {
        let snapshot = GitHistoryMiner::default().mine(vec![
            commit(" ", None, "patch", vec![change("ignored", &[])]),
            commit(
                "one",
                None,
                "patch",
                vec![
                    change("/private", &[]),
                    change("C:\\private", &[]),
                    change("src/../secret", &[]),
                    change("src\\safe.rs", &[]),
                ],
            ),
        ]);
        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].path, "src/safe.rs");
        assert_eq!(snapshot.files[0].author_count, 0);
        assert_eq!(snapshot.files[0].bus_factor, None);
        assert_eq!(snapshot.files[0].top_author_share_per_mille, None);
        assert_eq!(snapshot.report.invalid_commit_ids, 1);
        assert_eq!(snapshot.report.invalid_path_entries, 3);
        assert!(snapshot.top_decile_hotspot_cutoff().is_none());
    }

    #[test]
    fn recognizes_fix_subjects_without_false_positive_prefixes() {
        assert!(looks_like_bug_fix("fix: parser"));
        assert!(looks_like_bug_fix("bug(parser): bounds"));
        assert!(looks_like_bug_fix("hotfix"));
        assert!(looks_like_bug_fix("Regression in indexing"));
        assert!(!looks_like_bug_fix("prefix cleanup"));
        assert!(!looks_like_bug_fix("fixture cleanup"));
        assert!(!looks_like_bug_fix("bugbear cleanup"));
        assert!(!looks_like_bug_fix("patchwork cleanup"));
        assert!(!looks_like_bug_fix("nonregression cleanup"));
        assert!(!looks_like_bug_fix("feature: repair docs"));
    }
}
