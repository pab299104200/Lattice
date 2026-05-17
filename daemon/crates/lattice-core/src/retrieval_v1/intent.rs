use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

const IMPERATIVE_SCAN_LIMIT: usize = 4;
pub const MAX_INTENT_TASK_CHARS: usize = 2048;

/// Intent labels for Retrieval V1 task statements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum IntentLabel {
    /// Biases `diagnose_failure`, `trace_scenario`, and bug-fix retrieval paths.
    Debug,
    /// Biases `prepare_change`, `plan_edit`, and refactor-heavy graph expansion.
    Refactor,
    /// Biases `get_context_capsule`, `summarize_subsystem`, and `expand_context`.
    Explain,
    /// Biases `prepare_change` and feature-entrypoint retrieval for new capabilities.
    AddFeature,
    /// Biases `prepare_change` and `plan_edit` for changing an existing capability.
    ModifyFeature,
    /// Biases `find_relevant_tests` and regression-oriented edit planning.
    AddTest,
    /// Biases `get_docs_capsule`, `find_stale_docs`, and documentation edits.
    UpdateDocs,
    /// Biases migration-aware planning and schema-touching retrieval paths.
    Migration,
    /// Biases benchmark, hot-path, and cost-sensitive retrieval paths.
    Performance,
    /// Biases `impact_from_diff`, review checklists, and audit-style retrieval.
    Review,
    /// Fallback when no deterministic task signal fires.
    Unknown,
}

/// Finite, inspectable feature kinds used by the classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum IntentFeatureKind {
    LemmaMatch,
    ImperativeVerb,
    ErrorKeyword,
    FileTokenShape,
    SymbolTokenShape,
    QuestionForm,
    DescriptivePhrase,
}

/// A literal feature that contributed to one or more labels.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct IntentFeature {
    pub kind: IntentFeatureKind,
    pub value: String,
}

/// Diagnostic tuple for explainable ranking stages.
pub type IntentDiagnostic = (IntentLabel, Vec<IntentFeature>, f64);

/// Inspectable classifier output for Retrieval V1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntentClassification {
    pub primary_label: IntentLabel,
    pub secondary_labels: Vec<IntentLabel>,
    pub feature_scores: BTreeMap<IntentLabel, f64>,
    pub contributing_features: BTreeMap<IntentLabel, Vec<IntentFeature>>,
    pub fired_features: Vec<IntentFeature>,
    pub inspected_text: String,
    pub input_was_truncated: bool,
}

impl IntentClassification {
    pub fn diagnostics(&self) -> Vec<IntentDiagnostic> {
        let mut diagnostics = Vec::new();
        for label in ordered_labels() {
            let score = self.feature_scores.get(&label).copied().unwrap_or(0.0);
            if score <= 0.0 && label != IntentLabel::Unknown {
                continue;
            }
            let features = self
                .contributing_features
                .get(&label)
                .cloned()
                .unwrap_or_default();
            diagnostics.push((label, features, score));
        }
        diagnostics.sort_by(|left, right| compare_labels(left.0, right.0, left.2, right.2));
        diagnostics
    }
}

/// Feature-based and inspectable scoring is required by
/// `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md#ranking-complexity`.
pub fn classify_intent(task_text: &str) -> IntentClassification {
    let prepared = PreparedTask::new(task_text);
    if prepared.inspected_text.is_empty() {
        return unknown_classification(prepared);
    }

    let mut scores = ScoreCard::new();
    add_rule_features(&prepared, &mut scores);
    add_question_features(&prepared, &mut scores);
    add_shape_features(&prepared, &mut scores);
    add_descriptive_phrase_fallback(&prepared, &mut scores);

    finalize_classification(prepared, scores)
}

#[derive(Debug, Clone)]
struct PreparedTask {
    inspected_text: String,
    tokens: Vec<String>,
    raw_tokens: Vec<String>,
    input_was_truncated: bool,
}

