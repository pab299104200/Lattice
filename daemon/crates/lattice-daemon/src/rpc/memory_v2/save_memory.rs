use super::{coarse_memory_type, default_assertion_type, git_provenance, MemoryRecord};
use crate::rpc::memory_v2::get_task_memory::build_bundle;
use lattice_core::memory::model::{MemoryAssertionType, MemoryFreshnessPolicy, MemoryProvenance};
use lattice_core::memory::{
    Memory, MemoryClass, MemoryScope, MemoryStore, MemoryStructuredFields, MemoryVerificationStatus,
};
use lattice_core::working_memory::WorkingMemoryState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Arguments for the `save_memory` tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SaveMemoryArgs {
    /// Memory content to persist.
    pub content: String,
    /// Spec-level memory class for the durable claim.
    pub memory_class: MemoryClass,
    /// Assertion type for the claim.
    #[serde(default)]
    pub assertion_type: Option<MemoryAssertionType>,
    /// Durability scope for the claim.
    pub scope: MemoryScopeArg,
    /// Confidence score between 0.0 and 1.0.
    pub confidence: f64,
    /// Reason explaining the confidence level.
    pub confidence_reason: String,
    /// Freshness policy for revalidation.
    pub freshness_policy: FreshnessPolicyArg,
    /// Validity conditions that must hold for the claim.
    #[serde(default)]
    pub validity_conditions: Vec<String>,
    /// Triggers that should invalidate the claim.
    #[serde(default)]
    pub invalidation_triggers: Vec<String>,
    /// Event ids that support the claim's provenance.
    #[serde(default)]
    pub provenance_event_ids: Vec<String>,
    /// Evidence references to persist with the memory.
    #[serde(default)]
    pub evidence: Vec<lattice_core::memory::MemoryEvidence>,
    /// Linked files.
    #[serde(default)]
    pub linked_files: Vec<String>,
    /// Linked symbols.
    #[serde(default)]
    pub linked_symbols: Vec<String>,
    /// Linked docs.
    #[serde(default)]
    pub linked_docs: Vec<String>,
    /// Linked tests.
    #[serde(default)]
    pub linked_tests: Vec<String>,
    /// Linked memory ids.
    #[serde(default)]
    pub linked_memories: Vec<String>,
    /// Optional source query or note.
    #[serde(default)]
    pub source_query: Option<String>,
    /// Optional refresh key.
    #[serde(default)]
    pub refresh_key: Option<String>,
    /// Optional branch override for branch-scoped memory.
    #[serde(default)]
    pub branch: Option<String>,
    /// Optional organization id for organization-scoped memory.
    #[serde(default)]
    pub organization_id: Option<String>,
}

/// Scope values accepted by `save_memory`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScopeArg {
    Session,
    Branch,
    Repo,
    Organization,
}

/// Freshness policy wire enum for `save_memory`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessPolicyArg {
    SessionScoped,
    BranchScoped,
    RepoScoped,
    TimeBound,
    ManualReview,
}

/// Response for `save_memory`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SaveMemoryResponse {
    /// Newly created memory id.
    pub memory_id: String,
    /// Fully annotated memory record.
    pub memory: MemoryRecord,
    /// Verification job id queued for the saved memory.
    pub verification_job_id: String,
}

pub fn tool_definition() -> Value {
    json!({
        "name": "save_memory",
        "description": "Create a durable memory record with evidence, validity conditions, invalidation triggers, linked artifacts, default unverified status, and a queued verification job.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "content": {"type": "string"},
                "memory_class": {
                    "type": "string",
                    "enum": [
                        "observation", "decision", "constraint", "pattern", "anti_pattern",
                        "workflow_outcome", "failure_pattern", "procedure", "preference",
                        "architecture_invariant", "docs_contract", "open_question", "counter_memory"
                    ]
                },
                "assertion_type": {
                    "type": "string",
                    "enum": [
                        "observation", "decision", "exploration", "pattern", "anti_pattern",
                        "workflow_outcome", "constraint", "hypothesis", "procedure",
                        "outcome", "preference", "question", "counter"
                    ]
                },
                "scope": {
                    "type": "string",
                    "enum": ["session", "branch", "repo", "organization"]
                },
                "confidence": {"type": "number"},
                "confidence_reason": {"type": "string"},
                "freshness_policy": {
                    "type": "string",
                    "enum": ["session_scoped", "branch_scoped", "repo_scoped", "time_bound", "manual_review"]
                },
                "validity_conditions": {"type": "array", "items": {"type": "string"}},
                "invalidation_triggers": {"type": "array", "items": {"type": "string"}},
                "provenance_event_ids": {"type": "array", "items": {"type": "string"}},
                "evidence": {"type": "array", "items": {"type": "object"}},
                "linked_files": {"type": "array", "items": {"type": "string"}},
                "linked_symbols": {"type": "array", "items": {"type": "string"}},
                "linked_docs": {"type": "array", "items": {"type": "string"}},
                "linked_tests": {"type": "array", "items": {"type": "string"}},
                "linked_memories": {"type": "array", "items": {"type": "string"}},
                "source_query": {"type": "string"},
                "refresh_key": {"type": "string"},
                "branch": {"type": "string"},
                "organization_id": {"type": "string"}
            },
            "required": ["content", "memory_class", "scope", "confidence", "confidence_reason", "freshness_policy"]
        }
    })
}

