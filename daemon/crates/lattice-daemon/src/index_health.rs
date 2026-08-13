use lattice_core::indexer::{BatchIndexReport, IndexFailureKind};
use serde::Serialize;
use std::collections::BTreeSet;
use std::sync::Mutex;

/// The bounded, user-visible truth about files that could not be parsed into
/// the currently published shard.  It is intentionally independent from
/// watcher availability: a healthy watcher can still leave an incomplete
/// graph when a source file does not parse.
#[derive(Debug, Default)]
pub(crate) struct IndexHealth {
    failed_files: Mutex<BTreeSet<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct IndexHealthSnapshot {
    pub(crate) is_partial: bool,
    pub(crate) parse_failures: usize,
    pub(crate) failed_files: Vec<String>,
}

impl IndexHealth {
    /// Replace state after a full index. Files absent from a full report are
    /// known-good (or absent), so retaining old failures would be stale.
    pub(crate) fn replace_from_report(&self, report: &BatchIndexReport) {
        let Ok(mut failed_files) = self.failed_files.lock() else {
            return;
        };
        *failed_files = parse_failure_files(report).collect();
    }

    /// Merge a watcher batch into existing state. A successful reparse and a
    /// deletion both resolve a prior parse failure; a new parse failure adds
    /// the affected file. This preserves failures from untouched files.
    pub(crate) fn merge_change_report(&self, report: &BatchIndexReport) {
        let Ok(mut failed_files) = self.failed_files.lock() else {
            return;
        };
        for file in report
            .indexed_files
            .iter()
            .chain(report.removed_files.iter())
        {
            failed_files.remove(file);
        }
        failed_files.extend(parse_failure_files(report));
    }

    pub(crate) fn snapshot(&self, failed_file_limit: usize) -> IndexHealthSnapshot {
        let (parse_failures, failed_files) = self
            .failed_files
            .lock()
            .map(|files| {
                (
                    files.len(),
                    files.iter().take(failed_file_limit).cloned().collect(),
                )
            })
            .unwrap_or_default();
        IndexHealthSnapshot {
            is_partial: parse_failures > 0,
            parse_failures,
            failed_files,
        }
    }
}

fn parse_failure_files(report: &BatchIndexReport) -> impl Iterator<Item = String> + '_ {
    report.failures.iter().filter_map(|failure| {
        (failure.kind == IndexFailureKind::ParseError).then(|| failure.file.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lattice_core::indexer::{IndexFailure, IndexFailureKind};

    fn report(
        indexed_files: &[&str],
        removed_files: &[&str],
        failed_files: &[&str],
    ) -> BatchIndexReport {
        BatchIndexReport {
            requested_count: indexed_files.len() + failed_files.len(),
            indexed_count: indexed_files.len(),
            is_partial: !failed_files.is_empty(),
            indexed_files: indexed_files.iter().map(ToString::to_string).collect(),
            removed_files: removed_files.iter().map(ToString::to_string).collect(),
            failures: failed_files
                .iter()
                .map(|file| IndexFailure {
                    file: (*file).to_string(),
                    kind: IndexFailureKind::ParseError,
                    message: "invalid source".to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn change_reports_merge_failures_and_clear_successes_or_deletions() {
        let health = IndexHealth::default();
        health.replace_from_report(&report(&[], &[], &["z.rs", "a.rs"]));
        health.merge_change_report(&report(&["a.rs"], &["z.rs"], &["m.rs"]));

        assert_eq!(
            health.snapshot(10),
            IndexHealthSnapshot {
                is_partial: true,
                parse_failures: 1,
                failed_files: vec!["m.rs".to_string()],
            }
        );
    }

    #[test]
    fn snapshot_is_deterministic_and_bounded() {
        let health = IndexHealth::default();
        health.replace_from_report(&report(&[], &[], &["z.rs", "a.rs", "m.rs"]));

        assert_eq!(
            health.snapshot(2).failed_files,
            vec!["a.rs".to_string(), "m.rs".to_string()]
        );
    }
}
