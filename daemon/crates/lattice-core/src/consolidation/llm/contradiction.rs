//! Contradiction detection across overlapping memory assertions.
//!
//! Implements `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 6. Consolidation Engine` "LLM-driven consolidation" bullet:
//! "Several consolidation jobs (... contradiction detection...) cannot be
//! implemented deterministically and require LLM inference." It also supports
//! `## Phase 6: Consolidation Engine` DoD for auditable consolidation.

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    complete_structured_with_provenance, existing_state, forbid_hot_path, malformed_with_event,
    memory_state, submit_memory_proposal, ConsolidationJobKind, LlmJobContext, LlmJobError,
    LlmJobServices,
};
use crate::consolidation::{ProposalKind, ProposalTarget};
use crate::memory::{Memory, MemoryLinkRecord, MemoryVerificationStatus};

pub struct ContradictionDetectionJob;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContradictionCandidatePair {
    pub first: Memory,
    pub second: Memory,
    pub deterministic_reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContradictionDecision {
    pub is_contradiction: bool,
    pub contradicted_memory_id: String,
    pub contradicting_memory_id: String,
    pub rationale: String,
}

impl ContradictionDetectionJob {
    pub fn run(
        ctx: &LlmJobContext,
        services: &mut LlmJobServices<'_>,
        pair: &ContradictionCandidatePair,
    ) -> Result<Option<crate::consolidation::ConsolidationProposal>, LlmJobError> {
        let kind = ConsolidationJobKind::ContradictionDetection;
        forbid_hot_path(ctx, kind)?;
        let prompt = contradiction_prompt(pair);
        let completion: super::LlmCompletion<ContradictionDecision> =
            complete_structured_with_provenance(ctx, services, kind, prompt, CONTRADICTION_SCHEMA)?;
        let decision = completion.value;
        if !decision.is_contradiction {
            return Err(malformed_with_event(
                ctx,
                services,
                kind,
                "contradiction job response did not confirm a contradiction".to_string(),
            ));
        }
        let target = target_memory(ctx, services, kind, pair, &decision)?;
        let prior = existing_state(services.memory_store, target)?;
        let proposed = contradicted_state(services.memory_store, target, &decision)?;
        submit_memory_proposal(
            ctx,
            services,
            kind,
            ProposalTarget::ExistingMemory(target.id.clone()),
            ProposalKind::UpdateMemory,
            prior,
            proposed,
            contradiction_evidence(pair, &decision),
            Some(completion.provenance),
        )
    }
}

fn contradicted_state(
    memory_store: &crate::memory::MemoryStore,
    target: &Memory,
    decision: &ContradictionDecision,
) -> Result<serde_json::Value, LlmJobError> {
    let mut fields = memory_store
        .get_structured_fields(&target.id)?
        .unwrap_or_default();
    fields.verification_status = MemoryVerificationStatus::Contradicted;
    if !fields
        .contradicted_by_memory_ids
        .iter()
        .any(|id| id == &decision.contradicting_memory_id)
    {
        fields
            .contradicted_by_memory_ids
            .push(decision.contradicting_memory_id.clone());
    }
    let link = MemoryLinkRecord {
        link_id: format!(
            "contradiction-{}-{}",
            decision.contradicted_memory_id, decision.contradicting_memory_id
        ),
        source_memory_id: decision.contradicted_memory_id.clone(),
        target_memory_id: decision.contradicting_memory_id.clone(),
        link_type: "contradicts".to_string(),
        reason: decision.rationale.clone(),
        created_at: crate::consolidation::now_unix_micros().max(0) as u64,
        verification_status: "in_review".to_string(),
    };
    memory_state(target.clone(), fields, vec![link])
}

fn target_memory<'a>(
    ctx: &LlmJobContext,
    services: &LlmJobServices<'_>,
    kind: ConsolidationJobKind,
    pair: &'a ContradictionCandidatePair,
    decision: &ContradictionDecision,
) -> Result<&'a Memory, LlmJobError> {
    if decision.contradicted_memory_id == pair.first.id {
        return Ok(&pair.first);
    }
    if decision.contradicted_memory_id == pair.second.id {
        return Ok(&pair.second);
    }
    Err(malformed_with_event(
        ctx,
        services,
        kind,
        format!(
            "unknown contradicted memory id {}",
            decision.contradicted_memory_id
        ),
    ))
}

fn contradiction_prompt(pair: &ContradictionCandidatePair) -> String {
    format!(
        "Determine whether these memories contradict. Return JSON only.\nReason flagged: {}\nFirst {}: {}\nSecond {}: {}",
        pair.deterministic_reason,
        pair.first.id,
        pair.first.content,
        pair.second.id,
        pair.second.content
    )
}

fn contradiction_evidence(
    pair: &ContradictionCandidatePair,
    decision: &ContradictionDecision,
) -> serde_json::Value {
    json!({
        "source_memory_ids": [pair.first.id, pair.second.id],
        "deterministic_reason": pair.deterministic_reason,
        "rationale": decision.rationale,
        "contradicted_memory_id": decision.contradicted_memory_id,
        "contradicting_memory_id": decision.contradicting_memory_id,
    })
}

const CONTRADICTION_SCHEMA: &str = r#"{
  "is_contradiction": true,
  "contradicted_memory_id": "memory id",
  "contradicting_memory_id": "memory id",
  "rationale": "string"
}"#;
