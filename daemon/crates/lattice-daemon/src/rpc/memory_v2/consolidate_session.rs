//! Manual session-consolidation trigger for the Memory V2 MCP surface.
//!
//! This module implements the plan contract headings `## MCP Surface` and
//! `## Consolidation Engine` from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`.
//! It preserves the requirement that consolidation emits proposals only:
//! no consolidation job may silently rewrite memory without auditable prior
//! and proposed state.

use lattice_core::consolidation::{
    episode_memory_states, ConsolidationJobMode, ConsolidationProposal, EpisodeOutcome,
    EpisodeTemplate, ProposalKind, ProposalTarget,
};
use lattice_core::events::{EventEnvelope, StableRef};
use lattice_core::memory::{Memory, MemoryScope, MemoryStore, MemoryType};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Arguments for the `consolidate_session` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidateSessionArgs {
    /// Session id whose task traces should be consolidated.
    pub session_id: String,
    /// Consolidation execution mode.
    #[serde(default)]
    pub mode: Option<ConsolidationMode>,
    /// Optional synchronous budget hint in milliseconds.
    #[serde(default)]
    pub budget_ms: Option<u64>,
    /// Response verbosity for the report payload.
    #[serde(default)]
    pub render_mode: Option<ConsolidationRenderMode>,
}

/// Consolidation modes accepted by the tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidationMode {
    PostTask,
    Background,
    ManualReview,
    Replay,
}

impl ConsolidationMode {
    /// Canonical wire name for the mode.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PostTask => "post_task",
            Self::Background => "background",
            Self::ManualReview => "manual_review",
            Self::Replay => "replay",
        }
    }

    pub fn job_mode(self) -> ConsolidationJobMode {
        match self {
            Self::PostTask => ConsolidationJobMode::SynchronousPostTask,
            Self::Background => ConsolidationJobMode::Background,
            Self::ManualReview => ConsolidationJobMode::ManualReview,
            Self::Replay => ConsolidationJobMode::Replay,
        }
    }
}

impl Default for ConsolidationMode {
    fn default() -> Self {
        Self::PostTask
    }
}

/// Render mode accepted by the tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidationRenderMode {
    Compact,
    Full,
    Diagnostic,
}

impl Default for ConsolidationRenderMode {
    fn default() -> Self {
        Self::Compact
    }
}

/// One proposal returned by the consolidation report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsolidationProposalItem {
    /// Auditable proposal id.
    pub proposal_id: String,
    /// Consolidation job id that created the proposal.
    pub job_id: String,
    /// Consolidation proposal kind.
    pub proposal_kind: String,
    /// Session task id summarized by this proposal.
    pub task_id: String,
    /// Human-readable category name.
    pub category: String,
    /// Why this proposal was emitted.
    pub summary: String,
    /// Existing target memory id when the proposal mutates an existing memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_memory_id: Option<String>,
    /// Queue timestamp for stable sort order in review surfaces.
    pub enqueued_at: i64,
    /// Current proposal decision status.
    pub decision: String,
    /// Human-readable proposed memory class for review tables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_class: Option<String>,
    /// Prior memory scope when the proposal updates an existing memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_scope: Option<String>,
    /// Resulting memory scope if the proposal is applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_scope: Option<String>,
    /// Proposed memory confidence score when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Count of evidence references attached to the proposal.
    pub evidence_count: usize,
    /// Auditable prior state preview for the review dialog.
    pub prior_state: Value,
    /// Auditable after-state preview for the review dialog.
    pub proposed_state: Value,
    /// Structured evidence payload surfaced to operators.
    pub evidence: Value,
    /// Structured LLM provenance when the proposal was model-generated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Value>,
}

