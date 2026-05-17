//! Procedure extraction from repeated successful workflow traces.
//!
//! Implements `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 6. Consolidation Engine` "LLM-driven consolidation" bullet:
//! "Several consolidation jobs (... procedure extraction...) cannot be
//! implemented deterministically and require LLM inference." It also satisfies
//! `## Phase 6: Consolidation Engine` DoD for repeated successful traces.

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    complete_structured_with_provenance, create_memory, forbid_hot_path, malformed_with_event,
    memory_state, structured_fields, submit_memory_proposal, ConsolidationJobKind, LlmJobContext,
    LlmJobError, LlmJobServices,
};
use crate::consolidation::{empty_state, ProposalKind, ProposalTarget};
use crate::events::EventEnvelope;
use crate::memory::model::MemoryAssertionType;
use crate::memory::{MemoryType, MemoryVerificationStatus};

pub struct ProcedureExtractionJob;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkflowOccurrence {
    pub occurrence_id: String,
    pub events: Vec<EventEnvelope>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcedureMemoryCandidate {
    pub title: String,
    pub steps: Vec<String>,
    #[serde(default)]
    pub preconditions: Vec<String>,
    #[serde(default)]
    pub postconditions: Vec<String>,
    #[serde(default)]
    pub tools_used: Vec<String>,
    pub confidence: f64,
}

impl ProcedureExtractionJob {
    pub fn run(
        ctx: &LlmJobContext,
        services: &mut LlmJobServices<'_>,
        workflow_id: &str,
        occurrences: &[WorkflowOccurrence],
    ) -> Result<Option<crate::consolidation::ConsolidationProposal>, LlmJobError> {
        let kind = ConsolidationJobKind::ProcedureExtraction;
        forbid_hot_path(ctx, kind)?;
        if occurrences.len() < 3 {
            return Err(malformed_with_event(
                ctx,
                services,
                kind,
                "procedure extraction requires at least 3 successful traces".to_string(),
            ));
        }
        let prompt = procedure_prompt(workflow_id, occurrences);
        let completion: super::LlmCompletion<ProcedureMemoryCandidate> =
            complete_structured_with_provenance(ctx, services, kind, prompt, PROCEDURE_SCHEMA)?;
        let candidate = completion.value;
        let refresh_key = format!("llm_procedure::{workflow_id}");
        let proposed = proposed_procedure_state(ctx, workflow_id, refresh_key, &candidate)?;
        submit_memory_proposal(
            ctx,
            services,
            kind,
            ProposalTarget::NewMemory,
            ProposalKind::CreateMemory,
            empty_state(),
            proposed,
            procedure_evidence(workflow_id, occurrences, &candidate),
            Some(completion.provenance),
        )
    }
}

fn proposed_procedure_state(
    ctx: &LlmJobContext,
    workflow_id: &str,
    refresh_key: String,
    candidate: &ProcedureMemoryCandidate,
) -> Result<serde_json::Value, LlmJobError> {
    let content = format!(
        "{}\n\nPreconditions: {}\nSteps:\n{}\nPostconditions: {}",
        candidate.title,
        candidate.preconditions.join("; "),
        candidate.steps.join("\n"),
        candidate.postconditions.join("; ")
    );
    let mut memory = create_memory(
        ctx,
        content,
        MemoryType::Pattern,
        refresh_key,
        format!("llm_procedure_extraction:{workflow_id}"),
        candidate.confidence,
    );
    memory.linked_symbols = candidate.tools_used.clone();
    let fields = structured_fields(
        MemoryAssertionType::Pattern,
        MemoryVerificationStatus::InReview,
        format!("extracted from repeated successful workflow {workflow_id}"),
        format!("procedure candidate for workflow {workflow_id}"),
    );
    memory_state(memory, fields, Vec::new())
}

fn procedure_prompt(workflow_id: &str, occurrences: &[WorkflowOccurrence]) -> String {
    let traces = occurrences
        .iter()
        .map(|occurrence| {
            let events = occurrence
                .events
                .iter()
                .map(|event| format!("{}: {}", event.kind.as_str(), event.summary.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            format!("Occurrence {}:\n{}", occurrence.occurrence_id, events)
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("Extract a reusable procedure as JSON for successful workflow {workflow_id}.\n{traces}")
}

fn procedure_evidence(
    workflow_id: &str,
    occurrences: &[WorkflowOccurrence],
    candidate: &ProcedureMemoryCandidate,
) -> serde_json::Value {
    let occurrence_ids = occurrences
        .iter()
        .map(|occurrence| occurrence.occurrence_id.clone())
        .collect::<Vec<_>>();
    json!({
        "workflow_id": workflow_id,
        "occurrence_ids": occurrence_ids,
        "steps": candidate.steps,
        "tools_used": candidate.tools_used,
    })
}

const PROCEDURE_SCHEMA: &str = r#"{
  "title": "string",
  "steps": ["string"],
  "preconditions": ["string"],
  "postconditions": ["string"],
  "tools_used": ["string"],
  "confidence": 0.0
}"#;
