use super::EvolutionAction;
use lattice_core::consolidation::{ConsolidationProposal, ProposalKind, ProposalTarget};
use lattice_core::memory::{Memory, MemoryStore, MemoryStructuredFields, MemoryVerificationStatus};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Arguments for the unified `propose_memory_evolution` tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposeMemoryEvolutionArgs {
    /// Whether to propose, apply, or reject a memory evolution.
    pub action: EvolutionAction,
    /// Existing proposal id for apply/reject.
    #[serde(default)]
    pub proposal_id: Option<String>,
    /// Source memory id for a new proposal.
    #[serde(default)]
    pub memory_id: Option<String>,
    /// Proposed content replacement.
    #[serde(default)]
    pub content: Option<String>,
    /// Linked files to append or replace in the proposed state.
    #[serde(default)]
    pub linked_files: Vec<String>,
    /// Linked symbols to append or replace in the proposed state.
    #[serde(default)]
    pub linked_symbols: Vec<String>,
    /// Linked docs to append or replace in the proposed state.
    #[serde(default)]
    pub linked_docs: Vec<String>,
    /// Linked tests to append or replace in the proposed state.
    #[serde(default)]
    pub linked_tests: Vec<String>,
    /// Linked memories to append or replace in the proposed state.
    #[serde(default)]
    pub linked_memories: Vec<String>,
    /// Validity conditions to persist in the proposed state.
    #[serde(default)]
    pub validity_conditions: Vec<String>,
    /// Invalidation triggers to persist in the proposed state.
    #[serde(default)]
    pub invalidation_triggers: Vec<String>,
    /// Optional target memory id that supersedes the source memory.
    #[serde(default)]
    pub superseded_by_memory_id: Option<String>,
    /// Optional invalidation reason.
    #[serde(default)]
    pub invalidate_reason: Option<String>,
    /// Optional decision reason or proposal summary.
    #[serde(default)]
    pub reason: Option<String>,
    /// Optional operator/assistant identifier recorded on apply/reject.
    #[serde(default)]
    pub decided_by: Option<String>,
}

pub fn tool_definition() -> Value {
    json!({
        "name": "propose_memory_evolution",
        "description": "Persist, apply, or reject auditable memory evolution proposals with provenance-preserving transactional updates.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["propose", "apply", "reject"]},
                "proposal_id": {"type": "string"},
                "memory_id": {"type": "string"},
                "content": {"type": "string"},
                "linked_files": {"type": "array", "items": {"type": "string"}},
                "linked_symbols": {"type": "array", "items": {"type": "string"}},
                "linked_docs": {"type": "array", "items": {"type": "string"}},
                "linked_tests": {"type": "array", "items": {"type": "string"}},
                "linked_memories": {"type": "array", "items": {"type": "string"}},
                "validity_conditions": {"type": "array", "items": {"type": "string"}},
                "invalidation_triggers": {"type": "array", "items": {"type": "string"}},
                "superseded_by_memory_id": {"type": "string"},
                "invalidate_reason": {"type": "string"},
                "reason": {"type": "string"},
                "decided_by": {"type": "string"}
            },
            "required": ["action"]
        }
    })
}

pub fn parse_args(args: &Value) -> Result<ProposeMemoryEvolutionArgs, String> {
    serde_json::from_value(args.clone())
        .map_err(|error| format!("Invalid propose_memory_evolution arguments: {error}"))
}

pub fn validate_args(args: &ProposeMemoryEvolutionArgs) -> Result<(), String> {
    match args.action {
        EvolutionAction::Propose => {
            if args.memory_id.as_deref().unwrap_or("").trim().is_empty() {
                return Err(
                    "propose_memory_evolution(action=propose) requires memory_id".to_string(),
                );
            }
        }
        EvolutionAction::Apply | EvolutionAction::Reject => {
            if args.proposal_id.as_deref().unwrap_or("").trim().is_empty() {
                return Err(format!(
                    "propose_memory_evolution(action={}) requires proposal_id",
                    match args.action {
                        EvolutionAction::Apply => "apply",
                        EvolutionAction::Reject => "reject",
                        EvolutionAction::Propose => "propose",
                    }
                ));
            }
        }
    }
    Ok(())
}

