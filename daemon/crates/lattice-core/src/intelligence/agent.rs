use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::graph::model::{CodeGraph, EdgeKind, GraphNode};
use crate::query::{ContextCapsule, QueryIntent};

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
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub relationship: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExpandedFileSymbolContext {
    pub symbol: String,
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
    via: Vec<String>,
    score: f64,
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
    Symbol(String),
    File(String),
    Test(String),
    Memory(usize),
}

impl ExpansionTarget {
    fn kind(&self) -> &'static str {
        match self {
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
            pivot.kind.clone(),
            pivot.line,
            "pivot".to_string(),
            4.0 + pivot.score,
        );
        if let Some(node) = find_exact_node(graph, &pivot.file, &pivot.symbol) {
            push_seed_node(&mut seed_nodes, &mut seed_seen, node);
        }
    }

    for context in &capsule.context {
        if !is_queryable_graph_file(&context.file) {
            continue;
        }
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
            context.kind.clone(),
            context.line,
            "context".to_string(),
            1.75 + context.score,
        );
        if let Some(node) = find_exact_node(graph, &context.file, &context.symbol) {
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

    let test_anchor_files = prepare_change_test_anchor_files(
        entry_files,
        &primary_files,
        mode.primary_file_limit(),
    );
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
    let suggested_expand = suggest_task_bundle_expand(
        mode,
        &primary_files,
        &secondary_files,
        &symbols,
        &tests,
    );
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
    for test_file in all_files
        .into_iter()
        .filter(|file| {
            is_test_file(file) && !is_test_support_file(file) && is_queryable_graph_file(file)
        })
    {
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
    let mut file_scores: HashMap<String, FileAccumulator> = HashMap::new();
    let mut symbol_scores: HashMap<(String, String), SymbolAccumulator> = HashMap::new();
    let mut seed_nodes: Vec<&GraphNode> = Vec::new();
    let mut seed_seen: HashSet<(String, String)> = HashSet::new();
    let mut rationale = Vec::new();
    let query_tokens: HashSet<String> = tokenize_path(query).into_iter().collect();

    for file in files {
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
        for node in find_symbol_matches(graph, symbol, files).into_iter().take(4) {
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
                node.kind.short_code().to_string(),
                node.line,
                "entry_symbol".to_string(),
                6.0,
            );
            push_seed_node(&mut seed_nodes, &mut seed_seen, node);
        }
    }

    for file in unique_graph_files(graph)
        .into_iter()
        .filter(|file| !is_test_file(file) && is_queryable_graph_file(file))
    {
        let mut delta = 0.0;
        let mut reasons = Vec::new();

        let path_overlap = overlap_count(&query_tokens, &tokenize_path(&file));
        if path_overlap > 0 {
            delta += path_overlap as f64;
            reasons.push(format!("shares {} path token(s) with the subsystem query", path_overlap));
        }

        let focus_overlap = overlap_count(&query_tokens, &focus_tokens_for_file(&file));
        if focus_overlap > 0 {
            delta += 2.0 * focus_overlap as f64;
            reasons.push(format!(
                "matches {} core subsystem token(s)",
                focus_overlap
            ));
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
    ranked_files = promote_explicit_files(ranked_files, files);
    ranked_files = prioritize_entry_scope_files(ranked_files, files);
    if ranked_files.is_empty() {
        ranked_files = fallback_repo_file_recommendations(graph, mode.working_file_limit());
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
                dependent.kind.short_code().to_string(),
                dependent.line,
                format!("dependent:{}", short_edge(edge)),
                1.0,
            );
        }
    }

    let mut ranked_files = finalize_file_recommendations(file_scores);
    ranked_files = promote_explicit_files(ranked_files, files);
    ranked_files = prioritize_entry_scope_files(ranked_files, files);
    if ranked_files.is_empty() {
        ranked_files = fallback_repo_file_recommendations(graph, mode.working_file_limit());
    }
    calibrate_file_recommendations(&mut ranked_files);

    let mut ranked_symbols =
        prefer_symbols_in_files(finalize_symbol_recommendations(symbol_scores), &seed_files);
    calibrate_symbol_recommendations(&mut ranked_symbols);

    let key_files = compress_file_summaries(&ranked_files, mode.working_file_limit());
    let key_symbols = compress_symbol_summaries(&ranked_symbols, mode.symbol_limit().min(6));
    let key_file_names: Vec<String> = key_files.iter().map(|item| item.file.clone()).collect();
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
        rationale.push("Included the most relevant tests so follow-up work can stay local.".to_string());
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
        ranked_files = fallback_repo_file_recommendations(graph, mode.working_file_limit());
    }
    calibrate_file_recommendations(&mut ranked_files);
    let key_files = compress_file_summaries(&ranked_files, mode.working_file_limit());
    let key_file_names: Vec<String> = key_files.iter().map(|item| item.file.clone()).collect();

    let mut ranked_symbols =
        prefer_symbols_in_files(finalize_symbol_recommendations(symbol_scores), &key_file_names);
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
            "The requested focus did not map cleanly to cached symbols or files; use a file:, symbol:, test:, or memory: focus from the previous handle."
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
        if let Some(node) = find_exact_node(graph, &suspect.file, &suspect.symbol) {
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
        rationale.push("Suggested tests based on direct failure anchors, suspect files, and symbol hints.".to_string());
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
    let overview = build_failure_overview(
        &failure_kind,
        &suspects,
        &tests,
        &likely_causes,
        &[],
    );
    let suggested_expand = suggest_failure_expand(mode, &suspects, &extracted_files);
    let mut next_steps = build_failure_next_steps(
        &failure_kind,
        &suspects,
        &tests,
        &extracted_files,
    );
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
    graph
        .all_nodes()
        .into_iter()
        .find(|node| is_queryable_graph_file(&node.file) && node.file == file && node.name == symbol)
}