impl PreparedTask {
    fn new(task_text: &str) -> Self {
        let trimmed = task_text.trim();
        let total_chars = trimmed.chars().count();
        let inspected_text: String = trimmed.chars().take(MAX_INTENT_TASK_CHARS).collect();
        let input_was_truncated = total_chars > MAX_INTENT_TASK_CHARS;
        let raw_tokens = split_raw_tokens(&inspected_text);
        let tokens = normalize_tokens(&inspected_text);

        Self {
            inspected_text,
            tokens,
            raw_tokens,
            input_was_truncated,
        }
    }
}

#[derive(Default)]
struct ScoreCard {
    scores: BTreeMap<IntentLabel, f64>,
    features: BTreeMap<IntentLabel, Vec<IntentFeature>>,
}

impl ScoreCard {
    fn new() -> Self {
        let mut scores = BTreeMap::new();
        let mut features = BTreeMap::new();
        for label in ordered_labels() {
            scores.insert(label, 0.0);
            features.insert(label, Vec::new());
        }
        Self { scores, features }
    }

    fn add(&mut self, label: IntentLabel, feature: IntentFeature, score: f64) {
        let current = self.scores.entry(label).or_insert(0.0);
        *current += score;

        let label_features = self.features.entry(label).or_default();
        if !label_features.contains(&feature) {
            label_features.push(feature);
            label_features.sort();
        }
    }
}

#[derive(Clone, Copy)]
struct RuleSet {
    label: IntentLabel,
    lemmas: &'static [&'static str],
    imperative_verbs: &'static [&'static str],
    lemma_weight: f64,
    imperative_weight: f64,
}

const DEBUG_RULES: RuleSet = RuleSet {
    label: IntentLabel::Debug,
    lemmas: &[
        "fix",
        "bug",
        "debug",
        "error",
        "failure",
        "broken",
        "exception",
        "panic",
        "traceback",
        "stack trace",
    ],
    imperative_verbs: &["fix", "debug", "investigate", "diagnose", "trace"],
    lemma_weight: 2.0,
    imperative_weight: 3.0,
};

const REFACTOR_RULES: RuleSet = RuleSet {
    label: IntentLabel::Refactor,
    lemmas: &[
        "refactor",
        "cleanup",
        "clean up",
        "restructure",
        "simplify",
        "untangle",
        "consolidate",
    ],
    imperative_verbs: &["refactor", "simplify", "restructure", "consolidate"],
    lemma_weight: 2.0,
    imperative_weight: 3.0,
};

const EXPLAIN_RULES: RuleSet = RuleSet {
    label: IntentLabel::Explain,
    lemmas: &[
        "explain",
        "describe",
        "understand",
        "overview",
        "walkthrough",
        "flow",
        "architecture",
    ],
    imperative_verbs: &["explain", "describe", "show", "walkthrough"],
    lemma_weight: 2.0,
    imperative_weight: 3.0,
};

const ADD_FEATURE_RULES: RuleSet = RuleSet {
    label: IntentLabel::AddFeature,
    lemmas: &[
        "add",
        "implement",
        "create",
        "build",
        "introduce",
        "support",
        "new feature",
    ],
    imperative_verbs: &["add", "implement", "create", "build", "introduce"],
    lemma_weight: 1.5,
    imperative_weight: 3.0,
};

const MODIFY_FEATURE_RULES: RuleSet = RuleSet {
    label: IntentLabel::ModifyFeature,
    lemmas: &[
        "change", "modify", "update", "adjust", "extend", "edit", "rename",
    ],
    imperative_verbs: &["change", "modify", "update", "adjust", "extend", "edit"],
    lemma_weight: 1.75,
    imperative_weight: 2.5,
};

