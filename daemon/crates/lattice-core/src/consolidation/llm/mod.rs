//! LLM-driven consolidation jobs for proposal-only memory changes.
//!
//! This module implements `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 6. Consolidation Engine` "LLM-driven consolidation" and supports the
//! `## Phase 6: Consolidation Engine` DoD that repeated traces produce
//! procedures, recurring diagnostics produce failure patterns, and consolidation
//! stays auditable, reversible, and test-covered.
//!
//! Required LLM-driven constraints from `## 6. Consolidation Engine`:
//!
//! - each LLM-driven job produces a proposal record, not a direct write; proposals require explicit apply or reject before altering durable memory
//! - failed or malformed LLM responses must leave the prior memory state unchanged and emit a consolidation failure event
//! - LLM-driven jobs run only in background or manual review modes, never on the synchronous post-task hot path
//! - each job records its model, prompt hash, and response hash as part of the proposal provenance so outputs are auditable and reproducible
//! - consolidation queue depth must be bounded; when the queue is full, new jobs are dropped with a log warning rather than stalling the daemon
//! - cost and latency budgets for LLM consolidation should be documented per job type before Phase 6 begins; jobs exceeding budget must fall back to deterministic approximations or skip with a stale flag

use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{field, info_span, warn};

use crate::consolidation::{
    empty_state, now_unix_micros, ConsolidationJobMode, ConsolidationJobRuntime,
    ConsolidationJobSpec, ConsolidationMemoryState, PendingProposalSpec, ProposalKind,
    ProposalTarget,
};
use crate::error::LatticeError;
use crate::events::{
    Actor, BranchRef, CompactSummary, ConsolidationFailedPayload, EventKind, EventPayload,
    EventWriter, PartialEnvelope, SessionId,
};
use crate::memory::model::{MemoryAssertionType, MemoryFreshnessPolicy, MemoryProvenance};
use crate::memory::{
    Memory, MemoryEvidence, MemoryLinkRecord, MemoryScope, MemoryStore, MemoryStructuredFields,
    MemoryType, MemoryVerificationStatus,
};

pub mod budget;
pub mod contradiction;
pub mod episode;
pub mod failure_pattern;
pub mod procedure;
pub mod provenance;
pub mod session_digest;

#[cfg(test)]
mod llm_tests;
#[cfg(test)]
mod provenance_tests;
#[cfg(test)]
mod session_digest_tests;

pub use budget::{
    BudgetCatalog, BudgetError, BudgetOutcome, BudgetUsage, DeterministicFallback, LlmBudget,
};
pub use contradiction::{ContradictionCandidatePair, ContradictionDetectionJob};
pub use episode::{EpisodeMemoryCandidate, EpisodeSummaryJob};
pub use failure_pattern::{DiagnosticCluster, FailurePatternJob, FailurePatternMemoryCandidate};
pub use procedure::{ProcedureExtractionJob, ProcedureMemoryCandidate, WorkflowOccurrence};
pub use provenance::{LlmProvenance, ProvenanceError};
pub use session_digest::{
    SessionDigestConsolidationConfig, SessionDigestConsolidationConfigError,
    SessionDigestConsolidationOutcome, SessionDigestLlmConsolidator, SessionDigestLlmProvider,
};

