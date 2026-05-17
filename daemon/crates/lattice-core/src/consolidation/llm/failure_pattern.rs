//! Failure-pattern extraction from repeated diagnostics.
//!
//! Implements `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 6. Consolidation Engine` "LLM-driven consolidation" bullet:
//! "Several consolidation jobs (... failure-pattern extraction) cannot be
//! implemented deterministically and require LLM inference." It also satisfies
//! `## Phase 6: Consolidation Engine` DoD for recurring diagnostics.

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    complete_structured_with_provenance, create_memory, forbid_hot_path, malformed_with_event,
    memory_state, structured_fields, submit_memory_proposal, ConsolidationJobKind, LlmJobContext,
    LlmJobError, LlmJobServices,
};
use crate::consolidation::{empty_state, ProposalKind, ProposalTarget};
use crate::events::{DiagnosticObservedPayload, EventEnvelope, EventPayload};
use crate::memory::model::MemoryAssertionType;
use crate::memory::{MemoryType, MemoryVerificationStatus};

pub struct FailurePatternJob;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticCluster {
    pub cluster_id: String,
    pub diagnostics: Vec<EventEnvelope>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FailurePatternMemoryCandidate {
    pub summary: String,
    pub recurrence_signal: String,
    #[serde(default)]
    pub likely_causes: Vec<String>,
    #[serde(default)]
    pub recovery_steps: Vec<String>,
    pub confidence: f64,
}

impl FailurePatternJob {
    pub fn run(
        ctx: &LlmJobContext,
        services: &mut LlmJobServices<'_>,
        cluster: &DiagnosticCluster,
    ) -> Result<Option<crate::consolidation::ConsolidationProposal>, LlmJobError> {
        let kind = ConsolidationJobKind::FailurePatternExtraction;
        forbid_hot_path(ctx, kind)?;
        if cluster.diagnostics.len() < 2 {
            return Err(malformed_with_event(
                ctx,
                services,
                kind,
                "failure-pattern extraction requires at least 2 diagnostics".to_string(),
            ));
        }
        let prompt = failure_pattern_prompt(cluster);
        let completion: super::LlmCompletion<FailurePatternMemoryCandidate> =
            complete_structured_with_provenance(
                ctx,
                services,
                kind,
                prompt,
                FAILURE_PATTERN_SCHEMA,
            )?;
        let candidate = completion.value;
        let refresh_key = format!("llm_failure_pattern::{}", cluster.cluster_id);
        let proposed = proposed_failure_pattern_state(ctx, cluster, refresh_key, &candidate)?;
        submit_memory_proposal(
            ctx,
            services,
            kind,
            ProposalTarget::NewMemory,
            ProposalKind::CreateMemory,
            empty_state(),
            proposed,
            failure_pattern_evidence(cluster, &candidate),
            Some(completion.provenance),
        )
    }
}

fn proposed_failure_pattern_state(
    ctx: &LlmJobContext,
    cluster: &DiagnosticCluster,
    refresh_key: String,
    candidate: &FailurePatternMemoryCandidate,
) -> Result<serde_json::Value, LlmJobError> {
    let content = format!(
        "{}\nLikely causes: {}\nRecovery: {}",
        candidate.summary,
        candidate.likely_causes.join("; "),
        candidate.recovery_steps.join("; ")
    );
    let mut memory = create_memory(
        ctx,
        content,
        MemoryType::AntiPattern,
        refresh_key,
        format!("llm_failure_pattern:{}", cluster.cluster_id),
        candidate.confidence,
    );
    memory.linked_files = diagnostic_files(cluster);
    let fields = structured_fields(
        MemoryAssertionType::AntiPattern,
        MemoryVerificationStatus::InReview,
        candidate.recurrence_signal.clone(),
        format!("diagnostic cluster {}", cluster.cluster_id),
    );
    memory_state(memory, fields, Vec::new())
}

fn failure_pattern_prompt(cluster: &DiagnosticCluster) -> String {
    let diagnostics = cluster
        .diagnostics
        .iter()
        .filter_map(diagnostic_payload)
        .map(|payload| {
            format!(
                "{} {:?}: {}",
                payload.diagnostic_id, payload.severity, payload.message
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Summarize the recurring failure pattern for cluster {} as JSON.\n{}",
        cluster.cluster_id, diagnostics
    )
}

fn diagnostic_payload(event: &EventEnvelope) -> Option<&DiagnosticObservedPayload> {
    match &event.payload {
        EventPayload::DiagnosticObserved(payload) => Some(payload),
        _ => None,
    }
}

fn diagnostic_files(cluster: &DiagnosticCluster) -> Vec<String> {
    cluster
        .diagnostics
        .iter()
        .filter_map(diagnostic_payload)
        .map(|payload| payload.file_id.repo_relative_path.clone())
        .collect()
}

fn failure_pattern_evidence(
    cluster: &DiagnosticCluster,
    candidate: &FailurePatternMemoryCandidate,
) -> serde_json::Value {
    let diagnostic_ids = cluster
        .diagnostics
        .iter()
        .filter_map(diagnostic_payload)
        .map(|payload| payload.diagnostic_id.clone())
        .collect::<Vec<_>>();
    json!({
        "cluster_id": cluster.cluster_id,
        "diagnostic_ids": diagnostic_ids,
        "recurrence_signal": candidate.recurrence_signal,
        "likely_causes": candidate.likely_causes,
    })
}

const FAILURE_PATTERN_SCHEMA: &str = r#"{
  "summary": "string",
  "recurrence_signal": "string",
  "likely_causes": ["string"],
  "recovery_steps": ["string"],
  "confidence": 0.0
}"#;