const ADD_TEST_RULES: RuleSet = RuleSet {
    label: IntentLabel::AddTest,
    lemmas: &[
        "test",
        "tests",
        "coverage",
        "regression",
        "spec",
        "assertion",
        "integration test",
        "unit test",
    ],
    imperative_verbs: &["test", "cover", "verify", "assert"],
    lemma_weight: 2.0,
    imperative_weight: 2.5,
};

const UPDATE_DOCS_RULES: RuleSet = RuleSet {
    label: IntentLabel::UpdateDocs,
    lemmas: &[
        "docs",
        "documentation",
        "readme",
        "guide",
        "manual",
        "runbook",
        "comment",
        "comments",
    ],
    imperative_verbs: &["document", "write", "update", "clarify"],
    lemma_weight: 2.0,
    imperative_weight: 2.5,
};

const MIGRATION_RULES: RuleSet = RuleSet {
    label: IntentLabel::Migration,
    lemmas: &[
        "migration",
        "migrate",
        "backfill",
        "schema",
        "upgrade",
        "downgrade",
        "alembic",
    ],
    imperative_verbs: &["migrate", "backfill", "upgrade", "downgrade"],
    lemma_weight: 2.0,
    imperative_weight: 3.0,
};

const PERFORMANCE_RULES: RuleSet = RuleSet {
    label: IntentLabel::Performance,
    lemmas: &[
        "performance",
        "latency",
        "slow",
        "faster",
        "optimize",
        "benchmark",
        "profile",
    ],
    imperative_verbs: &["optimize", "benchmark", "profile", "speed"],
    lemma_weight: 2.0,
    imperative_weight: 3.0,
};

const REVIEW_RULES: RuleSet = RuleSet {
    label: IntentLabel::Review,
    lemmas: &[
        "review",
        "audit",
        "inspect",
        "critique",
        "regression risk",
        "pull request",
        "pr",
    ],
    imperative_verbs: &["review", "audit", "inspect"],
    lemma_weight: 2.0,
    imperative_weight: 3.0,
};

const RULE_SETS: &[RuleSet] = &[
    DEBUG_RULES,
    REFACTOR_RULES,
    EXPLAIN_RULES,
    ADD_FEATURE_RULES,
    MODIFY_FEATURE_RULES,
    ADD_TEST_RULES,
    UPDATE_DOCS_RULES,
    MIGRATION_RULES,
    PERFORMANCE_RULES,
    REVIEW_RULES,
];

const ERROR_KEYWORDS: &[&str] = &[
    "error",
    "errors",
    "failed",
    "failing",
    "panic",
    "crash",
    "broken",
    "traceback",
    "stack trace",
];

const QUESTION_WORDS: &[&str] = &["how", "what", "why", "where", "which", "who", "when"];

fn add_rule_features(task: &PreparedTask, scores: &mut ScoreCard) {
    for rule in RULE_SETS {
        add_phrase_features(task, scores, *rule);
        add_imperative_features(task, scores, *rule);
    }
    add_error_keyword_features(task, scores);
}

fn add_phrase_features(task: &PreparedTask, scores: &mut ScoreCard, rule: RuleSet) {
    let lowered = task.inspected_text.to_lowercase();
    for lemma in rule.lemmas {
        if lowered.contains(lemma) {
            scores.add(
                rule.label,
                feature(IntentFeatureKind::LemmaMatch, *lemma),
                rule.lemma_weight,
            );
        }
    }
}

fn add_imperative_features(task: &PreparedTask, scores: &mut ScoreCard, rule: RuleSet) {
    for token in task.tokens.iter().take(IMPERATIVE_SCAN_LIMIT) {
        if rule.imperative_verbs.contains(&token.as_str()) {
            scores.add(
                rule.label,
                feature(IntentFeatureKind::ImperativeVerb, token),
                rule.imperative_weight,
            );
        }
    }
}

