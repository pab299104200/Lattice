pub mod agent;
pub mod docs;

#[cfg(test)]
mod agent_tests;

#[cfg(test)]
mod benchmark_tests;

#[cfg(test)]
mod docs_tests;

use std::collections::HashMap;

pub use agent::{
    diagnose_failure, expand_context, find_relevant_tests, get_repo_playbook,
    get_working_set_context, impact_from_diff, plan_edit, prepare_change, summarize_subsystem,
    trace_scenario, AffectedSymbolImpact, BundleMode, ChangedFileImpact, ChangedSymbolImpact,
    CompactFileSummary, CompactSymbolSummary, DiffImpactReport, DiffImpactStats,
    EditSpanRecommendation, ExpandContextSeed, ExpandedContext, ExpandedContextStats,
    ExpandedFileContext, ExpandedFileSymbolContext, ExpandedRelationshipContext,
    ExpandedSymbolContext, ExpandedTestContext, FailureDiagnosis, FailureDiagnosisStats,
    FileRecommendation, MemoryHighlight, PlanEditBundle, PlanEditDocRecommendation, PlanEditImpact,
    PlanEditStats, RepoPlaybook, RepoPlaybookStats, ReviewChecklistItem, RiskRecommendation,
    ScenarioPathSegment, ScenarioSignal, ScenarioTraceBundle, ScenarioTraceStats, SubsystemSummary,
    SubsystemSummaryStats, SymbolRecommendation, TaskBundle, TaskBundleStats, TestRecommendation,
    TestSelectionReport, WorkingSetContext, WorkingSetStats,
};
pub use docs::{
    find_stale_docs, get_backlinks, get_docs_capsule, get_outgoing_links, BacklinksReport, DocHit,
    DocsCapsule, DocsCapsuleStats, DocsTargetKind, LinkReference, OutgoingLinksReport,
    RelatedDocSymbol, StaleDocHit, StaleDocsReport, StaleDocsStats,
};

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