pub fn proposal_kind(args: &ProposeMemoryEvolutionArgs) -> ProposalKind {
    if args.invalidate_reason.is_some() {
        ProposalKind::MarkInvalidated
    } else if args.superseded_by_memory_id.is_some() {
        ProposalKind::Supersede
    } else {
        ProposalKind::UpdateMemory
    }
}

pub fn build_proposal(
    _workspace_id: &str,
    store: &MemoryStore,
    args: &ProposeMemoryEvolutionArgs,
) -> Result<ConsolidationProposal, String> {
    let memory_id = args
        .memory_id
        .as_deref()
        .ok_or_else(|| "missing memory_id".to_string())?;
    let memory = store
        .get_by_id(memory_id)
        .map_err(|error| format!("Failed to load source memory: {error}"))?
        .ok_or_else(|| format!("Memory `{memory_id}` was not found"))?;
    let prior_fields = store
        .get_structured_fields(memory_id)
        .map_err(|error| format!("Failed to load source structured fields: {error}"))?
        .unwrap_or_default();
    let prior_state = capture_state_json(store, &memory, &prior_fields)?;
    let next = apply_delta(memory.clone(), prior_fields, args);
    let proposed_state = capture_state_json(store, &next.0, &next.1)?;
    Ok(ConsolidationProposal {
        proposal_id: format!("memory-evolution-{}-{}", memory_id, now_unix_micros()),
        job_id: format!("memory-evolution-job-{}-{}", memory_id, now_unix_micros()),
        target: ProposalTarget::ExistingMemory(memory_id.to_string()),
        proposal_kind: proposal_kind(args),
        prior_state,
        proposed_state,
        evidence: json!({
            "source_memory_ids": [memory_id],
            "reason": args.reason,
            "invalidate_reason": args.invalidate_reason,
            "superseded_by_memory_id": args.superseded_by_memory_id,
        }),
        provenance: None,
    })
}

fn apply_delta(
    mut memory: Memory,
    mut fields: MemoryStructuredFields,
    args: &ProposeMemoryEvolutionArgs,
) -> (Memory, MemoryStructuredFields) {
    if let Some(content) = args.content.as_ref() {
        memory.content = content.clone();
    }
    if !args.linked_files.is_empty() {
        memory.linked_files = args.linked_files.clone();
    }
    if !args.linked_symbols.is_empty() {
        memory.linked_symbols = args.linked_symbols.clone();
    }
    if !args.linked_docs.is_empty() {
        fields.linked_docs = args.linked_docs.clone();
    }
    if !args.linked_tests.is_empty() {
        fields.linked_tests = args.linked_tests.clone();
    }
    if !args.linked_memories.is_empty() {
        fields.linked_memories = args.linked_memories.clone();
    }
    if !args.validity_conditions.is_empty() {
        fields.validity_conditions = args.validity_conditions.clone();
    }
    if !args.invalidation_triggers.is_empty() {
        fields.invalidation_triggers = args.invalidation_triggers.clone();
    }
    if let Some(superseded_by) = args.superseded_by_memory_id.as_ref() {
        fields.superseded_by_memory_id = Some(superseded_by.clone());
        fields.verification_status = MemoryVerificationStatus::Superseded;
    }
    if let Some(reason) = args.invalidate_reason.as_ref() {
        memory.is_stale = false;
        memory.stale_reason = Some(reason.clone());
        fields.verification_status = MemoryVerificationStatus::Invalidated;
    }
    (memory, fields)
}

fn capture_state_json(
    store: &MemoryStore,
    memory: &Memory,
    fields: &MemoryStructuredFields,
) -> Result<Value, String> {
    let links = store
        .list_memory_links_from(&memory.id)
        .map_err(|error| format!("Failed to load memory links: {error}"))?;
    Ok(json!({
        "memory": memory,
        "structured_fields": fields,
        "memory_links": links,
        "last_verified_at": store.get_last_verified_at(&memory.id).map_err(|error| format!("Failed to load verification timestamp: {error}"))?,
        "last_verified_graph_snapshot_id": store.get_last_verified_graph_snapshot_id(&memory.id).map_err(|error| format!("Failed to load verification graph snapshot id: {error}"))?,
        "expires_at": store.get_expires_at(&memory.id).map_err(|error| format!("Failed to load expiry timestamp: {error}"))?,
    }))
}

fn now_unix_micros() -> i64 {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}
