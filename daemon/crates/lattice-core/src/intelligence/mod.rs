pub mod agent;
pub mod docs;

#[cfg(test)]
mod agent_tests;

#[cfg(test)]
mod benchmark_tests;

#[cfg(test)]
mod docs_tests;

use crate::diff::{ChangeKind, SymbolChange};
use std::collections::HashMap;

pub use agent::{
    diagnose_failure, expand_context, find_relevant_tests, get_repo_playbook,
    get_working_set_context, impact_from_diff, prepare_change, summarize_subsystem,
    AffectedSymbolImpact, BundleMode, ChangedFileImpact, ChangedSymbolImpact,
    CompactFileSummary, CompactSymbolSummary, DiffImpactReport, DiffImpactStats,
    ExpandContextSeed, ExpandedContext, ExpandedContextStats, ExpandedFileContext,
    ExpandedFileSymbolContext, ExpandedRelationshipContext, ExpandedSymbolContext,
    ExpandedTestContext, FailureDiagnosis, FailureDiagnosisStats, FileRecommendation,
    MemoryHighlight, RepoPlaybook, RepoPlaybookStats, ReviewChecklistItem, RiskRecommendation,
    SubsystemSummary, SubsystemSummaryStats, SymbolRecommendation, TaskBundle, TaskBundleStats,
    TestRecommendation, TestSelectionReport, WorkingSetContext, WorkingSetStats,
};
pub use docs::{
    find_stale_docs, get_backlinks, get_docs_capsule, get_outgoing_links, BacklinksReport,
    DocHit, DocsCapsule, DocsCapsuleStats, DocsTargetKind, LinkReference,
    OutgoingLinksReport, RelatedDocSymbol, StaleDocHit, StaleDocsReport, StaleDocsStats,
};

/// Tracks symbol-level changes during a coding session to detect patterns
/// such as hotspots (frequently edited symbols), anti-patterns
/// (thrashing, dead ends), and co-change pairs.
pub struct ChangeTracker {
    /// Number of times each symbol has been edited.
    edit_counts: HashMap<String, u32>,
    /// Chronological record of (symbol_name, change_kind, timestamp).
    session_changes: Vec<(String, String, u64)>,
    /// Track symbols changed in the same "batch" (within a single file event).
    batch_changes: Vec<(Vec<String>, u64)>,
    /// Co-change pair counts: (sym_a, sym_b) sorted lexicographically -> count.
    co_change_pairs: HashMap<(String, String), u32>,
}

impl ChangeTracker {
    pub fn new() -> Self {
        Self {
            edit_counts: HashMap::new(),
            session_changes: Vec::new(),
            batch_changes: Vec::new(),
            co_change_pairs: HashMap::new(),
        }
    }

    /// Record a symbol change. Increments the edit count for the symbol
    /// and appends to the session change log.
    pub fn record_change(&mut self, change: &SymbolChange) {
        let count = self.edit_counts.entry(change.name.clone()).or_insert(0);
        *count += 1;

        let kind_str = match change.kind {
            ChangeKind::Added => "added",
            ChangeKind::Removed => "removed",
            ChangeKind::Modified => "modified",
        };

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        self.session_changes
            .push((change.name.clone(), kind_str.to_string(), timestamp));
    }

    /// Return the hotspot score (edit count) for a symbol.
    /// A higher score means the symbol has been edited more frequently.
    pub fn get_hotspot_score(&self, symbol_name: &str) -> u32 {
        self.edit_counts.get(symbol_name).copied().unwrap_or(0)
    }

    /// Detect symbols that are being "thrashed" — edited 5 or more times
    /// in a single session, suggesting instability or uncertainty.
    pub fn detect_thrashing(&self) -> Vec<String> {
        self.edit_counts
            .iter()
            .filter(|(_, &count)| count >= 5)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Detect "dead ends" — symbols that were added and then later removed
    /// within the same session, suggesting abandoned approaches.
    pub fn detect_dead_ends(&self) -> Vec<String> {
        let mut added: HashMap<String, bool> = HashMap::new();
        let mut dead_ends = Vec::new();

        for (name, kind, _) in &self.session_changes {
            match kind.as_str() {
                "added" => {
                    added.insert(name.clone(), true);
                }
                "removed" => {
                    if added.get(name).copied().unwrap_or(false) {
                        dead_ends.push(name.clone());
                    }
                }
                _ => {}
            }
        }

        dead_ends
    }

    /// Record a batch of symbols that changed together (e.g., in a single file save).
    /// For each pair of symbols in the batch, increment the co-change count.
    pub fn record_batch(&mut self, changed_symbols: Vec<String>) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // For each pair of symbols in the batch, increment co-change count
        for i in 0..changed_symbols.len() {
            for j in (i + 1)..changed_symbols.len() {
                let pair = if changed_symbols[i] < changed_symbols[j] {
                    (changed_symbols[i].clone(), changed_symbols[j].clone())
                } else {
                    (changed_symbols[j].clone(), changed_symbols[i].clone())
                };
                *self.co_change_pairs.entry(pair).or_insert(0) += 1;
            }
        }

        self.batch_changes.push((changed_symbols, now));
    }

