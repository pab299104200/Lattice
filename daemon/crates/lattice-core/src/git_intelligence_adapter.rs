//! Bounded `git2` history extraction for [`crate::git_intelligence`].
//!
//! This adapter deliberately does not aggregate or persist history signals. It
//! only turns the newest reachable commits into deterministic `CommitSample`
//! values for `GitHistoryMiner`. Keeping git IO here lets the miner remain a
//! pure, replayable component and gives the runtime one narrow integration seam.

use std::collections::BTreeSet;
use std::path::Path;

use git2::{Delta, DiffOptions, Repository, Sort};

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

    let mut paths = BTreeSet::new();
    // Keep one extra valid path so the pure miner can mark this commit as an
    // overflow instead of silently publishing a partial signal.
    let maximum_observations = limits.paths_per_commit.saturating_add(1);
    for delta in diff.deltas() {
        for path in delta_paths(delta.status(), &delta) {
            let Some(path) = path else {
                continue;
            };
            paths.insert(path.to_owned());
            if paths.len() >= maximum_observations {
                break;
            }
        }
        if paths.len() >= maximum_observations {
            break;
        }
    }

    Ok(paths
        .into_iter()
        .map(|path| PathChange {
            path,
            symbols: Vec::new(),
        })
        .collect())
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
}
