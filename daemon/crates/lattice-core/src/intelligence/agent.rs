use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::graph::model::{CodeGraph, EdgeKind, GraphNode};
use crate::query::{detect_intent, ContextCapsule, QueryIntent};
use crate::symbols::{parse_stable_file_handle, stable_file_handle, SymbolId};

use super::docs::find_stale_docs;
use super::ProjectRule;

const COMPACT_PRIMARY_FILE_LIMIT: usize = 4;
const COMPACT_SECONDARY_FILE_LIMIT: usize = 6;
const COMPACT_SYMBOL_LIMIT: usize = 8;
const COMPACT_TEST_LIMIT: usize = 6;
const COMPACT_AFFECTED_LIMIT: usize = 10;
const FULL_PRIMARY_FILE_LIMIT: usize = 6;
const FULL_SECONDARY_FILE_LIMIT: usize = 10;
const FULL_SYMBOL_LIMIT: usize = 12;
const FULL_TEST_LIMIT: usize = 10;
const FULL_AFFECTED_LIMIT: usize = 16;
const COMPACT_WORKING_FILE_LIMIT: usize = 5;
const FULL_WORKING_FILE_LIMIT: usize = 8;
const COMPACT_ACTIVE_SYMBOL_LIMIT: usize = 5;
const FULL_ACTIVE_SYMBOL_LIMIT: usize = 8;
const COMPACT_NEARBY_SYMBOL_LIMIT: usize = 8;
const FULL_NEARBY_SYMBOL_LIMIT: usize = 12;
const ULTRA_COMPACT_PRIMARY_FILE_LIMIT: usize = 2;
const ULTRA_COMPACT_SECONDARY_FILE_LIMIT: usize = 1;
const ULTRA_COMPACT_SYMBOL_LIMIT: usize = 2;
const ULTRA_COMPACT_TEST_LIMIT: usize = 2;
const ULTRA_COMPACT_RISK_LIMIT: usize = 1;
const ULTRA_COMPACT_CHANGED_FILE_LIMIT: usize = 1;
const ULTRA_COMPACT_CHANGED_SYMBOL_LIMIT: usize = 2;
const ULTRA_COMPACT_AFFECTED_SYMBOL_LIMIT: usize = 2;
const ULTRA_COMPACT_CHECKLIST_LIMIT: usize = 2;
const ULTRA_COMPACT_ARCHITECTURE_LIMIT: usize = 1;
const ULTRA_COMPACT_CONVENTION_LIMIT: usize = 1;
const ULTRA_COMPACT_MEMORY_TEXT_LIMIT: usize = 72;
const DEFAULT_EXPAND_MAX_TOKENS: usize = 1200;
const MIN_EXPAND_MAX_TOKENS: usize = 200;
const MAX_EXPAND_MAX_TOKENS: usize = 4000;
const CHARS_PER_TOKEN_ESTIMATE: usize = 4;
const SUBSYSTEM_DOCUMENT_QUERY_KEYWORDS: &[&str] = &[
    "docs",
    "documentation",
    "markdown",
    "readme",
    "runbook",
    "manual",
    "guide",
    "adr",
    "heading",
    "section",
];
const SUBSYSTEM_TEST_QUERY_KEYWORDS: &[&str] = &[
    "test",
    "tests",
    "spec",
    "coverage",
    "regression",
    "failure",
    "failing",
];

const PATH_STOP_WORDS: &[&str] = &[
    "src",
    "lib",
    "app",
    "apps",
    "pkg",
    "internal",
    "shared",
    "common",
    "tests",
    "test",
    "spec",
    "unit",
    "integration",
    "e2e",
    "index",
    "mod",
    "main",
    "route",
    "routes",
    "router",
    "routers",
    "handler",
    "handlers",
    "controller",
    "controllers",
    "model",
    "models",
    "service",
    "services",
    "core",
];

const ASSISTANT_ARTIFACT_DIRS: &[&str] = &[".claude", ".codex"];
const SCENARIO_FAILURE_HINTS: &[&str] = &[
    "fail",
    "failure",
    "error",
    "panic",
    "exception",
    "timeout",
    "forbidden",
    "unauthorized",
    "denied",
    "status_code=4",
    "status_code=5",
];
const SCENARIO_GUARD_HINTS: &[&str] = &[
    "if ",
    "match ",
    "guard",
    "validate",
    "verify",
    "check",
    "ensure",
    "authorize",
    "require",
];
const SCENARIO_SIDE_EFFECT_HINTS: &[&str] = &[
    "insert", "update", "delete", "write", "save", "commit", "publish", "emit", "send", "dispatch",
    "enqueue", "persist", "cache", "create",
];
const COMPACT_SCENARIO_ENTRYPOINT_LIMIT: usize = 6;
const FULL_SCENARIO_ENTRYPOINT_LIMIT: usize = 10;
const COMPACT_SCENARIO_PATH_LIMIT: usize = 8;
const FULL_SCENARIO_PATH_LIMIT: usize = 14;
const COMPACT_SCENARIO_SIGNAL_LIMIT: usize = 5;
const FULL_SCENARIO_SIGNAL_LIMIT: usize = 9;
const ULTRA_COMPACT_SCENARIO_PATH_LIMIT: usize = 3;
const ULTRA_COMPACT_SCENARIO_SIGNAL_LIMIT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleMode {
    Compact,
    Full,
}

impl BundleMode {
    pub fn from_str(value: &str) -> Self {
        match value {
            "full" => Self::Full,
            _ => Self::Compact,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Full => "full",
        }
    }

    fn primary_file_limit(self) -> usize {
        match self {
            Self::Compact => COMPACT_PRIMARY_FILE_LIMIT,
            Self::Full => FULL_PRIMARY_FILE_LIMIT,
        }
    }

    fn secondary_file_limit(self) -> usize {
        match self {
            Self::Compact => COMPACT_SECONDARY_FILE_LIMIT,
            Self::Full => FULL_SECONDARY_FILE_LIMIT,
        }
    }

    fn symbol_limit(self) -> usize {
        match self {
            Self::Compact => COMPACT_SYMBOL_LIMIT,
            Self::Full => FULL_SYMBOL_LIMIT,
        }
    }

    fn test_limit(self) -> usize {
        match self {
            Self::Compact => COMPACT_TEST_LIMIT,
            Self::Full => FULL_TEST_LIMIT,
        }
    }

    fn affected_limit(self) -> usize {
        match self {
            Self::Compact => COMPACT_AFFECTED_LIMIT,
            Self::Full => FULL_AFFECTED_LIMIT,
        }
    }

    fn working_file_limit(self) -> usize {
        match self {
            Self::Compact => COMPACT_WORKING_FILE_LIMIT,
            Self::Full => FULL_WORKING_FILE_LIMIT,
        }
    }

    fn active_symbol_limit(self) -> usize {
        match self {
            Self::Compact => COMPACT_ACTIVE_SYMBOL_LIMIT,
            Self::Full => FULL_ACTIVE_SYMBOL_LIMIT,
        }
    }