fn add_error_keyword_features(task: &PreparedTask, scores: &mut ScoreCard) {
    let lowered = task.inspected_text.to_lowercase();
    for keyword in ERROR_KEYWORDS {
        if lowered.contains(keyword) {
            scores.add(
                IntentLabel::Debug,
                feature(IntentFeatureKind::ErrorKeyword, *keyword),
                2.5,
            );
        }
    }
}

fn add_question_features(task: &PreparedTask, scores: &mut ScoreCard) {
    if question_signal(task).is_none() {
        return;
    }
    let feature_value = question_signal(task).unwrap_or_default();
    scores.add(
        IntentLabel::Explain,
        feature(IntentFeatureKind::QuestionForm, feature_value),
        3.5,
    );
}

fn add_shape_features(task: &PreparedTask, scores: &mut ScoreCard) {
    for token in &task.raw_tokens {
        if let Some((label, weight)) = classify_file_shape(token) {
            scores.add(
                label,
                feature(IntentFeatureKind::FileTokenShape, token),
                weight,
            );
        }
        if let Some((label, weight)) = classify_symbol_shape(token) {
            scores.add(
                label,
                feature(IntentFeatureKind::SymbolTokenShape, token),
                weight,
            );
        }
    }
}

fn add_descriptive_phrase_fallback(task: &PreparedTask, scores: &mut ScoreCard) {
    if has_non_unknown_score(scores) || task.tokens.len() < 4 {
        return;
    }
    scores.add(
        IntentLabel::Explain,
        feature(IntentFeatureKind::DescriptivePhrase, "content_words>=4"),
        2.0,
    );
}

fn finalize_classification(task: PreparedTask, scores: ScoreCard) -> IntentClassification {
    let ranked = ranked_labels(&scores.scores);
    if ranked.is_empty() {
        return unknown_classification(task);
    }

    let primary_label = ranked[0];
    let secondary_labels = ranked.into_iter().skip(1).collect();
    let fired_features = collect_fired_features(&scores.features);

    IntentClassification {
        primary_label,
        secondary_labels,
        feature_scores: scores.scores,
        contributing_features: scores.features,
        fired_features,
        inspected_text: task.inspected_text,
        input_was_truncated: task.input_was_truncated,
    }
}

fn has_non_unknown_score(scores: &ScoreCard) -> bool {
    ordered_labels()
        .into_iter()
        .filter(|label| *label != IntentLabel::Unknown)
        .any(|label| scores.scores.get(&label).copied().unwrap_or(0.0) > 0.0)
}

fn unknown_classification(task: PreparedTask) -> IntentClassification {
    let mut feature_scores = BTreeMap::new();
    let mut contributing_features = BTreeMap::new();
    for label in ordered_labels() {
        feature_scores.insert(label, 0.0);
        contributing_features.insert(label, Vec::new());
    }
    feature_scores.insert(IntentLabel::Unknown, 1.0);

    IntentClassification {
        primary_label: IntentLabel::Unknown,
        secondary_labels: Vec::new(),
        feature_scores,
        contributing_features,
        fired_features: Vec::new(),
        inspected_text: task.inspected_text,
        input_was_truncated: task.input_was_truncated,
    }
}

fn ranked_labels(scores: &BTreeMap<IntentLabel, f64>) -> Vec<IntentLabel> {
    let mut labels: Vec<IntentLabel> = scores
        .iter()
        .filter_map(|(label, score)| (*score > 0.0).then_some(*label))
        .collect();
    labels.sort_by(|left, right| {
        let left_score = scores.get(left).copied().unwrap_or(0.0);
        let right_score = scores.get(right).copied().unwrap_or(0.0);
        compare_labels(*left, *right, left_score, right_score)
    });
    labels
}

fn compare_labels(
    left_label: IntentLabel,
    right_label: IntentLabel,
    left_score: f64,
    right_score: f64,
) -> std::cmp::Ordering {
    right_score
        .total_cmp(&left_score)
        .then_with(|| intent_priority(left_label).cmp(&intent_priority(right_label)))
}