    /// Get all co-change pairs that have been observed at least `min_count` times.
    /// Returns Vec<(sym_a, sym_b, count)>.
    pub fn get_co_change_pairs(&self, min_count: u32) -> Vec<(&str, &str, u32)> {
        self.co_change_pairs
            .iter()
            .filter(|(_, count)| **count >= min_count)
            .map(|((a, b), count)| (a.as_str(), b.as_str(), *count))
            .collect()
    }
}

// ── Project Rules Detection ──────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProjectRule {
    pub description: String,
    pub confidence: f64,
    pub occurrences: usize,
    pub example_files: Vec<String>,
}

pub struct RulesDetector {
    /// Tracks file creation patterns: when a file matching pattern A is created,
    /// a file matching pattern B is also created.
    file_pair_counts: HashMap<(String, String), usize>,
    /// Track naming conventions
    #[allow(dead_code)]
    naming_patterns: HashMap<String, usize>,
}

impl RulesDetector {
    pub fn new() -> Self {
        Self {
            file_pair_counts: HashMap::new(),
            naming_patterns: HashMap::new(),
        }
    }

    /// Analyze the indexed files to detect project conventions.
    pub fn detect_rules(&self, files: &[String]) -> Vec<ProjectRule> {
        let mut rules = Vec::new();

        // Rule: Test file conventions
        let source_files: Vec<_> = files
            .iter()
            .filter(|f| !f.contains("test") && !f.contains("spec"))
            .collect();
        let test_files: Vec<_> = files
            .iter()
            .filter(|f| f.contains("test") || f.contains("spec"))
            .collect();

        if !test_files.is_empty() {
            // Detect test file naming pattern
            let test_patterns = detect_test_patterns(&source_files, &test_files);
            for (pattern, count, examples) in test_patterns {
                if count >= 3 {
                    rules.push(ProjectRule {
                        description: format!("Test files follow pattern: {}", pattern),
                        confidence: (count as f64 / source_files.len().max(1) as f64).min(1.0),
                        occurrences: count,
                        example_files: examples,
                    });
                }
            }
        }

        // Rule: Directory structure conventions
        let dir_patterns = detect_directory_patterns(files);
        for (pattern, count) in dir_patterns {
            if count >= 3 {
                rules.push(ProjectRule {
                    description: pattern,
                    confidence: 0.7,
                    occurrences: count,
                    example_files: vec![],
                });
            }
        }

        // Rule: Module index files
        let has_index_files = files
            .iter()
            .filter(|f| {
                f.ends_with("index.ts")
                    || f.ends_with("index.js")
                    || f.ends_with("mod.rs")
                    || f.ends_with("__init__.py")
            })
            .count();
        if has_index_files >= 3 {
            rules.push(ProjectRule {
                description: "Modules use index/barrel files for exports".to_string(),
                confidence: 0.8,
                occurrences: has_index_files,
                example_files: files
                    .iter()
                    .filter(|f| f.ends_with("index.ts") || f.ends_with("mod.rs"))
                    .take(3)
                    .cloned()
                    .collect(),
            });
        }

        rules
    }

    /// Record that two files were created/modified together.
    pub fn record_co_creation(&mut self, file_a: &str, file_b: &str) {
        let key = if file_a < file_b {
            (file_a.to_string(), file_b.to_string())
        } else {
            (file_b.to_string(), file_a.to_string())
        };
        *self.file_pair_counts.entry(key).or_insert(0) += 1;
    }
}

fn detect_test_patterns(
    _sources: &[&String],
    tests: &[&String],
) -> Vec<(String, usize, Vec<String>)> {
    let mut patterns = Vec::new();

    // Check for .test. pattern
    let dot_test_count = tests.iter().filter(|t| t.contains(".test.")).count();
    if dot_test_count > 0 {
        patterns.push((
            "source.test.ext".to_string(),
            dot_test_count,
            tests
                .iter()
                .filter(|t| t.contains(".test."))
                .take(3)
                .map(|s| s.to_string())
                .collect(),
        ));
    }

    // Check for .spec. pattern
    let dot_spec_count = tests.iter().filter(|t| t.contains(".spec.")).count();
    if dot_spec_count > 0 {
        patterns.push((
            "source.spec.ext".to_string(),
            dot_spec_count,
            tests
                .iter()
                .filter(|t| t.contains(".spec."))
                .take(3)
                .map(|s| s.to_string())
                .collect(),
        ));
    }

    // Check for tests/ directory pattern
    let tests_dir_count = tests
        .iter()
        .filter(|t| t.contains("tests/") || t.contains("test/"))
        .count();
    if tests_dir_count > 0 {
        patterns.push((
            "tests/ directory".to_string(),
            tests_dir_count,
            tests
                .iter()
                .filter(|t| t.contains("tests/"))
                .take(3)
                .map(|s| s.to_string())
                .collect(),
        ));
    }

    patterns
}