    fn nearby_symbol_limit(self) -> usize {
        match self {
            Self::Compact => COMPACT_NEARBY_SYMBOL_LIMIT,
            Self::Full => FULL_NEARBY_SYMBOL_LIMIT,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FileRecommendation {
    pub file: String,
    pub score: f64,
    pub confidence_band: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolRecommendation {
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_handle: Option<String>,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub role: String,
    pub score: f64,
    pub confidence_band: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestRecommendation {
    pub file: String,
    pub confidence: f64,
    pub confidence_band: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RiskRecommendation {
    pub level: String,
    pub symbol: String,
    pub file: String,
    pub reason: String,
    pub impact_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskBundleStats {
    pub primary_file_count: usize,
    pub secondary_file_count: usize,
    pub symbol_count: usize,
    pub test_count: usize,
    pub memory_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskBundle {
    pub query: String,
    pub intent: QueryIntent,
    pub overview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_expand: Option<ExpandSuggestion>,
    pub primary_files: Vec<FileRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secondary_files: Vec<FileRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<SymbolRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub test_gaps: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_rules: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub memory_highlights: Vec<MemoryHighlight>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub risks: Vec<RiskRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rationale: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<TaskBundleStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EditSpanRecommendation {
    pub file: String,
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_handle: Option<String>,
    pub line_span: String,
    pub start_line: usize,
    pub end_line: usize,
    pub reason: String,
    pub confidence_band: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanEditImpact {
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_handle: Option<String>,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub relationship: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub via: Vec<String>,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanEditDocRecommendation {
    pub file: String,
    pub line: usize,
    pub summary: String,
    pub score: f64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_symbols: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanEditStats {
    pub edit_file_count: usize,
    pub supporting_file_count: usize,
    pub symbol_count: usize,
    pub span_count: usize,
    pub caller_count: usize,
    pub dependency_count: usize,
    pub doc_count: usize,
    pub stale_doc_signal_count: usize,
    pub test_count: usize,
    pub memory_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanEditBundle {
    pub query: String,
    pub intent: QueryIntent,
    pub overview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_expand: Option<ExpandSuggestion>,
    pub edit_files: Vec<FileRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub supporting_files: Vec<FileRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<SymbolRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidate_spans: Vec<EditSpanRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub affected_callers: Vec<PlanEditImpact>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub affected_dependencies: Vec<PlanEditImpact>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub relevant_docs: Vec<PlanEditDocRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stale_doc_signals: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub test_gaps: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_rules: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub memory_highlights: Vec<MemoryHighlight>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub risks: Vec<RiskRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rationale: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<PlanEditStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioPathSegment {
    pub from_symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_symbol_handle: Option<String>,
    pub from_kind: String,
    pub from_file: String,
    pub from_line: usize,
    pub to_symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_symbol_handle: Option<String>,
    pub to_kind: String,
    pub to_file: String,
    pub to_line: usize,
    pub relationship: String,
    pub score: f64,
    pub confidence_band: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rationale: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioSignal {
    pub signal_type: String,
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_handle: Option<String>,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub summary: String,
    pub score: f64,
    pub confidence_band: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioTraceStats {
    pub likely_entrypoint_count: usize,
    pub plausible_entrypoint_count: usize,
    pub execution_path_count: usize,
    pub plausible_path_count: usize,
    pub guard_count: usize,
    pub side_effect_count: usize,
    pub failure_branch_count: usize,
    pub doc_count: usize,
    pub test_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioTraceBundle {
    pub scenario: String,
    pub intent: QueryIntent,
    pub overview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_expand: Option<ExpandSuggestion>,
    pub likely_entrypoints: Vec<SymbolRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub plausible_entrypoints: Vec<SymbolRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub execution_path: Vec<ScenarioPathSegment>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub plausible_paths: Vec<ScenarioPathSegment>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub guards: Vec<ScenarioSignal>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub side_effects: Vec<ScenarioSignal>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failure_branches: Vec<ScenarioSignal>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub relevant_docs: Vec<PlanEditDocRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub test_gaps: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_rules: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rationale: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<ScenarioTraceStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestSelectionReport {
    pub source_files: Vec<String>,
    pub tests: Vec<TestRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_rules: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rationale: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedFileImpact {
    pub file: String,
    pub status: String,
    pub added_lines: usize,
    pub removed_lines: usize,
    pub hunk_count: usize,
    pub changed_symbols: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub line_ranges: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedSymbolImpact {
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_handle: Option<String>,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub end_line: usize,
    pub change_kind: String,
    pub impact_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AffectedSymbolImpact {
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_handle: Option<String>,
    pub kind: String,
    pub file: String,
    pub line: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub via: Vec<String>,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewChecklistItem {
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffImpactStats {
    pub changed_file_count: usize,
    pub changed_symbol_count: usize,
    pub affected_symbol_count: usize,
    pub risky_symbol_count: usize,
    pub test_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffImpactReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_expand: Option<ExpandSuggestion>,
    pub changed_files: Vec<ChangedFileImpact>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changed_symbols: Vec<ChangedSymbolImpact>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub affected_symbols: Vec<AffectedSymbolImpact>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub risks: Vec<RiskRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub review_checklist: Vec<ReviewChecklistItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub test_gaps: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_rules: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rationale: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<DiffImpactStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkingSetStats {
    pub file_count: usize,
    pub active_symbol_count: usize,
    pub nearby_symbol_count: usize,
    pub test_count: usize,
    pub memory_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkingSetContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    pub overview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_expand: Option<ExpandSuggestion>,
    pub files: Vec<FileRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub active_symbols: Vec<SymbolRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub nearby_symbols: Vec<SymbolRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_rules: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub memory_highlights: Vec<MemoryHighlight>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rationale: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<WorkingSetStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompactFileSummary {
    pub file: String,
    pub summary: String,
    pub why: String,
    pub confidence_band: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompactSymbolSummary {
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_handle: Option<String>,
    pub file: String,
    pub line: usize,
    pub role: String,
    pub summary: String,
    pub confidence_band: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryHighlight {
    pub content: String,
    pub memory_type: String,
    pub scope: String,
    pub is_stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assertion_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness_policy_detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExpandSuggestion {
    pub focus: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubsystemSummaryStats {
    pub file_count: usize,
    pub symbol_count: usize,
    pub test_count: usize,
    pub memory_count: usize,
    pub approx_tokens: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubsystemSummary {
    pub query: String,
    pub overview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_expand: Option<ExpandSuggestion>,
    pub key_files: Vec<CompactFileSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub key_symbols: Vec<CompactSymbolSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_rules: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<MemoryHighlight>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rationale: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<SubsystemSummaryStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoPlaybookStats {
    pub file_count: usize,
    pub symbol_count: usize,
    pub convention_count: usize,
    pub memory_count: usize,
    pub approx_tokens: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoPlaybook {
    pub overview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_expand: Option<ExpandSuggestion>,
    pub architecture: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conventions: Vec<String>,
    pub key_files: Vec<CompactFileSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notable_symbols: Vec<CompactSymbolSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub durable_patterns: Vec<MemoryHighlight>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rationale: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<RepoPlaybookStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailureDiagnosisStats {
    pub extracted_file_count: usize,
    pub extracted_symbol_count: usize,
    pub suspect_count: usize,
    pub related_symbol_count: usize,
    pub test_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailureDiagnosis {
    pub kind: String,
    pub overview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_expand: Option<ExpandSuggestion>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extracted_files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extracted_symbols: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub suspects: Vec<SymbolRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub related_symbols: Vec<SymbolRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestRecommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub memory_highlights: Vec<MemoryHighlight>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub likely_causes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub next_steps: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rationale: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<FailureDiagnosisStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandContextSeed {
    pub query: Option<String>,
    pub files: Vec<String>,
    pub symbols: Vec<String>,
    pub tests: Vec<String>,
    pub memories: Vec<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExpandedRelationshipContext {
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_handle: Option<String>,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub relationship: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExpandedFileSymbolContext {
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_handle: Option<String>,
    pub kind: String,
    pub line: usize,
    pub signature: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExpandedFileContext {
    pub file: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<ExpandedFileSymbolContext>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub related_tests: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExpandedTestContext {
    pub file: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<ExpandedFileSymbolContext>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub related_files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExpandedSymbolContext {
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_handle: Option<String>,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub end_line: usize,
    pub signature: Arc<str>,
    pub source: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<ExpandedRelationshipContext>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dependents: Vec<ExpandedRelationshipContext>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub same_file: Vec<ExpandedRelationshipContext>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExpandedContextStats {
    pub file_count: usize,
    pub symbol_count: usize,
    pub test_count: usize,
    pub memory_count: usize,
    pub approx_tokens: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExpandedContext {
    pub focus: String,
    pub focus_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<ExpandedFileContext>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<ExpandedSymbolContext>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<ExpandedTestContext>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rationale: Vec<String>,
    pub stats: ExpandedContextStats,
}

#[derive(Default, Clone)]
struct FileAccumulator {
    score: f64,
    reasons: Vec<String>,
}

#[derive(Default)]
struct SymbolAccumulator {
    kind: String,
    line: usize,
    byte_offset: Option<usize>,
    role: String,
    score: f64,
}

#[derive(Default)]
struct TestAccumulator {
    score: f64,
    reasons: Vec<String>,
}

#[derive(Default)]
struct AffectedAccumulator {
    kind: String,
    line: usize,
    byte_offset: Option<usize>,
    via: Vec<String>,
    score: f64,
}

#[derive(Default)]
struct ScenarioPathAccumulator {
    from_kind: String,
    from_line: usize,
    from_offset: Option<usize>,
    to_kind: String,
    to_line: usize,
    to_offset: Option<usize>,
    score: f64,
    rationale: Vec<String>,
}

#[derive(Debug, Clone)]
struct LineRange {
    start: usize,
    end: usize,
}

#[derive(Default)]
struct ParsedDiffFile {
    file: String,
    status: String,
    old_path: Option<String>,
    new_path: Option<String>,
    line_ranges: Vec<LineRange>,
    added_lines: usize,
    removed_lines: usize,
    hunk_count: usize,
}

enum ExpansionTarget {
    SymbolId(SymbolId),
    Symbol(String),
    File(String),
    Test(String),
    Memory(usize),
}

impl ExpansionTarget {
    fn kind(&self) -> &'static str {
        match self {
            Self::SymbolId(_) => "symbol",
            Self::Symbol(_) => "symbol",
            Self::File(_) => "file",
            Self::Test(_) => "test",
            Self::Memory(_) => "memory",
        }
    }
}

pub fn prepare_change(
    graph: &CodeGraph,
    capsule: &ContextCapsule,
    entry_files: &[String],
    entry_symbols: &[String],
    rules: &[ProjectRule],
    mode: BundleMode,
) -> TaskBundle {
    let mut file_scores: HashMap<String, FileAccumulator> = HashMap::new();
    let mut symbol_scores: HashMap<(String, String), SymbolAccumulator> = HashMap::new();
    let mut seed_nodes: Vec<&GraphNode> = Vec::new();
    let mut seed_seen: HashSet<(String, String)> = HashSet::new();
    let mut risks: Vec<RiskRecommendation> = Vec::new();
    let mut rationale: Vec<String> = Vec::new();
    let query_tokens: HashSet<String> = tokenize_path(&capsule.query).into_iter().collect();
    let entry_file_set: HashSet<String> = entry_files.iter().cloned().collect();
    let entry_symbol_set: HashSet<String> = entry_symbols.iter().cloned().collect();

    for file in entry_files {
        if !is_queryable_graph_file(file) {
            continue;
        }
        add_file_score(
            &mut file_scores,
            file,
            10.0,
            "user supplied entry file".to_string(),
        );
    }

    for pivot in &capsule.pivots {
        if !is_queryable_graph_file(&pivot.file) {
            continue;
        }
        let pivot_exact = find_exact_node(graph, &pivot.file, &pivot.symbol);
        add_file_score(
            &mut file_scores,
            &pivot.file,
            4.0 + pivot.score,
            format!("pivot symbol {}", pivot.symbol),
        );
        add_symbol_score(
            &mut symbol_scores,
            &pivot.file,
            &pivot.symbol,
            pivot_exact.map(|node| &node.id),
            pivot.kind.clone(),
            pivot.line,
            "pivot".to_string(),
            4.0 + pivot.score,
        );
        if let Some(node) = pivot_exact {
            push_seed_node(&mut seed_nodes, &mut seed_seen, node);
        }
    }

    for context in &capsule.context {
        if !is_queryable_graph_file(&context.file) {
            continue;
        }
        let context_exact = find_exact_node(graph, &context.file, &context.symbol);
        add_file_score(
            &mut file_scores,
            &context.file,
            1.75 + context.score,
            format!("supporting symbol {}", context.symbol),
        );
        add_symbol_score(
            &mut symbol_scores,
            &context.file,
            &context.symbol,
            context_exact.map(|node| &node.id),
            context.kind.clone(),
            context.line,
            "context".to_string(),
            1.75 + context.score,
        );
        if let Some(node) = context_exact {
            push_seed_node(&mut seed_nodes, &mut seed_seen, node);
        }
    }

    for symbol in entry_symbols {
        let matches = find_symbol_matches(graph, symbol, entry_files);
        for node in matches.into_iter().take(3) {
            if !is_queryable_graph_file(&node.file) {
                continue;
            }
            add_file_score(
                &mut file_scores,
                &node.file,
                7.5,
                format!("user supplied symbol {}", symbol),
            );
            add_symbol_score(
                &mut symbol_scores,
                &node.file,
                &node.name,
                Some(&node.id),
                node.kind.short_code().to_string(),
                node.line,
                "entry_symbol".to_string(),
                7.5,
            );
            push_seed_node(&mut seed_nodes, &mut seed_seen, node);
        }
    }

    for node in &seed_nodes {
        let dependencies = rank_related_nodes(graph.get_dependencies(&node.id));
        for (dep, edge) in dependencies.into_iter().take(4) {
            if !is_queryable_graph_file(&dep.file) {
                continue;
            }
            add_file_score(
                &mut file_scores,
                &dep.file,
                1.35,
                format!("{} dependency via {}", node.name, short_edge(edge)),
            );
            add_symbol_score(
                &mut symbol_scores,
                &dep.file,
                &dep.name,
                Some(&dep.id),
                dep.kind.short_code().to_string(),
                dep.line,
                format!("dependency:{}", short_edge(edge)),
                1.35,
            );
        }

        let dependents = rank_related_nodes(graph.get_dependents(&node.id));
        for (dependent, edge) in dependents.iter().take(4) {
            if !is_queryable_graph_file(&dependent.file) {
                continue;
            }
            add_file_score(
                &mut file_scores,
                &dependent.file,
                1.15,
                format!("{} dependent via {}", dependent.name, short_edge(*edge)),
            );
            add_symbol_score(
                &mut symbol_scores,
                &dependent.file,
                &dependent.name,
                Some(&dependent.id),
                dependent.kind.short_code().to_string(),
                dependent.line,
                format!("dependent:{}", short_edge(*edge)),
                1.15,
            );
        }

        if task_relevance_hits(
            &node.file,
            &node.name,
            &query_tokens,
            &entry_file_set,
            &entry_symbol_set,
        ) == 0
        {
            continue;
        }

        if let Some(risk) = risk_for_node(graph, node, 2, "candidate change") {
            risks.push(risk);
        }
    }

    let ranked_files = finalize_file_recommendations(file_scores);
    let ranked_files = promote_explicit_files(ranked_files, entry_files);
    let mut ranked_files = prioritize_entry_scope_files(ranked_files, entry_files);
    calibrate_file_recommendations(&mut ranked_files);
    let primary_files: Vec<FileRecommendation> = ranked_files
        .iter()
        .take(mode.primary_file_limit())
        .cloned()
        .collect();
    let secondary_files: Vec<FileRecommendation> = ranked_files
        .iter()
        .skip(primary_files.len())
        .take(mode.secondary_file_limit())
        .cloned()
        .collect();

    let mut ranked_symbols =
        prefer_symbols_in_files(finalize_symbol_recommendations(symbol_scores), entry_files);
    calibrate_symbol_recommendations(&mut ranked_symbols);
    let symbols: Vec<SymbolRecommendation> = ranked_symbols
        .into_iter()
        .take(mode.symbol_limit())
        .collect();

    let test_anchor_files =
        prepare_change_test_anchor_files(entry_files, &primary_files, mode.primary_file_limit());
    let test_anchor_symbols = prepare_change_test_anchor_symbols(entry_symbols, &symbols);
    let test_report = find_relevant_tests(
        graph,
        &test_anchor_files,
        &test_anchor_symbols,
        None,
        rules,
        mode.test_limit(),
    );

    if !primary_files.is_empty() {
        rationale.push(format!(
            "Primary edit files are ranked from pivot symbols, entry hints, and direct graph neighbors."
        ));
    }
    if !test_report.tests.is_empty() {
        rationale.push(format!(
            "Test suggestions are scored from graph-linked test dependencies plus filename and directory overlap with the likely edit files."
        ));
    }
    if !capsule.memories.is_empty() {
        rationale.push(format!(
            "Included {} matching memories to reduce rediscovery across sessions.",
            capsule.memories.len()
        ));
    }

    dedupe_strings(&mut rationale);
    let selected_files: HashSet<String> = primary_files
        .iter()
        .chain(secondary_files.iter())
        .map(|item| item.file.clone())
        .collect();
    risks.retain(|risk| selected_files.contains(&risk.file));
    if !entry_files.is_empty() {
        risks.retain(|risk| {
            entry_file_set.contains(&risk.file)
                || entry_symbol_set.contains(&risk.symbol)
                || file_matches_entry_scope(&risk.file, entry_files)
        });
    }
    risks.sort_by(|a, b| {
        severity_rank(&a.level)
            .cmp(&severity_rank(&b.level))
            .then_with(|| b.impact_count.cmp(&a.impact_count))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    risks.truncate(6);

    let mut primary_files = primary_files;
    let mut secondary_files = secondary_files;
    let mut symbols = symbols;
    let mut tests = test_report.tests;
    let mut test_gaps = test_report.gaps;
    let mut matched_rules = test_report.matched_rules;
    if matches!(mode, BundleMode::Compact) {
        compactify_file_recommendations(&mut primary_files);
        compactify_file_recommendations(&mut secondary_files);
        compactify_symbol_recommendations(&mut symbols);
        compactify_test_recommendations(&mut tests);
        ultra_compactify_task_bundle(
            &mut primary_files,
            &mut secondary_files,
            &mut symbols,
            &mut tests,
            &mut risks,
            &mut test_gaps,
            &mut matched_rules,
            &mut rationale,
        );
    }

    let primary_file_count = primary_files.len();
    let secondary_file_count = secondary_files.len();
    let symbol_count = symbols.len();
    let test_count = tests.len();
    let memory_highlight_limit = compact_memory_highlight_limit(mode);
    let memory_highlights = memory_highlights_from_values(
        &capsule.memories,
        memory_highlight_limit,
        memory_highlight_text_limit(mode),
    );
    let overview = build_task_bundle_overview(
        &capsule.query,
        &primary_files,
        &symbols,
        &tests,
        &risks,
        &memory_highlights,
    );
    let suggested_expand =
        suggest_task_bundle_expand(mode, &primary_files, &secondary_files, &symbols, &tests);
    let compact_memories = compact_response_memories(&capsule.memories, mode);

    TaskBundle {
        query: capsule.query.clone(),
        intent: capsule.intent,
        overview,
        suggested_expand,
        primary_files,
        secondary_files,
        symbols,
        tests,
        test_gaps,
        matched_rules,
        memories: compact_memories,
        memory_highlights,
        risks,
        rationale,
        stats: if matches!(mode, BundleMode::Full) {
            Some(TaskBundleStats {
                primary_file_count,
                secondary_file_count,
                symbol_count,
                test_count,
                memory_count: capsule.memories.len(),
            })
        } else {
            None
        },
    }
}

pub fn plan_edit(
    graph: &CodeGraph,
    capsule: &ContextCapsule,
    entry_files: &[String],
    entry_symbols: &[String],
    rules: &[ProjectRule],
    mode: BundleMode,
) -> PlanEditBundle {
    let task_bundle = prepare_change(graph, capsule, entry_files, entry_symbols, rules, mode);
    let mut edit_files = task_bundle.primary_files.clone();
    let mut supporting_files = task_bundle.secondary_files.clone();
    let symbols = task_bundle.symbols.clone();
    let mut candidate_spans = build_plan_edit_spans(graph, &symbols, mode);
    let span_symbol_keys: HashSet<(String, String)> = candidate_spans
        .iter()
        .map(|span| (span.file.clone(), span.symbol.clone()))
        .collect();

    let mut affected_callers = collect_plan_edit_impacts(
        graph,
        &span_symbol_keys,
        mode.affected_limit(),
        &[EdgeKind::Calls],
        "caller",
        true,
        false,
    );
    let mut affected_dependencies = collect_plan_edit_impacts(
        graph,
        &span_symbol_keys,
        mode.affected_limit(),
        &[
            EdgeKind::Imports,
            EdgeKind::Implements,
            EdgeKind::Extends,
            EdgeKind::TypeRef,
        ],
        "dependency",
        false,
        true,
    );

    let stale_doc_report = find_stale_docs(
        graph,
        &edit_files
            .iter()
            .chain(supporting_files.iter())
            .map(|item| item.file.clone())
            .collect::<Vec<_>>(),
        &symbols
            .iter()
            .map(|item| item.symbol.clone())
            .collect::<Vec<_>>(),
        mode.affected_limit().max(6),
    );
    let mut relevant_docs = stale_doc_report
        .docs
        .iter()
        .map(|item| PlanEditDocRecommendation {
            file: item.file.clone(),
            line: item.line,
            summary: truncate_text(&item.summary, 140),
            score: round_score(item.score),
            matched_files: item.matched_files.clone(),
            matched_symbols: item.matched_symbols.clone(),
            reasons: item.reasons.clone(),
        })
        .collect::<Vec<_>>();
    let mut stale_doc_signals = stale_doc_report
        .docs
        .iter()
        .take(6)
        .map(|item| {
            let mut parts = Vec::new();
            if !item.matched_symbols.is_empty() {
                parts.push(format!(
                    "mentions changed symbols {}",
                    summarize_item_list(
                        &item
                            .matched_symbols
                            .iter()
                            .take(3)
                            .cloned()
                            .collect::<Vec<_>>()
                    )
                ));
            }
            if !item.matched_files.is_empty() {
                parts.push(format!(
                    "touches changed files {}",
                    summarize_item_list(
                        &item
                            .matched_files
                            .iter()
                            .take(3)
                            .cloned()
                            .collect::<Vec<_>>()
                    )
                ));
            }
            if parts.is_empty() {
                format!(
                    "{}:{} may be stale against edit plan anchors",
                    item.file, item.line
                )
            } else {
                format!(
                    "{}:{} {}",
                    item.file,
                    item.line,
                    truncate_text(&parts.join("; "), 92)
                )
            }
        })
        .collect::<Vec<_>>();

    let mut rationale = task_bundle.rationale.clone();
    if !candidate_spans.is_empty() {
        rationale.push(format!(
            "Candidate edit spans map likely symbols to concrete line ranges for first-pass edits."
        ));
    }
    if !affected_callers.is_empty() {
        rationale.push(format!(
            "Caller impact was derived from direct call edges around the candidate edit spans."
        ));
    }
    if !affected_dependencies.is_empty() {
        rationale.push(format!(
            "Dependency impact highlights imports, type references, and interface contracts near planned edits."
        ));
    }
    if !relevant_docs.is_empty() {
        rationale.push(format!(
            "Found {} doc section(s) that may drift from the planned edits.",
            relevant_docs.len()
        ));
    }
    dedupe_strings(&mut rationale);

    if matches!(mode, BundleMode::Compact) {
        compactify_file_recommendations(&mut edit_files);
        compactify_file_recommendations(&mut supporting_files);
        compactify_plan_edit_impacts(&mut affected_callers);
        compactify_plan_edit_impacts(&mut affected_dependencies);
        compactify_plan_edit_docs(&mut relevant_docs);
        ultra_compactify_plan_edit(
            &mut supporting_files,
            &mut candidate_spans,
            &mut affected_callers,
            &mut affected_dependencies,
            &mut relevant_docs,
            &mut stale_doc_signals,
            &mut rationale,
        );
    }

    let suggested_expand = candidate_spans
        .first()
        .and_then(|span| span.symbol_handle.clone())
        .map(|focus| ExpandSuggestion {
            focus,
            reason: "Inspect the top candidate edit span to apply the first patch safely."
                .to_string(),
        })
        .or(task_bundle.suggested_expand.clone());

    let overview = build_plan_edit_overview(
        &task_bundle.overview,
        &candidate_spans,
        &affected_callers,
        &relevant_docs,
    );
    let stats = if matches!(mode, BundleMode::Full) {
        Some(PlanEditStats {
            edit_file_count: edit_files.len(),
            supporting_file_count: supporting_files.len(),
            symbol_count: symbols.len(),
            span_count: candidate_spans.len(),
            caller_count: affected_callers.len(),
            dependency_count: affected_dependencies.len(),
            doc_count: relevant_docs.len(),
            stale_doc_signal_count: stale_doc_signals.len(),
            test_count: task_bundle.tests.len(),
            memory_count: task_bundle.memories.len(),
        })
    } else {
        None
    };

    PlanEditBundle {
        query: task_bundle.query,
        intent: task_bundle.intent,
        overview,
        suggested_expand,
        edit_files,
        supporting_files,
        symbols,
        candidate_spans,
        affected_callers,
        affected_dependencies,
        relevant_docs,
        stale_doc_signals,
        tests: task_bundle.tests,
        test_gaps: task_bundle.test_gaps,
        matched_rules: task_bundle.matched_rules,
        memories: task_bundle.memories,
        memory_highlights: task_bundle.memory_highlights,
        risks: task_bundle.risks,
        rationale,
        stats,
    }
}

pub fn trace_scenario(
    graph: &CodeGraph,
    scenario: &str,
    entry_files: &[String],
    entry_symbols: &[String],
    rules: &[ProjectRule],
    mode: BundleMode,
) -> ScenarioTraceBundle {
    let intent = detect_intent(scenario);
    let all_nodes = graph.all_nodes();
    let scenario_tokens: HashSet<String> = tokenize_path(scenario).into_iter().collect();
    let scenario_lower = scenario.to_ascii_lowercase();
    let scenario_has_failure_hints = SCENARIO_FAILURE_HINTS
        .iter()
        .any(|hint| scenario_lower.contains(hint));
    let scenario_has_guard_hints = SCENARIO_GUARD_HINTS
        .iter()
        .any(|hint| scenario_lower.contains(hint));
    let scenario_has_side_effect_hints = SCENARIO_SIDE_EFFECT_HINTS
        .iter()
        .any(|hint| scenario_lower.contains(hint));

    let mut file_scores: HashMap<String, FileAccumulator> = HashMap::new();
    let mut symbol_scores: HashMap<(String, String), SymbolAccumulator> = HashMap::new();
    let mut seed_nodes: Vec<&GraphNode> = Vec::new();
    let mut seed_seen: HashSet<(String, String)> = HashSet::new();
    let mut rationale = Vec::new();

    for file in entry_files {
        if !is_queryable_graph_file(file) {
            continue;
        }
        add_file_score(
            &mut file_scores,
            file,
            8.5,
            "user supplied scenario file anchor".to_string(),
        );
        for (index, node) in rank_file_focus_nodes(graph, &all_nodes, file)
            .into_iter()
            .take(3)
            .enumerate()
        {
            let delta = (5.2 - index as f64 * 0.7).max(2.0);
            add_symbol_score(
                &mut symbol_scores,
                &node.file,
                &node.name,
                Some(&node.id),
                node.kind.short_code().to_string(),
                node.line,
                "entry_file".to_string(),
                delta,
            );
            push_seed_node(&mut seed_nodes, &mut seed_seen, node);
        }
    }

    for symbol in entry_symbols {
        for (index, node) in find_symbol_matches(graph, symbol, entry_files)
            .into_iter()
            .take(4)
            .enumerate()
        {
            let delta = (7.5 - index as f64 * 0.75).max(2.5);
            add_file_score(
                &mut file_scores,
                &node.file,
                delta,
                format!("user supplied scenario symbol {}", symbol),
            );
            add_symbol_score(
                &mut symbol_scores,
                &node.file,
                &node.name,
                Some(&node.id),
                node.kind.short_code().to_string(),
                node.line,
                "entry_symbol".to_string(),
                delta,
            );
            push_seed_node(&mut seed_nodes, &mut seed_seen, node);
        }
    }

    let mut lexical_candidates: Vec<(&GraphNode, f64, Vec<String>)> = Vec::new();
    for node in &all_nodes {
        if !is_queryable_graph_file(&node.file) || is_test_file(&node.file) {
            continue;
        }

        let mut score = 0.0;
        let mut reasons = Vec::new();
        if input_mentions_symbol(scenario, &node.name) {
            score += 5.0;
            reasons.push("scenario text directly names this symbol".to_string());
        }

        let symbol_tokens: HashSet<String> = tokenize_path(&node.name).into_iter().collect();
        let file_tokens: HashSet<String> = focus_tokens_for_file(&node.file).into_iter().collect();
        let symbol_overlap = overlap_count_set(&scenario_tokens, &symbol_tokens);
        if symbol_overlap > 0 {
            score += symbol_overlap as f64 * 2.0;
            reasons.push(format!(
                "symbol name overlaps {} scenario token(s)",
                symbol_overlap
            ));
        }
        let file_overlap = overlap_count_set(&scenario_tokens, &file_tokens);
        if file_overlap > 0 {
            score += file_overlap as f64 * 1.4;
            reasons.push(format!(
                "file focus overlaps {} scenario token(s)",
                file_overlap
            ));
        }

        let lowered_name = node.name.to_ascii_lowercase();
        let lowered_file = node.file.to_ascii_lowercase();
        let substring_overlap =
            scenario_substring_overlap_hits(&scenario_tokens, &node.name, &node.file);
        if substring_overlap > 0 {
            score += substring_overlap as f64 * 1.15;
            reasons.push(format!(
                "substring overlap on {} scenario token(s)",
                substring_overlap
            ));
        }
        if lowered_file.contains("/routes/") {
            score += 2.0;
            reasons.push("route-layer symbol is a likely runtime entrypoint".to_string());
        }
        if lowered_name.contains("route") {
            score += 1.3;
        }
        if scenario_tokens.contains("refresh") && lowered_name.contains("refresh") {
            score += 0.9;
        }
        if scenario_tokens.contains("login") && lowered_name.contains("login") {
            score += 0.7;
        }
        if lowered_name.contains("fallback")
            && (scenario_tokens.contains("refresh") || scenario_tokens.contains("fail"))
        {
            score -= 0.5;
            reasons.push("fallback branch treated as plausible alternative".to_string());
        }

        if node.is_exported {
            score += 0.35;
        }
        score += graph.centrality(&node.id).min(3.0) * 0.2;

        if scenario_has_failure_hints && node_has_body_hint(node, SCENARIO_FAILURE_HINTS) {
            score += 1.4;
            reasons.push("contains failure-path hints in symbol body".to_string());
        }
        if scenario_has_guard_hints && node_has_body_hint(node, SCENARIO_GUARD_HINTS) {
            score += 0.8;
            reasons.push("contains guard logic hints in symbol body".to_string());
        }
        if scenario_has_side_effect_hints && node_has_body_hint(node, SCENARIO_SIDE_EFFECT_HINTS) {
            score += 0.8;
            reasons.push("contains side-effect hints in symbol body".to_string());
        }

        if score >= 1.5 {
            lexical_candidates.push((node, score, reasons));
        }
    }

    lexical_candidates.sort_by(|(node_a, score_a, _), (node_b, score_b, _)| {
        score_b
            .partial_cmp(score_a)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                graph
                    .centrality(&node_b.id)
                    .partial_cmp(&graph.centrality(&node_a.id))
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| node_a.file.cmp(&node_b.file))
            .then_with(|| node_a.line.cmp(&node_b.line))
            .then_with(|| node_a.name.cmp(&node_b.name))
    });
    lexical_candidates.truncate(mode.symbol_limit().max(6) * 3);

    for (index, (node, score, mut reasons)) in lexical_candidates.into_iter().enumerate() {
        let delta = (score - index as f64 * 0.15).max(0.6);
        dedupe_strings(&mut reasons);
        let reason = reasons
            .into_iter()
            .next()
            .unwrap_or_else(|| "scenario lexical overlap".to_string());

        add_file_score(&mut file_scores, &node.file, delta, reason);
        add_symbol_score(
            &mut symbol_scores,
            &node.file,
            &node.name,
            Some(&node.id),
            node.kind.short_code().to_string(),
            node.line,
            "scenario_match".to_string(),
            delta,
        );
        push_seed_node(&mut seed_nodes, &mut seed_seen, node);
    }

    for node in &seed_nodes {
        for (dependency, edge) in rank_related_nodes(graph.get_dependencies(&node.id))
            .into_iter()
            .take(5)
        {
            let delta = match edge {
                EdgeKind::Calls => 1.8,
                EdgeKind::Contains => 0.9,
                _ => 1.2,
            };
            add_file_score(
                &mut file_scores,
                &dependency.file,
                delta,
                format!("{} scenario dependency via {}", node.name, short_edge(edge)),
            );
            add_symbol_score(
                &mut symbol_scores,
                &dependency.file,
                &dependency.name,
                Some(&dependency.id),
                dependency.kind.short_code().to_string(),
                dependency.line,
                format!("dependency:{}", short_edge(edge)),
                delta,
            );
        }

        for (dependent, edge) in rank_related_nodes(graph.get_dependents(&node.id))
            .into_iter()
            .take(4)
        {
            let delta = match edge {
                EdgeKind::Calls => 1.6,
                EdgeKind::Contains => 0.9,
                _ => 1.1,
            };
            add_file_score(
                &mut file_scores,
                &dependent.file,
                delta,
                format!(
                    "{} scenario dependent via {}",
                    dependent.name,
                    short_edge(edge)
                ),
            );
            add_symbol_score(
                &mut symbol_scores,
                &dependent.file,
                &dependent.name,
                Some(&dependent.id),
                dependent.kind.short_code().to_string(),
                dependent.line,
                format!("dependent:{}", short_edge(edge)),
                delta,
            );
        }
    }

    let mut ranked_files = finalize_file_recommendations(file_scores);
    ranked_files = promote_explicit_files(ranked_files, entry_files);
    ranked_files = prioritize_entry_scope_files(ranked_files, entry_files);
    calibrate_file_recommendations(&mut ranked_files);

    let mut ranked_symbols =
        prefer_symbols_in_files(finalize_symbol_recommendations(symbol_scores), entry_files);
    calibrate_symbol_recommendations(&mut ranked_symbols);

    let mut candidate_entrypoints: Vec<SymbolRecommendation> = ranked_symbols
        .iter()
        .filter(|item| !is_test_file(&item.file))
        .cloned()
        .collect();
    if candidate_entrypoints.is_empty() {
        candidate_entrypoints = ranked_symbols.clone();
    }

    let mut likely_entrypoints = Vec::new();
    let mut plausible_entrypoints = Vec::new();
    let entrypoint_sample_limit =
        scenario_entrypoint_limit(mode) + scenario_plausible_entrypoint_limit(mode);
    for item in candidate_entrypoints
        .iter()
        .take(entrypoint_sample_limit)
        .cloned()
    {
        if item.confidence_band == "high" || item.confidence_band == "medium" {
            likely_entrypoints.push(item);
        } else {
            plausible_entrypoints.push(item);
        }
    }
    if likely_entrypoints.is_empty() {
        if let Some(first) = candidate_entrypoints.first().cloned() {
            likely_entrypoints.push(first);
        }
        plausible_entrypoints = candidate_entrypoints
            .iter()
            .skip(1)
            .take(scenario_plausible_entrypoint_limit(mode))
            .cloned()
            .collect();
    }
    likely_entrypoints.truncate(scenario_entrypoint_limit(mode));
    plausible_entrypoints.truncate(scenario_plausible_entrypoint_limit(mode));

    let mut traced_nodes: Vec<&GraphNode> = Vec::new();
    let mut traced_seen: HashSet<(String, String)> = HashSet::new();
    for item in likely_entrypoints
        .iter()
        .chain(plausible_entrypoints.iter())
    {
        if let Some(node) = resolve_recommended_symbol_node(graph, item) {
            push_seed_node(&mut traced_nodes, &mut traced_seen, node);
        }
    }

    let mut path_scores: HashMap<
        (String, String, String, String, String),
        ScenarioPathAccumulator,
    > = HashMap::new();
    let mut add_path_segment =
        |from: &GraphNode, to: &GraphNode, relationship: String, delta: f64, reason: String| {
            if !is_queryable_graph_file(&from.file)
                || !is_queryable_graph_file(&to.file)
                || is_test_file(&from.file)
            {
                return;
            }
            let key = (
                from.file.clone(),
                from.name.clone(),
                to.file.clone(),
                to.name.clone(),
                relationship.clone(),
            );
            let entry = path_scores.entry(key).or_default();
            if entry.from_kind.is_empty() {
                entry.from_kind = from.kind.short_code().to_string();
            }
            if entry.from_line == 0 {
                entry.from_line = from.line;
            }
            if entry.from_offset.is_none() {
                entry.from_offset = Some(from.id.byte_offset);
            }
            if entry.to_kind.is_empty() {
                entry.to_kind = to.kind.short_code().to_string();
            }
            if entry.to_line == 0 {
                entry.to_line = to.line;
            }
            if entry.to_offset.is_none() {
                entry.to_offset = Some(to.id.byte_offset);
            }
            entry.score += delta;
            entry.rationale.push(reason);
        };

    for node in &traced_nodes {
        for (dependency, edge) in rank_related_nodes(graph.get_dependencies(&node.id))
            .into_iter()
            .take(5)
        {
            let mut delta = match edge {
                EdgeKind::Calls => 2.3,
                EdgeKind::Imports | EdgeKind::TypeRef => 1.45,
                _ => 1.1,
            };
            delta +=
                scenario_path_alignment_bonus(&scenario_tokens, &dependency.name, &dependency.file);
            if scenario_tokens.contains("refresh")
                && dependency.name.to_ascii_lowercase().contains("fallback")
            {
                delta -= 0.65;
            }
            if scenario_tokens.contains("fail")
                && likely_failure_symbol_name(&dependency.name, &dependency.body)
            {
                delta += 0.7;
            }
            add_path_segment(
                node,
                dependency,
                format!("forward:{}", short_edge(edge)),
                delta,
                format!("outgoing {} from {}", short_edge(edge), node.name),
            );

            if edge == EdgeKind::Calls {
                for (next, next_edge) in rank_related_nodes(graph.get_dependencies(&dependency.id))
                    .into_iter()
                    .take(2)
                {
                    if next_edge != EdgeKind::Calls && next_edge != EdgeKind::TypeRef {
                        continue;
                    }
                    let mut hop_delta = 0.95;
                    hop_delta +=
                        scenario_path_alignment_bonus(&scenario_tokens, &next.name, &next.file);
                    if scenario_tokens.contains("refresh")
                        && next.name.to_ascii_lowercase().contains("fallback")
                    {
                        hop_delta -= 0.45;
                    }
                    add_path_segment(
                        dependency,
                        next,
                        format!("forward2:{}", short_edge(next_edge)),
                        hop_delta,
                        format!(
                            "second-hop {} after {}",
                            short_edge(next_edge),
                            dependency.name
                        ),
                    );
                }
            }
        }

        for (dependent, edge) in rank_related_nodes(graph.get_dependents(&node.id))
            .into_iter()
            .take(4)
        {
            let mut delta = match edge {
                EdgeKind::Calls => 1.95,
                _ => 1.15,
            };
            delta +=
                scenario_path_alignment_bonus(&scenario_tokens, &dependent.name, &dependent.file);
            if scenario_tokens.contains("refresh")
                && dependent.name.to_ascii_lowercase().contains("fallback")
            {
                delta -= 0.5;
            }
            add_path_segment(
                dependent,
                node,
                format!("incoming:{}", short_edge(edge)),
                delta,
                format!("incoming {} into {}", short_edge(edge), node.name),
            );
        }
    }

    let mut all_path_segments = finalize_scenario_path_segments(path_scores);
    let mut execution_path = Vec::new();
    let mut plausible_paths = Vec::new();
    for segment in all_path_segments.drain(..) {
        if segment.confidence_band == "high" || segment.confidence_band == "medium" {
            execution_path.push(segment);
        } else {
            plausible_paths.push(segment);
        }
    }
    if execution_path.is_empty() && !plausible_paths.is_empty() {
        execution_path.push(plausible_paths.remove(0));
    }
    execution_path.truncate(scenario_path_limit(mode));
    plausible_paths.truncate(scenario_plausible_path_limit(mode));

    let mut signal_nodes: Vec<&GraphNode> = Vec::new();
    let mut signal_seen: HashSet<(String, String)> = HashSet::new();
    for node in &traced_nodes {
        push_seed_node(&mut signal_nodes, &mut signal_seen, node);
    }
    for segment in execution_path.iter().chain(plausible_paths.iter()) {
        if let Some(node) = find_exact_node(graph, &segment.from_file, &segment.from_symbol) {
            push_seed_node(&mut signal_nodes, &mut signal_seen, node);
        }
        if let Some(node) = find_exact_node(graph, &segment.to_file, &segment.to_symbol) {
            push_seed_node(&mut signal_nodes, &mut signal_seen, node);
        }
    }
    let likely_entrypoint_set: HashSet<(String, String)> = likely_entrypoints
        .iter()
        .map(|item| (item.file.clone(), item.symbol.clone()))
        .collect();

    let mut guards = collect_scenario_signals(
        &signal_nodes,
        "guard",
        SCENARIO_GUARD_HINTS,
        &likely_entrypoint_set,
        &scenario_tokens,
        scenario_signal_limit(mode),
    );
    let mut side_effects = collect_scenario_signals(
        &signal_nodes,
        "side_effect",
        SCENARIO_SIDE_EFFECT_HINTS,
        &likely_entrypoint_set,
        &scenario_tokens,
        scenario_signal_limit(mode),
    );
    let mut failure_branches = collect_scenario_signals(
        &signal_nodes,
        "failure_branch",
        SCENARIO_FAILURE_HINTS,
        &likely_entrypoint_set,
        &scenario_tokens,
        scenario_signal_limit(mode),
    );

    let mut doc_anchor_files: Vec<String> = likely_entrypoints
        .iter()
        .map(|item| item.file.clone())
        .collect();
    doc_anchor_files.extend(execution_path.iter().map(|item| item.from_file.clone()));
    doc_anchor_files.extend(execution_path.iter().map(|item| item.to_file.clone()));
    doc_anchor_files.extend(plausible_paths.iter().map(|item| item.from_file.clone()));
    doc_anchor_files.extend(plausible_paths.iter().map(|item| item.to_file.clone()));
    dedupe_strings(&mut doc_anchor_files);
    doc_anchor_files.retain(|file| is_queryable_graph_file(file));

    let mut doc_anchor_symbols: Vec<String> = likely_entrypoints
        .iter()
        .map(|item| item.symbol.clone())
        .collect();
    doc_anchor_symbols.extend(execution_path.iter().map(|item| item.from_symbol.clone()));
    doc_anchor_symbols.extend(execution_path.iter().map(|item| item.to_symbol.clone()));
    dedupe_strings(&mut doc_anchor_symbols);

    let stale_doc_report = find_stale_docs(
        graph,
        &doc_anchor_files,
        &doc_anchor_symbols,
        scenario_doc_limit(mode),
    );
    let mut relevant_docs = stale_doc_report
        .docs
        .iter()
        .map(|item| PlanEditDocRecommendation {
            file: item.file.clone(),
            line: item.line,
            summary: truncate_text(&item.summary, 140),
            score: round_score(item.score),
            matched_files: item.matched_files.clone(),
            matched_symbols: item.matched_symbols.clone(),
            reasons: item.reasons.clone(),
        })
        .collect::<Vec<_>>();

    let test_anchor_files: Vec<String> = doc_anchor_files
        .iter()
        .filter(|file| !is_test_file(file))
        .cloned()
        .collect();
    let test_report = find_relevant_tests(
        graph,
        &test_anchor_files,
        &doc_anchor_symbols,
        None,
        rules,
        mode.test_limit(),
    );
    let mut tests = test_report.tests;
    let mut test_gaps = test_report.gaps;
    let mut matched_rules = test_report.matched_rules;

    if !likely_entrypoints.is_empty() {
        rationale.push(
            "Likely entrypoints are ranked from scenario-token overlap, explicit anchors, and nearby graph structure."
                .to_string(),
        );
    } else {
        rationale.push(
            "No high-confidence scenario entrypoint was found; consider adding file or symbol anchors."
                .to_string(),
        );
    }
    if !execution_path.is_empty() {
        rationale.push(
            "Execution path segments prioritize direct call edges and adjacent dependency hops from likely entrypoints."
                .to_string(),
        );
    }
    if !guards.is_empty() || !side_effects.is_empty() || !failure_branches.is_empty() {
        rationale.push(
            "Guard, side-effect, and failure signals are mined from candidate symbol bodies along the traced path."
                .to_string(),
        );
    }
    if !relevant_docs.is_empty() {
        rationale.push(format!(
            "Found {} relevant doc section(s) tied to the traced files and symbols.",
            relevant_docs.len()
        ));
    }
    if !tests.is_empty() {
        rationale.push(format!(
            "Suggested {} test target(s) using traced files/symbols and graph-linked test dependencies.",
            tests.len()
        ));
    }
    dedupe_strings(&mut rationale);

    if matches!(mode, BundleMode::Compact) {
        compactify_symbol_recommendations(&mut likely_entrypoints);
        compactify_symbol_recommendations(&mut plausible_entrypoints);
        compactify_scenario_path_segments(&mut execution_path);
        compactify_scenario_path_segments(&mut plausible_paths);
        compactify_scenario_signals(&mut guards);
        compactify_scenario_signals(&mut side_effects);
        compactify_scenario_signals(&mut failure_branches);
        compactify_plan_edit_docs(&mut relevant_docs);
        compactify_test_recommendations(&mut tests);
        ultra_compactify_trace_scenario(
            &mut plausible_entrypoints,
            &mut execution_path,
            &mut plausible_paths,
            &mut guards,
            &mut side_effects,
            &mut failure_branches,
            &mut relevant_docs,
            &mut tests,
            &mut test_gaps,
            &mut matched_rules,
            &mut rationale,
        );
    }

    let overview = build_trace_scenario_overview(
        scenario,
        &likely_entrypoints,
        &execution_path,
        &failure_branches,
        &tests,
    );
    let suggested_expand =
        suggest_trace_scenario_expand(mode, &likely_entrypoints, &execution_path);
    let likely_entrypoint_count = likely_entrypoints.len();
    let plausible_entrypoint_count = plausible_entrypoints.len();
    let execution_path_count = execution_path.len();
    let plausible_path_count = plausible_paths.len();
    let guard_count = guards.len();
    let side_effect_count = side_effects.len();
    let failure_branch_count = failure_branches.len();
    let doc_count = relevant_docs.len();
    let test_count = tests.len();
    let stats = if matches!(mode, BundleMode::Full) {
        Some(ScenarioTraceStats {
            likely_entrypoint_count,
            plausible_entrypoint_count,
            execution_path_count,
            plausible_path_count,
            guard_count,
            side_effect_count,
            failure_branch_count,
            doc_count,
            test_count,
        })
    } else {
        None
    };

    ScenarioTraceBundle {
        scenario: scenario.to_string(),
        intent,
        overview,
        suggested_expand,
        likely_entrypoints,
        plausible_entrypoints,
        execution_path,
        plausible_paths,
        guards,
        side_effects,
        failure_branches,
        relevant_docs,
        tests,
        test_gaps,
        matched_rules,
        rationale,
        stats,
    }
}

fn build_plan_edit_spans(
    graph: &CodeGraph,
    symbols: &[SymbolRecommendation],
    mode: BundleMode,
) -> Vec<EditSpanRecommendation> {
    let span_limit = match mode {
        BundleMode::Compact => 5,
        BundleMode::Full => 12,
    };
    let mut spans = Vec::new();
    let mut seen = HashSet::new();

    for symbol in symbols.iter().take(span_limit.max(1)) {
        let node = symbol
            .symbol_handle
            .as_deref()
            .and_then(SymbolId::from_stable_handle)
            .and_then(|id| find_exact_node_by_id(graph, &id))
            .or_else(|| find_exact_node(graph, &symbol.file, &symbol.symbol));

        let (start_line, end_line, line_span) = if let Some(node) = node {
            (
                node.line,
                node.end_line.max(node.line),
                format_line_span(node),
            )
        } else {
            (symbol.line, symbol.line, symbol.line.to_string())
        };

        let key = (
            symbol.file.clone(),
            symbol.symbol.clone(),
            start_line,
            end_line,
        );
        if !seen.insert(key) {
            continue;
        }

        spans.push(EditSpanRecommendation {
            file: symbol.file.clone(),
            symbol: symbol.symbol.clone(),
            symbol_handle: symbol.symbol_handle.clone(),
            line_span,
            start_line,
            end_line,
            reason: truncate_text(
                &format!(
                    "{} candidate from {}",
                    symbol_role_label(&symbol.role),
                    basename_without_extension(&symbol.file)
                ),
                96,
            ),
            confidence_band: symbol.confidence_band.clone(),
        });
    }

    spans
}

fn collect_plan_edit_impacts(
    graph: &CodeGraph,
    span_symbol_keys: &HashSet<(String, String)>,
    limit: usize,
    edges: &[EdgeKind],
    relationship_prefix: &str,
    include_dependents: bool,
    include_dependencies: bool,
) -> Vec<PlanEditImpact> {
    if span_symbol_keys.is_empty() {
        return Vec::new();
    }

    let edge_filter: HashSet<EdgeKind> = edges.iter().copied().collect();
    let seed_nodes: Vec<&GraphNode> = graph
        .all_nodes()
        .into_iter()
        .filter(|node| {
            is_queryable_graph_file(&node.file)
                && span_symbol_keys.contains(&(node.file.clone(), node.name.clone()))
        })
        .collect();

    if seed_nodes.is_empty() {
        return Vec::new();
    }

    let mut scores: HashMap<(String, String, String), AffectedAccumulator> = HashMap::new();
    let mut record_related = |related: &GraphNode, edge: EdgeKind, via: &str, delta: f64| {
        if !edge_filter.contains(&edge) || !is_queryable_graph_file(&related.file) {
            return;
        }
        let relationship = format!("{}:{}", relationship_prefix, short_edge(edge));
        let key = (related.file.clone(), related.name.clone(), relationship);
        let entry = scores.entry(key).or_default();
        if entry.kind.is_empty() {
            entry.kind = related.kind.short_code().to_string();
        }
        if entry.line == 0 {
            entry.line = related.line;
        }
        if entry.byte_offset.is_none() {
            entry.byte_offset = Some(related.id.byte_offset);
        }
        entry.score += delta;
        entry.via.push(via.to_string());
    };

    for node in seed_nodes {
        if include_dependents {
            for (related, edge) in graph.get_dependents(&node.id) {
                record_related(related, edge, &node.name, 1.25);
            }
        }
        if include_dependencies {
            for (related, edge) in graph.get_dependencies(&node.id) {
                record_related(related, edge, &node.name, 1.1);
            }
        }
    }

    let mut impacts = scores
        .into_iter()
        .map(|((file, symbol, relationship), mut acc)| {
            dedupe_strings(&mut acc.via);
            acc.via.truncate(3);
            let symbol_handle = acc.byte_offset.map(|byte_offset| {
                SymbolId {
                    file: file.clone(),
                    name: symbol.clone(),
                    byte_offset,
                }
                .stable_handle()
            });
            PlanEditImpact {
                symbol,
                symbol_handle,
                kind: acc.kind,
                file,
                line: acc.line,
                relationship,
                via: acc.via,
                score: round_score(acc.score),
            }
        })
        .collect::<Vec<_>>();

    impacts.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    impacts.truncate(limit.max(1));
    impacts
}

fn scenario_entrypoint_limit(mode: BundleMode) -> usize {
    match mode {
        BundleMode::Compact => COMPACT_SCENARIO_ENTRYPOINT_LIMIT,
        BundleMode::Full => FULL_SCENARIO_ENTRYPOINT_LIMIT,
    }
}

fn scenario_plausible_entrypoint_limit(mode: BundleMode) -> usize {
    match mode {
        BundleMode::Compact => 3,
        BundleMode::Full => 6,
    }
}

fn scenario_path_limit(mode: BundleMode) -> usize {
    match mode {
        BundleMode::Compact => COMPACT_SCENARIO_PATH_LIMIT,
        BundleMode::Full => FULL_SCENARIO_PATH_LIMIT,
    }
}

fn scenario_plausible_path_limit(mode: BundleMode) -> usize {
    match mode {
        BundleMode::Compact => 4,
        BundleMode::Full => 8,
    }
}

fn scenario_signal_limit(mode: BundleMode) -> usize {
    match mode {
        BundleMode::Compact => COMPACT_SCENARIO_SIGNAL_LIMIT,
        BundleMode::Full => FULL_SCENARIO_SIGNAL_LIMIT,
    }
}

fn scenario_doc_limit(mode: BundleMode) -> usize {
    match mode {
        BundleMode::Compact => 6,
        BundleMode::Full => 12,
    }
}

fn finalize_scenario_path_segments(
    scores: HashMap<(String, String, String, String, String), ScenarioPathAccumulator>,
) -> Vec<ScenarioPathSegment> {
    let mut segments = scores
        .into_iter()
        .map(
            |((from_file, from_symbol, to_file, to_symbol, relationship), mut acc)| {
                dedupe_strings(&mut acc.rationale);
                acc.rationale.truncate(3);
                let score = round_score(acc.score);
                let from_symbol_handle = acc.from_offset.map(|byte_offset| {
                    SymbolId {
                        file: from_file.clone(),
                        name: from_symbol.clone(),
                        byte_offset,
                    }
                    .stable_handle()
                });
                let to_symbol_handle = acc.to_offset.map(|byte_offset| {
                    SymbolId {
                        file: to_file.clone(),
                        name: to_symbol.clone(),
                        byte_offset,
                    }
                    .stable_handle()
                });
                ScenarioPathSegment {
                    from_symbol,
                    from_symbol_handle,
                    from_kind: acc.from_kind,
                    from_file,
                    from_line: acc.from_line,
                    to_symbol,
                    to_symbol_handle,
                    to_kind: acc.to_kind,
                    to_file,
                    to_line: acc.to_line,
                    relationship,
                    score,
                    confidence_band: "low".to_string(),
                    rationale: acc.rationale,
                }
            },
        )
        .collect::<Vec<_>>();

    segments.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.from_file.cmp(&b.from_file))
            .then_with(|| a.from_line.cmp(&b.from_line))
            .then_with(|| a.to_file.cmp(&b.to_file))
            .then_with(|| a.to_line.cmp(&b.to_line))
            .then_with(|| a.from_symbol.cmp(&b.from_symbol))
    });

    let top_score = segments.first().map(|item| item.score).unwrap_or(0.0);
    for item in &mut segments {
        item.confidence_band = scenario_confidence_band(item.score, top_score);
    }

    segments
}

fn collect_scenario_signals(
    nodes: &[&GraphNode],
    signal_type: &str,
    hints: &[&str],
    likely_entrypoint_set: &HashSet<(String, String)>,
    scenario_tokens: &HashSet<String>,
    limit: usize,
) -> Vec<ScenarioSignal> {
    let mut items = Vec::new();

    for node in nodes {
        let Some(snippet) = extract_scenario_signal_hint(node, signal_type, hints) else {
            continue;
        };

        let symbol_tokens: HashSet<String> = tokenize_path(&node.name).into_iter().collect();
        let file_tokens: HashSet<String> = focus_tokens_for_file(&node.file).into_iter().collect();
        let symbol_overlap = overlap_count_set(scenario_tokens, &symbol_tokens);
        let file_overlap = overlap_count_set(scenario_tokens, &file_tokens);
        let is_likely_entrypoint =
            likely_entrypoint_set.contains(&(node.file.clone(), node.name.clone()));

        let mut score = 1.5 + symbol_overlap as f64 * 0.8 + file_overlap as f64 * 0.5;
        if is_likely_entrypoint {
            score += 2.2;
        }
        if node.is_exported {
            score += 0.35;
        }

        items.push(ScenarioSignal {
            signal_type: signal_type.to_string(),
            symbol: node.name.clone(),
            symbol_handle: Some(symbol_focus_for_node(node)),
            kind: node.kind.short_code().to_string(),
            file: node.file.clone(),
            line: node.line,
            summary: truncate_text(&snippet, 140),
            score: round_score(score),
            confidence_band: "low".to_string(),
        });
    }

    items.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    items.dedup_by(|a, b| {
        a.file == b.file && a.symbol == b.symbol && a.signal_type == b.signal_type
    });
    let top_score = items.first().map(|item| item.score).unwrap_or(0.0);
    for item in &mut items {
        item.confidence_band = scenario_confidence_band(item.score, top_score);
    }
    items.truncate(limit.max(1));
    items
}

fn scenario_substring_overlap_hits(
    scenario_tokens: &HashSet<String>,
    symbol: &str,
    file: &str,
) -> usize {
    let lowered_symbol = symbol.to_ascii_lowercase();
    let lowered_file = file.to_ascii_lowercase();
    scenario_tokens
        .iter()
        .filter(|token| {
            lowered_symbol.contains(token.as_str()) || lowered_file.contains(token.as_str())
        })
        .count()
}

fn scenario_path_alignment_bonus(
    scenario_tokens: &HashSet<String>,
    symbol: &str,
    file: &str,
) -> f64 {
    let hits = scenario_substring_overlap_hits(scenario_tokens, symbol, file);
    if hits == 0 {
        0.0
    } else {
        hits as f64 * 0.55
    }
}

fn likely_failure_symbol_name(symbol: &str, body: &str) -> bool {
    let lowered_symbol = symbol.to_ascii_lowercase();
    let lowered_body = body.to_ascii_lowercase();
    lowered_symbol.contains("reject")
        || lowered_symbol.contains("deny")
        || lowered_symbol.contains("forbid")
        || lowered_symbol.contains("fail")
        || lowered_symbol.contains("error")
        || lowered_symbol.contains("unauthor")
        || lowered_symbol.contains("expired")
        || lowered_body.contains("401")
        || lowered_body.contains("403")
        || lowered_body.contains("404")
        || lowered_body.contains("500")
        || lowered_body.contains("throw")
        || lowered_body.contains("panic")
        || lowered_body.contains("error")
}

fn node_has_body_hint(node: &GraphNode, hints: &[&str]) -> bool {
    let lower = node.body.to_ascii_lowercase();
    hints.iter().any(|hint| lower.contains(hint))
}

fn extract_node_body_hint(node: &GraphNode, hints: &[&str]) -> Option<String> {
    for line in node.body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if hints.iter().any(|hint| lower.contains(hint)) {
            return Some(trimmed.to_string());
        }
    }

    let body = node.body.trim();
    if body.is_empty() || !node_has_body_hint(node, hints) {
        return None;
    }

    Some(truncate_text(body, 140))
}

fn extract_scenario_signal_hint(
    node: &GraphNode,
    signal_type: &str,
    hints: &[&str],
) -> Option<String> {
    if let Some(snippet) = extract_node_body_hint(node, hints) {
        return Some(snippet);
    }

    let lowered_name = node.name.to_ascii_lowercase();
    let lowered_signature = node.signature.to_ascii_lowercase();
    let lowered_body = node.body.to_ascii_lowercase();

    let keyword_hit = |keywords: &[&str]| {
        keywords
            .iter()
            .any(|keyword| lowered_name.contains(keyword) || lowered_signature.contains(keyword))
    };

    match signal_type {
        "guard" => keyword_hit(&[
            "verify",
            "validate",
            "check",
            "guard",
            "authorize",
            "require",
        ])
        .then(|| format!("{} appears to guard token/access checks", node.name)),
        "side_effect" => keyword_hit(&[
            "create", "update", "write", "save", "persist", "dispatch", "send", "publish",
            "enqueue",
        ])
        .then(|| {
            format!(
                "{} appears to trigger a state or external side effect",
                node.name
            )
        }),
        "failure_branch" => (keyword_hit(&[
            "reject", "deny", "forbid", "fail", "error", "unauthor", "expired",
        ]) || lowered_body.contains("401")
            || lowered_body.contains("403")
            || lowered_body.contains("404")
            || lowered_body.contains("500")
            || lowered_body.contains("throw")
            || lowered_body.contains("panic"))
        .then(|| {
            format!(
                "{} appears to return or trigger failure handling",
                node.name
            )
        }),
        _ => None,
    }
}

pub fn find_relevant_tests(
    graph: &CodeGraph,
    files: &[String],
    symbols: &[String],
    diff: Option<&str>,
    rules: &[ProjectRule],
    limit: usize,
) -> TestSelectionReport {
    let mut source_files: Vec<String> = files.to_vec();
    source_files.extend(extract_diff_files(diff));

    let preferred_symbol_files = source_files.clone();
    for symbol in symbols {
        for node in find_symbol_matches(graph, symbol, &preferred_symbol_files) {
            source_files.push(node.file.clone());
        }
    }

    dedupe_strings(&mut source_files);
    source_files.retain(|file| !is_test_file(file) && is_queryable_graph_file(file));

    let mut test_scores: HashMap<String, TestAccumulator> = HashMap::new();
    let source_tokens: HashSet<String> = source_files
        .iter()
        .flat_map(|file| tokenize_path(file))
        .collect();
    let source_focus_tokens: HashSet<String> = source_files
        .iter()
        .flat_map(|file| focus_tokens_for_file(file))
        .collect();
    let symbol_tokens: HashSet<String> = symbols
        .iter()
        .flat_map(|symbol| tokenize_path(symbol))
        .collect();
    let source_file_set: HashSet<String> = source_files.iter().cloned().collect();
    let source_symbol_set: HashSet<String> = symbols.iter().cloned().collect();

    let all_nodes = graph.all_nodes();
    let all_files = unique_graph_files(graph);
    for test_file in all_files.into_iter().filter(|file| {
        is_test_file(file) && !is_test_support_file(file) && is_queryable_graph_file(file)
    }) {
        let test_tokens = tokenize_path(&test_file);
        let test_token_set: HashSet<String> = test_tokens.iter().cloned().collect();
        let test_nodes: Vec<&GraphNode> = all_nodes
            .iter()
            .copied()
            .filter(|node| node.file == test_file)
            .collect();
        let test_symbol_tokens: HashSet<String> = test_nodes
            .iter()
            .flat_map(|node| tokenize_path(&node.name))
            .collect();
        let mut score = 0.0;
        let mut reasons = Vec::new();
        let mut has_strong_signal = false;

        let mut direct_source_file_hits: HashSet<String> = HashSet::new();
        let mut direct_symbol_hits: HashSet<String> = HashSet::new();
        for test_node in &test_nodes {
            for (dependency, _edge) in graph.get_dependencies(&test_node.id) {
                if !is_queryable_graph_file(&dependency.file) {
                    continue;
                }
                if source_file_set.contains(&dependency.file) {
                    direct_source_file_hits.insert(dependency.file.clone());
                }
                if source_symbol_set.contains(&dependency.name) {
                    direct_symbol_hits.insert(dependency.name.clone());
                }
            }
        }

        if !direct_source_file_hits.is_empty() {
            has_strong_signal = true;
            score += 5.0 + direct_source_file_hits.len() as f64 * 1.5;
            reasons.push(format!(
                "directly depends on {} likely source file(s)",
                direct_source_file_hits.len()
            ));
        }
        if !direct_symbol_hits.is_empty() {
            has_strong_signal = true;
            score += 4.0 + direct_symbol_hits.len() as f64;
            reasons.push(format!(
                "directly exercises {} hinted symbol(s)",
                direct_symbol_hits.len()
            ));
        }

        for source_file in &source_files {
            if normalized_stem(source_file) == normalized_stem(&test_file) {
                has_strong_signal = true;
                score += 4.0;
                reasons.push(format!("matches source file stem {}", source_file));
            }

            let directory_overlap = shared_directory_prefix_len(source_file, &test_file);
            if directory_overlap > 0 {
                let capped_overlap = directory_overlap.min(2);
                score += 0.35 * capped_overlap as f64;
                reasons.push(format!(
                    "shares {} directory segment(s) with {}",
                    capped_overlap, source_file
                ));
            }
        }

        let shared_source_tokens = overlap_count(&source_tokens, &test_tokens);
        if shared_source_tokens > 0 {
            has_strong_signal = true;
            score += shared_source_tokens as f64;
            reasons.push(format!(
                "shares {} path token(s) with the edit set",
                shared_source_tokens
            ));
        }

        let shared_symbol_tokens = overlap_count(&symbol_tokens, &test_tokens);
        if shared_symbol_tokens > 0 {
            score += 0.75 * shared_symbol_tokens as f64;
            reasons.push(format!("mentions {} symbol token(s)", shared_symbol_tokens));
        }

        let shared_focus_tokens = overlap_count_set(&source_focus_tokens, &test_token_set);
        if shared_focus_tokens > 0 {
            has_strong_signal = true;
            score += 2.0 * shared_focus_tokens as f64;
            reasons.push(format!(
                "matches {} core domain token(s) from the source file",
                shared_focus_tokens
            ));
        }

        let shared_test_symbol_tokens = overlap_count_set(&symbol_tokens, &test_symbol_tokens);
        if shared_test_symbol_tokens > 0 {
            score += 1.5 * shared_test_symbol_tokens as f64;
            reasons.push(format!(
                "shares {} token(s) with test symbol names",
                shared_test_symbol_tokens
            ));
        }

        let exact_symbol_name_hits = test_nodes
            .iter()
            .filter(|node| source_symbol_set.contains(&node.name))
            .count();
        if exact_symbol_name_hits > 0 {
            has_strong_signal = true;
            score += 3.0 + exact_symbol_name_hits as f64;
            reasons.push(format!(
                "contains {} test symbol(s) that exactly match the suspect set",
                exact_symbol_name_hits
            ));
        }

        if score > 0.0 && has_strong_signal {
            let entry = test_scores.entry(test_file.clone()).or_default();
            entry.score += score;
            entry.reasons.extend(reasons);
        }
    }

    let mut tests: Vec<TestRecommendation> = test_scores
        .into_iter()
        .map(|(file, mut acc)| {
            dedupe_strings(&mut acc.reasons);
            acc.reasons.truncate(3);
            let confidence = round_score(acc.score);
            let mut evidence = infer_test_evidence(&acc.reasons);
            dedupe_strings(&mut evidence);
            evidence.truncate(3);
            TestRecommendation {
                file,
                confidence,
                confidence_band: calibrate_test_confidence(confidence, &evidence),
                evidence,
                reasons: acc.reasons,
            }
        })
        .collect();
    tests.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
    });
    tests.truncate(limit.max(1));
    calibrate_test_recommendations(&mut tests);

    let mut gaps = Vec::new();
    if source_files.is_empty() {
        gaps.push(
            "No source files or symbols were available to anchor test selection.".to_string(),
        );
    } else if tests.is_empty() {
        gaps.push("No graph-indexed tests matched the likely edit set.".to_string());
    } else if tests.iter().all(|item| item.confidence < 2.0) {
        gaps.push(
            "Only low-confidence tests were found; consider widening the task bundle.".to_string(),
        );
    }

    let matched_rules: Vec<String> = rules
        .iter()
        .filter(|rule| rule.description.contains("Test files follow pattern"))
        .map(|rule| rule.description.clone())
        .collect();

    let mut rationale = Vec::new();
    if !source_files.is_empty() {
        rationale.push(format!(
            "Ranked tests using {} source file(s), {} symbol hint(s), and diff-derived paths.",
            source_files.len(),
            symbols.len()
        ));
    }
    if !matched_rules.is_empty() {
        rationale.push(format!(
            "Detected {} project test convention(s) that can guide future selection.",
            matched_rules.len()
        ));
    }

    TestSelectionReport {
        source_files,
        tests,
        gaps,
        matched_rules,
        rationale,
    }
}

pub fn get_working_set_context(
    graph: &CodeGraph,
    files: &[String],
    symbols: &[String],
    query: Option<&str>,
    memories: &[Value],
    rules: &[ProjectRule],
    mode: BundleMode,
) -> WorkingSetContext {
    let mut file_scores: HashMap<String, FileAccumulator> = HashMap::new();
    let mut active_symbol_scores: HashMap<(String, String), SymbolAccumulator> = HashMap::new();
    let mut nearby_symbol_scores: HashMap<(String, String), SymbolAccumulator> = HashMap::new();
    let mut seed_nodes: Vec<&GraphNode> = Vec::new();
    let mut seed_seen: HashSet<(String, String)> = HashSet::new();
    let mut rationale = Vec::new();
    let all_nodes = graph.all_nodes();
    let query_tokens: HashSet<String> = query
        .map(tokenize_path)
        .unwrap_or_default()
        .into_iter()
        .collect();

    for file in files {
        add_file_score(
            &mut file_scores,
            file,
            6.0,
            "active working-set file".to_string(),
        );

        for (index, node) in rank_file_focus_nodes(graph, &all_nodes, file)
            .into_iter()
            .take(mode.active_symbol_limit())
            .enumerate()
        {
            let delta = (4.0 - index as f64 * 0.45).max(1.5);
            add_symbol_score(
                &mut active_symbol_scores,
                &node.file,
                &node.name,
                Some(&node.id),
                node.kind.short_code().to_string(),
                node.line,
                "working_file".to_string(),
                delta,
            );
            push_seed_node(&mut seed_nodes, &mut seed_seen, node);
        }
    }

    for symbol in symbols {
        for (index, node) in find_symbol_matches(graph, symbol, files)
            .into_iter()
            .take(4)
            .enumerate()
        {
            let delta = (5.5 - index as f64 * 0.75).max(2.0);
            add_file_score(
                &mut file_scores,
                &node.file,
                delta,
                format!("focused symbol {}", symbol),
            );
            add_symbol_score(
                &mut active_symbol_scores,
                &node.file,
                &node.name,
                Some(&node.id),
                node.kind.short_code().to_string(),
                node.line,
                "focus_symbol".to_string(),
                delta,
            );
            push_seed_node(&mut seed_nodes, &mut seed_seen, node);
        }
    }

    if !query_tokens.is_empty() {
        let mut query_matches: Vec<(&GraphNode, usize)> = all_nodes
            .iter()
            .copied()
            .filter_map(|node| {
                let symbol_tokens = tokenize_path(&node.name);
                let file_tokens = tokenize_path(&node.file);
                let hits = overlap_count(&query_tokens, &symbol_tokens)
                    + overlap_count(&query_tokens, &file_tokens);
                (hits > 0).then_some((node, hits))
            })
            .collect();

        query_matches.sort_by(|(node_a, hits_a), (node_b, hits_b)| {
            hits_b
                .cmp(hits_a)
                .then_with(|| {
                    graph
                        .centrality(&node_b.id)
                        .partial_cmp(&graph.centrality(&node_a.id))
                        .unwrap_or(Ordering::Equal)
                })
                .then_with(|| node_a.file.cmp(&node_b.file))
                .then_with(|| node_a.line.cmp(&node_b.line))
        });

        for (index, (node, hits)) in query_matches
            .into_iter()
            .take(mode.active_symbol_limit())
            .enumerate()
        {
            let delta = 1.25 + hits as f64 * 0.5;
            add_file_score(
                &mut file_scores,
                &node.file,
                delta,
                format!("query token overlap with {}", node.name),
            );

            if files.is_empty() && symbols.is_empty() && index < 2 {
                add_symbol_score(
                    &mut active_symbol_scores,
                    &node.file,
                    &node.name,
                    Some(&node.id),
                    node.kind.short_code().to_string(),
                    node.line,
                    "query_match".to_string(),
                    delta,
                );
                push_seed_node(&mut seed_nodes, &mut seed_seen, node);
            } else {
                add_symbol_score(
                    &mut nearby_symbol_scores,
                    &node.file,
                    &node.name,
                    Some(&node.id),
                    node.kind.short_code().to_string(),
                    node.line,
                    "query_overlap".to_string(),
                    delta,
                );
            }
        }
    }

    for node in &seed_nodes {
        for (dependency, edge) in rank_related_nodes(graph.get_dependencies(&node.id))
            .into_iter()
            .take(4)
        {
            add_file_score(
                &mut file_scores,
                &dependency.file,
                1.1,
                format!("{} dependency via {}", node.name, short_edge(edge)),
            );
            add_symbol_score(
                &mut nearby_symbol_scores,
                &dependency.file,
                &dependency.name,
                Some(&dependency.id),
                dependency.kind.short_code().to_string(),
                dependency.line,
                format!("dependency:{}", short_edge(edge)),
                1.1,
            );
        }

        for (dependent, edge) in rank_related_nodes(graph.get_dependents(&node.id))
            .into_iter()
            .take(4)
        {
            add_file_score(
                &mut file_scores,
                &dependent.file,
                1.0,
                format!("{} dependent via {}", dependent.name, short_edge(edge)),
            );
            add_symbol_score(
                &mut nearby_symbol_scores,
                &dependent.file,
                &dependent.name,
                Some(&dependent.id),
                dependent.kind.short_code().to_string(),
                dependent.line,
                format!("dependent:{}", short_edge(edge)),
                1.0,
            );
        }

        for sibling in rank_file_focus_nodes(graph, &all_nodes, &node.file)
            .into_iter()
            .filter(|candidate| candidate.name != node.name)
            .take(2)
        {
            add_symbol_score(
                &mut nearby_symbol_scores,
                &sibling.file,
                &sibling.name,
                Some(&sibling.id),
                sibling.kind.short_code().to_string(),
                sibling.line,
                "same_file".to_string(),
                0.8,
            );
        }
    }

    let mut ranked_files = finalize_file_recommendations(file_scores.clone());
    calibrate_file_recommendations(&mut ranked_files);
    let files: Vec<FileRecommendation> = ranked_files
        .into_iter()
        .take(mode.working_file_limit())
        .collect();

    let mut active_symbols = finalize_symbol_recommendations(active_symbol_scores);
    calibrate_symbol_recommendations(&mut active_symbols);
    active_symbols.truncate(mode.active_symbol_limit());
    let active_keys: HashSet<(String, String)> = active_symbols
        .iter()
        .map(|item| (item.file.clone(), item.symbol.clone()))
        .collect();

    let mut nearby_symbols: Vec<SymbolRecommendation> =
        finalize_symbol_recommendations(nearby_symbol_scores)
            .into_iter()
            .filter(|item| !active_keys.contains(&(item.file.clone(), item.symbol.clone())))
            .collect();
    calibrate_symbol_recommendations(&mut nearby_symbols);
    nearby_symbols.truncate(mode.nearby_symbol_limit());

    let mut related_files: Vec<String> = files.iter().map(|item| item.file.clone()).collect();
    related_files.extend(active_symbols.iter().map(|item| item.file.clone()));
    related_files.extend(nearby_symbols.iter().map(|item| item.file.clone()));
    dedupe_strings(&mut related_files);

    let mut related_symbols: Vec<String> = active_symbols
        .iter()
        .map(|item| item.symbol.clone())
        .collect();
    related_symbols.extend(nearby_symbols.iter().map(|item| item.symbol.clone()));
    dedupe_strings(&mut related_symbols);

    let test_report = find_relevant_tests(
        graph,
        &related_files,
        &related_symbols,
        None,
        rules,
        mode.test_limit(),
    );

    if !files.is_empty() {
        rationale.push(format!(
            "Working-set files are ranked from active file hints, focused symbols, and nearby graph neighbors."
        ));
    }
    if !active_symbols.is_empty() {
        rationale.push(format!(
            "Active symbols prioritize the current file focus before expanding into nearby dependencies."
        ));
    }
    if !test_report.tests.is_empty() {
        rationale.push(format!(
            "Suggested tests are derived from the working-set files and active symbols."
        ));
    }
    if !memories.is_empty() {
        rationale.push(format!(
            "Recalled {} recent memory item(s) to reduce repeated rediscovery.",
            memories.len()
        ));
    }
    if files.is_empty() && active_symbols.is_empty() && nearby_symbols.is_empty() {
        rationale.push(
            "No working-set signals were available; provide files, symbols, or a query to anchor context."
                .to_string(),
        );
    }

    dedupe_strings(&mut rationale);

    let mut files = files;
    let mut active_symbols = active_symbols;
    let mut nearby_symbols = nearby_symbols;
    let mut tests = test_report.tests;
    let mut matched_rules = test_report.matched_rules;
    if matches!(mode, BundleMode::Compact) {
        compactify_file_recommendations(&mut files);
        compactify_symbol_recommendations(&mut active_symbols);
        compactify_symbol_recommendations(&mut nearby_symbols);
        compactify_test_recommendations(&mut tests);
        ultra_compactify_working_set(
            &mut files,
            &mut active_symbols,
            &mut nearby_symbols,
            &mut tests,
            &mut matched_rules,
            &mut rationale,
        );
    }

    let file_count = files.len();
    let active_symbol_count = active_symbols.len();
    let nearby_symbol_count = nearby_symbols.len();
    let test_count = tests.len();
    let memory_highlight_limit = compact_memory_highlight_limit(mode);
    let memory_highlights = memory_highlights_from_values(
        memories,
        memory_highlight_limit,
        memory_highlight_text_limit(mode),
    );
    let overview = build_working_set_overview(
        query,
        &files,
        &active_symbols,
        &nearby_symbols,
        &tests,
        &memory_highlights,
    );
    let suggested_expand =
        suggest_working_set_expand(mode, &files, &active_symbols, &nearby_symbols);
    let compact_memories = compact_response_memories(memories, mode);

    WorkingSetContext {
        query: query.map(|value| value.to_string()),
        overview,
        suggested_expand,
        files,
        active_symbols,
        nearby_symbols,
        tests,
        matched_rules,
        memories: compact_memories,
        memory_highlights,
        rationale,
        stats: if matches!(mode, BundleMode::Full) {
            Some(WorkingSetStats {
                file_count,
                active_symbol_count,
                nearby_symbol_count,
                test_count,
                memory_count: memories.len(),
            })
        } else {
            None
        },
    }
}

pub fn summarize_subsystem(
    graph: &CodeGraph,
    query: &str,
    files: &[String],
    symbols: &[String],
    memories: &[Value],
    rules: &[ProjectRule],
    mode: BundleMode,
) -> SubsystemSummary {
    let all_nodes = graph.all_nodes();
    let prefer_document_files = query_prefers_subsystem_documents(query);
    let anchor_files = normalize_subsystem_explicit_files(files, prefer_document_files);
    let candidate_files = subsystem_candidate_files(graph, prefer_document_files);
    let prefer_test_files = query_prefers_subsystem_tests(query, &anchor_files);
    let mut file_scores: HashMap<String, FileAccumulator> = HashMap::new();
    let mut symbol_scores: HashMap<(String, String), SymbolAccumulator> = HashMap::new();
    let mut seed_nodes: Vec<&GraphNode> = Vec::new();
    let mut seed_seen: HashSet<(String, String)> = HashSet::new();
    let mut rationale = Vec::new();
    let query_tokens: HashSet<String> = tokenize_path(query).into_iter().collect();

    for file in &anchor_files {
        if !is_queryable_graph_file(file) {
            continue;
        }
        add_file_score(
            &mut file_scores,
            file,
            8.0,
            "user supplied subsystem file".to_string(),
        );
    }

    for symbol in symbols {
        for node in find_symbol_matches(graph, symbol, &anchor_files)
            .into_iter()
            .filter(|node| prefer_document_files || !is_markdown_graph_file(&node.file))
            .take(4)
        {
            add_file_score(
                &mut file_scores,
                &node.file,
                6.0,
                format!("user supplied subsystem symbol {}", symbol),
            );
            add_symbol_score(
                &mut symbol_scores,
                &node.file,
                &node.name,
                Some(&node.id),
                node.kind.short_code().to_string(),
                node.line,
                "entry_symbol".to_string(),
                6.0,
            );
            push_seed_node(&mut seed_nodes, &mut seed_seen, node);
        }
    }

    for file in candidate_files
        .into_iter()
        .filter(|file| !is_test_file(file) && is_queryable_graph_file(file))
    {
        let mut delta = 0.0;
        let mut reasons = Vec::new();

        let path_overlap = overlap_count(&query_tokens, &tokenize_path(&file));
        if path_overlap > 0 {
            delta += path_overlap as f64;
            reasons.push(format!(
                "shares {} path token(s) with the subsystem query",
                path_overlap
            ));
        }

        let focus_overlap = overlap_count(&query_tokens, &focus_tokens_for_file(&file));
        if focus_overlap > 0 {
            delta += 2.0 * focus_overlap as f64;
            reasons.push(format!("matches {} core subsystem token(s)", focus_overlap));
        }

        let symbol_overlap = file_symbol_token_overlap(&all_nodes, &file, &query_tokens);
        if symbol_overlap > 0 {
            delta += 1.25 * symbol_overlap as f64;
            reasons.push(format!(
                "contains {} symbol token hit(s) related to the subsystem query",
                symbol_overlap
            ));
        }

        if delta > 0.0 {
            for reason in reasons {
                add_file_score(&mut file_scores, &file, delta, reason);
            }
        }
    }

    let mut ranked_files = finalize_file_recommendations(file_scores.clone());
    ranked_files = promote_explicit_files(ranked_files, &anchor_files);
    ranked_files = prioritize_entry_scope_files(ranked_files, &anchor_files);
    if ranked_files.is_empty() {
        ranked_files = fallback_repo_file_recommendations(
            graph,
            mode.working_file_limit(),
            prefer_document_files,
        );
    }
    calibrate_file_recommendations(&mut ranked_files);

    let seed_files: Vec<String> = ranked_files
        .iter()
        .take(mode.working_file_limit())
        .map(|item| item.file.clone())
        .collect();

    for file in &seed_files {
        for (index, node) in rank_file_focus_nodes(graph, &all_nodes, file)
            .into_iter()
            .take(3)
            .enumerate()
        {
            let delta = (4.0 - index as f64 * 0.75).max(1.5);
            add_symbol_score(
                &mut symbol_scores,
                &node.file,
                &node.name,
                Some(&node.id),
                node.kind.short_code().to_string(),
                node.line,
                "file_reference".to_string(),
                delta,
            );
            push_seed_node(&mut seed_nodes, &mut seed_seen, node);
        }
    }

    for node in &seed_nodes {
        for (dependency, edge) in rank_related_nodes(graph.get_dependencies(&node.id))
            .into_iter()
            .take(3)
        {
            if !prefer_test_files && is_test_file(&dependency.file) {
                continue;
            }
            if !prefer_document_files && is_markdown_graph_file(&dependency.file) {
                continue;
            }
            add_file_score(
                &mut file_scores,
                &dependency.file,
                1.2,
                format!("{} dependency via {}", node.name, short_edge(edge)),
            );
            add_symbol_score(
                &mut symbol_scores,
                &dependency.file,
                &dependency.name,
                Some(&dependency.id),
                dependency.kind.short_code().to_string(),
                dependency.line,
                format!("dependency:{}", short_edge(edge)),
                1.2,
            );
        }

        for (dependent, edge) in rank_related_nodes(graph.get_dependents(&node.id))
            .into_iter()
            .take(3)
        {
            if !prefer_test_files && is_test_file(&dependent.file) {
                continue;
            }
            if !prefer_document_files && is_markdown_graph_file(&dependent.file) {
                continue;
            }
            add_file_score(
                &mut file_scores,
                &dependent.file,
                1.0,
                format!("{} dependent via {}", dependent.name, short_edge(edge)),
            );
            add_symbol_score(
                &mut symbol_scores,
                &dependent.file,
                &dependent.name,
                Some(&dependent.id),
                dependent.kind.short_code().to_string(),
                dependent.line,
                format!("dependent:{}", short_edge(edge)),
                1.0,
            );
        }
    }

    let mut ranked_files = finalize_file_recommendations(file_scores);
    ranked_files = promote_explicit_files(ranked_files, &anchor_files);
    ranked_files = prioritize_entry_scope_files(ranked_files, &anchor_files);
    if ranked_files.is_empty() {
        ranked_files = fallback_repo_file_recommendations(
            graph,
            mode.working_file_limit(),
            prefer_document_files,
        );
    }
    calibrate_file_recommendations(&mut ranked_files);

    let mut ranked_symbols =
        prefer_symbols_in_files(finalize_symbol_recommendations(symbol_scores), &seed_files);
    if !prefer_test_files && ranked_symbols.iter().any(|item| !is_test_file(&item.file)) {
        ranked_symbols.retain(|item| !is_test_file(&item.file));
    }
    calibrate_symbol_recommendations(&mut ranked_symbols);

    let key_files = compress_file_summaries(&ranked_files, mode.working_file_limit());
    let key_file_names: Vec<String> = key_files.iter().map(|item| item.file.clone()).collect();
    let mut visible_ranked_symbols: Vec<SymbolRecommendation> = ranked_symbols
        .iter()
        .filter(|item| key_file_names.iter().any(|file| file == &item.file))
        .cloned()
        .collect();
    if visible_ranked_symbols.is_empty() {
        for file in &key_file_names {
            for (index, node) in rank_file_focus_nodes(graph, &all_nodes, file)
                .into_iter()
                .take(2)
                .enumerate()
            {
                visible_ranked_symbols.push(SymbolRecommendation {
                    symbol: node.name.clone(),
                    symbol_handle: Some(symbol_focus_for_node(node)),
                    kind: node.kind.short_code().to_string(),
                    file: node.file.clone(),
                    line: node.line,
                    role: "file_reference".to_string(),
                    score: (4.0 - index as f64).max(1.5),
                    confidence_band: "medium".to_string(),
                    evidence: vec!["direct_symbol".to_string()],
                });
            }
        }
        calibrate_symbol_recommendations(&mut visible_ranked_symbols);
    }
    let key_symbols = if visible_ranked_symbols.is_empty() {
        compress_symbol_summaries(&ranked_symbols, mode.symbol_limit().min(6))
    } else {
        compress_symbol_summaries(&visible_ranked_symbols, mode.symbol_limit().min(6))
    };
    let key_symbol_names: Vec<String> =
        key_symbols.iter().map(|item| item.symbol.clone()).collect();

    let test_report = find_relevant_tests(
        graph,
        &key_file_names,
        &key_symbol_names,
        None,
        rules,
        mode.test_limit(),
    );
    let memory_highlights = memory_highlights_from_values(
        &select_memories_for_focus(memories, query, 3),
        3,
        memory_highlight_text_limit(mode),
    );
    let matched_rules = select_relevant_rules(rules, &key_file_names, &test_report.tests, 4);

    if !key_files.is_empty() {
        rationale.push(format!(
            "Compressed the subsystem into {} key file(s) ranked from query overlap and graph proximity.",
            key_files.len()
        ));
    }
    if !key_symbols.is_empty() {
        rationale.push(format!(
            "Highlighted {} symbol anchor(s) from the top subsystem files and their nearest neighbors.",
            key_symbols.len()
        ));
    }
    if !test_report.tests.is_empty() {
        rationale
            .push("Included the most relevant tests so follow-up work can stay local.".to_string());
    }
    if !memory_highlights.is_empty() {
        rationale.push(format!(
            "Reused {} prior memory highlight(s) to avoid rediscovering subsystem context.",
            memory_highlights.len()
        ));
    }
    dedupe_strings(&mut rationale);

    let mut key_files = key_files;
    let mut key_symbols = key_symbols;
    let mut tests = test_report.tests;
    let mut matched_rules = matched_rules;
    let mut memory_highlights = memory_highlights;
    if matches!(mode, BundleMode::Compact) {
        compactify_test_recommendations(&mut tests);
        ultra_compactify_subsystem_summary(
            &mut key_files,
            &mut key_symbols,
            &mut tests,
            &mut matched_rules,
            &mut memory_highlights,
            &mut rationale,
        );
    }

    let overview = build_subsystem_overview(
        query,
        &key_files,
        &key_symbols,
        &tests,
        &matched_rules,
        &memory_highlights,
    );
    let suggested_expand = suggest_summary_expand(mode, &key_symbols, &key_files);
    let file_count = key_files.len();
    let symbol_count = key_symbols.len();
    let test_count = tests.len();
    let memory_count = memory_highlights.len();
    let approx_tokens = estimate_subsystem_summary_tokens(
        &overview,
        &key_files,
        &key_symbols,
        &tests,
        &matched_rules,
        &memory_highlights,
        &rationale,
    );

    SubsystemSummary {
        query: query.to_string(),
        overview,
        suggested_expand,
        key_files,
        key_symbols,
        tests,
        matched_rules,
        memories: memory_highlights,
        rationale,
        stats: if matches!(mode, BundleMode::Full) {
            Some(SubsystemSummaryStats {
                file_count,
                symbol_count,
                test_count,
                memory_count,
                approx_tokens,
            })
        } else {
            None
        },
    }
}

pub fn get_repo_playbook(
    graph: &CodeGraph,
    memories: &[Value],
    rules: &[ProjectRule],
    mode: BundleMode,
) -> RepoPlaybook {
    let all_nodes = graph.all_nodes();
    let mut file_scores: HashMap<String, FileAccumulator> = HashMap::new();
    let mut symbol_scores: HashMap<(String, String), SymbolAccumulator> = HashMap::new();

    for node in all_nodes
        .iter()
        .copied()
        .filter(|node| !is_test_file(&node.file) && is_queryable_graph_file(&node.file))
    {
        let centrality = graph.centrality(&node.id).min(4.0);
        let mut delta = centrality.max(0.5);
        let reason = if node.is_exported {
            delta += 2.0;
            format!("exported symbol {}", node.name)
        } else {
            format!("central symbol {}", node.name)
        };
        add_file_score(&mut file_scores, &node.file, delta, reason);
        add_symbol_score(
            &mut symbol_scores,
            &node.file,
            &node.name,
            Some(&node.id),
            node.kind.short_code().to_string(),
            node.line,
            if node.is_exported {
                "entry_symbol".to_string()
            } else {
                "context".to_string()
            },
            delta,
        );
    }

    let mut ranked_files = finalize_file_recommendations(file_scores);
    if ranked_files.is_empty() {
        ranked_files = fallback_repo_file_recommendations(graph, mode.working_file_limit(), true);
    }
    calibrate_file_recommendations(&mut ranked_files);
    let key_files = compress_file_summaries(&ranked_files, mode.working_file_limit());
    let key_file_names: Vec<String> = key_files.iter().map(|item| item.file.clone()).collect();

    let mut ranked_symbols = prefer_symbols_in_files(
        finalize_symbol_recommendations(symbol_scores),
        &key_file_names,
    );
    calibrate_symbol_recommendations(&mut ranked_symbols);
    let notable_symbols = compress_symbol_summaries(&ranked_symbols, mode.symbol_limit().min(6));

    let conventions: Vec<String> = rules
        .iter()
        .map(|rule| rule.description.clone())
        .take(mode.symbol_limit().min(6))
        .collect();
    let durable_patterns = memory_highlights_from_values(
        memories,
        mode.symbol_limit().min(5),
        memory_highlight_text_limit(mode),
    );
    let architecture = build_architecture_highlights(graph, &key_file_names);

    let mut rationale = vec![format!(
        "Built the playbook from {} indexed source file(s) and {} graph symbol(s).",
        unique_graph_files(graph)
            .into_iter()
            .filter(|file| is_queryable_graph_file(file))
            .count(),
        all_nodes
            .iter()
            .filter(|node| is_queryable_graph_file(&node.file))
            .count()
    )];
    if !conventions.is_empty() {
        rationale.push(format!(
            "Surfaced {} repository convention(s) from detected rules.",
            conventions.len()
        ));
    }
    if !durable_patterns.is_empty() {
        rationale.push(format!(
            "Included {} durable memory pattern(s) to keep repo knowledge persistent across sessions.",
            durable_patterns.len()
        ));
    }
    dedupe_strings(&mut rationale);

    let mut architecture = architecture;
    let mut conventions = conventions;
    let mut key_files = key_files;
    let mut notable_symbols = notable_symbols;
    let mut durable_patterns = durable_patterns;
    if matches!(mode, BundleMode::Compact) {
        ultra_compactify_repo_playbook(
            &mut architecture,
            &mut conventions,
            &mut key_files,
            &mut notable_symbols,
            &mut durable_patterns,
            &mut rationale,
        );
    }

    let overview = build_repo_playbook_overview(
        &architecture,
        &key_files,
        &notable_symbols,
        &conventions,
        &durable_patterns,
    );
    let suggested_expand = suggest_summary_expand(mode, &notable_symbols, &key_files);
    let file_count = key_files.len();
    let symbol_count = notable_symbols.len();
    let convention_count = conventions.len();
    let memory_count = durable_patterns.len();
    let approx_tokens = estimate_repo_playbook_tokens(
        &overview,
        &architecture,
        &conventions,
        &key_files,
        &notable_symbols,
        &durable_patterns,
        &rationale,
    );

    RepoPlaybook {
        overview,
        suggested_expand,
        architecture,
        conventions,
        key_files,
        notable_symbols,
        durable_patterns,
        rationale,
        stats: if matches!(mode, BundleMode::Full) {
            Some(RepoPlaybookStats {
                file_count,
                symbol_count,
                convention_count,
                memory_count,
                approx_tokens,
            })
        } else {
            None
        },
    }
}

pub fn expand_context(
    graph: &CodeGraph,
    seed: &ExpandContextSeed,
    focus: &str,
    max_tokens: usize,
) -> ExpandedContext {
    let requested_tokens = if max_tokens == 0 {
        DEFAULT_EXPAND_MAX_TOKENS
    } else {
        max_tokens
    };
    let max_tokens = requested_tokens.clamp(MIN_EXPAND_MAX_TOKENS, MAX_EXPAND_MAX_TOKENS);
    let all_nodes = graph.all_nodes();
    let target = resolve_expansion_target(seed, focus);
    let mut files = Vec::new();
    let mut symbols = Vec::new();
    let mut tests = Vec::new();
    let mut rationale = Vec::new();
    let mut remaining_chars = max_tokens * CHARS_PER_TOKEN_ESTIMATE;

    match &target {
        ExpansionTarget::SymbolId(symbol_id) => {
            let exact = find_exact_node_by_id(graph, symbol_id);
            let fallback =
                exact.or_else(|| find_exact_node(graph, &symbol_id.file, &symbol_id.name));
            if let Some(node) = fallback {
                symbols.push(build_expanded_symbol_context(
                    graph,
                    &all_nodes,
                    node,
                    &mut remaining_chars,
                ));
                files.push(build_expanded_file_context(
                    graph,
                    &all_nodes,
                    &node.file,
                    seed,
                    mode_file_symbol_limit(BundleMode::Compact),
                ));
                if exact.is_some() {
                    rationale.push(format!(
                        "Expanded exact symbol handle for '{}' in '{}'.",
                        node.name, node.file
                    ));
                } else {
                    rationale.push(format!(
                        "Symbol handle did not resolve exactly; fell back to best symbol match '{}' in '{}'.",
                        node.name, node.file
                    ));
                }
            }
        }
        ExpansionTarget::Symbol(symbol_name) => {
            if let Some(node) = find_symbol_matches(graph, symbol_name, &seed.files)
                .into_iter()
                .next()
            {
                symbols.push(build_expanded_symbol_context(
                    graph,
                    &all_nodes,
                    node,
                    &mut remaining_chars,
                ));
                files.push(build_expanded_file_context(
                    graph,
                    &all_nodes,
                    &node.file,
                    seed,
                    mode_file_symbol_limit(BundleMode::Compact),
                ));
                rationale.push(format!(
                    "Expanded the cached symbol '{}' with direct dependencies, dependents, and same-file context.",
                    node.name
                ));
            }
        }
        ExpansionTarget::File(file) => {
            files.push(build_expanded_file_context(
                graph,
                &all_nodes,
                file,
                seed,
                mode_file_symbol_limit(BundleMode::Full),
            ));

            for node in rank_file_focus_nodes(graph, &all_nodes, file)
                .into_iter()
                .take(2)
            {
                symbols.push(build_expanded_symbol_context(
                    graph,
                    &all_nodes,
                    node,
                    &mut remaining_chars,
                ));
            }

            if !files.is_empty() {
                rationale.push(format!(
                    "Expanded the cached file '{}' with its top symbols and nearby source bodies.",
                    file
                ));
            }
        }
        ExpansionTarget::Test(test_file) => {
            tests.push(build_expanded_test_context(
                graph, &all_nodes, test_file, seed,
            ));

            for file in related_source_files_for_test(test_file, seed)
                .into_iter()
                .take(2)
            {
                files.push(build_expanded_file_context(
                    graph,
                    &all_nodes,
                    &file,
                    seed,
                    mode_file_symbol_limit(BundleMode::Compact),
                ));
            }

            if !tests.is_empty() {
                rationale.push(format!(
                    "Expanded the cached test '{}' with related source files from the handle.",
                    test_file
                ));
            }
        }
        ExpansionTarget::Memory(index) => {
            if let Some(memory) = seed.memories.get(*index).cloned() {
                rationale.push(format!(
                    "Expanded cached memory {} directly without rebuilding the surrounding bundle.",
                    index
                ));
                return finalize_expanded_context(
                    seed,
                    focus,
                    "memory",
                    files,
                    symbols,
                    tests,
                    vec![memory],
                    rationale,
                );
            }
        }
    }

    let memories = select_memories_for_focus(&seed.memories, focus, 2);
    if !memories.is_empty() {
        rationale.push(format!(
            "Included {} matching memory item(s) from the cached handle.",
            memories.len()
        ));
    }

    if files.is_empty() && symbols.is_empty() && tests.is_empty() {
        rationale.push(
            "The requested focus did not map cleanly to cached symbols or files; use a symbol_id:, file_id:, file:, symbol:, test:, or memory: focus from the previous handle."
                .to_string(),
        );
    }

    finalize_expanded_context(
        seed,
        focus,
        target.kind(),
        files,
        symbols,
        tests,
        memories,
        rationale,
    )
}

pub fn diagnose_failure(
    graph: &CodeGraph,
    input: &str,
    kind: Option<&str>,
    rules: &[ProjectRule],
    mode: BundleMode,
) -> FailureDiagnosis {
    let all_nodes = graph.all_nodes();
    let failure_kind = kind
        .map(|value| value.to_string())
        .unwrap_or_else(|| detect_failure_kind(input).to_string());
    let file_refs = extract_failure_file_refs(graph, input);
    let mut extracted_files: Vec<String> = file_refs.iter().map(|(file, _)| file.clone()).collect();
    dedupe_strings(&mut extracted_files);
    let extracted_symbols = extract_failure_symbol_hints(&all_nodes, input, &extracted_files);

    let mut suspect_scores: HashMap<(String, String), SymbolAccumulator> = HashMap::new();
    let mut related_scores: HashMap<(String, String), SymbolAccumulator> = HashMap::new();
    let mut rationale = Vec::new();
    let mut likely_causes = Vec::new();

    for (file, line) in &file_refs {
        let file_nodes: Vec<&GraphNode> = all_nodes
            .iter()
            .copied()
            .filter(|node| node.file == *file && is_queryable_graph_file(&node.file))
            .collect();
        if file_nodes.is_empty() {
            continue;
        }

        if let Some(line) = line {
            let matching: Vec<&GraphNode> = file_nodes
                .iter()
                .copied()
                .filter(|node| {
                    line_ranges_overlap(
                        &LineRange {
                            start: *line,
                            end: *line,
                        },
                        node,
                    )
                })
                .collect();

            let mut line_matches = if matching.is_empty() {
                nearest_symbol_for_line(&file_nodes, *line)
                    .into_iter()
                    .collect::<Vec<_>>()
            } else {
                matching
            };

            rank_line_matches_by_specificity(&mut line_matches, *line);

            if let Some(best_match) = line_matches.first() {
                likely_causes.push(format!(
                    "Failure line {}:{} maps most specifically to {} at {}:{}.",
                    file,
                    line,
                    best_match.name,
                    best_match.file,
                    format_line_span(best_match),
                ));
            }

            for (index, node) in line_matches.into_iter().enumerate() {
                let delta = if index == 0 {
                    6.5
                } else {
                    (3.5 - index as f64 * 0.6).max(1.25)
                };
                add_symbol_score(
                    &mut suspect_scores,
                    &node.file,
                    &node.name,
                    Some(&node.id),
                    node.kind.short_code().to_string(),
                    node.line,
                    "line_reference".to_string(),
                    delta,
                );
            }
        } else {
            for node in rank_file_focus_nodes(graph, &all_nodes, file)
                .into_iter()
                .take(2)
            {
                add_symbol_score(
                    &mut suspect_scores,
                    &node.file,
                    &node.name,
                    Some(&node.id),
                    node.kind.short_code().to_string(),
                    node.line,
                    "file_reference".to_string(),
                    3.0,
                );
            }
        }
    }

    for symbol in &extracted_symbols {
        for node in find_symbol_matches(graph, symbol, &extracted_files)
            .into_iter()
            .take(3)
        {
            add_symbol_score(
                &mut suspect_scores,
                &node.file,
                &node.name,
                Some(&node.id),
                node.kind.short_code().to_string(),
                node.line,
                "symbol_hint".to_string(),
                4.0,
            );
        }
    }

    let input_tokens: HashSet<String> = tokenize_path(input).into_iter().collect();
    if !input_tokens.is_empty() {
        let mut token_matches: Vec<(&GraphNode, usize)> = all_nodes
            .iter()
            .copied()
            .filter_map(|node| {
                if !is_queryable_graph_file(&node.file) {
                    return None;
                }
                let name_tokens = tokenize_path(&node.name);
                let file_tokens = tokenize_path(&node.file);
                let hits = overlap_count(&input_tokens, &name_tokens)
                    + overlap_count(&input_tokens, &file_tokens);
                (hits > 0).then_some((node, hits))
            })
            .collect();

        token_matches.sort_by(|(node_a, hits_a), (node_b, hits_b)| {
            hits_b
                .cmp(hits_a)
                .then_with(|| {
                    graph
                        .centrality(&node_b.id)
                        .partial_cmp(&graph.centrality(&node_a.id))
                        .unwrap_or(Ordering::Equal)
                })
                .then_with(|| node_a.file.cmp(&node_b.file))
                .then_with(|| node_a.line.cmp(&node_b.line))
        });

        for (node, hits) in token_matches
            .into_iter()
            .take(mode.active_symbol_limit() + 2)
        {
            let delta = 0.75 + hits as f64 * 0.4;
            add_symbol_score(
                &mut suspect_scores,
                &node.file,
                &node.name,
                Some(&node.id),
                node.kind.short_code().to_string(),
                node.line,
                "token_overlap".to_string(),
                delta,
            );
        }
    }

    let mut suspects = prefer_symbols_in_files(
        finalize_symbol_recommendations(suspect_scores),
        &extracted_files,
    );
    calibrate_symbol_recommendations(&mut suspects);
    suspects.truncate(mode.active_symbol_limit());
    let suspect_keys: HashSet<(String, String)> = suspects
        .iter()
        .map(|item| (item.file.clone(), item.symbol.clone()))
        .collect();

    for suspect in &suspects {
        let exact_node = suspect
            .symbol_handle
            .as_deref()
            .and_then(SymbolId::from_stable_handle)
            .and_then(|symbol_id| find_exact_node_by_id(graph, &symbol_id))
            .or_else(|| find_exact_node(graph, &suspect.file, &suspect.symbol));
        if let Some(node) = exact_node {
            for (dependency, edge) in rank_related_nodes(graph.get_dependencies(&node.id))
                .into_iter()
                .take(3)
            {
                if !is_queryable_graph_file(&dependency.file) {
                    continue;
                }
                add_symbol_score(
                    &mut related_scores,
                    &dependency.file,
                    &dependency.name,
                    Some(&dependency.id),
                    dependency.kind.short_code().to_string(),
                    dependency.line,
                    format!("dependency:{}", short_edge(edge)),
                    1.0,
                );
            }

            for (dependent, edge) in rank_related_nodes(graph.get_dependents(&node.id))
                .into_iter()
                .take(3)
            {
                if !is_queryable_graph_file(&dependent.file) {
                    continue;
                }
                add_symbol_score(
                    &mut related_scores,
                    &dependent.file,
                    &dependent.name,
                    Some(&dependent.id),
                    dependent.kind.short_code().to_string(),
                    dependent.line,
                    format!("dependent:{}", short_edge(edge)),
                    1.0,
                );
            }

            if let Some(risk) = risk_for_node(graph, node, 2, "failure signal") {
                likely_causes.push(risk.reason);
            }
        }
    }

    let mut related_symbols: Vec<SymbolRecommendation> =
        finalize_symbol_recommendations(related_scores)
            .into_iter()
            .filter(|item| !suspect_keys.contains(&(item.file.clone(), item.symbol.clone())))
            .collect();
    calibrate_symbol_recommendations(&mut related_symbols);
    related_symbols.truncate(mode.nearby_symbol_limit());

    let mut related_files = extracted_files.clone();
    related_files.extend(suspects.iter().map(|item| item.file.clone()));
    dedupe_strings(&mut related_files);
    let suspect_names = focused_failure_test_symbols(&suspects);
    let mut test_report = find_relevant_tests(
        graph,
        &related_files,
        &suspect_names,
        None,
        rules,
        mode.test_limit(),
    );
    boost_direct_failure_tests(&mut test_report.tests, &extracted_files);
    calibrate_test_recommendations(&mut test_report.tests);

    if !file_refs.is_empty() {
        rationale.push(format!(
            "Mapped failure text to {} graph-indexed file reference(s).",
            extracted_files.len()
        ));
    }
    if !extracted_symbols.is_empty() {
        rationale.push(format!(
            "Used {} symbol hint(s) extracted directly from the failure text.",
            extracted_symbols.len()
        ));
    }
    if !related_symbols.is_empty() {
        rationale.push(
            "Expanded suspects through direct dependencies and dependents to surface likely collateral impact."
                .to_string(),
        );
    }
    if !test_report.tests.is_empty() {
        rationale.push(
            "Suggested tests based on direct failure anchors, suspect files, and symbol hints."
                .to_string(),
        );
    }

    if failure_kind == "compiler" {
        likely_causes.push(
            "Compiler-style diagnostics usually point close to the real edit site; start with the referenced file and line."
                .to_string(),
        );
    } else if failure_kind == "test" {
        likely_causes.push(
            "Failing tests often implicate either the asserted symbol or its immediate dependencies.".to_string(),
        );
    } else if failure_kind == "runtime" {
        likely_causes.push(
            "Runtime failures often originate in the top stack frame or the first exported symbol in the failing path."
                .to_string(),
        );
    }

    if suspects.is_empty() {
        likely_causes.push(
            "No exact graph symbol matched the failure text; inspect the referenced files manually or pass a more specific stack trace."
                .to_string(),
        );
    }

    dedupe_strings(&mut rationale);
    dedupe_strings(&mut likely_causes);

    let mut suspects = suspects;
    let mut related_symbols = related_symbols;
    let mut tests = test_report.tests;
    let mut extracted_files = extracted_files;
    let mut extracted_symbols = extracted_symbols;
    if matches!(mode, BundleMode::Compact) {
        compactify_symbol_recommendations(&mut suspects);
        compactify_symbol_recommendations(&mut related_symbols);
        compactify_test_recommendations(&mut tests);
        ultra_compactify_failure_diagnosis(
            &mut extracted_files,
            &mut extracted_symbols,
            &mut suspects,
            &mut related_symbols,
            &mut tests,
            &mut likely_causes,
            &mut rationale,
        );
    }

    let suspect_count = suspects.len();
    let related_symbol_count = related_symbols.len();
    let test_count = tests.len();
    let extracted_file_count = extracted_files.len();
    let extracted_symbol_count = extracted_symbols.len();
    let overview = build_failure_overview(&failure_kind, &suspects, &tests, &likely_causes, &[]);
    let suggested_expand = suggest_failure_expand(mode, &suspects, &extracted_files);
    let mut next_steps =
        build_failure_next_steps(&failure_kind, &suspects, &tests, &extracted_files);
    if matches!(mode, BundleMode::Compact) {
        ultra_compactify_next_steps(&mut next_steps);
    }

    FailureDiagnosis {
        kind: failure_kind,
        overview,
        suggested_expand,
        extracted_files,
        extracted_symbols,
        suspects,
        related_symbols,
        tests,
        memory_highlights: Vec::new(),
        likely_causes,
        next_steps,
        rationale,
        stats: if matches!(mode, BundleMode::Full) {
            Some(FailureDiagnosisStats {
                extracted_file_count,
                extracted_symbol_count,
                suspect_count,
                related_symbol_count,
                test_count,
            })
        } else {
            None
        },
    }
}

pub fn impact_from_diff(
    graph: &CodeGraph,
    diff: &str,
    extra_files: &[String],
    extra_symbols: &[String],
    rules: &[ProjectRule],
    mode: BundleMode,
    hops: usize,
) -> DiffImpactReport {
    let parsed_files = parse_unified_diff(diff);
    let mut changed_files = Vec::new();
    let mut changed_symbols = Vec::new();
    let mut changed_symbol_names = Vec::new();
    let mut seed_nodes: Vec<&GraphNode> = Vec::new();
    let mut changed_keys: HashSet<(String, String)> = HashSet::new();
    let mut seen_seeds: HashSet<(String, String)> = HashSet::new();
    let mut risks = Vec::new();

    for parsed in &parsed_files {
        let matched_symbols = extract_changed_symbols_for_file(graph, parsed);
        changed_files.push(ChangedFileImpact {
            file: parsed.file.clone(),
            status: parsed.status.clone(),
            added_lines: parsed.added_lines,
            removed_lines: parsed.removed_lines,
            hunk_count: parsed.hunk_count,
            changed_symbols: matched_symbols.len(),
            line_ranges: parsed.line_ranges.iter().map(format_line_range).collect(),
        });

        for (node, reasons) in matched_symbols {
            let key = (node.file.clone(), node.name.clone());
            if !changed_keys.insert(key) {
                continue;
            }

            let impact_count = downstream_impact_count(graph, node, hops);
            changed_symbol_names.push(node.name.clone());
            push_seed_node(&mut seed_nodes, &mut seen_seeds, node);
            if let Some(risk) =
                risk_for_node(graph, node, hops, &format!("{} symbol", parsed.status))
            {
                risks.push(risk);
            }

            changed_symbols.push(ChangedSymbolImpact {
                symbol: node.name.clone(),
                symbol_handle: Some(symbol_focus_for_node(node)),
                kind: node.kind.short_code().to_string(),
                file: node.file.clone(),
                line: node.line,
                end_line: node.end_line,
                change_kind: parsed.status.clone(),
                impact_count,
                reasons,
            });
        }
    }

    for file in extra_files {
        if let Some(node) = graph
            .all_nodes()
            .into_iter()
            .find(|node| node.file == *file && is_queryable_graph_file(&node.file))
        {
            push_seed_node(&mut seed_nodes, &mut seen_seeds, node);
        }
    }

    for symbol in extra_symbols {
        for node in find_symbol_matches(graph, symbol, extra_files)
            .into_iter()
            .take(3)
        {
            push_seed_node(&mut seed_nodes, &mut seen_seeds, node);
        }
    }

    dedupe_strings(&mut changed_symbol_names);

    let affected_symbols = collect_affected_symbols(
        graph,
        &seed_nodes,
        &changed_keys,
        hops,
        mode.affected_limit(),
    );

    let mut source_files: Vec<String> = parsed_files.iter().map(|item| item.file.clone()).collect();
    source_files.extend(extra_files.iter().cloned());
    dedupe_strings(&mut source_files);

    let test_report = find_relevant_tests(
        graph,
        &source_files,
        &changed_symbol_names,
        Some(diff),
        rules,
        mode.test_limit(),
    );

    risks.sort_by(|a, b| {
        severity_rank(&a.level)
            .cmp(&severity_rank(&b.level))
            .then_with(|| b.impact_count.cmp(&a.impact_count))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    risks.truncate(mode.symbol_limit());

    changed_symbols.sort_by(|a, b| {
        b.impact_count
            .cmp(&a.impact_count)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    changed_symbols.truncate(mode.symbol_limit());

    let review_checklist =
        build_review_checklist(&changed_files, &changed_symbols, &risks, &test_report);

    let mut rationale = Vec::new();
    if !changed_files.is_empty() {
        rationale.push(format!(
            "Parsed {} changed file(s) from unified diff hunks.",
            changed_files.len()
        ));
    }
    if !changed_symbols.is_empty() {
        rationale.push(format!(
            "Mapped changed lines onto {} graph symbol(s) using file + line overlap.",
            changed_symbols.len()
        ));
    } else {
        rationale.push(
            "No graph symbols overlapped the changed lines; fallback review should focus on changed files."
                .to_string(),
        );
    }
    if !affected_symbols.is_empty() {
        rationale.push(format!(
            "Expanded downstream impact {} hop(s) to surface likely regression areas.",
            hops
        ));
    }
    rationale.extend(test_report.rationale.iter().cloned());
    dedupe_strings(&mut rationale);

    let mut changed_files = changed_files;
    let mut changed_symbols = changed_symbols;
    let mut affected_symbols = affected_symbols;
    let mut tests = test_report.tests;
    let mut test_gaps = test_report.gaps;
    let mut matched_rules = test_report.matched_rules;
    let mut review_checklist = review_checklist;
    if matches!(mode, BundleMode::Compact) {
        compactify_changed_file_impacts(&mut changed_files);
        compactify_changed_symbol_impacts(&mut changed_symbols);
        compactify_affected_symbol_impacts(&mut affected_symbols);
        compactify_test_recommendations(&mut tests);
        ultra_compactify_diff_impact(
            &mut changed_files,
            &mut changed_symbols,
            &mut affected_symbols,
            &mut risks,
            &mut review_checklist,
            &mut tests,
            &mut test_gaps,
            &mut matched_rules,
            &mut rationale,
        );
    }

    let changed_file_count = changed_files.len();
    let changed_symbol_count = changed_symbols.len();
    let affected_symbol_count = affected_symbols.len();
    let risky_symbol_count = risks.len();
    let test_count = tests.len();
    let suggested_expand = suggest_diff_expand(mode, &changed_symbols, &changed_files);

    DiffImpactReport {
        suggested_expand,
        changed_files,
        changed_symbols,
        affected_symbols,
        risks,
        review_checklist,
        tests,
        test_gaps,
        matched_rules,
        rationale,
        stats: if matches!(mode, BundleMode::Full) {
            Some(DiffImpactStats {
                changed_file_count,
                changed_symbol_count,
                affected_symbol_count,
                risky_symbol_count,
                test_count,
            })
        } else {
            None
        },
    }
}

fn find_exact_node<'a>(graph: &'a CodeGraph, file: &str, symbol: &str) -> Option<&'a GraphNode> {
    graph.all_nodes().into_iter().find(|node| {
        is_queryable_graph_file(&node.file) && node.file == file && node.name == symbol
    })
}

fn find_exact_node_by_id<'a>(graph: &'a CodeGraph, symbol_id: &SymbolId) -> Option<&'a GraphNode> {
    graph
        .get_node(symbol_id)
        .filter(|node| is_queryable_graph_file(&node.file))
}

fn resolve_recommended_symbol_node<'a>(
    graph: &'a CodeGraph,
    recommendation: &SymbolRecommendation,
) -> Option<&'a GraphNode> {
    recommendation
        .symbol_handle
        .as_deref()
        .and_then(SymbolId::from_stable_handle)
        .and_then(|id| find_exact_node_by_id(graph, &id))
        .or_else(|| find_exact_node(graph, &recommendation.file, &recommendation.symbol))
}

fn symbol_focus_for_id(symbol_id: &SymbolId) -> String {
    symbol_id.stable_handle()
}

fn symbol_focus_for_node(node: &GraphNode) -> String {
    symbol_focus_for_id(&node.id)
}

fn symbol_focus_for_recommendation(item: &SymbolRecommendation) -> String {
    item.symbol_handle
        .clone()
        .unwrap_or_else(|| format!("symbol:{}", item.symbol))
}

fn file_focus(file: &str) -> String {
    stable_file_handle(file)
}

fn resolve_expansion_target(seed: &ExpandContextSeed, focus: &str) -> ExpansionTarget {
    let focus = focus.trim();

    if let Some(symbol_id) = SymbolId::from_stable_handle(focus) {
        return ExpansionTarget::SymbolId(symbol_id);
    }
    if let Some(file) = parse_stable_file_handle(focus) {
        return ExpansionTarget::File(file);
    }
    if let Some(rest) = focus.strip_prefix("file:") {
        return ExpansionTarget::File(rest.trim().to_string());
    }
    if let Some(rest) = focus.strip_prefix("symbol:") {
        let symbol = rest.trim();
        if let Some(symbol_id) = SymbolId::from_stable_handle(symbol) {
            return ExpansionTarget::SymbolId(symbol_id);
        }
        return ExpansionTarget::Symbol(symbol.to_string());
    }
    if let Some(rest) = focus.strip_prefix("test:") {
        return ExpansionTarget::Test(rest.trim().to_string());
    }
    if let Some(rest) = focus.strip_prefix("memory:") {
        if let Ok(index) = rest.trim().parse::<usize>() {
            return ExpansionTarget::Memory(index);
        }
    }

    if seed.tests.iter().any(|file| file == focus) || is_test_file(focus) {
        return ExpansionTarget::Test(focus.to_string());
    }
    if seed.files.iter().any(|file| file == focus) || focus.contains('/') {
        return ExpansionTarget::File(focus.to_string());
    }
    if let Some(handle) = seed
        .symbols
        .iter()
        .find(|symbol| symbol.as_str() == focus)
        .and_then(|symbol| SymbolId::from_stable_handle(symbol))
    {
        return ExpansionTarget::SymbolId(handle);
    }
    if seed.symbols.iter().any(|symbol| symbol == focus) {
        return ExpansionTarget::Symbol(focus.to_string());
    }

    if let Some(index) = best_matching_memory_index(&seed.memories, focus) {
        return ExpansionTarget::Memory(index);
    }

    ExpansionTarget::Symbol(focus.to_string())
}

fn build_expanded_symbol_context(
    graph: &CodeGraph,
    all_nodes: &[&GraphNode],
    node: &GraphNode,
    remaining_chars: &mut usize,
) -> ExpandedSymbolContext {
    let source = truncate_for_budget(&node.body, remaining_chars, 320);
    let dependencies = build_expanded_relationships(graph.get_dependencies(&node.id), "dependency");
    let dependents = build_expanded_relationships(graph.get_dependents(&node.id), "dependent");
    let same_file = rank_file_focus_nodes(graph, all_nodes, &node.file)
        .into_iter()
        .filter(|candidate| candidate.name != node.name)
        .take(4)
        .map(|candidate| ExpandedRelationshipContext {
            symbol: candidate.name.clone(),
            symbol_handle: Some(symbol_focus_for_node(candidate)),
            kind: candidate.kind.short_code().to_string(),
            file: candidate.file.clone(),
            line: candidate.line,
            relationship: "same_file".to_string(),
        })
        .collect();

    ExpandedSymbolContext {
        symbol: node.name.clone(),
        symbol_handle: Some(symbol_focus_for_node(node)),
        kind: node.kind.short_code().to_string(),
        file: node.file.clone(),
        line: node.line,
        end_line: node.end_line,
        signature: node.signature.clone(),
        source,
        dependencies,
        dependents,
        same_file,
    }
}

fn build_expanded_relationships(
    related: Vec<(&GraphNode, EdgeKind)>,
    default_relationship: &str,
) -> Vec<ExpandedRelationshipContext> {
    rank_related_nodes(related)
        .into_iter()
        .take(4)
        .map(|(node, edge)| ExpandedRelationshipContext {
            symbol: node.name.clone(),
            symbol_handle: Some(symbol_focus_for_node(node)),
            kind: node.kind.short_code().to_string(),
            file: node.file.clone(),
            line: node.line,
            relationship: match default_relationship {
                "dependency" => format!("dependency:{}", short_edge(edge)),
                "dependent" => format!("dependent:{}", short_edge(edge)),
                _ => default_relationship.to_string(),
            },
        })
        .collect()
}

fn build_expanded_file_context(
    graph: &CodeGraph,
    all_nodes: &[&GraphNode],
    file: &str,
    seed: &ExpandContextSeed,
    symbol_limit: usize,
) -> ExpandedFileContext {
    let symbols = rank_file_focus_nodes(graph, all_nodes, file)
        .into_iter()
        .take(symbol_limit)
        .map(|node| ExpandedFileSymbolContext {
            symbol: node.name.clone(),
            symbol_handle: Some(symbol_focus_for_node(node)),
            kind: node.kind.short_code().to_string(),
            line: node.line,
            signature: node.signature.to_string(),
            role: if seed_symbol_match(seed, node) {
                "handle_symbol".to_string()
            } else if seed_file_match(seed, file) {
                "handle_file".to_string()
            } else if node.is_exported {
                "exported".to_string()
            } else {
                "neighbor".to_string()
            },
        })
        .collect();

    ExpandedFileContext {
        file: file.to_string(),
        symbols,
        related_tests: related_tests_for_file(file, seed),
    }
}

fn build_expanded_test_context(
    graph: &CodeGraph,
    all_nodes: &[&GraphNode],
    test_file: &str,
    seed: &ExpandContextSeed,
) -> ExpandedTestContext {
    let symbols = rank_file_focus_nodes(graph, all_nodes, test_file)
        .into_iter()
        .take(5)
        .map(|node| ExpandedFileSymbolContext {
            symbol: node.name.clone(),
            symbol_handle: Some(symbol_focus_for_node(node)),
            kind: node.kind.short_code().to_string(),
            line: node.line,
            signature: node.signature.to_string(),
            role: "test_symbol".to_string(),
        })
        .collect();

    let mut related_files = related_source_files_for_test(test_file, seed);
    related_files.truncate(4);
    let mut reasons = Vec::new();
    for file in &related_files {
        if normalized_stem(file) == normalized_stem(test_file) {
            reasons.push(format!("matches source file stem {}", file));
        } else if shared_directory_prefix_len(file, test_file) > 0 {
            reasons.push(format!("shares directories with {}", file));
        }
    }
    dedupe_strings(&mut reasons);

    ExpandedTestContext {
        file: test_file.to_string(),
        symbols,
        related_files,
        reasons,
    }
}

fn seed_symbol_match(seed: &ExpandContextSeed, node: &GraphNode) -> bool {
    if seed.symbols.iter().any(|symbol| symbol == &node.name) {
        return true;
    }
    seed.symbols.iter().any(|symbol| {
        SymbolId::from_stable_handle(symbol)
            .map(|value| value == node.id)
            .unwrap_or(false)
    })
}

fn seed_file_match(seed: &ExpandContextSeed, file: &str) -> bool {
    if seed.files.iter().any(|handle_file| handle_file == file) {
        return true;
    }
    seed.files.iter().any(|handle_file| {
        parse_stable_file_handle(handle_file)
            .map(|value| value == file)
            .unwrap_or(false)
    })
}

fn related_tests_for_file(file: &str, seed: &ExpandContextSeed) -> Vec<String> {
    let mut tests = Vec::new();
    for test in &seed.tests {
        if normalized_stem(test) == normalized_stem(file)
            || shared_directory_prefix_len(file, test) > 0
        {
            tests.push(test.clone());
        }
    }
    dedupe_strings(&mut tests);
    tests.truncate(3);
    tests
}

fn related_source_files_for_test(test_file: &str, seed: &ExpandContextSeed) -> Vec<String> {
    let mut files = Vec::new();
    for file in &seed.files {
        if is_test_file(file) {
            continue;
        }
        if normalized_stem(file) == normalized_stem(test_file)
            || shared_directory_prefix_len(file, test_file) > 0
        {
            files.push(file.clone());
        }
    }
    dedupe_strings(&mut files);
    files
}

fn truncate_for_budget(source: &str, remaining_chars: &mut usize, max_tokens: usize) -> String {
    if *remaining_chars == 0 {
        return String::new();
    }

    let max_chars = (*remaining_chars).min(max_tokens * CHARS_PER_TOKEN_ESTIMATE);
    let mut output = String::new();
    for ch in source.chars().take(max_chars) {
        output.push(ch);
    }
    *remaining_chars = (*remaining_chars).saturating_sub(output.len());
    output
}

fn select_memories_for_focus(memories: &[Value], focus: &str, limit: usize) -> Vec<Value> {
    let mut matches: Vec<Value> = assistant_ordered_memory_values(memories)
        .into_iter()
        .filter(|memory| memory_matches_focus(memory, focus))
        .cloned()
        .collect();
    matches.truncate(limit);

    if matches.is_empty() {
        matches.extend(
            assistant_ordered_memory_values(memories)
                .into_iter()
                .take(limit.min(1))
                .cloned(),
        );
    }

    matches
}

fn memory_matches_focus(memory: &Value, focus: &str) -> bool {
    let focus_lower = focus.to_lowercase();
    memory.to_string().to_lowercase().contains(&focus_lower)
}

fn assistant_ordered_memory_values(values: &[Value]) -> Vec<&Value> {
    let mut ranked: Vec<&Value> = values.iter().collect();
    ranked.sort_by(|a, b| compare_memory_priority(a, b));
    ranked
}

fn best_matching_memory_index(memories: &[Value], focus: &str) -> Option<usize> {
    let mut matches: Vec<usize> = memories
        .iter()
        .enumerate()
        .filter_map(|(index, memory)| memory_matches_focus(memory, focus).then_some(index))
        .collect();

    matches.sort_by(|left, right| {
        compare_memory_priority(&memories[*left], &memories[*right]).then_with(|| left.cmp(right))
    });

    matches.into_iter().next()
}

fn compare_memory_priority(left: &Value, right: &Value) -> Ordering {
    memory_priority_score(right)
        .partial_cmp(&memory_priority_score(left))
        .unwrap_or(Ordering::Equal)
        .then_with(|| memory_created_at(right).cmp(&memory_created_at(left)))
        .then_with(|| memory_last_accessed(right).cmp(&memory_last_accessed(left)))
        .then_with(|| memory_access_count(right).cmp(&memory_access_count(left)))
        .then_with(|| memory_scope_rank(right).cmp(&memory_scope_rank(left)))
        .then_with(|| memory_assertion_rank(right).cmp(&memory_assertion_rank(left)))
        .then_with(|| memory_content(left).cmp(memory_content(right)))
}

fn memory_priority_score(value: &Value) -> f64 {
    let mut score = memory_status_rank(value) as f64 * 100.0;
    score += memory_scope_rank(value) as f64 * 10.0;
    score += memory_assertion_rank(value) as f64 * 2.5;
    score += memory_structured_evidence_score(value);
    if non_empty_string_field(value, "confidence_reason").is_some() {
        score += 0.8;
    }
    if non_empty_string_field(value, "supersedes_memory_id").is_some() {
        score += 0.6;
    }
    if !truncated_string_array_field(value, "contradicts_memory_ids", 1).is_empty() {
        score += 0.6;
    }
    score
}

fn memory_status_rank(value: &Value) -> usize {
    match memory_effective_verification_status(value).as_deref() {
        Some("verified") => 5,
        Some("in_review") => 4,
        Some("unverified") | None => 3,
        Some("stale") => 2,
        Some("superseded") => 1,
        Some("contradicted") => 0,
        Some(_) => 3,
    }
}

fn memory_scope_rank(value: &Value) -> usize {
    match value
        .get("scope")
        .and_then(|item| item.as_str())
        .unwrap_or("session")
    {
        "repo" => 2,
        "branch" => 1,
        _ => 0,
    }
}

fn memory_assertion_rank(value: &Value) -> usize {
    match value
        .get("assertion_type")
        .or_else(|| value.get("type"))
        .or_else(|| value.get("memory_type"))
        .and_then(|item| item.as_str())
        .unwrap_or("observation")
    {
        "workflow_outcome" => 6,
        "constraint" => 5,
        "pattern" => 4,
        "decision" => 4,
        "anti_pattern" => 3,
        "observation" => 2,
        "exploration" => 1,
        _ => 2,
    }
}

fn memory_structured_evidence_score(value: &Value) -> f64 {
    let provenance_count = value
        .get("provenance")
        .and_then(|item| item.as_array())
        .map(|items| items.len())
        .unwrap_or(0)
        .min(3);
    let evidence_count = value
        .get("evidence")
        .and_then(|item| item.as_array())
        .map(|items| items.len())
        .unwrap_or(0)
        .min(3);

    provenance_count as f64 * 0.7 + evidence_count as f64 * 0.9
}

fn memory_effective_verification_status(value: &Value) -> Option<String> {
    if !truncated_string_array_field(value, "contradicted_by_memory_ids", 1).is_empty() {
        return Some("contradicted".to_string());
    }
    if non_empty_string_field(value, "superseded_by_memory_id").is_some() {
        return Some("superseded".to_string());
    }
    if value
        .get("is_stale")
        .and_then(|item| item.as_bool())
        .unwrap_or(false)
    {
        return Some("stale".to_string());
    }

    non_empty_string_field(value, "verification_status")
}

fn memory_content(value: &Value) -> &str {
    value
        .get("content")
        .and_then(|item| item.as_str())
        .unwrap_or("")
}

fn memory_created_at(value: &Value) -> u64 {
    value
        .get("created_at")
        .and_then(|item| item.as_u64())
        .unwrap_or(0)
}

fn memory_last_accessed(value: &Value) -> u64 {
    value
        .get("last_accessed")
        .and_then(|item| item.as_u64())
        .unwrap_or(0)
}

fn memory_access_count(value: &Value) -> u64 {
    value
        .get("access_count")
        .and_then(|item| item.as_u64())
        .unwrap_or(0)
}

fn non_empty_string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| item.to_string())
}

fn truncated_string_array_field(value: &Value, key: &str, limit: usize) -> Vec<String> {
    value
        .get(key)
        .and_then(|item| item.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .take(limit.max(1))
        .map(|item| item.to_string())
        .collect()
}

fn compact_memory_object_array(
    value: &Value,
    key: &str,
    limit: usize,
    text_limit: usize,
) -> Vec<Value> {
    value
        .get(key)
        .and_then(|item| item.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_object())
        .take(limit.max(1))
        .map(|item| {
            let mut compact = serde_json::Map::new();
            for field in ["source", "kind", "reference", "note", "detail"] {
                if let Some(text) = item.get(field).and_then(|value| value.as_str()) {
                    compact.insert(
                        field.to_string(),
                        json!(truncate_text(text.trim(), text_limit)),
                    );
                }
            }
            if let Some(captured_at) = item.get("captured_at").and_then(|value| value.as_u64()) {
                compact.insert("captured_at".to_string(), json!(captured_at));
            }
            Value::Object(compact)
        })
        .collect()
}

fn finalize_expanded_context(
    seed: &ExpandContextSeed,
    focus: &str,
    focus_type: &str,
    files: Vec<ExpandedFileContext>,
    symbols: Vec<ExpandedSymbolContext>,
    tests: Vec<ExpandedTestContext>,
    memories: Vec<Value>,
    mut rationale: Vec<String>,
) -> ExpandedContext {
    dedupe_strings(&mut rationale);
    let approx_tokens =
        estimate_expanded_context_tokens(&files, &symbols, &tests, &memories, &rationale);
    let file_count = files.len();
    let symbol_count = symbols.len();
    let test_count = tests.len();
    let memory_count = memories.len();

    ExpandedContext {
        focus: focus.to_string(),
        focus_type: focus_type.to_string(),
        query: seed.query.clone(),
        files,
        symbols,
        tests,
        memories,
        rationale,
        stats: ExpandedContextStats {
            file_count,
            symbol_count,
            test_count,
            memory_count,
            approx_tokens,
        },
    }
}

fn estimate_expanded_context_tokens(
    files: &[ExpandedFileContext],
    symbols: &[ExpandedSymbolContext],
    tests: &[ExpandedTestContext],
    memories: &[Value],
    rationale: &[String],
) -> usize {
    let mut chars = 0usize;
    chars += rationale.iter().map(|item| item.len()).sum::<usize>();
    chars += memories
        .iter()
        .map(|item| item.to_string().len())
        .sum::<usize>();
    chars += files
        .iter()
        .map(|file| {
            file.file.len()
                + file
                    .related_tests
                    .iter()
                    .map(|test| test.len())
                    .sum::<usize>()
                + file
                    .symbols
                    .iter()
                    .map(|symbol| {
                        symbol.symbol.len()
                            + symbol.signature.len()
                            + symbol.role.len()
                            + symbol.kind.len()
                    })
                    .sum::<usize>()
        })
        .sum::<usize>();
    chars += symbols
        .iter()
        .map(|symbol| {
            symbol.symbol.len()
                + symbol.signature.len()
                + symbol.source.len()
                + symbol
                    .dependencies
                    .iter()
                    .chain(symbol.dependents.iter())
                    .chain(symbol.same_file.iter())
                    .map(|item| {
                        item.symbol.len()
                            + item.file.len()
                            + item.kind.len()
                            + item.relationship.len()
                    })
                    .sum::<usize>()
        })
        .sum::<usize>();
    chars += tests
        .iter()
        .map(|test| {
            test.file.len()
                + test
                    .related_files
                    .iter()
                    .map(|file| file.len())
                    .sum::<usize>()
                + test
                    .reasons
                    .iter()
                    .map(|reason| reason.len())
                    .sum::<usize>()
                + test
                    .symbols
                    .iter()
                    .map(|symbol| symbol.symbol.len() + symbol.signature.len() + symbol.role.len())
                    .sum::<usize>()
        })
        .sum::<usize>();

    chars / CHARS_PER_TOKEN_ESTIMATE
}

fn mode_file_symbol_limit(mode: BundleMode) -> usize {
    match mode {
        BundleMode::Compact => 6,
        BundleMode::Full => 8,
    }
}

fn detect_failure_kind(input: &str) -> &'static str {
    let lower = input.to_lowercase();
    if lower.contains("traceback")
        || lower.contains("panic")
        || lower.contains("exception")
        || lower.contains("segmentation fault")
    {
        "runtime"
    } else if lower.contains("fail")
        || lower.contains("assert")
        || lower.contains("expected")
        || lower.contains("snapshot")
    {
        "test"
    } else if lower.contains("error ts")
        || lower.contains("cannot find")
        || lower.contains("type error")
        || lower.contains("syntax error")
        || lower.contains("compile")
    {
        "compiler"
    } else {
        "runtime"
    }
}

fn extract_failure_file_refs(graph: &CodeGraph, input: &str) -> Vec<(String, Option<usize>)> {
    let mut refs = Vec::new();

    for file in unique_graph_files(graph) {
        if !input.contains(&file) {
            continue;
        }

        let mut saw_file = false;
        let mut saw_specific_line = false;
        for line in input.lines() {
            if let Some(index) = line.find(&file) {
                saw_file = true;
                let line_hint = parse_line_hint_after_path(&line[index + file.len()..]);
                if let Some(line_hint) = line_hint {
                    saw_specific_line = true;
                    if !refs.iter().any(|(existing_file, existing_line)| {
                        existing_file == &file && *existing_line == Some(line_hint)
                    }) {
                        refs.push((file.clone(), Some(line_hint)));
                    }
                }
            }
        }

        if saw_file && !saw_specific_line {
            refs.push((file, None));
        }
    }

    refs
}

fn parse_line_hint_after_path(suffix: &str) -> Option<usize> {
    let trimmed = suffix.trim_start();
    if let Some(rest) = trimmed.strip_prefix(':') {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            return digits.parse::<usize>().ok();
        }
    }

    let marker = "line ";
    let lower = trimmed.to_lowercase();
    if let Some(index) = lower.find(marker) {
        let digits: String = lower[index + marker.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            return digits.parse::<usize>().ok();
        }
    }

    None
}

fn extract_failure_symbol_hints(
    all_nodes: &[&GraphNode],
    input: &str,
    extracted_files: &[String],
) -> Vec<String> {
    let mut matches: Vec<&GraphNode> = all_nodes
        .iter()
        .copied()
        .filter(|node| {
            is_queryable_graph_file(&node.file)
                && node.name.len() >= 4
                && input_mentions_symbol(input, &node.name)
        })
        .collect();

    matches.sort_by(|a, b| {
        let a_preferred = extracted_files.iter().any(|file| file == &a.file);
        let b_preferred = extracted_files.iter().any(|file| file == &b.file);
        b_preferred
            .cmp(&a_preferred)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut symbols: Vec<String> = matches.into_iter().map(|node| node.name.clone()).collect();
    dedupe_strings(&mut symbols);
    symbols.truncate(6);
    symbols
}

fn rank_file_focus_nodes<'a>(
    graph: &CodeGraph,
    all_nodes: &[&'a GraphNode],
    file: &str,
) -> Vec<&'a GraphNode> {
    if !is_queryable_graph_file(file) {
        return Vec::new();
    }

    let mut nodes: Vec<&GraphNode> = all_nodes
        .iter()
        .copied()
        .filter(|node| node.file == file && is_queryable_graph_file(&node.file))
        .collect();

    nodes.sort_by(|a, b| {
        b.is_exported
            .cmp(&a.is_exported)
            .then_with(|| {
                graph
                    .centrality(&b.id)
                    .partial_cmp(&graph.centrality(&a.id))
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.name.cmp(&b.name))
    });

    nodes
}

fn find_symbol_matches<'a>(
    graph: &'a CodeGraph,
    symbol: &str,
    preferred_files: &[String],
) -> Vec<&'a GraphNode> {
    let mut matches: Vec<&GraphNode> = graph
        .all_nodes()
        .into_iter()
        .filter(|node| {
            is_queryable_graph_file(&node.file)
                && (node.name == symbol || node.name.ends_with(symbol))
        })
        .collect();

    if !preferred_files.is_empty() {
        let preferred_matches: Vec<&GraphNode> = matches
            .iter()
            .copied()
            .filter(|node| preferred_files.iter().any(|file| file == &node.file))
            .collect();
        if !preferred_matches.is_empty() {
            matches = preferred_matches;
        }
    }

    matches.sort_by(|a, b| {
        let a_preferred = preferred_files.iter().any(|file| file == &a.file);
        let b_preferred = preferred_files.iter().any(|file| file == &b.file);
        b_preferred
            .cmp(&a_preferred)
            .then_with(|| {
                graph
                    .centrality(&b.id)
                    .partial_cmp(&graph.centrality(&a.id))
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    matches
}

fn add_file_score(
    scores: &mut HashMap<String, FileAccumulator>,
    file: &str,
    delta: f64,
    reason: String,
) {
    let entry = scores.entry(file.to_string()).or_default();
    entry.score += delta;
    entry.reasons.push(reason);
}

fn add_symbol_score(
    scores: &mut HashMap<(String, String), SymbolAccumulator>,
    file: &str,
    symbol: &str,
    symbol_id: Option<&SymbolId>,
    kind: String,
    line: usize,
    role: String,
    delta: f64,
) {
    let entry = scores
        .entry((file.to_string(), symbol.to_string()))
        .or_default();
    if entry.kind.is_empty() {
        entry.kind = kind;
    }
    if entry.line == 0 {
        entry.line = line;
    }
    if entry.byte_offset.is_none() {
        entry.byte_offset = symbol_id.map(|item| item.byte_offset);
    }
    if entry.role.is_empty() || delta > entry.score {
        entry.role = role;
    }
    entry.score += delta;
}

fn finalize_file_recommendations(
    scores: HashMap<String, FileAccumulator>,
) -> Vec<FileRecommendation> {
    let mut items: Vec<FileRecommendation> = scores
        .into_iter()
        .map(|(file, mut acc)| {
            dedupe_strings(&mut acc.reasons);
            acc.reasons.truncate(3);
            let score = round_score(acc.score);
            let mut evidence = infer_file_evidence(&acc.reasons);
            dedupe_strings(&mut evidence);
            evidence.truncate(3);
            FileRecommendation {
                file,
                score,
                confidence_band: calibrate_file_confidence(score, &evidence),
                evidence,
                reasons: acc.reasons,
            }
        })
        .collect();
    items.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
    });
    calibrate_file_recommendations(&mut items);
    items
}

fn finalize_symbol_recommendations(
    scores: HashMap<(String, String), SymbolAccumulator>,
) -> Vec<SymbolRecommendation> {
    let mut items: Vec<SymbolRecommendation> = scores
        .into_iter()
        .map(|((file, symbol), acc)| {
            let score = round_score(acc.score);
            let mut evidence = infer_symbol_evidence(&acc.role);
            dedupe_strings(&mut evidence);
            evidence.truncate(3);
            let symbol_handle = acc.byte_offset.map(|byte_offset| {
                SymbolId {
                    file: file.clone(),
                    name: symbol.clone(),
                    byte_offset,
                }
                .stable_handle()
            });
            SymbolRecommendation {
                symbol,
                symbol_handle,
                kind: acc.kind,
                file,
                line: acc.line,
                role: acc.role,
                score,
                confidence_band: calibrate_symbol_confidence(score, &evidence),
                evidence,
            }
        })
        .collect();
    items.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    calibrate_symbol_recommendations(&mut items);
    items
}

fn compress_file_summaries(items: &[FileRecommendation], limit: usize) -> Vec<CompactFileSummary> {
    items
        .iter()
        .take(limit.max(1))
        .map(|item| CompactFileSummary {
            file: item.file.clone(),
            summary: truncate_text(&summarize_file_recommendation(item), 48),
            why: truncate_text(
                &item
                    .reasons
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "ranked as part of the current focus".to_string()),
                40,
            ),
            confidence_band: item.confidence_band.clone(),
        })
        .collect()
}

fn compress_symbol_summaries(
    items: &[SymbolRecommendation],
    limit: usize,
) -> Vec<CompactSymbolSummary> {
    items
        .iter()
        .take(limit.max(1))
        .map(|item| CompactSymbolSummary {
            symbol: item.symbol.clone(),
            symbol_handle: item.symbol_handle.clone(),
            file: item.file.clone(),
            line: item.line,
            role: item.role.clone(),
            summary: truncate_text(&summarize_symbol_recommendation(item), 52),
            confidence_band: item.confidence_band.clone(),
        })
        .collect()
}

fn summarize_file_recommendation(item: &FileRecommendation) -> String {
    let domain = summarize_domain(&item.file);
    let role = file_role_label(&item.file);
    if domain.is_empty() {
        role.to_string()
    } else {
        format!("{} for {}", role, domain)
    }
}

fn summarize_symbol_recommendation(item: &SymbolRecommendation) -> String {
    let role = symbol_role_label(&item.role);
    let kind = kind_label(&item.kind);
    let domain = summarize_domain(&item.file);
    if domain.is_empty() {
        format!("{} {} anchor", role, kind)
    } else {
        format!("{} {} anchor in {}", role, kind, domain)
    }
}

fn summarize_domain(file: &str) -> String {
    let tokens = focus_tokens_for_file(file);
    if !tokens.is_empty() {
        return tokens.into_iter().take(2).collect::<Vec<_>>().join(" ");
    }
    basename_without_extension(file)
}

fn basename_without_extension(file: &str) -> String {
    let basename = file.rsplit('/').next().unwrap_or(file);
    basename
        .split('.')
        .next()
        .unwrap_or(basename)
        .replace('_', " ")
}

fn file_role_label(file: &str) -> &'static str {
    if is_test_file(file) {
        "test coverage"
    } else if file.contains("/routers/") || file.contains("/routes/") {
        "request/entrypoint layer"
    } else if file.contains("/models/") {
        "data model layer"
    } else if file.contains("/services/") || file.contains("/service/") {
        "service layer"
    } else if file.contains("/core/") || file.contains("/lib/") {
        "shared core logic"
    } else if file.contains("/api/") {
        "API surface"
    } else if file.ends_with(".tsx") || file.ends_with(".jsx") {
        "UI layer"
    } else {
        "source module"
    }
}

fn symbol_role_label(role: &str) -> &'static str {
    match role {
        "line_reference" => "direct",
        "file_reference" => "file-local",
        "entry_symbol" => "primary",
        "pivot" => "pivot",
        "context" => "supporting",
        "test_symbol" => "test",
        role if role.starts_with("dependency:") => "dependency",
        role if role.starts_with("dependent:") => "downstream",
        _ => "supporting",
    }
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "fn" => "function",
        "meth" => "method",
        "cls" => "class",
        "var" => "variable",
        "const" => "constant",
        _ => "symbol",
    }
}

fn file_symbol_token_overlap(
    all_nodes: &[&GraphNode],
    file: &str,
    query_tokens: &HashSet<String>,
) -> usize {
    let symbol_tokens: HashSet<String> = all_nodes
        .iter()
        .copied()
        .filter(|node| node.file == file)
        .flat_map(|node| tokenize_path(&node.name))
        .collect();
    overlap_count_set(query_tokens, &symbol_tokens)
}

fn fallback_repo_file_recommendations(
    graph: &CodeGraph,
    limit: usize,
    prefer_document_files: bool,
) -> Vec<FileRecommendation> {
    let mut file_scores: HashMap<String, FileAccumulator> = HashMap::new();
    for node in graph.all_nodes().into_iter().filter(|node| {
        !is_test_file(&node.file)
            && is_queryable_graph_file(&node.file)
            && (prefer_document_files || !is_markdown_graph_file(&node.file))
    }) {
        let delta = graph.centrality(&node.id).max(0.5) + if node.is_exported { 1.5 } else { 0.0 };
        add_file_score(
            &mut file_scores,
            &node.file,
            delta,
            if node.is_exported {
                format!("exported symbol {}", node.name)
            } else {
                format!("central symbol {}", node.name)
            },
        );
    }
    let mut items = finalize_file_recommendations(file_scores);
    if items.is_empty() && !prefer_document_files {
        return fallback_repo_file_recommendations(graph, limit, true);
    }
    items.truncate(limit.max(1));
    items
}

fn memory_highlights_from_values(
    values: &[Value],
    limit: usize,
    text_limit: usize,
) -> Vec<MemoryHighlight> {
    assistant_ordered_memory_values(values)
        .into_iter()
        .filter_map(|value| memory_highlight_from_value(value, text_limit))
        .take(limit.max(1))
        .collect()
}

fn memory_highlight_from_value(value: &Value, text_limit: usize) -> Option<MemoryHighlight> {
    let content = value.get("content")?.as_str()?.trim();
    if content.is_empty() {
        return None;
    }

    let verification_status = memory_effective_verification_status(value);
    let confidence_reason =
        non_empty_string_field(value, "confidence_reason").map(|reason| truncate_text(&reason, 72));
    let freshness_policy =
        non_empty_string_field(value, "freshness_policy").map(|policy| policy.to_string());
    let freshness_policy_detail = non_empty_string_field(value, "freshness_policy_detail")
        .map(|detail| truncate_text(&detail, 72));

    Some(MemoryHighlight {
        content: truncate_text(content, text_limit),
        memory_type: value
            .get("type")
            .or_else(|| value.get("memory_type"))
            .and_then(|item| item.as_str())
            .unwrap_or("observation")
            .to_string(),
        scope: value
            .get("scope")
            .and_then(|item| item.as_str())
            .unwrap_or("session")
            .to_string(),
        is_stale: value
            .get("is_stale")
            .and_then(|item| item.as_bool())
            .unwrap_or(false),
        assertion_type: non_empty_string_field(value, "assertion_type"),
        verification_status,
        confidence_reason,
        freshness_policy,
        freshness_policy_detail,
    })
}

fn compact_memory_highlight_limit(mode: BundleMode) -> usize {
    match mode {
        BundleMode::Compact => 1,
        BundleMode::Full => 3,
    }
}

fn memory_highlight_text_limit(mode: BundleMode) -> usize {
    match mode {
        BundleMode::Compact => ULTRA_COMPACT_MEMORY_TEXT_LIMIT,
        BundleMode::Full => 120,
    }
}

fn compact_response_memories(values: &[Value], mode: BundleMode) -> Vec<Value> {
    match mode {
        BundleMode::Compact => Vec::new(),
        BundleMode::Full => compress_memory_values(values, 2),
    }
}

fn memory_reference_phrase(memory: &MemoryHighlight) -> String {
    let assertion = memory
        .assertion_type
        .as_deref()
        .unwrap_or(&memory.memory_type)
        .replace('_', " ");

    match memory.verification_status.as_deref() {
        Some("verified") => format!("verified {} {}", memory.scope, assertion),
        Some("in_review") => format!("in-review {} {}", memory.scope, assertion),
        Some("stale") => format!("stale {} {}", memory.scope, assertion),
        Some("superseded") => format!("superseded {} {}", memory.scope, assertion),
        Some("contradicted") => format!("contradicted {} {}", memory.scope, assertion),
        Some(status) if status != "unverified" => {
            format!(
                "{} {} {}",
                status.replace('_', " "),
                memory.scope,
                assertion
            )
        }
        _ => format!("prior {} {}", memory.scope, assertion),
    }
}

fn memory_overview_phrase(memory: &MemoryHighlight) -> String {
    match memory.verification_status.as_deref() {
        Some("stale") | Some("superseded") | Some("contradicted") => {
            format!("note {}", memory_reference_phrase(memory))
        }
        _ => format!("reuse {}", memory_reference_phrase(memory)),
    }
}

fn compactify_file_recommendations(items: &mut [FileRecommendation]) {
    for item in items {
        item.reasons.clear();
        item.evidence.clear();
    }
}

fn compactify_symbol_recommendations(items: &mut [SymbolRecommendation]) {
    for item in items {
        item.evidence.truncate(1);
    }
}

fn compactify_test_recommendations(items: &mut [TestRecommendation]) {
    for item in items {
        item.reasons.clear();
        item.evidence.clear();
    }
}

fn compactify_changed_symbol_impacts(items: &mut [ChangedSymbolImpact]) {
    for item in items {
        item.reasons.clear();
    }
}

fn compactify_changed_file_impacts(items: &mut [ChangedFileImpact]) {
    for item in items {
        item.line_ranges.clear();
    }
}

fn compactify_affected_symbol_impacts(items: &mut [AffectedSymbolImpact]) {
    for item in items {
        item.via.clear();
    }
}

fn compactify_plan_edit_impacts(items: &mut [PlanEditImpact]) {
    for item in items {
        item.via.clear();
    }
}

fn compactify_plan_edit_docs(items: &mut [PlanEditDocRecommendation]) {
    for item in items {
        item.reasons.clear();
        item.matched_files.clear();
        item.matched_symbols.clear();
        item.summary = truncate_text(&item.summary, 96);
    }
}

fn compactify_scenario_path_segments(items: &mut [ScenarioPathSegment]) {
    for item in items {
        item.rationale.clear();
    }
}

fn compactify_scenario_signals(items: &mut [ScenarioSignal]) {
    for item in items {
        item.summary = truncate_text(&item.summary, 96);
    }
}

fn ultra_compactify_task_bundle(
    primary_files: &mut Vec<FileRecommendation>,
    secondary_files: &mut Vec<FileRecommendation>,
    symbols: &mut Vec<SymbolRecommendation>,
    tests: &mut Vec<TestRecommendation>,
    risks: &mut Vec<RiskRecommendation>,
    test_gaps: &mut Vec<String>,
    matched_rules: &mut Vec<String>,
    rationale: &mut Vec<String>,
) {
    if top_file_is_high(primary_files) || top_symbol_is_high(symbols) {
        primary_files.truncate(ULTRA_COMPACT_PRIMARY_FILE_LIMIT);
        secondary_files.clear();
        symbols.truncate(ULTRA_COMPACT_SYMBOL_LIMIT);
        tests.truncate(ULTRA_COMPACT_TEST_LIMIT);
        risks.truncate(ULTRA_COMPACT_RISK_LIMIT);
        test_gaps.clear();
        matched_rules.clear();
        rationale.clear();
    } else {
        primary_files.truncate(ULTRA_COMPACT_PRIMARY_FILE_LIMIT + 1);
        secondary_files.truncate(ULTRA_COMPACT_SECONDARY_FILE_LIMIT);
        symbols.truncate(ULTRA_COMPACT_SYMBOL_LIMIT + 1);
        tests.truncate(ULTRA_COMPACT_TEST_LIMIT + 1);
        risks.truncate(ULTRA_COMPACT_RISK_LIMIT + 1);
        test_gaps.truncate(1);
        matched_rules.truncate(1);
        rationale.truncate(1);
    }
}

fn ultra_compactify_plan_edit(
    supporting_files: &mut Vec<FileRecommendation>,
    candidate_spans: &mut Vec<EditSpanRecommendation>,
    affected_callers: &mut Vec<PlanEditImpact>,
    affected_dependencies: &mut Vec<PlanEditImpact>,
    relevant_docs: &mut Vec<PlanEditDocRecommendation>,
    stale_doc_signals: &mut Vec<String>,
    rationale: &mut Vec<String>,
) {
    supporting_files.truncate(ULTRA_COMPACT_SECONDARY_FILE_LIMIT);
    candidate_spans.truncate(ULTRA_COMPACT_SYMBOL_LIMIT + 1);
    affected_callers.truncate(ULTRA_COMPACT_AFFECTED_SYMBOL_LIMIT + 1);
    affected_dependencies.truncate(ULTRA_COMPACT_AFFECTED_SYMBOL_LIMIT + 1);
    relevant_docs.truncate(ULTRA_COMPACT_PRIMARY_FILE_LIMIT);
    stale_doc_signals.truncate(ULTRA_COMPACT_PRIMARY_FILE_LIMIT);
    rationale.truncate(2);

    for span in candidate_spans.iter_mut() {
        span.reason = truncate_text(&span.reason, 52);
    }
    for doc in relevant_docs.iter_mut() {
        doc.summary = truncate_text(&doc.summary, 88);
    }
}

fn ultra_compactify_trace_scenario(
    plausible_entrypoints: &mut Vec<SymbolRecommendation>,
    execution_path: &mut Vec<ScenarioPathSegment>,
    plausible_paths: &mut Vec<ScenarioPathSegment>,
    guards: &mut Vec<ScenarioSignal>,
    side_effects: &mut Vec<ScenarioSignal>,
    failure_branches: &mut Vec<ScenarioSignal>,
    relevant_docs: &mut Vec<PlanEditDocRecommendation>,
    tests: &mut Vec<TestRecommendation>,
    test_gaps: &mut Vec<String>,
    matched_rules: &mut Vec<String>,
    rationale: &mut Vec<String>,
) {
    plausible_entrypoints.truncate(2);
    execution_path.truncate(ULTRA_COMPACT_SCENARIO_PATH_LIMIT);
    plausible_paths.truncate(ULTRA_COMPACT_SCENARIO_PATH_LIMIT.saturating_sub(1));
    guards.truncate(ULTRA_COMPACT_SCENARIO_SIGNAL_LIMIT);
    side_effects.truncate(ULTRA_COMPACT_SCENARIO_SIGNAL_LIMIT);
    failure_branches.truncate(ULTRA_COMPACT_SCENARIO_SIGNAL_LIMIT + 1);
    relevant_docs.truncate(ULTRA_COMPACT_PRIMARY_FILE_LIMIT);
    tests.truncate(ULTRA_COMPACT_TEST_LIMIT + 1);
    test_gaps.truncate(1);
    matched_rules.truncate(1);
    rationale.truncate(2);
}

fn ultra_compactify_working_set(
    files: &mut Vec<FileRecommendation>,
    active_symbols: &mut Vec<SymbolRecommendation>,
    nearby_symbols: &mut Vec<SymbolRecommendation>,
    tests: &mut Vec<TestRecommendation>,
    matched_rules: &mut Vec<String>,
    rationale: &mut Vec<String>,
) {
    files.truncate(ULTRA_COMPACT_PRIMARY_FILE_LIMIT);
    active_symbols.truncate(ULTRA_COMPACT_SYMBOL_LIMIT);
    tests.truncate(ULTRA_COMPACT_TEST_LIMIT);
    matched_rules.clear();
    rationale.clear();

    nearby_symbols.truncate(2);
}

fn ultra_compactify_failure_diagnosis(
    extracted_files: &mut Vec<String>,
    extracted_symbols: &mut Vec<String>,
    suspects: &mut Vec<SymbolRecommendation>,
    related_symbols: &mut Vec<SymbolRecommendation>,
    tests: &mut Vec<TestRecommendation>,
    likely_causes: &mut Vec<String>,
    rationale: &mut Vec<String>,
) {
    extracted_files.truncate(2);
    extracted_symbols.truncate(2);
    reprioritize_compact_failure_suspects(suspects);
    suspects.truncate(ULTRA_COMPACT_SYMBOL_LIMIT);
    tests.truncate(ULTRA_COMPACT_TEST_LIMIT);
    likely_causes.truncate(2);
    rationale.clear();
    related_symbols.truncate(1);
}

fn ultra_compactify_next_steps(steps: &mut Vec<String>) {
    for step in steps.iter_mut() {
        *step = truncate_text(step, 72);
    }
    steps.truncate(2);
}

fn ultra_compactify_diff_impact(
    changed_files: &mut Vec<ChangedFileImpact>,
    changed_symbols: &mut Vec<ChangedSymbolImpact>,
    affected_symbols: &mut Vec<AffectedSymbolImpact>,
    risks: &mut Vec<RiskRecommendation>,
    review_checklist: &mut Vec<ReviewChecklistItem>,
    tests: &mut Vec<TestRecommendation>,
    test_gaps: &mut Vec<String>,
    matched_rules: &mut Vec<String>,
    rationale: &mut Vec<String>,
) {
    changed_files.truncate(ULTRA_COMPACT_CHANGED_FILE_LIMIT);
    changed_symbols.truncate(ULTRA_COMPACT_CHANGED_SYMBOL_LIMIT);
    affected_symbols.truncate(ULTRA_COMPACT_AFFECTED_SYMBOL_LIMIT);
    risks.truncate(ULTRA_COMPACT_RISK_LIMIT);
    review_checklist.truncate(ULTRA_COMPACT_CHECKLIST_LIMIT);
    tests.truncate(ULTRA_COMPACT_TEST_LIMIT);
    test_gaps.clear();
    matched_rules.clear();
    rationale.truncate(1);
}

fn ultra_compactify_subsystem_summary(
    key_files: &mut Vec<CompactFileSummary>,
    key_symbols: &mut Vec<CompactSymbolSummary>,
    tests: &mut Vec<TestRecommendation>,
    matched_rules: &mut Vec<String>,
    memories: &mut Vec<MemoryHighlight>,
    rationale: &mut Vec<String>,
) {
    key_files.truncate(ULTRA_COMPACT_PRIMARY_FILE_LIMIT);
    key_symbols.truncate(ULTRA_COMPACT_SYMBOL_LIMIT);
    tests.truncate(ULTRA_COMPACT_TEST_LIMIT);
    matched_rules.clear();
    memories.truncate(1);
    rationale.clear();

    for item in key_files.iter_mut() {
        item.summary = truncate_text(&item.summary, 36);
        item.why = truncate_text(&item.why, 32);
    }
    for item in key_symbols.iter_mut() {
        item.summary = truncate_text(&item.summary, 40);
    }
}

fn ultra_compactify_repo_playbook(
    architecture: &mut Vec<String>,
    conventions: &mut Vec<String>,
    key_files: &mut Vec<CompactFileSummary>,
    notable_symbols: &mut Vec<CompactSymbolSummary>,
    durable_patterns: &mut Vec<MemoryHighlight>,
    rationale: &mut Vec<String>,
) {
    architecture.truncate(ULTRA_COMPACT_ARCHITECTURE_LIMIT);
    conventions.truncate(ULTRA_COMPACT_CONVENTION_LIMIT);
    key_files.truncate(ULTRA_COMPACT_PRIMARY_FILE_LIMIT);
    notable_symbols.truncate(ULTRA_COMPACT_SYMBOL_LIMIT);
    durable_patterns.truncate(1);
    rationale.clear();

    for item in key_files.iter_mut() {
        item.summary = truncate_text(&item.summary, 36);
        item.why = truncate_text(&item.why, 32);
    }
    for item in notable_symbols.iter_mut() {
        item.summary = truncate_text(&item.summary, 40);
    }
}

fn top_file_is_high(items: &[FileRecommendation]) -> bool {
    items
        .first()
        .map(|item| item.confidence_band == "high")
        .unwrap_or(false)
}

fn top_symbol_is_high(items: &[SymbolRecommendation]) -> bool {
    items
        .first()
        .map(|item| item.confidence_band == "high")
        .unwrap_or(false)
}

fn reprioritize_compact_failure_suspects(items: &mut Vec<SymbolRecommendation>) {
    if items.len() <= 2 {
        return;
    }

    let lead = items.remove(0);
    let lead_file = lead.file.clone();
    items.sort_by(|a, b| {
        (a.file != lead_file)
            .cmp(&(b.file != lead_file))
            .then_with(|| is_test_file(&a.file).cmp(&is_test_file(&b.file)))
            .then_with(|| (a.kind != "F" && a.kind != "M").cmp(&(b.kind != "F" && b.kind != "M")))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.file.cmp(&b.file))
    });
    items.insert(0, lead);
}

fn suggest_task_bundle_expand(
    mode: BundleMode,
    primary_files: &[FileRecommendation],
    secondary_files: &[FileRecommendation],
    symbols: &[SymbolRecommendation],
    tests: &[TestRecommendation],
) -> Option<ExpandSuggestion> {
    if !matches!(mode, BundleMode::Compact) {
        return None;
    }

    if let Some(symbol) = symbols.first() {
        return Some(ExpandSuggestion {
            focus: symbol_focus_for_recommendation(symbol),
            reason: "Inspect the top ranked symbol to see nearby code and relationships."
                .to_string(),
        });
    }
    if primary_files.len() > 1 || !secondary_files.is_empty() || !tests.is_empty() {
        return primary_files.first().map(|file| ExpandSuggestion {
            focus: file_focus(&file.file),
            reason: "Expand the top file to inspect its local symbols before widening further."
                .to_string(),
        });
    }

    None
}

fn suggest_working_set_expand(
    mode: BundleMode,
    files: &[FileRecommendation],
    active_symbols: &[SymbolRecommendation],
    nearby_symbols: &[SymbolRecommendation],
) -> Option<ExpandSuggestion> {
    if !matches!(mode, BundleMode::Compact) {
        return None;
    }

    if let Some(symbol) = active_symbols.first().or_else(|| nearby_symbols.first()) {
        return Some(ExpandSuggestion {
            focus: symbol_focus_for_recommendation(symbol),
            reason: "Expand the strongest working-set symbol to inspect surrounding implementation details."
                .to_string(),
        });
    }

    files.first().map(|file| ExpandSuggestion {
        focus: file_focus(&file.file),
        reason: "Expand the top working-set file to inspect its neighboring symbols.".to_string(),
    })
}

fn suggest_trace_scenario_expand(
    mode: BundleMode,
    likely_entrypoints: &[SymbolRecommendation],
    execution_path: &[ScenarioPathSegment],
) -> Option<ExpandSuggestion> {
    if !matches!(mode, BundleMode::Compact) {
        return None;
    }

    if let Some(entrypoint) = likely_entrypoints.first() {
        return Some(ExpandSuggestion {
            focus: symbol_focus_for_recommendation(entrypoint),
            reason: "Expand the lead scenario entrypoint to inspect its downstream behavior path."
                .to_string(),
        });
    }

    execution_path.first().map(|segment| ExpandSuggestion {
        focus: segment
            .from_symbol_handle
            .clone()
            .unwrap_or_else(|| file_focus(&segment.from_file)),
        reason: "Expand the first traced path segment to inspect branch and failure behavior."
            .to_string(),
    })
}

fn suggest_failure_expand(
    mode: BundleMode,
    suspects: &[SymbolRecommendation],
    extracted_files: &[String],
) -> Option<ExpandSuggestion> {
    if !matches!(mode, BundleMode::Compact) {
        return None;
    }

    if let Some(suspect) = suspects.first() {
        return Some(ExpandSuggestion {
            focus: symbol_focus_for_recommendation(suspect),
            reason:
                "Expand the top suspect symbol to inspect its body, dependencies, and dependents."
                    .to_string(),
        });
    }

    extracted_files.first().map(|file| ExpandSuggestion {
        focus: file_focus(file),
        reason: "Expand the directly referenced file to inspect the failing path in local context."
            .to_string(),
    })
}

fn suggest_diff_expand(
    mode: BundleMode,
    changed_symbols: &[ChangedSymbolImpact],
    changed_files: &[ChangedFileImpact],
) -> Option<ExpandSuggestion> {
    if !matches!(mode, BundleMode::Compact) {
        return None;
    }

    if let Some(symbol) = changed_symbols.first() {
        return Some(ExpandSuggestion {
            focus: symbol
                .symbol_handle
                .clone()
                .unwrap_or_else(|| format!("symbol:{}", symbol.symbol)),
            reason: "Expand the top changed symbol to inspect downstream impact in source context."
                .to_string(),
        });
    }

    changed_files.first().map(|file| ExpandSuggestion {
        focus: file_focus(&file.file),
        reason: "Expand the changed file to inspect the affected region with nearby symbols."
            .to_string(),
    })
}

fn suggest_summary_expand(
    mode: BundleMode,
    key_symbols: &[CompactSymbolSummary],
    key_files: &[CompactFileSummary],
) -> Option<ExpandSuggestion> {
    if !matches!(mode, BundleMode::Compact) {
        return None;
    }

    if let Some(symbol) = key_symbols.first() {
        return Some(ExpandSuggestion {
            focus: symbol
                .symbol_handle
                .clone()
                .unwrap_or_else(|| format!("symbol:{}", symbol.symbol)),
            reason: "Expand the lead subsystem symbol to inspect the implementation details behind the summary."
                .to_string(),
        });
    }

    key_files.first().map(|file| ExpandSuggestion {
        focus: file_focus(&file.file),
        reason: "Expand the lead file to inspect the concrete structure behind the summary."
            .to_string(),
    })
}

fn truncate_text(value: &str, limit: usize) -> String {
    let mut output = String::new();
    for ch in value.chars().take(limit) {
        output.push(ch);
    }
    if value.chars().count() > limit {
        output.push_str("...");
    }
    output
}

fn select_relevant_rules(
    rules: &[ProjectRule],
    files: &[String],
    tests: &[TestRecommendation],
    limit: usize,
) -> Vec<String> {
    let file_set: HashSet<&str> = files.iter().map(|item| item.as_str()).collect();
    let test_set: HashSet<&str> = tests.iter().map(|item| item.file.as_str()).collect();
    let mut matches = Vec::new();

    for rule in rules {
        let example_overlap = rule
            .example_files
            .iter()
            .any(|file| file_set.contains(file.as_str()) || test_set.contains(file.as_str()));
        if example_overlap || rule.description.contains("Test files follow pattern") {
            matches.push(rule.description.clone());
        }
    }

    dedupe_strings(&mut matches);
    matches.truncate(limit.max(1));
    matches
}

fn build_subsystem_overview(
    query: &str,
    key_files: &[CompactFileSummary],
    key_symbols: &[CompactSymbolSummary],
    tests: &[TestRecommendation],
    rules: &[String],
    memories: &[MemoryHighlight],
) -> String {
    let mut parts = vec![format!(
        "{}: {}",
        truncate_text(query, 42),
        summarize_item_list(
            &key_files
                .iter()
                .map(|item| basename_without_extension(&item.file))
                .collect::<Vec<_>>()
        )
    )];

    if let Some(symbol) = key_symbols.first() {
        parts.push(format!("start {}", symbol.symbol));
    }
    if let Some(test) = tests.first() {
        parts.push(format!("test {}", basename_without_extension(&test.file)));
    }
    if let Some(rule) = rules.first() {
        parts.push(format!("rule {}", truncate_text(&rule.to_lowercase(), 28)));
    }
    if let Some(memory) = memories.first() {
        parts.push(format!(
            "memory {}",
            truncate_text(&memory_reference_phrase(memory), 28)
        ));
    }

    parts.join(". ") + "."
}

fn build_repo_playbook_overview(
    architecture: &[String],
    key_files: &[CompactFileSummary],
    notable_symbols: &[CompactSymbolSummary],
    conventions: &[String],
    memories: &[MemoryHighlight],
) -> String {
    let mut parts = Vec::new();

    if let Some(first) = architecture.first() {
        parts.push(truncate_text(first, 42));
    }
    if !key_files.is_empty() {
        parts.push(format!(
            "start {}",
            summarize_item_list(
                &key_files
                    .iter()
                    .map(|item| basename_without_extension(&item.file))
                    .collect::<Vec<_>>()
            )
        ));
    }
    if !notable_symbols.is_empty() {
        parts.push(format!(
            "symbols {}",
            summarize_item_list(
                &notable_symbols
                    .iter()
                    .map(|item| item.symbol.clone())
                    .collect::<Vec<_>>()
            )
        ));
    }
    if let Some(rule) = conventions.first() {
        parts.push(format!("rule {}", truncate_text(rule, 32)));
    }
    if let Some(memory) = memories.first() {
        parts.push(format!(
            "pattern {}",
            truncate_text(&memory_reference_phrase(memory), 28)
        ));
    }

    parts.join(". ") + "."
}

fn build_architecture_highlights(graph: &CodeGraph, key_files: &[String]) -> Vec<String> {
    let mut files = unique_graph_files(graph);
    files.retain(|file| is_queryable_graph_file(file));

    let mut dir_counts: HashMap<String, usize> = HashMap::new();
    for file in &files {
        let top = file.split('/').next().unwrap_or(file).to_string();
        *dir_counts.entry(top).or_insert(0) += 1;
    }

    let mut top_dirs: Vec<(String, usize)> = dir_counts.into_iter().collect();
    top_dirs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut highlights = Vec::new();
    if !top_dirs.is_empty() {
        let line = top_dirs
            .iter()
            .take(3)
            .map(|(dir, count)| format!("{} ({} files)", dir, count))
            .collect::<Vec<_>>()
            .join(", ");
        highlights.push(format!("Primary layout: {}", line));
    }
    if !key_files.is_empty() {
        highlights.push(format!(
            "High-signal files: {}",
            summarize_item_list(
                &key_files
                    .iter()
                    .map(|file| basename_without_extension(file))
                    .collect::<Vec<_>>()
            )
        ));
    }

    highlights
}

fn estimate_subsystem_summary_tokens(
    overview: &str,
    key_files: &[CompactFileSummary],
    key_symbols: &[CompactSymbolSummary],
    tests: &[TestRecommendation],
    rules: &[String],
    memories: &[MemoryHighlight],
    rationale: &[String],
) -> usize {
    let chars = overview.len()
        + key_files
            .iter()
            .map(|item| item.file.len() + item.summary.len() + item.why.len())
            .sum::<usize>()
        + key_symbols
            .iter()
            .map(|item| item.symbol.len() + item.file.len() + item.summary.len() + item.role.len())
            .sum::<usize>()
        + tests
            .iter()
            .map(|item| {
                item.file.len()
                    + item
                        .reasons
                        .iter()
                        .map(|reason| reason.len())
                        .sum::<usize>()
            })
            .sum::<usize>()
        + rules.iter().map(|item| item.len()).sum::<usize>()
        + memories
            .iter()
            .map(estimate_memory_highlight_chars)
            .sum::<usize>()
        + rationale.iter().map(|item| item.len()).sum::<usize>();
    chars / CHARS_PER_TOKEN_ESTIMATE
}

fn estimate_repo_playbook_tokens(
    overview: &str,
    architecture: &[String],
    conventions: &[String],
    key_files: &[CompactFileSummary],
    notable_symbols: &[CompactSymbolSummary],
    memories: &[MemoryHighlight],
    rationale: &[String],
) -> usize {
    let chars = overview.len()
        + architecture.iter().map(|item| item.len()).sum::<usize>()
        + conventions.iter().map(|item| item.len()).sum::<usize>()
        + key_files
            .iter()
            .map(|item| item.file.len() + item.summary.len() + item.why.len())
            .sum::<usize>()
        + notable_symbols
            .iter()
            .map(|item| item.symbol.len() + item.file.len() + item.summary.len() + item.role.len())
            .sum::<usize>()
        + memories
            .iter()
            .map(estimate_memory_highlight_chars)
            .sum::<usize>()
        + rationale.iter().map(|item| item.len()).sum::<usize>();
    chars / CHARS_PER_TOKEN_ESTIMATE
}

fn estimate_memory_highlight_chars(item: &MemoryHighlight) -> usize {
    item.content.len()
        + item.memory_type.len()
        + item.scope.len()
        + item
            .assertion_type
            .as_ref()
            .map(|value| value.len())
            .unwrap_or(0)
        + item
            .verification_status
            .as_ref()
            .map(|value| value.len())
            .unwrap_or(0)
        + item
            .confidence_reason
            .as_ref()
            .map(|value| value.len())
            .unwrap_or(0)
        + item
            .freshness_policy
            .as_ref()
            .map(|value| value.len())
            .unwrap_or(0)
        + item
            .freshness_policy_detail
            .as_ref()
            .map(|value| value.len())
            .unwrap_or(0)
}

fn summarize_item_list(items: &[String]) -> String {
    if items.is_empty() {
        return "the indexed repo".to_string();
    }
    if items.len() == 1 {
        return items[0].clone();
    }
    if items.len() == 2 {
        return format!("{} and {}", items[0], items[1]);
    }
    format!("{}, {}, and {}", items[0], items[1], items[2])
}

fn compress_memory_values(values: &[Value], limit: usize) -> Vec<Value> {
    assistant_ordered_memory_values(values)
        .into_iter()
        .filter_map(|value| {
            let content = value.get("content")?.as_str()?;
            let mut compressed = json!({
                "id": value.get("id"),
                "content": truncate_text(content, 120),
                "type": value.get("type").or_else(|| value.get("memory_type")),
                "scope": value.get("scope"),
                "is_stale": value.get("is_stale").and_then(|item| item.as_bool()).unwrap_or(false),
                "linked_files": value.get("linked_files").cloned().unwrap_or_else(|| json!([])),
                "linked_symbols": value.get("linked_symbols").cloned().unwrap_or_else(|| json!([]))
            });

            let object = compressed.as_object_mut()?;
            if let Some(assertion_type) = non_empty_string_field(value, "assertion_type") {
                object.insert("assertion_type".to_string(), json!(assertion_type));
            }
            if let Some(verification_status) = memory_effective_verification_status(value) {
                object.insert(
                    "verification_status".to_string(),
                    json!(verification_status),
                );
            }
            if let Some(confidence_reason) = non_empty_string_field(value, "confidence_reason") {
                object.insert(
                    "confidence_reason".to_string(),
                    json!(truncate_text(&confidence_reason, 80)),
                );
            }
            if let Some(supersedes_memory_id) =
                non_empty_string_field(value, "supersedes_memory_id")
            {
                object.insert(
                    "supersedes_memory_id".to_string(),
                    json!(supersedes_memory_id),
                );
            }
            if let Some(superseded_by_memory_id) =
                non_empty_string_field(value, "superseded_by_memory_id")
            {
                object.insert(
                    "superseded_by_memory_id".to_string(),
                    json!(superseded_by_memory_id),
                );
            }

            let contradicts_memory_ids =
                truncated_string_array_field(value, "contradicts_memory_ids", 3);
            if !contradicts_memory_ids.is_empty() {
                object.insert(
                    "contradicts_memory_ids".to_string(),
                    json!(contradicts_memory_ids),
                );
            }
            let contradicted_by_memory_ids =
                truncated_string_array_field(value, "contradicted_by_memory_ids", 3);
            if !contradicted_by_memory_ids.is_empty() {
                object.insert(
                    "contradicted_by_memory_ids".to_string(),
                    json!(contradicted_by_memory_ids),
                );
            }
            if let Some(freshness_policy) = non_empty_string_field(value, "freshness_policy") {
                object.insert("freshness_policy".to_string(), json!(freshness_policy));
            }
            if let Some(freshness_policy_detail) =
                non_empty_string_field(value, "freshness_policy_detail")
            {
                object.insert(
                    "freshness_policy_detail".to_string(),
                    json!(truncate_text(&freshness_policy_detail, 80)),
                );
            }

            let provenance = compact_memory_object_array(value, "provenance", 2, 60);
            if !provenance.is_empty() {
                object.insert("provenance".to_string(), Value::Array(provenance));
            }
            let evidence = compact_memory_object_array(value, "evidence", 2, 60);
            if !evidence.is_empty() {
                object.insert("evidence".to_string(), Value::Array(evidence));
            }

            Some(compressed)
        })
        .take(limit.max(1))
        .collect()
}

fn build_plan_edit_overview(
    base_overview: &str,
    candidate_spans: &[EditSpanRecommendation],
    affected_callers: &[PlanEditImpact],
    relevant_docs: &[PlanEditDocRecommendation],
) -> String {
    let mut parts = vec![truncate_text(base_overview.trim(), 88)];

    if let Some(span) = candidate_spans.first() {
        parts.push(format!(
            "first span {}:{} ({})",
            basename_without_extension(&span.file),
            span.symbol,
            span.line_span
        ));
    }
    if let Some(caller) = affected_callers.first() {
        parts.push(format!(
            "caller watch {}",
            truncate_text(&caller.symbol, 28)
        ));
    }
    if let Some(doc) = relevant_docs.first() {
        parts.push(format!(
            "doc check {}",
            basename_without_extension(&doc.file)
        ));
    }

    parts.join(". ") + "."
}

fn build_trace_scenario_overview(
    scenario: &str,
    likely_entrypoints: &[SymbolRecommendation],
    execution_path: &[ScenarioPathSegment],
    failure_branches: &[ScenarioSignal],
    tests: &[TestRecommendation],
) -> String {
    let mut parts = vec![format!("Trace: {}", truncate_text(scenario, 52))];

    if let Some(entrypoint) = likely_entrypoints.first() {
        parts.push(format!("entry {}", entrypoint.symbol));
    }
    if let Some(segment) = execution_path.first() {
        parts.push(format!(
            "path {} -> {}",
            truncate_text(&segment.from_symbol, 20),
            truncate_text(&segment.to_symbol, 20)
        ));
    }
    if let Some(branch) = failure_branches.first() {
        parts.push(format!("failure {}", truncate_text(&branch.symbol, 24)));
    }
    if let Some(test) = tests.first() {
        parts.push(format!("test {}", basename_without_extension(&test.file)));
    }

    parts.join(". ") + "."
}

fn build_task_bundle_overview(
    _query: &str,
    primary_files: &[FileRecommendation],
    symbols: &[SymbolRecommendation],
    tests: &[TestRecommendation],
    risks: &[RiskRecommendation],
    memories: &[MemoryHighlight],
) -> String {
    let mut parts = vec![format!(
        "Likely edit: {}",
        summarize_item_list(
            &primary_files
                .iter()
                .map(|item| basename_without_extension(&item.file))
                .collect::<Vec<_>>()
        )
    )];

    if let Some(symbol) = symbols.first() {
        parts.push(format!("focus {}", symbol.symbol));
    }
    if let Some(test) = tests.first() {
        parts.push(format!("test {}", basename_without_extension(&test.file)));
    }
    if let Some(risk) = risks.first() {
        parts.push(format!("watch {}", truncate_text(&risk.symbol, 24)));
    }
    if let Some(memory) = memories.first() {
        parts.push(memory_overview_phrase(memory));
    }

    parts.join(". ") + "."
}

fn build_working_set_overview(
    query: Option<&str>,
    files: &[FileRecommendation],
    active_symbols: &[SymbolRecommendation],
    nearby_symbols: &[SymbolRecommendation],
    tests: &[TestRecommendation],
    memories: &[MemoryHighlight],
) -> String {
    let lead = query.unwrap_or("Working set");
    let mut parts = vec![format!(
        "{}: {}",
        truncate_text(lead, 34),
        summarize_item_list(
            &files
                .iter()
                .map(|item| basename_without_extension(&item.file))
                .collect::<Vec<_>>()
        )
    )];

    if let Some(symbol) = active_symbols.first().or_else(|| nearby_symbols.first()) {
        parts.push(format!("focus {}", symbol.symbol));
    }
    if let Some(test) = tests.first() {
        parts.push(format!("test {}", basename_without_extension(&test.file)));
    }
    if let Some(memory) = memories.first() {
        parts.push(memory_overview_phrase(memory));
    }

    parts.join(". ") + "."
}

fn build_failure_overview(
    kind: &str,
    suspects: &[SymbolRecommendation],
    tests: &[TestRecommendation],
    likely_causes: &[String],
    memories: &[MemoryHighlight],
) -> String {
    let mut parts = vec![format!("{} diagnosis", kind)];

    if let Some(suspect) = suspects.first() {
        parts.push(format!("suspect {}", suspect.symbol));
    }
    if let Some(test) = tests.first() {
        parts.push(format!("test {}", basename_without_extension(&test.file)));
    }
    if let Some(cause) = likely_causes.first() {
        parts.push(truncate_text(cause, 52));
    }
    if let Some(memory) = memories.first() {
        parts.push(memory_overview_phrase(memory));
    }

    parts.join(". ") + "."
}

fn build_failure_next_steps(
    kind: &str,
    suspects: &[SymbolRecommendation],
    tests: &[TestRecommendation],
    extracted_files: &[String],
) -> Vec<String> {
    let mut steps = Vec::new();

    if let Some(suspect) = suspects.first() {
        steps.push(format!(
            "Inspect {} at {}:{} first.",
            suspect.symbol, suspect.file, suspect.line
        ));
    } else if let Some(file) = extracted_files.first() {
        steps.push(format!(
            "Inspect {} first because it was referenced directly.",
            file
        ));
    }

    if let Some(test) = tests.first() {
        steps.push(format!("Re-run {} after the first edit.", test.file));
    }

    steps.push(match kind {
        "compiler" => "Keep the compiler error line as the primary anchor before widening context."
            .to_string(),
        "test" => "Use the failing assertion path before exploring broader neighbors.".to_string(),
        _ => "Use the top stack frame before widening to broader graph neighbors.".to_string(),
    });

    steps.truncate(3);
    steps
}

fn push_seed_node<'a>(
    nodes: &mut Vec<&'a GraphNode>,
    seen: &mut HashSet<(String, String)>,
    node: &'a GraphNode,
) {
    let key = (node.file.clone(), node.name.clone());
    if seen.insert(key) {
        nodes.push(node);
    }
}

fn rank_related_nodes(nodes: Vec<(&GraphNode, EdgeKind)>) -> Vec<(&GraphNode, EdgeKind)> {
    let mut items: Vec<(&GraphNode, EdgeKind)> = nodes
        .into_iter()
        .filter(|(node, _)| is_queryable_graph_file(&node.file))
        .collect();
    items.sort_by(|(node_a, _), (node_b, _)| {
        node_a
            .file
            .cmp(&node_b.file)
            .then_with(|| node_a.line.cmp(&node_b.line))
    });
    items
}

fn collect_affected_symbols(
    graph: &CodeGraph,
    seed_nodes: &[&GraphNode],
    changed_keys: &HashSet<(String, String)>,
    hops: usize,
    limit: usize,
) -> Vec<AffectedSymbolImpact> {
    let mut scores: HashMap<(String, String), AffectedAccumulator> = HashMap::new();

    for node in seed_nodes {
        for dependent in graph.get_transitive_dependents(&node.id, hops) {
            if !is_queryable_graph_file(&dependent.file) {
                continue;
            }
            let key = (dependent.file.clone(), dependent.name.clone());
            if changed_keys.contains(&key) {
                continue;
            }

            let entry = scores.entry(key).or_default();
            if entry.kind.is_empty() {
                entry.kind = dependent.kind.short_code().to_string();
            }
            if entry.line == 0 {
                entry.line = dependent.line;
            }
            if entry.byte_offset.is_none() {
                entry.byte_offset = Some(dependent.id.byte_offset);
            }
            entry.score += 1.0;
            entry.via.push(node.name.clone());
        }
    }

    let mut items: Vec<AffectedSymbolImpact> = scores
        .into_iter()
        .map(|((file, symbol), mut acc)| {
            dedupe_strings(&mut acc.via);
            acc.via.truncate(3);
            let symbol_handle = acc.byte_offset.map(|byte_offset| {
                SymbolId {
                    file: file.clone(),
                    name: symbol.clone(),
                    byte_offset,
                }
                .stable_handle()
            });
            AffectedSymbolImpact {
                symbol,
                symbol_handle,
                kind: acc.kind,
                file,
                line: acc.line,
                via: acc.via,
                score: round_score(acc.score),
            }
        })
        .collect();

    items.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    items.truncate(limit);
    items
}

fn build_review_checklist(
    changed_files: &[ChangedFileImpact],
    changed_symbols: &[ChangedSymbolImpact],
    risks: &[RiskRecommendation],
    test_report: &TestSelectionReport,
) -> Vec<ReviewChecklistItem> {
    let mut items = Vec::new();

    if changed_symbols.is_empty() {
        items.push(ReviewChecklistItem {
            level: "high".to_string(),
            message: "Review the changed files manually because no graph symbols mapped cleanly to the diff hunks.".to_string(),
        });
    }

    if changed_files
        .iter()
        .any(|file| file.status == "deleted" || file.status == "renamed")
    {
        items.push(ReviewChecklistItem {
            level: "high".to_string(),
            message: "Verify deleted or renamed files have all imports, references, and generated artifacts cleaned up.".to_string(),
        });
    }

    if changed_symbols.iter().any(|symbol| symbol.impact_count > 0) {
        items.push(ReviewChecklistItem {
            level: "medium".to_string(),
            message: "Inspect direct callers and downstream dependents for behavioral regressions."
                .to_string(),
        });
    }

    if risks
        .iter()
        .any(|risk| risk.level == "high" || risk.level == "medium")
    {
        items.push(ReviewChecklistItem {
            level: "medium".to_string(),
            message: "Review public API or high-fanout symbols before merging.".to_string(),
        });
    }

    if !test_report.tests.is_empty() {
        items.push(ReviewChecklistItem {
            level: "low".to_string(),
            message: format!(
                "Run or update the top {} suggested test target(s).",
                test_report.tests.len()
            ),
        });
    }

    if !test_report.gaps.is_empty() {
        items.push(ReviewChecklistItem {
            level: "medium".to_string(),
            message: "Add or refresh regression coverage because test selection confidence is incomplete.".to_string(),
        });
    }

    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(item.message.clone()));
    items
}

fn extract_changed_symbols_for_file<'a>(
    graph: &'a CodeGraph,
    parsed: &ParsedDiffFile,
) -> Vec<(&'a GraphNode, Vec<String>)> {
    let file_nodes: Vec<&GraphNode> = graph
        .all_nodes()
        .into_iter()
        .filter(|node| node.file == parsed.file && is_queryable_graph_file(&node.file))
        .collect();

    if file_nodes.is_empty() {
        return Vec::new();
    }

    if parsed.status == "deleted" {
        return file_nodes
            .into_iter()
            .map(|node| (node, vec!["file deleted in diff".to_string()]))
            .collect();
    }

    let mut matches: Vec<(&GraphNode, Vec<String>)> = Vec::new();
    let mut seen = HashSet::new();

    for range in &parsed.line_ranges {
        let overlapping: Vec<&GraphNode> = file_nodes
            .iter()
            .copied()
            .filter(|node| line_ranges_overlap(range, node))
            .collect();

        if overlapping.is_empty() {
            if let Some(node) = nearest_symbol_for_line(&file_nodes, range.start) {
                let key = (node.file.clone(), node.name.clone());
                let reason = format!(
                    "nearest symbol to changed lines {}",
                    format_line_range(range)
                );
                if seen.insert(key) {
                    matches.push((node, vec![reason]));
                } else if let Some((_, reasons)) = matches.iter_mut().find(|(candidate, _)| {
                    candidate.file == node.file && candidate.name == node.name
                }) {
                    reasons.push(reason);
                }
            }
            continue;
        }

        for node in overlapping {
            let key = (node.file.clone(), node.name.clone());
            let reason = format!("overlaps changed lines {}", format_line_range(range));
            if seen.insert(key) {
                matches.push((node, vec![reason]));
            } else if let Some((_, reasons)) = matches
                .iter_mut()
                .find(|(candidate, _)| candidate.file == node.file && candidate.name == node.name)
            {
                reasons.push(reason);
            }
        }
    }

    if matches.is_empty() && file_nodes.len() == 1 {
        matches.push((
            file_nodes[0],
            vec!["single-symbol file changed in diff".to_string()],
        ));
    }

    for (_, reasons) in &mut matches {
        dedupe_strings(reasons);
        reasons.truncate(3);
    }

    matches
}

fn nearest_symbol_for_line<'a>(nodes: &[&'a GraphNode], line: usize) -> Option<&'a GraphNode> {
    nodes.iter().copied().min_by_key(|node| {
        if line < node.line {
            node.line - line
        } else if line > node.end_line {
            line - node.end_line
        } else {
            0
        }
    })
}

fn line_ranges_overlap(range: &LineRange, node: &GraphNode) -> bool {
    let node_start = node.line;
    let node_end = node.end_line.max(node.line);
    range.start <= node_end && node_start <= range.end
}

fn downstream_impact_count(graph: &CodeGraph, node: &GraphNode, hops: usize) -> usize {
    graph
        .get_transitive_dependents(&node.id, hops)
        .len()
        .max(graph.get_dependents(&node.id).len())
}

fn risk_for_node(
    graph: &CodeGraph,
    node: &GraphNode,
    hops: usize,
    change_context: &str,
) -> Option<RiskRecommendation> {
    let impact_count = downstream_impact_count(graph, node, hops);
    if impact_count == 0 && !node.is_exported {
        return None;
    }

    if impact_count >= 3 || (node.is_exported && impact_count > 0) {
        let level = if impact_count >= 6 {
            "high"
        } else if impact_count >= 3 {
            "medium"
        } else {
            "low"
        };
        let reason = if node.is_exported && impact_count > 0 {
            format!(
                "{} touches exported symbol {} with {} downstream dependents",
                change_context, node.name, impact_count
            )
        } else if node.is_exported {
            format!(
                "{} touches exported symbol {} and may affect public callers",
                change_context, node.name
            )
        } else {
            format!(
                "{} touches {} which fans out to {} downstream symbol(s)",
                change_context, node.name, impact_count
            )
        };

        return Some(RiskRecommendation {
            level: level.to_string(),
            symbol: node.name.clone(),
            file: node.file.clone(),
            reason,
            impact_count,
        });
    }

    None
}

fn parse_unified_diff(diff: &str) -> Vec<ParsedDiffFile> {
    let mut files = Vec::new();
    let mut current: Option<ParsedDiffFile> = None;

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(file) = finalize_parsed_diff_file(current.take()) {
                files.push(file);
            }

            let mut parsed = ParsedDiffFile::default();
            let mut parts = rest.split_whitespace();
            parsed.old_path = parts.next().map(normalize_diff_path);
            parsed.new_path = parts.next().map(normalize_diff_path);
            current = Some(parsed);
            continue;
        }

        let file = current.get_or_insert_with(ParsedDiffFile::default);

        if let Some(path) = line.strip_prefix("--- ") {
            file.old_path = Some(normalize_diff_path(path));
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ ") {
            file.new_path = Some(normalize_diff_path(path));
            continue;
        }
        if line.starts_with("new file mode ") {
            file.status = "added".to_string();
            continue;
        }
        if line.starts_with("deleted file mode ") {
            file.status = "deleted".to_string();
            continue;
        }
        if let Some(path) = line.strip_prefix("rename from ") {
            file.status = "renamed".to_string();
            file.old_path = Some(normalize_diff_path(path));
            continue;
        }
        if let Some(path) = line.strip_prefix("rename to ") {
            file.status = "renamed".to_string();
            file.new_path = Some(normalize_diff_path(path));
            continue;
        }
        if line.starts_with("@@") {
            if let Some(range) = parse_hunk_range(line) {
                file.line_ranges.push(range);
                file.hunk_count += 1;
            }
            continue;
        }
        if line.starts_with('+') && !line.starts_with("+++") {
            file.added_lines += 1;
            continue;
        }
        if line.starts_with('-') && !line.starts_with("---") {
            file.removed_lines += 1;
        }
    }

    if let Some(file) = finalize_parsed_diff_file(current.take()) {
        files.push(file);
    }

    files
}

fn finalize_parsed_diff_file(file: Option<ParsedDiffFile>) -> Option<ParsedDiffFile> {
    let mut file = file?;
    let old_path = file.old_path.clone().unwrap_or_default();
    let new_path = file.new_path.clone().unwrap_or_default();

    if file.status.is_empty() {
        if old_path == "/dev/null" && !new_path.is_empty() {
            file.status = "added".to_string();
        } else if new_path == "/dev/null" && !old_path.is_empty() {
            file.status = "deleted".to_string();
        } else if !old_path.is_empty() && !new_path.is_empty() && old_path != new_path {
            file.status = "renamed".to_string();
        } else {
            file.status = "modified".to_string();
        }
    }

    file.file = if new_path != "/dev/null" && !new_path.is_empty() {
        new_path
    } else {
        old_path
    };

    if file.file.is_empty() {
        return None;
    }

    Some(file)
}

fn parse_hunk_range(header: &str) -> Option<LineRange> {
    let new_part = header
        .split_whitespace()
        .find(|segment| segment.starts_with('+'))?
        .trim_start_matches('+');
    let mut pieces = new_part.split(',');
    let start = pieces.next()?.parse::<usize>().ok()?;
    let count = pieces
        .next()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1);
    let end = if count == 0 { start } else { start + count - 1 };
    Some(LineRange { start, end })
}

fn normalize_diff_path(path: &str) -> String {
    let trimmed = path.trim().trim_matches('"');
    if trimmed == "/dev/null" {
        return trimmed.to_string();
    }
    trimmed
        .strip_prefix("a/")
        .or_else(|| trimmed.strip_prefix("b/"))
        .unwrap_or(trimmed)
        .to_string()
}

fn format_line_range(range: &LineRange) -> String {
    if range.start == range.end {
        format!("{}", range.start)
    } else {
        format!("{}-{}", range.start, range.end)
    }
}

fn short_edge(edge: EdgeKind) -> &'static str {
    match edge {
        EdgeKind::Calls => "calls",
        EdgeKind::Imports => "imports",
        EdgeKind::Implements => "implements",
        EdgeKind::Extends => "extends",
        EdgeKind::TypeRef => "type_ref",
        EdgeKind::Contains => "contains",
        EdgeKind::LinksTo => "links_to",
        EdgeKind::Mentions => "mentions",
        EdgeKind::CoChanges => "co_changes",
    }
}

fn unique_graph_files(graph: &CodeGraph) -> Vec<String> {
    let mut files: Vec<String> = graph
        .all_nodes()
        .into_iter()
        .filter(|node| is_queryable_graph_file(&node.file))
        .map(|node| node.file.clone())
        .collect();
    dedupe_strings(&mut files);
    files
}

fn is_queryable_graph_file(file: &str) -> bool {
    !is_assistant_artifact_path(file)
}

fn is_markdown_graph_file(file: &str) -> bool {
    file.to_ascii_lowercase().ends_with(".md")
}

fn query_prefers_subsystem_documents(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    SUBSYSTEM_DOCUMENT_QUERY_KEYWORDS
        .iter()
        .any(|keyword| lower.contains(keyword))
}

fn query_prefers_subsystem_tests(query: &str, files: &[String]) -> bool {
    if files.iter().any(|file| is_test_file(file)) {
        return true;
    }

    let query_tokens: HashSet<String> = tokenize_path(query).into_iter().collect();
    SUBSYSTEM_TEST_QUERY_KEYWORDS
        .iter()
        .any(|keyword| query_tokens.contains(*keyword))
}

fn normalize_subsystem_explicit_files(
    files: &[String],
    prefer_document_files: bool,
) -> Vec<String> {
    let mut normalized: Vec<String> = files
        .iter()
        .filter(|file| is_queryable_graph_file(file))
        .cloned()
        .collect();
    dedupe_strings(&mut normalized);

    if prefer_document_files {
        return normalized;
    }

    let code_files: Vec<String> = normalized
        .into_iter()
        .filter(|file| !is_markdown_graph_file(file))
        .collect();
    if !code_files.is_empty() {
        return code_files;
    }

    Vec::new()
}

fn subsystem_candidate_files(graph: &CodeGraph, prefer_document_files: bool) -> Vec<String> {
    let all_files = unique_graph_files(graph);
    if prefer_document_files {
        return all_files;
    }

    let code_files: Vec<String> = all_files
        .iter()
        .filter(|file| !is_markdown_graph_file(file))
        .cloned()
        .collect();
    if !code_files.is_empty() {
        return code_files;
    }

    all_files
}

fn promote_explicit_files(
    ranked_files: Vec<FileRecommendation>,
    explicit_files: &[String],
) -> Vec<FileRecommendation> {
    if explicit_files.is_empty() {
        return ranked_files;
    }

    let explicit_set: HashSet<String> = explicit_files.iter().cloned().collect();
    let mut promoted = Vec::new();

    for explicit in explicit_files {
        if let Some(item) = ranked_files
            .iter()
            .find(|candidate| candidate.file == *explicit)
        {
            promoted.push(item.clone());
        } else {
            promoted.push(FileRecommendation {
                file: explicit.clone(),
                score: 10.0,
                confidence_band: "high".to_string(),
                evidence: vec!["entry".to_string()],
                reasons: vec!["user supplied entry file".to_string()],
            });
        }
    }

    for item in ranked_files {
        if !explicit_set.contains(&item.file) {
            promoted.push(item);
        }
    }

    promoted
}

fn prioritize_entry_scope_files(
    ranked_files: Vec<FileRecommendation>,
    entry_files: &[String],
) -> Vec<FileRecommendation> {
    if entry_files.is_empty() {
        return ranked_files;
    }

    let entry_set: HashSet<String> = entry_files.iter().cloned().collect();
    let mut scoped = ranked_files;
    scoped.sort_by(|a, b| {
        entry_scope_rank(&a.file, &entry_set, entry_files)
            .cmp(&entry_scope_rank(&b.file, &entry_set, entry_files))
            .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal))
            .then_with(|| a.file.cmp(&b.file))
    });
    scoped
}

fn entry_scope_rank(file: &str, entry_set: &HashSet<String>, entry_files: &[String]) -> usize {
    if entry_set.contains(file) {
        0
    } else if file_matches_entry_scope(file, entry_files) {
        1
    } else {
        2
    }
}

fn prepare_change_test_anchor_files(
    entry_files: &[String],
    primary_files: &[FileRecommendation],
    limit: usize,
) -> Vec<String> {
    let mut anchors: Vec<String> = entry_files
        .iter()
        .filter(|file| is_queryable_graph_file(file))
        .cloned()
        .collect();

    if entry_files.is_empty() {
        anchors.extend(
            primary_files
                .iter()
                .filter(|item| is_queryable_graph_file(&item.file))
                .take(limit.max(1).min(3))
                .map(|item| item.file.clone()),
        );
        dedupe_strings(&mut anchors);
        return anchors;
    }

    let entry_focus_tokens: HashSet<String> = entry_files
        .iter()
        .flat_map(|file| focus_tokens_for_file(file))
        .collect();

    for item in primary_files {
        if anchors.iter().any(|file| file == &item.file) || !is_queryable_graph_file(&item.file) {
            continue;
        }

        let shares_parent = entry_files
            .iter()
            .any(|entry| shared_directory_prefix_len(entry, &item.file) > 0);
        let file_focus_tokens: HashSet<String> =
            focus_tokens_for_file(&item.file).into_iter().collect();
        let shared_focus = overlap_count_set(&entry_focus_tokens, &file_focus_tokens);
        if shares_parent || shared_focus > 0 {
            anchors.push(item.file.clone());
        }

        if anchors.len() >= limit.max(1).min(4) {
            break;
        }
    }

    dedupe_strings(&mut anchors);
    anchors
}

fn prepare_change_test_anchor_symbols(
    entry_symbols: &[String],
    ranked_symbols: &[SymbolRecommendation],
) -> Vec<String> {
    let mut anchors = if !entry_symbols.is_empty() {
        entry_symbols.to_vec()
    } else {
        ranked_symbols
            .iter()
            .take(3)
            .map(|item| item.symbol.clone())
            .collect()
    };
    dedupe_strings(&mut anchors);
    anchors
}

fn prefer_symbols_in_files(
    ranked_symbols: Vec<SymbolRecommendation>,
    preferred_files: &[String],
) -> Vec<SymbolRecommendation> {
    if preferred_files.is_empty() {
        return ranked_symbols;
    }

    let preferred_set: HashSet<String> = preferred_files.iter().cloned().collect();
    let names_in_preferred: HashSet<String> = ranked_symbols
        .iter()
        .filter(|item| preferred_set.contains(&item.file))
        .map(|item| item.symbol.clone())
        .collect();

    ranked_symbols
        .into_iter()
        .filter(|item| {
            preferred_set.contains(&item.file) || !names_in_preferred.contains(&item.symbol)
        })
        .collect()
}

fn input_mentions_symbol(input: &str, symbol: &str) -> bool {
    let mut start = 0usize;
    while let Some(offset) = input[start..].find(symbol) {
        let index = start + offset;
        let before = input[..index].chars().next_back();
        let after = input[index + symbol.len()..].chars().next();
        let boundary_before = before
            .map(|ch| !(ch.is_alphanumeric() || ch == '_'))
            .unwrap_or(true);
        let boundary_after = after
            .map(|ch| !(ch.is_alphanumeric() || ch == '_'))
            .unwrap_or(true);
        if boundary_before && boundary_after {
            return true;
        }
        start = index + symbol.len();
    }
    false
}

fn is_assistant_artifact_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    normalized
        .split('/')
        .any(|component| ASSISTANT_ARTIFACT_DIRS.contains(&component))
}

fn task_relevance_hits(
    file: &str,
    symbol: &str,
    query_tokens: &HashSet<String>,
    entry_files: &HashSet<String>,
    entry_symbols: &HashSet<String>,
) -> usize {
    let mut hits = 0;
    if entry_files.contains(file) {
        hits += 3;
    }
    if entry_symbols.contains(symbol) {
        hits += 3;
    }
    hits += overlap_count(query_tokens, &tokenize_path(file));
    hits += overlap_count(query_tokens, &tokenize_path(symbol));
    hits
}

fn focused_failure_test_symbols(suspects: &[SymbolRecommendation]) -> Vec<String> {
    let Some(top_score) = suspects.first().map(|item| item.score) else {
        return Vec::new();
    };

    let is_focus_candidate = |item: &SymbolRecommendation| {
        item.score + 1.5 >= top_score
            || item.role == "line_reference"
            || item.role == "file_reference"
    };

    let mut symbols: Vec<String> = suspects
        .iter()
        .filter(|item| !is_test_file(&item.file) && is_focus_candidate(item))
        .take(4)
        .map(|item| item.symbol.clone())
        .collect();
    if symbols.is_empty() {
        symbols = suspects
            .iter()
            .filter(|item| is_focus_candidate(item))
            .take(4)
            .map(|item| item.symbol.clone())
            .collect();
    }
    dedupe_strings(&mut symbols);
    symbols
}

fn boost_direct_failure_tests(tests: &mut Vec<TestRecommendation>, extracted_files: &[String]) {
    let direct_test_files: Vec<String> = extracted_files
        .iter()
        .filter(|file| is_test_file(file) && is_queryable_graph_file(file))
        .cloned()
        .collect();
    if direct_test_files.is_empty() {
        return;
    }

    let mut next_confidence = tests
        .first()
        .map(|item| item.confidence.max(6.0) + 2.0)
        .unwrap_or(8.0);

    for file in direct_test_files {
        if let Some(existing) = tests.iter_mut().find(|item| item.file == file) {
            existing.confidence = existing.confidence.max(next_confidence);
            existing
                .reasons
                .insert(0, "referenced directly in failure output".to_string());
            existing.evidence.insert(0, "failure_anchor".to_string());
            dedupe_strings(&mut existing.reasons);
            existing.reasons.truncate(3);
            dedupe_strings(&mut existing.evidence);
            existing.evidence.truncate(3);
            existing.confidence_band =
                calibrate_test_confidence(existing.confidence, &existing.evidence);
        } else {
            let evidence = vec!["failure_anchor".to_string()];
            tests.push(TestRecommendation {
                file,
                confidence: next_confidence,
                confidence_band: calibrate_test_confidence(next_confidence, &evidence),
                evidence,
                reasons: vec!["referenced directly in failure output".to_string()],
            });
        }
        next_confidence -= 0.25;
    }

    tests.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
    });
}

fn rank_line_matches_by_specificity(nodes: &mut Vec<&GraphNode>, line: usize) {
    nodes.sort_by(|a, b| {
        symbol_span(*a)
            .cmp(&symbol_span(*b))
            .then_with(|| line_match_kind_rank(a).cmp(&line_match_kind_rank(b)))
            .then_with(|| line_distance_to_node(*a, line).cmp(&line_distance_to_node(*b, line)))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn symbol_span(node: &GraphNode) -> usize {
    node.end_line.max(node.line).saturating_sub(node.line)
}

fn line_match_kind_rank(node: &GraphNode) -> usize {
    match node.kind {
        crate::symbols::SymbolKind::Method | crate::symbols::SymbolKind::Function => 0,
        crate::symbols::SymbolKind::Variable | crate::symbols::SymbolKind::Constant => 1,
        _ => 2,
    }
}

fn line_distance_to_node(node: &GraphNode, line: usize) -> usize {
    if line < node.line {
        node.line - line
    } else if line > node.end_line {
        line - node.end_line
    } else {
        0
    }
}

fn format_line_span(node: &GraphNode) -> String {
    let end = node.end_line.max(node.line);
    if node.line == end {
        node.line.to_string()
    } else {
        format!("{}-{}", node.line, end)
    }
}

fn is_test_file(file: &str) -> bool {
    file.starts_with("tests/")
        || file.starts_with("test/")
        || file.starts_with("__tests__/")
        || file.contains("/tests/")
        || file.contains("/test/")
        || file.contains("/__tests__/")
        || file.contains("_tests.")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.ends_with("tests.rs")
        || file.ends_with("_test.rs")
        || file.ends_with("_test.go")
        || file.ends_with("_test.py")
}

fn is_test_support_file(file: &str) -> bool {
    let basename = file.rsplit('/').next().unwrap_or(file);
    basename == "conftest.py"
}

fn normalized_stem(file: &str) -> String {
    let basename = file.rsplit('/').next().unwrap_or(file);
    let stem = basename.split('.').next().unwrap_or(basename);
    stem.replace("_test", "")
        .replace("_spec", "")
        .replace("test_", "")
        .replace("spec_", "")
}

fn tokenize_path(value: &str) -> Vec<String> {
    value
        .split(|c: char| !c.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(normalize_token)
        .filter(|part| part.len() >= 3)
        .filter(|part| !PATH_STOP_WORDS.contains(&part.as_str()))
        .collect()
}

fn focus_tokens_for_file(file: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let basename = file.rsplit('/').next().unwrap_or(file);
    let stem = basename.split('.').next().unwrap_or(basename);
    tokens.extend(tokenize_path(stem));
    if let Some(parent) = file.rsplit('/').nth(1) {
        tokens.extend(tokenize_path(parent));
    }
    dedupe_strings(&mut tokens);
    tokens
}

fn normalize_token(part: &str) -> String {
    let lower = part.to_lowercase();
    if lower == "cert" {
        return "certificate".to_string();
    }
    if lower.len() > 5 && lower.ends_with("ies") {
        return format!("{}y", &lower[..lower.len() - 3]);
    }
    if lower.len() > 5
        && lower.ends_with('s')
        && !lower.ends_with("ss")
        && !lower.ends_with("us")
        && !lower.ends_with("is")
    {
        return lower[..lower.len() - 1].to_string();
    }
    lower
}

fn file_matches_entry_scope(file: &str, entry_files: &[String]) -> bool {
    let file_focus_tokens: HashSet<String> = focus_tokens_for_file(file).into_iter().collect();
    entry_files.iter().any(|entry| {
        let entry_focus_tokens: HashSet<String> =
            focus_tokens_for_file(entry).into_iter().collect();
        shared_directory_prefix_len(entry, file) > 0
            || overlap_count_set(&file_focus_tokens, &entry_focus_tokens) > 0
    })
}

fn shared_directory_prefix_len(left: &str, right: &str) -> usize {
    let left_parts: Vec<&str> = left.split('/').collect();
    let right_parts: Vec<&str> = right.split('/').collect();

    left_parts
        .iter()
        .zip(right_parts.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

fn overlap_count(left: &HashSet<String>, right: &[String]) -> usize {
    right
        .iter()
        .filter(|item| left.contains(item.as_str()))
        .count()
}

fn overlap_count_set(left: &HashSet<String>, right: &HashSet<String>) -> usize {
    right
        .iter()
        .filter(|item| left.contains(item.as_str()))
        .count()
}

fn extract_diff_files(diff: Option<&str>) -> Vec<String> {
    let Some(diff) = diff else {
        return Vec::new();
    };

    let mut files = Vec::new();
    for line in diff.lines() {
        if let Some(file) = line.strip_prefix("+++ b/") {
            if file != "/dev/null" {
                files.push(file.to_string());
            }
        } else if let Some(file) = line.strip_prefix("--- a/") {
            if file != "/dev/null" {
                files.push(file.to_string());
            }
        }
    }
    dedupe_strings(&mut files);
    files
}

fn dedupe_strings(items: &mut Vec<String>) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(item.clone()));
}

fn infer_file_evidence(reasons: &[String]) -> Vec<String> {
    let mut evidence = Vec::new();
    for reason in reasons {
        let tag = if reason.contains("user supplied entry file")
            || reason.contains("user supplied symbol")
        {
            Some("entry")
        } else if reason.starts_with("pivot symbol") || reason.starts_with("supporting symbol") {
            Some("capsule")
        } else if reason.contains(" dependency via ") || reason.contains(" dependent via ") {
            Some("graph")
        } else if reason.contains("matches source file stem")
            || reason.contains("shares ")
            || reason.contains("matches ")
        {
            Some("lexical")
        } else {
            None
        };

        if let Some(tag) = tag {
            evidence.push(tag.to_string());
        }
    }
    evidence
}

fn infer_symbol_evidence(role: &str) -> Vec<String> {
    let mut evidence = Vec::new();
    match role {
        "line_reference" => {
            evidence.push("line".to_string());
            evidence.push("direct".to_string());
        }
        "file_reference" => {
            evidence.push("file".to_string());
            evidence.push("direct".to_string());
        }
        "entry_symbol" => {
            evidence.push("entry".to_string());
            evidence.push("direct".to_string());
        }
        "pivot" | "context" => {
            evidence.push("capsule".to_string());
        }
        "token_overlap" => {
            evidence.push("lexical".to_string());
        }
        _ if role.starts_with("dependency:") || role.starts_with("dependent:") => {
            evidence.push("graph".to_string());
        }
        _ if role == "test_symbol" => {
            evidence.push("test".to_string());
            evidence.push("direct".to_string());
        }
        _ => {}
    }
    evidence
}

fn infer_test_evidence(reasons: &[String]) -> Vec<String> {
    let mut evidence = Vec::new();
    for reason in reasons {
        let tag = if reason.contains("referenced directly in failure output") {
            Some("failure_anchor")
        } else if reason.contains("directly depends on") {
            Some("graph")
        } else if reason.contains("directly exercises") {
            Some("direct_symbol")
        } else if reason.contains("contains ") && reason.contains("exactly match the suspect set") {
            Some("exact_symbol")
        } else if reason.contains("matches source file stem") {
            Some("stem")
        } else if reason.contains("shares ") && reason.contains("path token") {
            Some("path")
        } else if reason.contains("shares ") && reason.contains("directory segment") {
            Some("path")
        } else if reason.contains("matches ") && reason.contains("core domain token") {
            Some("domain")
        } else if reason.contains("mentions ") && reason.contains("symbol token") {
            Some("lexical_symbol")
        } else if reason.contains("shares ") && reason.contains("test symbol names") {
            Some("lexical_test_name")
        } else {
            None
        };

        if let Some(tag) = tag {
            evidence.push(tag.to_string());
        }
    }
    evidence
}

fn calibrate_file_confidence(score: f64, evidence: &[String]) -> String {
    calibrate_band(score, score, evidence, RecommendationKind::File)
}

fn calibrate_symbol_confidence(score: f64, evidence: &[String]) -> String {
    calibrate_symbol_band(score, score, evidence, "", 0)
}

fn calibrate_test_confidence(confidence: f64, evidence: &[String]) -> String {
    calibrate_band(confidence, confidence, evidence, RecommendationKind::Test)
}

enum RecommendationKind {
    File,
    Test,
}

fn calibrate_band(
    score: f64,
    top_score: f64,
    evidence: &[String],
    kind: RecommendationKind,
) -> String {
    let relative = relative_score(score, top_score);
    let strength = evidence_strength(evidence);
    let has_direct_anchor = evidence.iter().any(|item| {
        matches!(
            item.as_str(),
            "entry" | "line" | "file" | "failure_anchor" | "exact_symbol"
        )
    });
    let has_structural_support = evidence.iter().any(|item| {
        matches!(
            item.as_str(),
            "graph" | "direct" | "direct_symbol" | "capsule" | "path" | "domain" | "stem"
        )
    });

    match kind {
        RecommendationKind::File => {
            if evidence.iter().any(|item| item == "entry") || (strength >= 6.0 && relative >= 0.7) {
                "high".to_string()
            } else if has_structural_support && strength >= 3.0 && relative >= 0.35 {
                "medium".to_string()
            } else {
                "low".to_string()
            }
        }
        RecommendationKind::Test => {
            if evidence.iter().any(|item| item == "failure_anchor")
                || ((has_direct_anchor || evidence.iter().any(|item| item == "graph"))
                    && strength >= 5.0
                    && relative >= 0.65)
            {
                "high".to_string()
            } else if has_structural_support && strength >= 3.0 && relative >= 0.4 {
                "medium".to_string()
            } else {
                "low".to_string()
            }
        }
    }
}

fn calibrate_symbol_band(
    score: f64,
    top_score: f64,
    evidence: &[String],
    role: &str,
    rank: usize,
) -> String {
    let relative = relative_score(score, top_score);
    let strength = evidence_strength(evidence);
    let has_direct_anchor = evidence.iter().any(|item| {
        matches!(
            item.as_str(),
            "entry" | "line" | "file" | "failure_anchor" | "exact_symbol"
        )
    });
    let has_structural_support = evidence.iter().any(|item| {
        matches!(
            item.as_str(),
            "capsule" | "path" | "domain" | "stem" | "test" | "direct"
        )
    });
    let graph_only = evidence
        .iter()
        .all(|item| matches!(item.as_str(), "graph" | "direct_symbol"));
    let is_related_context = role.starts_with("dependency:") || role.starts_with("dependent:");

    if has_direct_anchor && strength >= 5.0 && relative >= 0.55 {
        "high".to_string()
    } else if graph_only && is_related_context {
        if strength >= 5.0 && relative >= 0.85 && score >= 3.0 && rank == 0 {
            "medium".to_string()
        } else {
            "low".to_string()
        }
    } else if (has_structural_support || has_direct_anchor) && strength >= 4.0 && relative >= 0.4 {
        "medium".to_string()
    } else {
        "low".to_string()
    }
}

fn evidence_strength(evidence: &[String]) -> f64 {
    let mut total = 0.0;
    let unique: HashSet<&str> = evidence.iter().map(|item| item.as_str()).collect();
    for tag in unique {
        total += match tag {
            "entry" | "line" | "file" | "failure_anchor" | "exact_symbol" => 4.0,
            "direct" | "graph" | "direct_symbol" => 3.0,
            "capsule" | "path" | "domain" | "stem" | "test" => 2.0,
            "lexical" | "lexical_symbol" | "lexical_test_name" => 1.0,
            _ => 0.5,
        };
    }
    total
}

fn relative_score(score: f64, top_score: f64) -> f64 {
    if top_score <= 0.0 {
        0.0
    } else {
        score / top_score
    }
}

fn scenario_confidence_band(score: f64, top_score: f64) -> String {
    let relative = relative_score(score, top_score);
    if relative >= 0.72 && score >= 2.6 {
        "high".to_string()
    } else if relative >= 0.42 && score >= 1.35 {
        "medium".to_string()
    } else {
        "low".to_string()
    }
}

fn calibrate_file_recommendations(items: &mut [FileRecommendation]) {
    let top_score = items.first().map(|item| item.score).unwrap_or(0.0);
    for item in items {
        item.confidence_band = calibrate_band(
            item.score,
            top_score,
            &item.evidence,
            RecommendationKind::File,
        );
    }
}

fn calibrate_symbol_recommendations(items: &mut [SymbolRecommendation]) {
    let top_score = items.first().map(|item| item.score).unwrap_or(0.0);
    for (index, item) in items.iter_mut().enumerate() {
        item.confidence_band =
            calibrate_symbol_band(item.score, top_score, &item.evidence, &item.role, index);
    }
}

fn calibrate_test_recommendations(items: &mut [TestRecommendation]) {
    let top_score = items.first().map(|item| item.confidence).unwrap_or(0.0);
    for item in items {
        item.confidence_band = calibrate_band(
            item.confidence,
            top_score,
            &item.evidence,
            RecommendationKind::Test,
        );
    }
}

fn round_score(score: f64) -> f64 {
    (score * 100.0).round() / 100.0
}

fn severity_rank(level: &str) -> usize {
    match level {
        "high" => 0,
        "medium" => 1,
        _ => 2,
    }
}
