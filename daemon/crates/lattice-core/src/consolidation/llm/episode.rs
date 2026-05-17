//! Rich LLM episode summaries.
//!
//! Implements `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 6. Consolidation Engine` "LLM-driven consolidation" bullet:
//! "Several consolidation jobs (episode summary generation...) cannot be
//! implemented deterministically and require LLM inference." It also satisfies
//! `## Phase 6: Consolidation Engine` DoD for useful episode memories.

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    complete_structured_with_provenance, create_memory, empty_state, existing_state,
    forbid_hot_path, memory_state, structured_fields, submit_memory_proposal, ConsolidationJobKind,
    LlmJobContext, LlmJobError, LlmJobServices,
};
use crate::consolidation::{EpisodeTemplate, ProposalKind, ProposalTarget};
use crate::events::{EventEnvelope, StableRef};
use crate::memory::model::MemoryAssertionType;
use crate::memory::{MemoryType, MemoryVerificationStatus};

pub struct EpisodeSummaryJob;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EpisodeMemoryCandidate {
    pub summary: String,
    pub outcome: String,
    #[serde(default)]
    pub salient_facts: Vec<String>,
    #[serde(default)]
    pub linked_files: Vec<String>,
    #[serde(default)]
    pub linked_symbols: Vec<String>,
    pub confidence: f64,
}

impl EpisodeSummaryJob {
    pub fn run(
        ctx: &LlmJobContext,
        services: &mut LlmJobServices<'_>,
        task_slice: &[EventEnvelope],
    ) -> Result<Option<crate::consolidation::ConsolidationProposal>, LlmJobError> {
        let kind = ConsolidationJobKind::EpisodeSummary;
        forbid_hot_path(ctx, kind)?;
        let template = EpisodeTemplate::from_task_slice(task_slice)
            .map_err(|e| LlmJobError::MalformedResponse(e.to_string()))?;
        let prompt = episode_prompt(&template, task_slice);
        let completion: super::LlmCompletion<EpisodeMemoryCandidate> =
            complete_structured_with_provenance(ctx, services, kind, prompt, EPISODE_SCHEMA)?;
        let candidate = completion.value;
        let refresh_key = format!(
            "llm_episode::{}::{}",
            template.session_id.value, template.task_id.value
        );
        let existing = services.memory_store.find_by_refresh_key(
            &refresh_key,
            Some(&ctx.workspace_id),
            None,
        )?;
        let target = existing
            .as_ref()
            .map(|memory| ProposalTarget::ExistingMemory(memory.id.clone()))
            .unwrap_or(ProposalTarget::NewMemory);
        let prior = existing
            .as_ref()
            .map(|memory| existing_state(services.memory_store, memory))
            .transpose()?
            .unwrap_or_else(empty_state);
        let proposed = proposed_episode_state(ctx, &candidate, &template, refresh_key)?;
        submit_memory_proposal(
            ctx,
            services,
            kind,
            target,
            proposal_kind(existing.as_ref()),
            prior,
            proposed,
            episode_evidence(&template, &candidate),
            Some(completion.provenance),
        )
    }
}

fn proposed_episode_state(
    ctx: &LlmJobContext,
    candidate: &EpisodeMemoryCandidate,
    template: &EpisodeTemplate,
    refresh_key: String,
) -> Result<serde_json::Value, LlmJobError> {
    let mut memory = create_memory(
        ctx,
        candidate.summary.clone(),
        MemoryType::Pattern,
        refresh_key,
        format!("llm_episode_summary:{}", template.task_id.value),
        candidate.confidence,
    );
    memory.linked_files = candidate.linked_files.clone();
    memory.linked_symbols = candidate.linked_symbols.clone();
    let fields = structured_fields(
        MemoryAssertionType::WorkflowOutcome,
        MemoryVerificationStatus::InReview,
        format!("LLM summarized task outcome as {}", candidate.outcome),
        format!("episode task {}", template.task_id.value),
    );
    memory_state(memory, fields, Vec::new())
}

fn episode_prompt(template: &EpisodeTemplate, events: &[EventEnvelope]) -> String {
    let event_summaries = events
        .iter()
        .map(|event| {
            format!(
                "{} {} {}",
                event.kind.as_str(),
                event.event_id.ulid,
                event.summary.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Create a rich episode memory as JSON for task {} with outcome {}.\nTools: {:?}\nAnchors: {:?}\nEvents:\n{}",
        template.task_id.value,
        template.outcome.as_str(),
        template.tools_used,
        compact_anchors(&template.salient_anchors),
        event_summaries
    )
}

fn compact_anchors(anchors: &[StableRef]) -> Vec<String> {
    anchors.iter().map(|anchor| format!("{anchor:?}")).collect()
}

fn proposal_kind(existing: Option<&crate::memory::Memory>) -> ProposalKind {
    if existing.is_some() {
        ProposalKind::UpdateMemory
    } else {
        ProposalKind::CreateMemory
    }
}

fn episode_evidence(
    template: &EpisodeTemplate,
    candidate: &EpisodeMemoryCandidate,
) -> serde_json::Value {
    json!({
        "task_id": template.task_id.value,
        "session_id": template.session_id.value,
        "outcome": candidate.outcome,
        "event_window": {
            "start": &template.event_window.start,
            "end": &template.event_window.end,
        },
        "salient_facts": candidate.salient_facts,
        "source_event_ids": [&template.event_window.start, &template.event_window.end],
    })
}

const EPISODE_SCHEMA: &str = r#"{
  "summary": "string",
  "outcome": "success|failure|abandoned",
  "salient_facts": ["string"],
  "linked_files": ["repo/path"],
  "linked_symbols": ["symbol"],
  "confidence": 0.0
}"#;