/// Category-level report for considered proposal families.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidationCategoryReport {
    /// Category name from the consolidation contract.
    pub category: String,
    /// Proposal ids emitted for this category.
    pub proposal_ids: Vec<String>,
    /// Explicit explanation when the category emitted no proposals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Tool response for `consolidate_session`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsolidationReport {
    /// Session id that was consolidated.
    pub session_id: String,
    /// Mode used to generate the report.
    pub mode: ConsolidationMode,
    /// Render mode used for the payload.
    pub render_mode: ConsolidationRenderMode,
    /// Optional synchronous budget hint echoed from the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_ms: Option<u64>,
    /// Proposals produced by the run.
    pub proposals: Vec<ConsolidationProposalItem>,
    /// Category-by-category rollup.
    pub categories: Vec<ConsolidationCategoryReport>,
    /// Whether the report omitted any higher-cost consolidation families.
    pub incomplete: bool,
    /// Notes about omissions or bounded behavior.
    pub notes: Vec<String>,
}

pub fn tool_definition() -> Value {
    json!({
        "name": "consolidate_session",
        "description": "Trigger proposal-only session consolidation and return auditable proposal ids for episode summaries and related consolidation categories.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "session_id": {"type": "string"},
                "mode": {"type": "string", "enum": ["post_task", "background", "manual_review", "replay"], "default": "post_task"},
                "budget_ms": {"type": "integer"},
                "render_mode": {"type": "string", "enum": ["compact", "full", "diagnostic"], "default": "compact"}
            },
            "required": ["session_id"]
        }
    })
}

pub fn parse_args(args: &Value) -> Result<ConsolidateSessionArgs, String> {
    serde_json::from_value(args.clone())
        .map_err(|error| format!("Invalid consolidate_session arguments: {error}"))
}

pub fn validate_args(args: &ConsolidateSessionArgs) -> Result<(), String> {
    if args.session_id.trim().is_empty() {
        return Err("consolidate_session requires a non-empty session_id".to_string());
    }
    Ok(())
}

pub fn group_task_slices(events: &[EventEnvelope]) -> Vec<Vec<EventEnvelope>> {
    let mut order = Vec::new();
    let mut groups: HashMap<String, Vec<EventEnvelope>> = HashMap::new();
    for event in events {
        let Some(task_id) = event.task_id.as_ref().map(|value| value.value.clone()) else {
            continue;
        };
        if !groups.contains_key(&task_id) {
            order.push(task_id.clone());
        }
        groups.entry(task_id).or_default().push(event.clone());
    }
    order
        .into_iter()
        .filter_map(|task_id| groups.remove(&task_id))
        .collect()
}

pub fn build_episode_proposal(
    workspace_id: &str,
    memory_store: &MemoryStore,
    template: &EpisodeTemplate,
    mode: ConsolidationMode,
) -> Result<ConsolidationProposal, String> {
    let refresh_key = episode_refresh_key(template);
    let existing = memory_store
        .find_by_refresh_key(&refresh_key, Some(workspace_id), None)
        .map_err(|error| format!("Failed to query episode memory by refresh key: {error}"))?;
    let proposal_kind = if existing.is_some() {
        ProposalKind::UpdateMemory
    } else {
        ProposalKind::CreateMemory
    };
    let proposed_memory =
        build_episode_memory(existing.as_ref(), template, &refresh_key, workspace_id);
    let (prior_state, proposed_state) =
        episode_memory_states(memory_store, existing.as_ref(), proposed_memory)
            .map_err(|error| format!("Failed to capture canonical episode state: {error}"))?;
    Ok(ConsolidationProposal {
        proposal_id: format!(
            "episode-proposal-{}-{}",
            lattice_core::consolidation::session::episode_task_key(&template.task_id.value),
            template.event_window.end.ulid
        ),
        job_id: format!(
            "session-consolidation-{}-{}-{}",
            mode.as_str(),
            lattice_core::consolidation::session::episode_task_key(&template.task_id.value),
            template.event_window.end.ulid
        ),
        target: existing
            .as_ref()
            .map(|memory| ProposalTarget::ExistingMemory(memory.id.clone()))
            .unwrap_or(ProposalTarget::NewMemory),
        proposal_kind,
        prior_state,
        proposed_state,
        evidence: episode_evidence(template),
        provenance: None,
    })
}

