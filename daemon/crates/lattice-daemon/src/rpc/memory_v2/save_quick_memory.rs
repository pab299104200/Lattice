use super::save_memory::{FreshnessPolicyArg, MemoryScopeArg, SaveMemoryArgs};
use lattice_core::memory::model::MemoryAssertionType;
use lattice_core::memory::{MemoryClass, MemoryEvidence};
use lattice_core::working_memory::WorkingMemoryState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SaveQuickMemoryArgs {
    pub content: String,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub task_statement: Option<String>,
    #[serde(default)]
    pub memory_class: Option<MemoryClass>,
    #[serde(default)]
    pub scope: Option<MemoryScopeArg>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub confidence_reason: Option<String>,
    #[serde(default)]
    pub linked_files: Vec<String>,
    #[serde(default)]
    pub linked_symbols: Vec<String>,
    #[serde(default)]
    pub linked_docs: Vec<String>,
    #[serde(default)]
    pub linked_tests: Vec<String>,
    #[serde(default)]
    pub linked_memories: Vec<String>,
    #[serde(default)]
    pub validity_conditions: Vec<String>,
    #[serde(default)]
    pub invalidation_triggers: Vec<String>,
    #[serde(default)]
    pub source_query: Option<String>,
    #[serde(default)]
    pub refresh_key: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
}

pub fn tool_definition() -> Value {
    json!({
        "name": "save_quick_memory",
        "description": "Capture a lightweight agent-authored memory with defaults and automatic context prefill from the current task state, focus paths, and recent failures.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "content": {"type": "string"},
                "task_id": {"type": "string"},
                "task_statement": {"type": "string"},
                "memory_class": {
                    "type": "string",
                    "enum": [
                        "observation", "decision", "constraint", "pattern", "anti_pattern",
                        "workflow_outcome", "failure_pattern", "procedure", "preference",
                        "architecture_invariant", "docs_contract", "open_question", "counter_memory"
                    ]
                },
                "scope": {
                    "type": "string",
                    "enum": ["session", "branch", "repo", "organization"]
                },
                "confidence": {"type": "number"},
                "confidence_reason": {"type": "string"},
                "linked_files": {"type": "array", "items": {"type": "string"}},
                "linked_symbols": {"type": "array", "items": {"type": "string"}},
                "linked_docs": {"type": "array", "items": {"type": "string"}},
                "linked_tests": {"type": "array", "items": {"type": "string"}},
                "linked_memories": {"type": "array", "items": {"type": "string"}},
                "validity_conditions": {"type": "array", "items": {"type": "string"}},
                "invalidation_triggers": {"type": "array", "items": {"type": "string"}},
                "source_query": {"type": "string"},
                "refresh_key": {"type": "string"},
                "branch": {"type": "string"}
            },
            "required": ["content"]
        }
    })
}

pub fn parse_args(args: &Value) -> Result<SaveQuickMemoryArgs, String> {
    serde_json::from_value(args.clone())
        .map_err(|error| format!("Invalid save_quick_memory arguments: {error}"))
}

pub fn validate_args(args: &SaveQuickMemoryArgs) -> Result<(), String> {
    if args.content.trim().is_empty() {
        return Err("save_quick_memory requires non-empty content".to_string());
    }
    if args
        .confidence
        .is_some_and(|value| !(0.0..=1.0).contains(&value))
    {
        return Err("save_quick_memory confidence must be between 0.0 and 1.0".to_string());
    }
    Ok(())
}

