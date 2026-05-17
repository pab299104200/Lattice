//! Memory V2 MCP surface for task-scoped retrieval, durable writes, and
//! auditable memory evolution.
//!
//! This module implements the task's `### 9. MCP Surface` and
//! `### 4. Memory Graph` contracts from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`.
//!
//! "Tool surface discipline: ten new memory tools is a meaningful cognitive
//! load for clients. Before shipping Phase 8, audit whether
//! `propose_memory_evolution` + `apply_memory_evolution` can collapse into a
//! single tool with an `action` parameter, and whether `verify_memory` +
//! `explain_memory` can be unified. The goal is the smallest surface that
//! covers all assistant workflows. Consolidate before stabilizing the MCP
//! contract."

use lattice_core::identity::{encode_identity, Identity, MemoryId};
use lattice_core::memory::model::{MemoryAssertionType, MemoryFreshnessPolicy, MemoryProvenance};
use lattice_core::memory::{
    Memory, MemoryAccessRecord, MemoryClass, MemoryEvidence, MemoryLinkRecord, MemoryScoreRecord,
    MemoryStructuredFields, MemoryType, MemoryVerificationStatus,
};
use serde::{Deserialize, Serialize};

pub mod consolidate_session;
pub mod get_event_trace;
pub mod get_memory_metrics;
pub mod get_task_memory;
pub mod list_memory_conflicts;
pub mod propose_memory_evolution;
pub mod save_memory;
pub mod verify_explain_memory;

#[cfg(test)]
mod admin_tools_tests;
#[cfg(test)]
mod memory_tools_tests;
#[cfg(test)]
mod verify_explain_tests;

/// The compact memory bundle returned for a task-scoped retrieval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskMemoryBundle {
    /// The task whose active and durable memory context was retrieved.
    pub task_id: String,
    /// The working-memory checkpoint used as the active context source, when present.
    pub checkpoint_id: Option<i64>,
    /// The current verification status of the task's working-memory state.
    pub working_memory_verification_status: String,
    /// The compact memory records returned for the task.
    pub memories: Vec<MemoryRecord>,
}

/// The fully annotated memory record returned by Memory V2 tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// Stable memory id.
    pub id: String,
    /// Encoded expansion handle for the memory identity.
    pub expansion_handle: String,
    /// Human-readable memory content.
    pub content: String,
    /// Spec-level memory class.
    pub memory_class: MemoryClass,
    /// Assertion type recorded for the claim.
    pub assertion_type: MemoryAssertionType,
    /// Durability scope for visibility.
    pub scope: String,
    /// Stored confidence score.
    pub confidence: f64,
    /// Optional explanation for the confidence score.
    pub confidence_reason: Option<String>,
    /// Current verification status.
    pub verification_status: String,
    /// Current freshness status for the memory surface.
    pub freshness_status: String,
    /// Contradiction state summary.
    pub contradiction_state: String,
    /// Supersession state summary.
    pub supersession_state: String,
    /// Why the memory was included in the response.
    pub inclusion_reason: String,
    /// Approximate evidence strength for the claim.
    pub evidence_strength: f64,
    /// Files linked to the memory.
    pub linked_files: Vec<String>,
    /// Symbols linked to the memory.
    pub linked_symbols: Vec<String>,
    /// Docs linked to the memory.
    pub linked_docs: Vec<String>,
    /// Tests linked to the memory.
    pub linked_tests: Vec<String>,
    /// Memory ids linked from the memory.
    pub linked_memories: Vec<String>,
    /// Validity conditions for the claim.
    pub validity_conditions: Vec<String>,
    /// Invalidation triggers for the claim.
    pub invalidation_triggers: Vec<String>,
    /// Structured provenance entries.
    pub provenance: Vec<MemoryProvenance>,
    /// Structured evidence entries.
    pub evidence: Vec<MemoryEvidence>,
    /// Memory-to-memory links.
    pub links: Vec<MemoryLinkRecord>,
    /// Access history rows for the memory.
    pub access_history: Vec<MemoryAccessRecord>,
    /// Usefulness scores for the memory.
    pub usefulness_scores: Vec<MemoryScoreRecord>,
    /// Optional source query captured with the memory.
    pub source_query: Option<String>,
    /// Optional branch for branch-scoped memory.
    pub branch: Option<String>,
    /// Optional refresh key.
    pub refresh_key: Option<String>,
    /// Optional last verification timestamp.
    pub last_verified_at: Option<u64>,
    /// Optional verification graph snapshot id.
    pub last_verified_graph_snapshot_id: Option<u64>,
}