fn detect_directory_patterns(files: &[String]) -> Vec<(String, usize)> {
    let mut dir_counts: HashMap<String, usize> = HashMap::new();
    for file in files {
        if let Some(dir) = file.rsplit_once('/').map(|(d, _)| d) {
            let last_dir = dir.rsplit_once('/').map(|(_, d)| d).unwrap_or(dir);
            *dir_counts.entry(last_dir.to_string()).or_insert(0) += 1;
        }
    }

    let mut patterns = Vec::new();
    for (dir, count) in dir_counts {
        if count >= 5 {
            patterns.push((
                format!("Files commonly grouped in '{}/' directories", dir),
                count,
            ));
        }
    }
    patterns
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{ChangeKind, SymbolChange};

    fn make_change(name: &str, kind: ChangeKind) -> SymbolChange {
        SymbolChange {
            name: name.to_string(),
            kind,
            file: "src/test.ts".to_string(),
        }
    }

    #[test]
    fn test_hotspot_score() {
        let mut tracker = ChangeTracker::new();

        let change = make_change("loginUser", ChangeKind::Modified);
        tracker.record_change(&change);
        tracker.record_change(&change);
        tracker.record_change(&change);

        assert_eq!(tracker.get_hotspot_score("loginUser"), 3);
        assert_eq!(tracker.get_hotspot_score("nonExistent"), 0);
    }

    #[test]
    fn test_detect_thrashing() {
        let mut tracker = ChangeTracker::new();

        let change = make_change("unstableFunc", ChangeKind::Modified);
        for _ in 0..5 {
            tracker.record_change(&change);
        }

        let stable_change = make_change("stableFunc", ChangeKind::Modified);
        tracker.record_change(&stable_change);
        tracker.record_change(&stable_change);

        let thrashing = tracker.detect_thrashing();
        assert_eq!(thrashing.len(), 1);
        assert!(thrashing.contains(&"unstableFunc".to_string()));
    }

    #[test]
    fn test_detect_dead_ends() {
        let mut tracker = ChangeTracker::new();

        // Add a symbol then remove it — dead end
        tracker.record_change(&make_change("tempHelper", ChangeKind::Added));
        tracker.record_change(&make_change("tempHelper", ChangeKind::Removed));

        // Add a symbol and keep it — not a dead end
        tracker.record_change(&make_change("keepThis", ChangeKind::Added));

        let dead_ends = tracker.detect_dead_ends();
        assert_eq!(dead_ends.len(), 1);
        assert!(dead_ends.contains(&"tempHelper".to_string()));
    }

    #[test]
    fn test_co_change_detection() {
        let mut tracker = ChangeTracker::new();

        // Simulate 3 batches where funcA and funcB always change together
        tracker.record_batch(vec!["funcA".to_string(), "funcB".to_string()]);
        tracker.record_batch(vec![
            "funcA".to_string(),
            "funcB".to_string(),
            "funcC".to_string(),
        ]);
        tracker.record_batch(vec!["funcA".to_string(), "funcB".to_string()]);

        // funcA-funcB should have count 3 (appeared together in all 3 batches)
        let pairs = tracker.get_co_change_pairs(3);
        assert!(
            pairs
                .iter()
                .any(|(a, b, c)| (*a == "funcA" && *b == "funcB" && *c >= 3)
                    || (*a == "funcB" && *b == "funcA" && *c >= 3)),
            "Expected funcA-funcB co-change pair with count >= 3, got: {:?}",
            pairs
        );

        // funcA-funcC should have count 1 (only appeared together in batch 2)
        let pairs_low = tracker.get_co_change_pairs(1);
        assert!(
            pairs_low
                .iter()
                .any(|(a, b, _)| (*a == "funcA" && *b == "funcC")
                    || (*a == "funcC" && *b == "funcA")),
            "Expected funcA-funcC co-change pair at threshold 1, got: {:?}",
            pairs_low
        );

        // funcA-funcC should NOT appear at threshold 2 (only count 1)
        let pairs_high = tracker.get_co_change_pairs(2);
        assert!(
            !pairs_high
                .iter()
                .any(|(a, b, _)| (*a == "funcA" && *b == "funcC")
                    || (*a == "funcC" && *b == "funcA")),
            "funcA-funcC should not appear at threshold 2"
        );
    }

    #[test]
    fn test_detect_project_rules() {
        let detector = RulesDetector::new();
        let files = vec![
            "src/auth.ts".to_string(),
            "src/auth.test.ts".to_string(),
            "src/user.ts".to_string(),
            "src/user.test.ts".to_string(),
            "src/session.ts".to_string(),
            "src/session.test.ts".to_string(),
            "src/index.ts".to_string(),
            "lib/index.ts".to_string(),
            "utils/index.ts".to_string(),
        ];
        let rules = detector.detect_rules(&files);
        assert!(!rules.is_empty(), "Should detect at least one pattern");
        // Should detect the .test. pattern
        let has_test_rule = rules.iter().any(|r| r.description.contains("test"));
        assert!(has_test_rule, "Should detect test file naming convention");
    }
}
