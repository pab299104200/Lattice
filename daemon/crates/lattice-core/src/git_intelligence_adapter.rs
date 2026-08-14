//! Bounded `git2` history extraction for [`crate::git_intelligence`].
//!
//! This adapter deliberately does not aggregate or persist history signals. It
//! only turns the newest reachable commits into deterministic `CommitSample`
//! values for `GitHistoryMiner`. Keeping git IO here lets the miner remain a
//! pure, replayable component and gives the runtime one narrow integration seam.

use std::collections::BTreeMap;
use std::path::Path;

use git2::{Delta, Diff, DiffOptions, Patch, Repository, Sort};

use crate::git_intelligence::{CommitSample, GitMiningLimits, PathChange};

/// Failure while opening or reading the repository history.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GitHistoryAdapterError {
    #[error("could not open Git repository at {path}: {source}")]
    Open {
        path: String,
        #[source]
        source: git2::Error,
    },
    #[error("could not walk Git history: {0}")]
    Walk(#[source] git2::Error),
    #[error("could not read commit {commit}: {source}")]
    Commit {
        commit: String,
        #[source]
        source: git2::Error,
    },
    #[error("could not diff commit {commit}: {source}")]
    Diff {
        commit: String,
        #[source]
        source: git2::Error,
    },
}

/// Reads a bounded newest-first history page from a repository.
///
/// Paths are intentionally collected without symbol data: historical blobs
/// require parser/version policy that belongs to the runtime integration, not
/// this transport adapter. The pure miner records a complete file-only signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GitHistoryAdapter {
    limits: GitMiningLimits,
}

impl GitHistoryAdapter {
    pub(crate) fn new(limits: GitMiningLimits) -> Self {
        // `GitHistoryMiner::with_limits` is the authority that clamps limits.
        // Reuse its public value so extraction cannot exceed its aggregation
        // window or admit more than one overflow sentinel per commit.
        let limits = crate::git_intelligence::GitHistoryMiner::with_limits(limits).limits();
        Self { limits }
    }

    /// Opens `repository_path` and returns newest-first samples for the miner.
    pub(crate) fn collect(
        &self,
        repository_path: &Path,
    ) -> Result<Vec<CommitSample>, GitHistoryAdapterError> {
        let repository =
            Repository::open(repository_path).map_err(|source| GitHistoryAdapterError::Open {
                path: repository_path.display().to_string(),
                source,
            })?;
        self.collect_repository(&repository)
    }

    fn collect_repository(
        &self,
        repository: &Repository,
    ) -> Result<Vec<CommitSample>, GitHistoryAdapterError> {
        let mut walk = repository.revwalk().map_err(GitHistoryAdapterError::Walk)?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)
            .map_err(GitHistoryAdapterError::Walk)?;
        if repository
            .is_empty()
            .map_err(GitHistoryAdapterError::Walk)?
        {
            return Ok(Vec::new());
        }
        walk.push_head().map_err(GitHistoryAdapterError::Walk)?;

        let mut samples = Vec::with_capacity(self.limits.history_limit);
        for oid in walk.take(self.limits.history_limit) {
            let oid = oid.map_err(GitHistoryAdapterError::Walk)?;
            let commit_id = oid.to_string();
            let commit =
                repository
                    .find_commit(oid)
                    .map_err(|source| GitHistoryAdapterError::Commit {
                        commit: commit_id.clone(),
                        source,
                    })?;
            let changes = commit_changes(repository, &commit, &commit_id, self.limits)?;
            samples.push(CommitSample {
                id: commit_id,
                author: author_identity(&commit),
                subject: commit.summary().unwrap_or_default().to_owned(),
                changes,
            });
        }
        Ok(samples)
    }
}

fn author_identity(commit: &git2::Commit<'_>) -> Option<String> {
    let signature = commit.author();
    match (signature.name(), signature.email()) {
        (Some(name), Some(email)) if !name.trim().is_empty() && !email.trim().is_empty() => {
            Some(format!("{} <{}>", name.trim(), email.trim()))
        }
        (Some(name), _) if !name.trim().is_empty() => Some(name.trim().to_owned()),
        (_, Some(email)) if !email.trim().is_empty() => Some(email.trim().to_owned()),
        _ => None,
    }
}