fn resolve_expansion_target(seed: &ExpandContextSeed, focus: &str) -> ExpansionTarget {
    let focus = focus.trim();

    if let Some(rest) = focus.strip_prefix("file:") {
        return ExpansionTarget::File(rest.trim().to_string());
    }
    if let Some(rest) = focus.strip_prefix("symbol:") {
        return ExpansionTarget::Symbol(rest.trim().to_string());
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
    if seed.symbols.iter().any(|symbol| symbol == focus) {
        return ExpansionTarget::Symbol(focus.to_string());
    }

    if let Some(index) = seed
        .memories
        .iter()
        .position(|memory| memory_matches_focus(memory, focus))
    {
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
            kind: candidate.kind.short_code().to_string(),
            file: candidate.file.clone(),
            line: candidate.line,
            relationship: "same_file".to_string(),
        })
        .collect();

    ExpandedSymbolContext {
        symbol: node.name.clone(),
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
            kind: node.kind.short_code().to_string(),
            line: node.line,
            signature: node.signature.to_string(),
            role: if seed.symbols.iter().any(|symbol| symbol == &node.name) {
                "handle_symbol".to_string()
            } else if seed.files.iter().any(|handle_file| handle_file == file) {
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
    let mut matches: Vec<Value> = memories
        .iter()
        .filter(|memory| memory_matches_focus(memory, focus))
        .take(limit)
        .cloned()
        .collect();

    if matches.is_empty() {
        matches.extend(memories.iter().take(limit.min(1)).cloned());
    }

    matches
}

fn memory_matches_focus(memory: &Value, focus: &str) -> bool {
    let focus_lower = focus.to_lowercase();
    memory.to_string().to_lowercase().contains(&focus_lower)
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
                    if !refs
                        .iter()
                        .any(|(existing_file, existing_line)| {
                            existing_file == &file && *existing_line == Some(line_hint)
                        })
                    {
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
            is_queryable_graph_file(&node.file) && (node.name == symbol || node.name.ends_with(symbol))
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
            SymbolRecommendation {
                symbol,
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

fn compress_file_summaries(
    items: &[FileRecommendation],
    limit: usize,
) -> Vec<CompactFileSummary> {
    items.iter()
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
    items.iter()
        .take(limit.max(1))
        .map(|item| CompactSymbolSummary {
            symbol: item.symbol.clone(),
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
) -> Vec<FileRecommendation> {
    let mut file_scores: HashMap<String, FileAccumulator> = HashMap::new();
    for node in graph
        .all_nodes()
        .into_iter()
        .filter(|node| !is_test_file(&node.file) && is_queryable_graph_file(&node.file))
    {
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
    items.truncate(limit.max(1));
    items
}

fn memory_highlights_from_values(
    values: &[Value],
    limit: usize,
    text_limit: usize,
) -> Vec<MemoryHighlight> {
    values
        .iter()
        .filter_map(|value| memory_highlight_from_value(value, text_limit))
        .take(limit.max(1))
        .collect()
}

fn memory_highlight_from_value(value: &Value, text_limit: usize) -> Option<MemoryHighlight> {
    let content = value.get("content")?.as_str()?.trim();
    if content.is_empty() {
        return None;
    }

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
    format!("prior {} {}", memory.scope, memory.memory_type)
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
            .then_with(|| is_test_file(&a.file)
            .cmp(&is_test_file(&b.file))
            )
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
            focus: format!("symbol:{}", symbol.symbol),
            reason: "Inspect the top ranked symbol to see nearby code and relationships.".to_string(),
        });
    }
    if primary_files.len() > 1 || !secondary_files.is_empty() || !tests.is_empty() {
        return primary_files.first().map(|file| ExpandSuggestion {
            focus: format!("file:{}", file.file),
            reason: "Expand the top file to inspect its local symbols before widening further.".to_string(),
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
            focus: format!("symbol:{}", symbol.symbol),
            reason: "Expand the strongest working-set symbol to inspect surrounding implementation details."
                .to_string(),
        });
    }

    files.first().map(|file| ExpandSuggestion {
        focus: format!("file:{}", file.file),
        reason: "Expand the top working-set file to inspect its neighboring symbols.".to_string(),
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
            focus: format!("symbol:{}", suspect.symbol),
            reason: "Expand the top suspect symbol to inspect its body, dependencies, and dependents."
                .to_string(),
        });
    }

    extracted_files.first().map(|file| ExpandSuggestion {
        focus: format!("file:{}", file),
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
            focus: format!("symbol:{}", symbol.symbol),
            reason: "Expand the top changed symbol to inspect downstream impact in source context."
                .to_string(),
        });
    }

    changed_files.first().map(|file| ExpandSuggestion {
        focus: format!("file:{}", file.file),
        reason: "Expand the changed file to inspect the affected region with nearby symbols.".to_string(),
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
            focus: format!("symbol:{}", symbol.symbol),
            reason: "Expand the lead subsystem symbol to inspect the implementation details behind the summary."
                .to_string(),
        });
    }

    key_files.first().map(|file| ExpandSuggestion {
        focus: format!("file:{}", file.file),
        reason: "Expand the lead file to inspect the concrete structure behind the summary.".to_string(),
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
        parts.push(format!("memory {}", truncate_text(&memory.content, 28)));
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
        parts.push(format!("pattern {}", truncate_text(&memory.content, 28)));
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
            .map(|item| item.file.len() + item.reasons.iter().map(|reason| reason.len()).sum::<usize>())
            .sum::<usize>()
        + rules.iter().map(|item| item.len()).sum::<usize>()
        + memories
            .iter()
            .map(|item| item.content.len() + item.memory_type.len() + item.scope.len())
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
            .map(|item| item.content.len() + item.memory_type.len() + item.scope.len())
            .sum::<usize>()
        + rationale.iter().map(|item| item.len()).sum::<usize>();
    chars / CHARS_PER_TOKEN_ESTIMATE
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
    values
        .iter()
        .filter_map(|value| {
            let content = value.get("content")?.as_str()?;
            Some(json!({
                "id": value.get("id"),
                "content": truncate_text(content, 120),
                "type": value.get("type").or_else(|| value.get("memory_type")),
                "scope": value.get("scope"),
                "is_stale": value.get("is_stale").and_then(|item| item.as_bool()).unwrap_or(false),
                "linked_files": value.get("linked_files").cloned().unwrap_or_else(|| json!([])),
                "linked_symbols": value.get("linked_symbols").cloned().unwrap_or_else(|| json!([]))
            }))
        })
        .take(limit.max(1))
        .collect()
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
        parts.push(format!("reuse {}", memory_reference_phrase(memory)));
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
        parts.push(format!("reuse {}", memory_reference_phrase(memory)));
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
        parts.push(format!("reuse {}", memory_reference_phrase(memory)));
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
        steps.push(format!("Inspect {} first because it was referenced directly.", file));
    }

    if let Some(test) = tests.first() {
        steps.push(format!("Re-run {} after the first edit.", test.file));
    }

    steps.push(match kind {
        "compiler" => "Keep the compiler error line as the primary anchor before widening context.".to_string(),
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
            entry.score += 1.0;
            entry.via.push(node.name.clone());
        }
    }

    let mut items: Vec<AffectedSymbolImpact> = scores
        .into_iter()
        .map(|((file, symbol), mut acc)| {
            dedupe_strings(&mut acc.via);
            acc.via.truncate(3);
            AffectedSymbolImpact {
                symbol,
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
        if let Some(item) = ranked_files.iter().find(|candidate| candidate.file == *explicit) {
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
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(Ordering::Equal)
            })
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
        || file.contains(".test.")
        || file.contains(".spec.")
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
        let entry_focus_tokens: HashSet<String> = focus_tokens_for_file(entry).into_iter().collect();
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
    right.iter().filter(|item| left.contains(item.as_str())).count()
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
            if evidence.iter().any(|item| item == "entry")
                || (strength >= 6.0 && relative >= 0.7)
            {
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
    let is_related_context =
        role.starts_with("dependency:") || role.starts_with("dependent:");

    if has_direct_anchor && strength >= 5.0 && relative >= 0.55 {
        "high".to_string()
    } else if graph_only && is_related_context {
        if strength >= 5.0 && relative >= 0.85 && score >= 3.0 && rank == 0 {
            "medium".to_string()
        } else {
            "low".to_string()
        }
    } else if (has_structural_support || has_direct_anchor) && strength >= 4.0 && relative >= 0.4
    {
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

fn calibrate_file_recommendations(items: &mut [FileRecommendation]) {
    let top_score = items.first().map(|item| item.score).unwrap_or(0.0);
    for item in items {
        item.confidence_band =
            calibrate_band(item.score, top_score, &item.evidence, RecommendationKind::File);
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