fn build_episode_memory(
    existing: Option<&Memory>,
    template: &EpisodeTemplate,
    refresh_key: &str,
    workspace_id: &str,
) -> Memory {
    Memory {
        id: existing.map(|memory| memory.id.clone()).unwrap_or_else(|| {
            format!(
                "episode-{}-{}",
                lattice_core::consolidation::session::episode_task_key(&template.task_id.value),
                template.event_window.end.ulid
            )
        }),
        session_id: template.session_id.value.clone(),
        content: template.summary_text.clone(),
        memory_type: MemoryType::Pattern,
        scope: MemoryScope::Session,
        confidence: match template.outcome {
            EpisodeOutcome::Success => 0.92,
            EpisodeOutcome::Failure => 0.72,
            EpisodeOutcome::Abandoned => 0.55,
        },
        linked_symbols: template
            .salient_anchors
            .iter()
            .filter_map(|reference| match reference {
                StableRef::SymbolRef(symbol) => Some(symbol.to_string()),
                StableRef::DocSectionRef(section) => Some(section.to_string()),
                _ => None,
            })
            .collect(),
        linked_files: template
            .salient_anchors
            .iter()
            .filter_map(|reference| match reference {
                StableRef::FileRef(file) => Some(file.repo_relative_path.clone()),
                _ => None,
            })
            .collect(),
        workspace_id: Some(workspace_id.to_string()),
        branch: None,
        scope_organization_id: None,
        refresh_key: Some(refresh_key.to_string()),
        source_query: Some(format!(
            "episode_summary:{}:{}",
            template.session_id.value, template.task_id.value
        )),
        created_at: existing.map(|memory| memory.created_at).unwrap_or_default(),
        last_accessed: existing
            .map(|memory| memory.last_accessed)
            .unwrap_or_default(),
        access_count: existing
            .map(|memory| memory.access_count)
            .unwrap_or_default(),
        is_stale: false,
        stale_reason: None,
        verification_status: lattice_core::memory::MemoryVerificationStatus::Unverified,
    }
}

fn episode_evidence(template: &EpisodeTemplate) -> Value {
    json!({
        "task_id": template.task_id.value,
        "session_id": template.session_id.value,
        "outcome": template.outcome.as_str(),
        "event_window": {
            "start": &template.event_window.start,
            "end": &template.event_window.end,
        },
        "salient_anchors": &template.salient_anchors,
        "tools_used": &template.tools_used,
        "source_event_ids": [
            &template.event_window.start,
            &template.event_window.end,
        ],
        "category": "episode_summary",
    })
}

fn episode_refresh_key(template: &EpisodeTemplate) -> String {
    format!(
        "workflow_episode::{}::{}",
        template.session_id.value, template.task_id.value
    )
}

#[cfg(test)]
#[test]
fn episode_task_keys_round_trip_long_and_distinct_inputs_as_memory_identities() {
    use lattice_core::identity::{decode_identity, encode_identity, Identity, MemoryId};
    let inputs = [
        "a".to_string(),
        "A".to_string(),
        "a/b".to_string(),
        "a-b".to_string(),
        "重复🙂".repeat(10_000),
        "x".repeat(10_000),
        format!("{}y", "x".repeat(10_000)),
    ];
    let mut ids = std::collections::HashSet::new();
    for input in inputs {
        let key = lattice_core::consolidation::session::episode_task_key(&input);
        let local_id = format!("episode-{key}-01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert!(local_id.len() <= 128);
        assert!(
            ids.insert(local_id.clone()),
            "distinct task inputs must stay distinct"
        );
        let identity = Identity::Memory(MemoryId {
            workspace_id: "repository".to_string(),
            ulid: local_id,
        });
        assert_eq!(
            decode_identity(&encode_identity(&identity)).unwrap(),
            identity
        );
    }
}