pub fn parse_args(args: &Value) -> Result<SaveMemoryArgs, String> {
    serde_json::from_value(args.clone())
        .map_err(|error| format!("Invalid save_memory arguments: {error}"))
}

pub fn validate_args(args: &SaveMemoryArgs) -> Result<(), String> {
    if args.content.trim().is_empty() {
        return Err("save_memory requires non-empty content".to_string());
    }
    if !(0.0..=1.0).contains(&args.confidence) {
        return Err("save_memory confidence must be between 0.0 and 1.0".to_string());
    }
    if args.confidence_reason.trim().is_empty() {
        return Err("save_memory requires a non-empty confidence_reason".to_string());
    }
    match args.scope {
        MemoryScopeArg::Branch if args.branch.as_deref().unwrap_or("").trim().is_empty() => {
            Err("branch-scoped memory requires a branch".to_string())
        }
        MemoryScopeArg::Organization
            if args
                .organization_id
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty() =>
        {
            Err("organization-scoped memory requires organization_id".to_string())
        }
        _ => Ok(()),
    }
}

pub fn memory_scope(arg: MemoryScopeArg) -> MemoryScope {
    match arg {
        MemoryScopeArg::Session => MemoryScope::Session,
        MemoryScopeArg::Branch => MemoryScope::Branch,
        MemoryScopeArg::Repo => MemoryScope::Repo,
        MemoryScopeArg::Organization => MemoryScope::Organization,
    }
}

pub fn freshness_policy(arg: FreshnessPolicyArg) -> MemoryFreshnessPolicy {
    match arg {
        FreshnessPolicyArg::SessionScoped => MemoryFreshnessPolicy::SessionScoped,
        FreshnessPolicyArg::BranchScoped => MemoryFreshnessPolicy::BranchScoped,
        FreshnessPolicyArg::RepoScoped => MemoryFreshnessPolicy::RepoScoped,
        FreshnessPolicyArg::TimeBound => MemoryFreshnessPolicy::TimeBound,
        FreshnessPolicyArg::ManualReview => MemoryFreshnessPolicy::ManualReview,
    }
}

pub fn build_memory(
    session_id: &str,
    workspace_id: &str,
    args: &SaveMemoryArgs,
) -> (Memory, MemoryStructuredFields) {
    let scope = memory_scope(args.scope);
    let assertion_type = args
        .assertion_type
        .unwrap_or_else(|| default_assertion_type(&args.memory_class));
    let memory = Memory {
        id: String::new(),
        session_id: session_id.to_string(),
        content: args.content.clone(),
        memory_type: coarse_memory_type(&args.memory_class),
        scope,
        confidence: args.confidence,
        linked_symbols: args.linked_symbols.clone(),
        linked_files: args.linked_files.clone(),
        workspace_id: Some(workspace_id.to_string()),
        branch: args.branch.clone(),
        scope_organization_id: args.organization_id.clone(),
        refresh_key: args.refresh_key.clone(),
        source_query: args.source_query.clone(),
        created_at: 0,
        last_accessed: 0,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: MemoryVerificationStatus::Unverified,
    };
    let structured = MemoryStructuredFields {
        memory_class: args.memory_class.clone(),
        assertion_type,
        verification_status: MemoryVerificationStatus::Unverified,
        confidence_reason: Some(args.confidence_reason.clone()),
        freshness_policy: freshness_policy(args.freshness_policy),
        freshness_policy_detail: None,
        validity_conditions: args.validity_conditions.clone(),
        invalidation_triggers: args.invalidation_triggers.clone(),
        provenance: build_provenance(args, workspace_id),
        evidence: args.evidence.clone(),
        linked_docs: args.linked_docs.clone(),
        linked_tests: args.linked_tests.clone(),
        linked_memories: args.linked_memories.clone(),
        ..MemoryStructuredFields::default()
    };
    (memory, structured)
}

fn build_provenance(args: &SaveMemoryArgs, workspace_id: &str) -> Vec<MemoryProvenance> {
    let mut provenance: Vec<MemoryProvenance> = args
        .provenance_event_ids
        .iter()
        .map(|event_id| MemoryProvenance {
            source: "event".to_string(),
            reference: Some(event_id.clone()),
            captured_at: None,
            note: Some("provenance_event".to_string()),
        })
        .collect();
    if let Some(source_query) = args.source_query.as_ref() {
        provenance.push(MemoryProvenance {
            source: "source_query".to_string(),
            reference: Some(source_query.clone()),
            captured_at: None,
            note: None,
        });
    }
    provenance.extend(git_provenance(workspace_id));
    provenance
}

pub fn build_response(
    store: &MemoryStore,
    workspace_id: &str,
    memory_id: &str,
    verification_job_id: String,
) -> Result<SaveMemoryResponse, String> {
    let memory = store
        .get_by_id(memory_id)
        .map_err(|error| format!("Failed to load saved memory: {error}"))?
        .ok_or_else(|| format!("Saved memory `{memory_id}` was not readable"))?;
    let bundle = build_bundle(
        store,
        workspace_id,
        "save_memory".to_string(),
        None,
        &WorkingMemoryState::default(),
        vec![(memory, "saved by save_memory".to_string(), 0)],
    )?;
    Ok(SaveMemoryResponse {
        memory_id: memory_id.to_string(),
        memory: bundle
            .memories
            .into_iter()
            .next()
            .ok_or_else(|| "Missing saved memory record".to_string())?,
        verification_job_id,
    })
}