fn collect_fired_features(
    labeled_features: &BTreeMap<IntentLabel, Vec<IntentFeature>>,
) -> Vec<IntentFeature> {
    let mut fired = Vec::new();
    for label in ordered_labels() {
        if let Some(features) = labeled_features.get(&label) {
            for feature in features {
                if !fired.contains(feature) {
                    fired.push(feature.clone());
                }
            }
        }
    }
    fired.sort();
    fired
}

fn question_signal(task: &PreparedTask) -> Option<String> {
    if task.inspected_text.ends_with('?') {
        return Some("?".to_string());
    }
    let first = task.tokens.first()?;
    QUESTION_WORDS
        .contains(&first.as_str())
        .then(|| first.to_string())
}

fn classify_file_shape(token: &str) -> Option<(IntentLabel, f64)> {
    let lowered = token.to_lowercase();
    let looks_like_path = lowered.contains('/') || lowered.contains('\\');
    let looks_like_file = lowered.contains('.');
    if !looks_like_path && !looks_like_file {
        return None;
    }
    if lowered.ends_with(".md") || lowered.contains("readme") || lowered.contains("/docs/") {
        return Some((IntentLabel::UpdateDocs, 2.0));
    }
    if lowered.contains("migration") || lowered.contains("alembic") {
        return Some((IntentLabel::Migration, 2.0));
    }
    if lowered.contains("test") || lowered.ends_with("_spec.rs") || lowered.ends_with(".spec.ts") {
        return Some((IntentLabel::AddTest, 1.5));
    }
    Some((IntentLabel::ModifyFeature, 1.0))
}

fn classify_symbol_shape(token: &str) -> Option<(IntentLabel, f64)> {
    if token.len() < 3 {
        return None;
    }
    let lowered = token.to_lowercase();
    if lowered.ends_with("_test") || lowered.starts_with("test_") {
        return Some((IntentLabel::AddTest, 1.5));
    }
    if token.contains("::") || token.contains('(') || token.contains('_') || has_camel_case(token) {
        return Some((IntentLabel::ModifyFeature, 1.0));
    }
    None
}

fn has_camel_case(token: &str) -> bool {
    let has_lower = token.chars().any(char::is_lowercase);
    let has_upper = token.chars().any(char::is_uppercase);
    has_lower && has_upper
}

fn split_raw_tokens(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(clean_raw_token)
        .filter(|token| !token.is_empty())
        .collect()
}

fn clean_raw_token(token: &str) -> String {
    token
        .trim_matches(|ch: char| ",:;!\"'[]{}<>".contains(ch))
        .to_string()
}

fn normalize_tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .filter(|token| token.len() >= 2)
        .map(ToString::to_string)
        .collect()
}

fn feature(kind: IntentFeatureKind, value: impl AsRef<str>) -> IntentFeature {
    IntentFeature {
        kind,
        value: value.as_ref().to_string(),
    }
}

fn ordered_labels() -> [IntentLabel; 11] {
    [
        IntentLabel::Debug,
        IntentLabel::Refactor,
        IntentLabel::Explain,
        IntentLabel::AddFeature,
        IntentLabel::ModifyFeature,
        IntentLabel::AddTest,
        IntentLabel::UpdateDocs,
        IntentLabel::Migration,
        IntentLabel::Performance,
        IntentLabel::Review,
        IntentLabel::Unknown,
    ]
}

fn intent_priority(label: IntentLabel) -> usize {
    match label {
        IntentLabel::Debug => 0,
        IntentLabel::Migration => 1,
        IntentLabel::Performance => 2,
        IntentLabel::Refactor => 3,
        IntentLabel::AddFeature => 4,
        IntentLabel::ModifyFeature => 5,
        IntentLabel::AddTest => 6,
        IntentLabel::UpdateDocs => 7,
        IntentLabel::Review => 8,
        IntentLabel::Explain => 9,
        IntentLabel::Unknown => 10,
    }
}
