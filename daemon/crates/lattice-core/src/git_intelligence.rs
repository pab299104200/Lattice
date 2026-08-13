//! Bounded, deterministic aggregation of repository history.
//!
//! The git adapter is deliberately outside this module.  It turns `git2` commits
//! into [`CommitSample`] values, while this module owns the pure aggregation and
//! its invariants.  Keeping those boundaries separate makes the ranking signal
//! testable without shelling out or depending on the caller's repository state.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// The maximum history window read during one refresh.
pub const DEFAULT_HISTORY_LIMIT: usize = 500;

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
}

impl GitIntelligenceSnapshot {
    /// Empty snapshots are honest: no history was available, not zero risk.
    pub fn empty() -> Self {
        Self {
            processed_commits: Vec::new(),
            files: Vec::new(),
            symbols: Vec::new(),
            co_changes: Vec::new(),
        }
    }

    /// Returns the decile cutoff for nonzero file hotness, if one exists.
    ///
    /// A caller should warn only when a file's score is at least this value. The
    /// value is derived from files, never commits, and intentionally excludes
    /// zeroes because no unobserved path belongs in a history-derived warning.
    pub fn top_decile_hotspot_cutoff(&self) -> Option<u32> {
        let mut scores: Vec<u32> = self
            .files
            .iter()
            .map(|file| file.hotspot_score)
            .filter(|score| *score > 0)
            .collect();
        if scores.is_empty() {
            return None;
        }
        scores.sort_unstable_by(|left, right| right.cmp(left));
        let index = ((scores.len() - 1) / 10).min(scores.len() - 1);
        scores.get(index).copied()
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

/// Pure miner with a deliberately bounded history window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitHistoryMiner {
    history_limit: usize,
}

impl Default for GitHistoryMiner {
    fn default() -> Self {
        Self::new(DEFAULT_HISTORY_LIMIT)
    }
}

impl GitHistoryMiner {
    pub fn new(history_limit: usize) -> Self {
        Self {
            // Zero is a useful explicit way for an operator to disable mining.
            history_limit,
        }
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

        for commit in commits.into_iter().take(self.history_limit) {
            if commit.id.trim().is_empty() || !seen_commits.insert(commit.id.clone()) {
                continue;
            }
            processed_commits.push(commit.id);
            let is_bug_fix = looks_like_bug_fix(&commit.subject);
            let mut commit_paths = BTreeSet::new();
            let mut commit_symbols = BTreeSet::new();

            for change in commit.changes {
                let Some(path) = canonical_repository_path(&change.path) else {
                    continue;
                };
                commit_paths.insert(path.clone());
                for symbol in change.symbols {
                    let symbol = symbol.trim();
                    if !symbol.is_empty() {
                        commit_symbols.insert(symbol.to_owned());
                    }
                }
            }

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
            for left_index in 0..paths.len() {
                for right_index in (left_index + 1)..paths.len() {
                    *co_changes
                        .entry((paths[left_index].clone(), paths[right_index].clone()))
                        .or_default() += 1;
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
        self.commits += 1;
        if is_bug_fix {
            self.bug_fix_commits += 1;
        }
        if let Some(author) = author.map(str::trim).filter(|author| !author.is_empty()) {
            *self.known_authors.entry(author.to_owned()).or_default() += 1;
        } else {
            self.unknown_author_commits += 1;
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
        self.commits += 1;
        if is_bug_fix {
            self.bug_fix_commits += 1;
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
    if path.is_empty() || path.starts_with('/') || path.contains('\0') {
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
    use super::*;

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
        assert_eq!(snapshot.processed_commits, vec!["first"]);
        assert_eq!(snapshot.files[0].hotspot_score, 1);
        assert_eq!(snapshot.symbols[0].hotspot_score, 1);
    }

    #[test]
    fn rejects_unsafe_paths_and_keeps_unknown_authorship_honest() {
        let snapshot = GitHistoryMiner::default().mine(vec![commit(
            "one",
            None,
            "patch",
            vec![
                change("/private", &[]),
                change("src/../secret", &[]),
                change("src\\safe.rs", &[]),
            ],
        )]);
        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].path, "src/safe.rs");
        assert_eq!(snapshot.files[0].author_count, 0);
        assert_eq!(snapshot.files[0].bus_factor, None);
        assert_eq!(snapshot.files[0].top_author_share_per_mille, None);
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