pub fn build_save_memory_args(
    quick: SaveQuickMemoryArgs,
    state: Option<&WorkingMemoryState>,
    _workspace_id: &str,
    focus_files: &[String],
    focus_dirs: &[String],
) -> SaveMemoryArgs {
    let memory_class = quick.memory_class.unwrap_or(MemoryClass::Observation);
    let scope = quick.scope.unwrap_or(MemoryScopeArg::Session);
    let confidence = quick.confidence.unwrap_or(0.82);
    let linked_files = merge_strings(
        quick.linked_files,
        state
            .map(|item| {
                item.active_files
                    .iter()
                    .map(|file| file.repo_relative_path.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        focus_files.to_vec(),
    );
    let linked_symbols = merge_strings(
        quick.linked_symbols,
        state
            .map(|item| {
                item.active_symbols
                    .iter()
                    .map(|symbol| symbol.qualified_name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        Vec::new(),
    );
    let confidence_reason = quick.confidence_reason.unwrap_or_else(|| {
        if let Some(item) = state {
            if !item.active_failures.is_empty() {
                return format!(
                    "Agent-authored quick memory captured during task context with {} recent failure(s).",
                    item.active_failures.len()
                );
            }
            "Agent-authored quick memory captured from active task context.".to_string()
        } else if !focus_files.is_empty() || !focus_dirs.is_empty() {
            "Agent-authored quick memory captured with focused file/directory context."
                .to_string()
        } else {
            "Agent-authored quick memory captured for later recall.".to_string()
        }
    });
    let source_query = quick.source_query.or_else(|| {
        quick.task_statement.clone().or_else(|| {
            state.map(|item| {
                if item.active_failures.is_empty() {
                    item.task_statement.clone()
                } else {
                    let failures = item
                        .active_failures
                        .iter()
                        .take(3)
                        .map(|failure| format!("{}: {}", failure.kind, failure.message))
                        .collect::<Vec<_>>()
                        .join("; ");
                    format!("{} | recent failures: {}", item.task_statement, failures)
                }
            })
        })
    });

    SaveMemoryArgs {
        content: quick.content,
        memory_class,
        assertion_type: Some(default_assertion(memory_class)),
        scope,
        confidence,
        confidence_reason,
        freshness_policy: default_freshness(scope),
        validity_conditions: quick.validity_conditions,
        invalidation_triggers: quick.invalidation_triggers,
        provenance_event_ids: Vec::new(),
        evidence: build_failure_evidence(state),
        linked_files,
        linked_symbols,
        linked_docs: quick.linked_docs,
        linked_tests: quick.linked_tests,
        linked_memories: quick.linked_memories,
        source_query,
        refresh_key: quick.refresh_key,
        branch: quick.branch,
        organization_id: None,
    }
}

fn default_assertion(memory_class: MemoryClass) -> MemoryAssertionType {
    match memory_class {
        MemoryClass::Observation => MemoryAssertionType::Observation,
        MemoryClass::Decision => MemoryAssertionType::Decision,
        MemoryClass::Constraint => MemoryAssertionType::Constraint,
        MemoryClass::Pattern | MemoryClass::ArchitectureInvariant | MemoryClass::DocsContract => {
            MemoryAssertionType::Pattern
        }
        MemoryClass::AntiPattern => MemoryAssertionType::AntiPattern,
        MemoryClass::WorkflowOutcome | MemoryClass::FailurePattern => {
            MemoryAssertionType::WorkflowOutcome
        }
        MemoryClass::Procedure => MemoryAssertionType::Procedure,
        MemoryClass::Preference => MemoryAssertionType::Preference,
        MemoryClass::OpenQuestion => MemoryAssertionType::Question,
        MemoryClass::CounterMemory => MemoryAssertionType::Counter,
    }
}

fn default_freshness(scope: MemoryScopeArg) -> FreshnessPolicyArg {
    match scope {
        MemoryScopeArg::Session => FreshnessPolicyArg::SessionScoped,
        MemoryScopeArg::Branch => FreshnessPolicyArg::BranchScoped,
        MemoryScopeArg::Repo | MemoryScopeArg::Organization => FreshnessPolicyArg::RepoScoped,
    }
}

fn build_failure_evidence(state: Option<&WorkingMemoryState>) -> Vec<MemoryEvidence> {
    state
        .map(|item| {
            item.active_failures
                .iter()
                .take(3)
                .map(|failure| MemoryEvidence {
                    kind: failure.kind.clone(),
                    reference: failure.evidence_refs.first().cloned(),
                    detail: Some(failure.message.clone()),
                    captured_at: Some(failure.observed_at),
                    span: None,
                    evidence_content_hash: None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn merge_strings(
    primary: Vec<String>,
    secondary: Vec<String>,
    tertiary: Vec<String>,
) -> Vec<String> {
    let mut merged = Vec::new();
    for value in primary.into_iter().chain(secondary).chain(tertiary) {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !merged.iter().any(|existing: &String| existing == trimmed) {
            merged.push(trimmed.to_string());
        }
    }
    merged
}