pub type ConsolidationMode = ConsolidationJobMode;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmRequest {
    pub workspace_id: String,
    pub job_kind: ConsolidationJobKind,
    pub prompt: String,
    pub response_schema: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmResponse {
    pub content: String,
}

pub trait LlmDriver {
    fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmDriverError>;
    fn name(&self) -> &str;
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LlmDriverError {
    #[error("LLM driver unavailable: {0}")]
    Unavailable(String),
    #[error("LLM driver budget exceeded: {0}")]
    BudgetExceeded(String),
    #[error("LLM driver failed: {0}")]
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlmJobContext {
    pub workspace_id: String,
    pub mode: ConsolidationMode,
    pub budget_catalog: BudgetCatalog,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidationJobKind {
    EpisodeSummary,
    ProcedureExtraction,
    ContradictionDetection,
    FailurePatternExtraction,
    SessionDigestConsolidation,
}

impl ConsolidationJobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EpisodeSummary => "episode_summary",
            Self::ProcedureExtraction => "procedure_extraction",
            Self::ContradictionDetection => "contradiction_detection",
            Self::FailurePatternExtraction => "failure_pattern_extraction",
            Self::SessionDigestConsolidation => "session_digest_llm_consolidation",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LlmJobError {
    #[error("LLM consolidation job `{0}` is forbidden on the synchronous post-task hot path")]
    ForbiddenOnHotPath(&'static str),
    #[error(transparent)]
    DriverError(#[from] LlmDriverError),
    #[error("malformed LLM response: {0}")]
    MalformedResponse(String),
    #[error("LLM budget exceeded: {0}")]
    BudgetExceeded(String),
    #[error("LLM provenance failed: {0}")]
    Provenance(String),
    #[error("consolidation storage failed: {0}")]
    Storage(String),
}

impl From<LatticeError> for LlmJobError {
    fn from(value: LatticeError) -> Self {
        Self::Storage(value.to_string())
    }
}

pub(crate) struct LlmCompletion<T> {
    pub value: T,
    pub provenance: LlmProvenance,
}

pub struct LlmJobServices<'a> {
    pub driver: &'a dyn LlmDriver,
    pub runtime: &'a mut ConsolidationJobRuntime,
    pub memory_store: &'a MemoryStore,
    pub event_writer: &'a EventWriter,
    pub authority: &'a crate::consolidation::EvolutionAuthority<'a>,
}

pub enum LlmJobInput {
    EpisodeSummary(Vec<crate::events::EventEnvelope>),
    ProcedureExtraction {
        workflow_id: String,
        occurrences: Vec<WorkflowOccurrence>,
    },
    ContradictionDetection(ContradictionCandidatePair),
    FailurePatternExtraction(DiagnosticCluster),
}

pub fn dispatch_llm_job(
    ctx: &LlmJobContext,
    services: &mut LlmJobServices<'_>,
    input: LlmJobInput,
) -> Result<Option<crate::consolidation::ConsolidationProposal>, LlmJobError> {
    forbid_hot_path(ctx, input.kind())?;
    match input {
        LlmJobInput::EpisodeSummary(slice) => EpisodeSummaryJob::run(ctx, services, &slice),
        LlmJobInput::ProcedureExtraction {
            workflow_id,
            occurrences,
        } => ProcedureExtractionJob::run(ctx, services, &workflow_id, &occurrences),
        LlmJobInput::ContradictionDetection(pair) => {
            ContradictionDetectionJob::run(ctx, services, &pair)
        }
        LlmJobInput::FailurePatternExtraction(cluster) => {
            FailurePatternJob::run(ctx, services, &cluster)
        }
    }
}

impl LlmJobInput {
    fn kind(&self) -> ConsolidationJobKind {
        match self {
            Self::EpisodeSummary(_) => ConsolidationJobKind::EpisodeSummary,
            Self::ProcedureExtraction { .. } => ConsolidationJobKind::ProcedureExtraction,
            Self::ContradictionDetection(_) => ConsolidationJobKind::ContradictionDetection,
            Self::FailurePatternExtraction(_) => ConsolidationJobKind::FailurePatternExtraction,
        }
    }
}

pub(crate) fn forbid_hot_path(
    ctx: &LlmJobContext,
    kind: ConsolidationJobKind,
) -> Result<(), LlmJobError> {
    if ctx.mode == ConsolidationMode::SynchronousPostTask {
        return Err(LlmJobError::ForbiddenOnHotPath(kind.as_str()));
    }
    Ok(())
}

pub(crate) fn complete_structured_with_provenance<T>(
    ctx: &LlmJobContext,
    services: &mut LlmJobServices<'_>,
    kind: ConsolidationJobKind,
    prompt: String,
    response_schema: &str,
) -> Result<LlmCompletion<T>, LlmJobError>
where
    T: serde::de::DeserializeOwned,
{
    let prompt_tokens = token_count(&prompt);
    enforce_budget(
        ctx,
        services,
        kind,
        BudgetUsage {
            prompt_tokens,
            response_tokens: 0,
            latency_ms: 0,
            cost_micro_usd: estimate_cost_micro_usd(prompt_tokens, 0),
        },
    )?;
    let _span = info_span!(
        "llm_consolidation",
        job_kind = kind.as_str(),
        workspace_id = ctx.workspace_id.as_str(),
        mode = ctx.mode.as_str(),
        model_name = services.driver.name(),
        outcome = field::Empty
    )
    .entered();
    let request = LlmRequest {
        workspace_id: ctx.workspace_id.clone(),
        job_kind: kind,
        prompt: prompt.clone(),
        response_schema: response_schema.to_string(),
    };
    let started = Instant::now();
    let response = match services.driver.complete(request) {
        Ok(response) => response,
        Err(error) => return fail_driver(ctx, services, kind, error),
    };
    let latency = started.elapsed();
    let response_tokens = token_count(&response.content);
    let latency_ms = u32::try_from(latency.as_millis()).unwrap_or(u32::MAX);
    enforce_budget(
        ctx,
        services,
        kind,
        BudgetUsage {
            prompt_tokens,
            response_tokens,
            latency_ms,
            cost_micro_usd: estimate_cost_micro_usd(prompt_tokens, response_tokens),
        },
    )?;
    let value = parse_response(ctx, services, kind, response.clone())?;
    let provenance = LlmProvenance::record(
        services.driver.name(),
        prompt.as_bytes(),
        response.content.as_bytes(),
        prompt_tokens,
        response_tokens,
        latency,
    )
    .map_err(|e| LlmJobError::Provenance(e.to_string()))?;
    Ok(LlmCompletion { value, provenance })
}

fn parse_response<T>(
    ctx: &LlmJobContext,
    services: &LlmJobServices<'_>,
    kind: ConsolidationJobKind,
    response: LlmResponse,
) -> Result<T, LlmJobError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_str(&response.content).map_err(|error| {
        let job_error = LlmJobError::MalformedResponse(error.to_string());
        let _ = emit_failure(
            ctx,
            services.event_writer,
            services.authority.branch,
            services.driver.name(),
            kind,
            &job_error,
        );
        job_error
    })
}

fn fail_driver<T>(
    ctx: &LlmJobContext,
    services: &LlmJobServices<'_>,
    kind: ConsolidationJobKind,
    error: LlmDriverError,
) -> Result<T, LlmJobError> {
    let job_error = LlmJobError::DriverError(error);
    emit_failure(
        ctx,
        services.event_writer,
        services.authority.branch,
        services.driver.name(),
        kind,
        &job_error,
    )?;
    Err(job_error)
}

pub(crate) fn submit_memory_proposal(
    ctx: &LlmJobContext,
    services: &mut LlmJobServices<'_>,
    kind: ConsolidationJobKind,
    target: ProposalTarget,
    proposal_kind: ProposalKind,
    prior_state: Value,
    proposed_state: Value,
    evidence: Value,
    provenance: Option<LlmProvenance>,
) -> Result<Option<crate::consolidation::ConsolidationProposal>, LlmJobError> {
    let proposal_id = stable_id(kind.as_str(), "proposal");
    let job_id = stable_id(kind.as_str(), "job");
    let job = ConsolidationJobSpec {
        job_id,
        workspace_id: ctx.workspace_id.clone(),
        kind: kind.as_str().to_string(),
        mode: ctx.mode,
        proposal: Some(PendingProposalSpec {
            proposal_id,
            target_memory_id: target.memory_id().map(str::to_string),
            proposal_kind,
            prior_state,
            proposed_state,
            evidence,
            provenance,
        }),
    };
    let (_, proposal) =
        services
            .runtime
            .submit_inline(job, services.memory_store, services.authority)?;
    Ok(proposal)
}

fn enforce_budget(
    ctx: &LlmJobContext,
    services: &LlmJobServices<'_>,
    kind: ConsolidationJobKind,
    usage: BudgetUsage,
) -> Result<(), LlmJobError> {
    match ctx.budget_catalog.require_within(kind, usage) {
        Ok(()) => Ok(()),
        Err(error) => {
            warn_budget_overrun(ctx, kind, usage, &error);
            let job_error = LlmJobError::BudgetExceeded(error.to_string());
            emit_failure(
                ctx,
                services.event_writer,
                services.authority.branch,
                services.driver.name(),
                kind,
                &job_error,
            )?;
            Err(job_error)
        }
    }
}

fn warn_budget_overrun(
    ctx: &LlmJobContext,
    kind: ConsolidationJobKind,
    usage: BudgetUsage,
    error: &BudgetError,
) {
    let budget = ctx.budget_catalog.for_kind(kind);
    warn!(
        workspace_id = ctx.workspace_id.as_str(),
        kind = kind.as_str(),
        actual_prompt_tokens = usage.prompt_tokens,
        budget_prompt_tokens = budget.max_prompt_tokens,
        actual_response_tokens = usage.response_tokens,
        budget_response_tokens = budget.max_response_tokens,
        actual_latency_ms = usage.latency_ms,
        budget_latency_ms = budget.max_latency_ms,
        actual_cost_micro_usd = usage.cost_micro_usd,
        budget_cost_micro_usd = budget.max_cost_micro_usd,
        error = error.to_string().as_str(),
        "LLM consolidation budget exceeded"
    );
}

fn token_count(text: &str) -> u32 {
    let count = text.split_whitespace().count();
    u32::try_from(count.max(1)).unwrap_or(u32::MAX)
}

fn estimate_cost_micro_usd(prompt_tokens: u32, response_tokens: u32) -> u32 {
    prompt_tokens
        .saturating_add(response_tokens.saturating_mul(2))
        .saturating_div(10)
        .max(1)
}

pub(crate) fn emit_failure(
    ctx: &LlmJobContext,
    event_writer: &EventWriter,
    branch: &str,
    model_name: &str,
    kind: ConsolidationJobKind,
    error: &LlmJobError,
) -> Result<(), LlmJobError> {
    let payload = ConsolidationFailedPayload {
        job_id: stable_id(kind.as_str(), "job"),
        job_kind: kind.as_str().to_string(),
        mode: ctx.mode.as_str().to_string(),
        model_name: model_name.to_string(),
        error_kind: error_kind(error).to_string(),
        error_message: error.to_string(),
    };
    warn!(
        workspace_id = ctx.workspace_id.as_str(),
        job_kind = payload.job_kind.as_str(),
        mode = payload.mode.as_str(),
        error_kind = payload.error_kind.as_str(),
        error_message = payload.error_message.as_str(),
        "LLM consolidation failed"
    );
    event_writer
        .append(PartialEnvelope {
            workspace_id: Some(ctx.workspace_id.clone()),
            branch: BranchRef {
                name: branch.to_string(),
            },
            session_id: SessionId {
                value: "llm-consolidation".to_string(),
            },
            task_id: None,
            actor: Actor::Daemon,
            kind: EventKind::ConsolidationFailed,
            references: Vec::new(),
            summary: CompactSummary::new(format!(
                "LLM consolidation {} failed: {}",
                kind.as_str(),
                error_kind(error)
            ))
            .map_err(|e| LlmJobError::Storage(e.to_string()))?,
            payload: EventPayload::ConsolidationFailed(payload),
        })
        .map_err(|e| LlmJobError::Storage(e.to_string()))?;
    Ok(())
}

pub(crate) fn malformed_with_event(
    ctx: &LlmJobContext,
    services: &LlmJobServices<'_>,
    kind: ConsolidationJobKind,
    message: impl Into<String>,
) -> LlmJobError {
    let error = LlmJobError::MalformedResponse(message.into());
    let _ = emit_failure(
        ctx,
        services.event_writer,
        services.authority.branch,
        services.driver.name(),
        kind,
        &error,
    );
    error
}

pub(crate) fn memory_state(
    memory: Memory,
    fields: MemoryStructuredFields,
    links: Vec<MemoryLinkRecord>,
) -> Result<Value, LlmJobError> {
    serde_json::to_value(ConsolidationMemoryState {
        memory,
        structured_fields: fields,
        last_verified_at: None,
        last_verified_graph_snapshot_id: None,
        expires_at: None,
        memory_links: links,
    })
    .map_err(|e| LlmJobError::MalformedResponse(e.to_string()))
}

pub(crate) fn create_memory(
    ctx: &LlmJobContext,
    branch: &str,
    content: String,
    memory_type: MemoryType,
    refresh_key: String,
    source_query: String,
    confidence: f64,
) -> Memory {
    Memory {
        id: stable_id(&refresh_key, "memory"),
        session_id: "llm-consolidation".to_string(),
        content,
        memory_type,
        scope: MemoryScope::Session,
        confidence,
        linked_symbols: Vec::new(),
        linked_files: Vec::new(),
        workspace_id: Some(ctx.workspace_id.clone()),
        branch: Some(branch.to_string()),
        scope_organization_id: None,
        refresh_key: Some(refresh_key),
        source_query: Some(source_query),
        created_at: 0,
        last_accessed: 0,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: crate::memory::MemoryVerificationStatus::Unverified,
    }
}

pub(crate) fn structured_fields(
    assertion_type: MemoryAssertionType,
    verification_status: MemoryVerificationStatus,
    confidence_reason: String,
    evidence_detail: String,
) -> MemoryStructuredFields {
    MemoryStructuredFields {
        memory_class: crate::memory::MemoryClass::Observation,
        assertion_type,
        verification_status,
        confidence_reason: Some(confidence_reason),
        supersedes_memory_id: None,
        superseded_by_memory_id: None,
        contradicts_memory_ids: Vec::new(),
        contradicted_by_memory_ids: Vec::new(),
        freshness_policy: MemoryFreshnessPolicy::SessionScoped,
        freshness_policy_detail: None,
        validity_conditions: Vec::new(),
        invalidation_triggers: Vec::new(),
        provenance: vec![MemoryProvenance {
            source: "llm_consolidation".to_string(),
            reference: None,
            captured_at: Some(now_unix_micros().max(0) as u64),
            note: Some("proposal-only LLM consolidation candidate".to_string()),
        }],
        evidence: vec![MemoryEvidence {
            kind: "llm_consolidation".to_string(),
            reference: None,
            detail: Some(evidence_detail),
            captured_at: Some(now_unix_micros().max(0) as u64),
            span: None,
            evidence_content_hash: None,
        }],
        linked_docs: Vec::new(),
        linked_tests: Vec::new(),
        linked_memories: Vec::new(),
    }
}

pub(crate) fn existing_state(
    memory_store: &MemoryStore,
    memory: &Memory,
) -> Result<Value, LlmJobError> {
    crate::consolidation::capture_memory_state(memory_store, memory)
        .map(|state| crate::consolidation::encode_memory_state(&state))
        .map_err(Into::into)
}

pub(crate) fn stable_id(seed: &str, prefix: &str) -> String {
    let mut hash = 14_695_981_039_346_656_037_u64;
    for byte in seed.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    format!("{prefix}-{hash:016x}-{}", now_unix_micros().max(0))
}

fn error_kind(error: &LlmJobError) -> &'static str {
    match error {
        LlmJobError::ForbiddenOnHotPath(_) => "forbidden_on_hot_path",
        LlmJobError::DriverError(_) => "driver_error",
        LlmJobError::MalformedResponse(_) => "malformed_response",
        LlmJobError::BudgetExceeded(_) => "budget_exceeded",
        LlmJobError::Provenance(_) => "provenance",
        LlmJobError::Storage(_) => "storage",
    }
}