fn commit_changes(
    repository: &Repository,
    commit: &git2::Commit<'_>,
    commit_id: &str,
    limits: GitMiningLimits,
) -> Result<Vec<PathChange>, GitHistoryAdapterError> {
    let tree = commit
        .tree()
        .map_err(|source| GitHistoryAdapterError::Commit {
            commit: commit_id.to_owned(),
            source,
        })?;
    let parent_tree = if commit.parent_count() == 0 {
        None
    } else {
        Some(
            commit
                .parent(0)
                .and_then(|parent| parent.tree())
                .map_err(|source| GitHistoryAdapterError::Commit {
                    commit: commit_id.to_owned(),
                    source,
                })?,
        )
    };
    let mut options = DiffOptions::new();
    // Disable expensive similarity detection: a history signal needs changed
    // paths, not heuristic rename attribution. libgit2's default is already
    // disabled; this keeps that policy explicit at this boundary.
    options.include_typechange(true);
    let diff = repository
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut options))
        .map_err(|source| GitHistoryAdapterError::Diff {
            commit: commit_id.to_owned(),
            source,
        })?;

    let mut paths: BTreeMap<String, LineStats> = BTreeMap::new();
    // Keep one extra valid path so the pure miner can mark this commit as an
    // overflow instead of silently publishing a partial signal.
    let maximum_observations = limits.paths_per_commit.saturating_add(1);
    'deltas: for (index, delta) in diff.deltas().enumerate() {
        // Diff stats only (H2.5, docs/plans/2026-08-13-health-engine.md): this
        // reads added/deleted line counts from the already-computed diff. No
        // blob content, blame, or rename similarity is retained beyond the
        // counts below — the same narrow boundary as the path extraction
        // above, deliberately widened by exactly one signal.
        let line_stats = delta_line_stats(&diff, index);
        for path in delta_paths(delta.status(), &delta) {
            let Some(path) = path else {
                continue;
            };
            let entry = paths.entry(path.to_owned()).or_insert(LineStats::default());
            entry.added = entry.added.saturating_add(line_stats.added);
            entry.deleted = entry.deleted.saturating_add(line_stats.deleted);
            if paths.len() >= maximum_observations {
                break 'deltas;
            }
        }
    }

    Ok(paths
        .into_iter()
        .map(|(path, stats)| PathChange {
            path,
            symbols: Vec::new(),
            lines_added: stats.added,
            lines_deleted: stats.deleted,
        })
        .collect())
}

/// Added/deleted line counts for one diff delta, diff stats only.
#[derive(Debug, Clone, Copy, Default)]
struct LineStats {
    added: u32,
    deleted: u32,
}

/// Reads per-delta line counts from an already-computed diff.
///
/// A patch is required to obtain per-file counts (`Diff::stats` is
/// diff-wide only), but only the resulting counts are kept; the patch value
/// itself is dropped at the end of this call. Binary deltas and any delta
/// libgit2 cannot turn into a patch (rare, e.g. degenerate typechanges)
/// contribute zero rather than failing the whole commit: a missing line-count
/// for one file must not discard that commit's hotspot signal for every file.
fn delta_line_stats(diff: &Diff<'_>, index: usize) -> LineStats {
    let patch = match Patch::from_diff(diff, index) {
        Ok(Some(patch)) => patch,
        Ok(None) | Err(_) => return LineStats::default(),
    };
    match patch.line_stats() {
        Ok((_context, insertions, deletions)) => LineStats {
            added: u32::try_from(insertions).unwrap_or(u32::MAX),
            deleted: u32::try_from(deletions).unwrap_or(u32::MAX),
        },
        Err(_) => LineStats::default(),
    }
}