/// The action accepted by the unified memory evolution tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionAction {
    /// Persist a proposal without mutating durable memory.
    Propose,
    /// Apply a previously staged proposal.
    Apply,
    /// Reject a previously staged proposal.
    Reject,
}

/// The auditable proposal returned by the unified memory evolution tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvolutionProposal {
    /// Proposal id persisted in the consolidation proposal store.
    pub proposal_id: String,
    /// Action executed by the tool.
    pub action: EvolutionAction,
    /// Source memory targeted by the proposal, when any.
    pub source_memory_id: Option<String>,
    /// Consolidation proposal kind inferred for the delta.
    pub proposal_kind: String,
    /// Current proposal decision state.
    pub decision: String,
    /// Prior state snapshot for auditable evolution.
    pub prior_state: serde_json::Value,
    /// Proposed state snapshot or applied state snapshot.
    pub proposed_state: serde_json::Value,
    /// Optional deprecation warning attached by shimmed calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecation_warning: Option<String>,
}

pub(crate) fn coarse_memory_type(memory_class: &MemoryClass) -> MemoryType {
    match memory_class {
        MemoryClass::Decision => MemoryType::Decision,
        MemoryClass::Pattern
        | MemoryClass::WorkflowOutcome
        | MemoryClass::FailurePattern
        | MemoryClass::Procedure
        | MemoryClass::Preference
        | MemoryClass::ArchitectureInvariant
        | MemoryClass::DocsContract => MemoryType::Pattern,
        MemoryClass::AntiPattern | MemoryClass::CounterMemory => MemoryType::AntiPattern,
        MemoryClass::Observation | MemoryClass::Constraint => MemoryType::Observation,
        MemoryClass::OpenQuestion => MemoryType::Exploration,
    }
}

pub(crate) fn default_assertion_type(memory_class: &MemoryClass) -> MemoryAssertionType {
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

pub(crate) fn expansion_handle(memory: &Memory, workspace_id: &str) -> String {
    encode_identity(&Identity::Memory(MemoryId {
        workspace_id: memory
            .workspace_id
            .clone()
            .unwrap_or_else(|| workspace_id.to_string()),
        ulid: memory.id.clone(),
    }))
}

pub(crate) fn freshness_status(
    memory: &Memory,
    fields: &MemoryStructuredFields,
    expires_at: Option<lattice_core::DateTime<lattice_core::Utc>>,
) -> String {
    if matches!(
        fields.verification_status,
        MemoryVerificationStatus::Expired
    ) {
        return "expired".to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    if expires_at.is_some_and(|value| value.unix_seconds() <= now) {
        return "expired".to_string();
    }
    if memory.is_stale || matches!(fields.verification_status, MemoryVerificationStatus::Stale) {
        return "stale".to_string();
    }
    match fields.freshness_policy {
        MemoryFreshnessPolicy::ManualReview => "manual_review".to_string(),
        MemoryFreshnessPolicy::TimeBound => "time_bound".to_string(),
        MemoryFreshnessPolicy::BranchScoped => "branch_scoped".to_string(),
        MemoryFreshnessPolicy::RepoScoped => "repo_scoped".to_string(),
        MemoryFreshnessPolicy::SessionScoped => "session_scoped".to_string(),
    }
}

pub(crate) fn contradiction_state(fields: &MemoryStructuredFields) -> String {
    if !fields.contradicted_by_memory_ids.is_empty() {
        "contradicted_by_other_memory".to_string()
    } else if !fields.contradicts_memory_ids.is_empty() {
        "contradicts_other_memory".to_string()
    } else {
        "none".to_string()
    }
}

pub(crate) fn supersession_state(fields: &MemoryStructuredFields) -> String {
    if fields.superseded_by_memory_id.is_some() {
        "superseded".to_string()
    } else if fields.supersedes_memory_id.is_some() {
        "supersedes_other_memory".to_string()
    } else {
        "none".to_string()
    }
}

pub(crate) fn evidence_strength(
    fields: &MemoryStructuredFields,
    scores: &[MemoryScoreRecord],
    access_count: u32,
) -> f64 {
    let evidence_weight = (fields.evidence.len() as f64 * 0.12).min(0.6);
    let provenance_weight = (fields.provenance.len() as f64 * 0.08).min(0.24);
    let score_weight = scores
        .iter()
        .map(|score| score.value.max(0.0) as f64)
        .fold(0.0_f64, f64::max)
        .min(1.0)
        * 0.12;
    let access_weight = (f64::from(access_count.min(8)) / 8.0) * 0.04;
    (0.1 + evidence_weight + provenance_weight + score_weight + access_weight).min(1.0)
}