fn delta_paths<'a>(status: Delta, delta: &'a git2::DiffDelta<'a>) -> [Option<&'a str>; 2] {
    let old = delta.old_file().path().and_then(Path::to_str);
    let new = delta.new_file().path().and_then(Path::to_str);
    match status {
        Delta::Deleted => [old, None],
        Delta::Renamed | Delta::Copied => [old, new],
        _ => [new, None],
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use git2::{IndexAddOption, Repository, Signature};
    use tempfile::TempDir;

    use super::*;

    fn fixture_repository() -> (TempDir, Repository) {
        let directory = tempfile::tempdir().expect("temp directory");
        let repository = Repository::init(directory.path()).expect("initialize fixture repository");
        (directory, repository)
    }

    fn write_fixture_file(workdir: &Path, name: &str, contents: &str) {
        let path = workdir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture parent directory");
        }
        fs::write(path, contents).expect("write fixture file");
    }

    fn commit_file(repository: &Repository, name: &str, contents: &str, subject: &str) {
        let workdir = repository.workdir().expect("non-bare fixture");
        write_fixture_file(workdir, name, contents);
        let mut index = repository.index().expect("fixture index");
        index
            .add_all([name], IndexAddOption::DEFAULT, None)
            .expect("stage fixture file");
        index.write().expect("write fixture index");
        let tree_id = index.write_tree().expect("write fixture tree");
        let tree = repository.find_tree(tree_id).expect("read fixture tree");
        let signature =
            Signature::now("Fixture Author", "fixture@example.test").expect("signature");
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

    fn commit_files(repository: &Repository, files: &[(&str, &str)], subject: &str) {
        let workdir = repository.workdir().expect("non-bare fixture");
        let names = files
            .iter()
            .map(|(name, contents)| {
                write_fixture_file(workdir, name, contents);
                *name
            })
            .collect::<Vec<_>>();
        let mut index = repository.index().expect("fixture index");
        index
            .add_all(names, IndexAddOption::DEFAULT, None)
            .expect("stage fixture files");
        index.write().expect("write fixture index");
        let tree_id = index.write_tree().expect("write fixture tree");
        let tree = repository.find_tree(tree_id).expect("read fixture tree");
        let signature =
            Signature::now("Fixture Author", "fixture@example.test").expect("signature");
        repository
            .commit(Some("HEAD"), &signature, &signature, subject, &tree, &[])
            .expect("create fixture commit");
    }

    #[test]
    fn collects_newest_first_file_only_samples() {
        let (directory, repository) = fixture_repository();
        commit_file(&repository, "src/a.rs", "one", "Initial implementation");
        commit_file(&repository, "src/b.rs", "two", "Fix parser regression");

        let samples = GitHistoryAdapter::new(GitMiningLimits {
            history_limit: 1,
            ..GitMiningLimits::default()
        })
        .collect(directory.path())
        .expect("collect fixture history");

        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].subject, "Fix parser regression");
        assert_eq!(
            samples[0].author.as_deref(),
            Some("Fixture Author <fixture@example.test>")
        );
        assert_eq!(samples[0].changes.len(), 1);
        assert_eq!(samples[0].changes[0].path, "src/b.rs");
        assert!(samples[0].changes[0].symbols.is_empty());
    }

    #[test]
    fn preserves_an_overflow_sentinel_for_the_pure_miner() {
        let (directory, repository) = fixture_repository();
        commit_files(
            &repository,
            &[("a.txt", "a"), ("b.txt", "b")],
            "Initial implementation",
        );

        let samples = GitHistoryAdapter::new(GitMiningLimits {
            history_limit: 1,
            paths_per_commit: 1,
            ..GitMiningLimits::default()
        })
        .collect(directory.path())
        .expect("collect fixture history");

        assert_eq!(samples[0].changes.len(), 2);
        let snapshot = crate::git_intelligence::GitHistoryMiner::with_limits(GitMiningLimits {
            history_limit: 1,
            paths_per_commit: 1,
            ..GitMiningLimits::default()
        })
        .mine(samples);
        assert_eq!(snapshot.report.path_overflow_commits, 1);
        assert!(snapshot.files.is_empty());
    }

    #[test]
    fn records_added_and_deleted_line_counts_from_diff_stats() {
        let (directory, repository) = fixture_repository();
        commit_file(&repository, "src/a.rs", "a\nb\nc\n", "Initial implementation");
        commit_file(&repository, "src/a.rs", "a\nx\nc\nd\n", "Fix line churn");

        let samples = GitHistoryAdapter::new(GitMiningLimits::default())
            .collect(directory.path())
            .expect("collect fixture history");

        // Newest first: the second commit replaces `b` with `x` (one deletion,
        // one insertion) and appends `d` (one insertion).
        assert_eq!(samples[0].subject, "Fix line churn");
        assert_eq!(samples[0].changes.len(), 1);
        assert_eq!(samples[0].changes[0].path, "src/a.rs");
        assert_eq!(samples[0].changes[0].lines_added, 2);
        assert_eq!(samples[0].changes[0].lines_deleted, 1);

        // The initial commit has no parent tree: every line is an addition.
        assert_eq!(samples[1].changes[0].lines_added, 3);
        assert_eq!(samples[1].changes[0].lines_deleted, 0);
    }

    #[test]
    fn line_stats_do_not_block_hotspot_accounting_for_a_binary_file() {
        let (directory, repository) = fixture_repository();
        let workdir = repository.workdir().expect("non-bare fixture");
        // Bytes that are not valid UTF-8 and contain a NUL: libgit2 treats this
        // as a binary blob, so the diff cannot yield meaningful line stats.
        std::fs::write(workdir.join("blob.bin"), [0u8, 159, 146, 150]).expect("write binary file");
        let mut index = repository.index().expect("fixture index");
        index
            .add_all(["blob.bin"], IndexAddOption::DEFAULT, None)
            .expect("stage binary file");
        index.write().expect("write fixture index");
        let tree_id = index.write_tree().expect("write fixture tree");
        let tree = repository.find_tree(tree_id).expect("read fixture tree");
        let signature = Signature::now("Fixture Author", "fixture@example.test").expect("sig");
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Add binary asset",
                &tree,
                &[],
            )
            .expect("create fixture commit");

        let samples = GitHistoryAdapter::new(GitMiningLimits::default())
            .collect(directory.path())
            .expect("collect fixture history");

        assert_eq!(samples[0].changes.len(), 1);
        assert_eq!(samples[0].changes[0].path, "blob.bin");
        // Diff stats are unavailable for a binary blob; the file is still
        // observed for hotspot purposes with zero (not fabricated) churn.
        assert_eq!(samples[0].changes[0].lines_added, 0);
        assert_eq!(samples[0].changes[0].lines_deleted, 0);
    }
}
