//! Opt-in background consolidation over persisted, sanitized session capture.
//!
//! Admission is checked before source selection or provider invocation. The
//! model sees only committed deterministic session-digest facts, and every
//! accepted output becomes a repo-scoped pending review proposal.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{
    complete_structured_with_provenance, create_memory, malformed_with_event, memory_state,
    stable_id, structured_fields, BudgetCatalog, ConsolidationJobKind, LlmJobContext, LlmJobError,
    LlmJobServices,
};
use crate::consolidation::{
    empty_state, ConsolidationJobMode, ConsolidationSkipReason, ProposalKind,
};
use crate::error::LatticeError;
use crate::memory::model::{MemoryAssertionType, MemoryFreshnessPolicy, MemoryProvenance};
use crate::memory::session_digest::{sanitize_text, SessionDigestEvidence};
use crate::memory::{MemoryClass, MemoryScope, MemoryType, MemoryVerificationStatus};

const SESSION_DIGEST_JOB_KIND: &str = "session_digest_llm_consolidation";
const DEFAULT_MAX_SOURCE_FACTS: usize = 32;
const DEFAULT_MAX_PROPOSALS_PER_RUN: usize = 4;
const DEFAULT_MAX_PENDING_REVIEW_PROPOSALS: usize = 128;
const MAX_PROVIDER_KEY_BYTES: usize = 16 * 1024;
const MAX_CONSOLIDATED_CONTENT_BYTES: usize = 2_000;
const MAX_EVIDENCE_SUMMARY_BYTES: usize = 512;
const MAX_UNCERTAINTY_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDigestLlmProvider {
    OpenAi,
    Anthropic,
}

impl SessionDigestLlmProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionDigestConsolidationConfigError {
    #[error("session-digest consolidation retention window must be non-zero")]
    EmptyRetentionWindow,
    #[error("session-digest consolidation bounds must be non-zero")]
    EmptyBound,
}

/// Daemon-derived readiness and hard bounds for optional session consolidation.
///
/// The provider key is validated and immediately discarded. The daemon-owned
/// driver retains credentials; core keeps only a non-secret readiness bit so
/// debug output, skip state, and proposals can never expose the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDigestConsolidationConfig {
    enabled: bool,
    provider: Option<SessionDigestLlmProvider>,
    usable_provider_key: bool,
    retention_window: Duration,
    max_source_facts: usize,
    max_proposals_per_run: usize,
    max_pending_review_proposals: usize,
}

impl SessionDigestConsolidationConfig {
    pub fn from_daemon_config(
        enabled: bool,
        provider: Option<SessionDigestLlmProvider>,
        provider_key: Option<&str>,
        retention_window: Duration,
    ) -> Result<Self, SessionDigestConsolidationConfigError> {
        if retention_window.is_zero() {
            return Err(SessionDigestConsolidationConfigError::EmptyRetentionWindow);
        }
        Ok(Self {
            enabled,
            provider,
            usable_provider_key: provider_key.is_some_and(is_usable_provider_key),
            retention_window,
            max_source_facts: DEFAULT_MAX_SOURCE_FACTS,
            max_proposals_per_run: DEFAULT_MAX_PROPOSALS_PER_RUN,
            max_pending_review_proposals: DEFAULT_MAX_PENDING_REVIEW_PROPOSALS,
        })
    }

    pub fn with_bounds(
        mut self,
        max_source_facts: usize,
        max_proposals_per_run: usize,
        max_pending_review_proposals: usize,
    ) -> Result<Self, SessionDigestConsolidationConfigError> {
        if max_source_facts == 0 || max_proposals_per_run == 0 || max_pending_review_proposals == 0
        {
            return Err(SessionDigestConsolidationConfigError::EmptyBound);
        }
        self.max_source_facts = max_source_facts;
        self.max_proposals_per_run = max_proposals_per_run;
        self.max_pending_review_proposals = max_pending_review_proposals;
        Ok(self)
    }
}

impl Default for SessionDigestConsolidationConfig {
    fn default() -> Self {
        Self::from_daemon_config(false, None, None, Duration::from_secs(30 * 24 * 60 * 60))
            .expect("default retention window is non-zero")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SessionDigestConsolidationOutcome {
    Skipped {
        job_id: String,
        reason: ConsolidationSkipReason,
    },
    Proposed {
        proposal_ids: Vec<String>,
        source_fact_count: usize,
    },
}

pub struct SessionDigestLlmConsolidator;

impl SessionDigestLlmConsolidator {
    pub fn run(
        config: &SessionDigestConsolidationConfig,
        repository_id: &str,
        services: &mut LlmJobServices<'_>,
    ) -> Result<SessionDigestConsolidationOutcome, LlmJobError> {
        Self::run_selected(config, repository_id, services, None)
    }

    /// Process exactly the capture leased by the repository worker.
    pub fn run_for_capture(
        config: &SessionDigestConsolidationConfig,
        repository_id: &str,
        services: &mut LlmJobServices<'_>,
        delivery_key: &str,
    ) -> Result<SessionDigestConsolidationOutcome, LlmJobError> {
        if delivery_key.is_empty() {
            return Err(LlmJobError::Storage(
                "capture delivery key must not be empty".into(),
            ));
        }
        Self::run_selected(config, repository_id, services, Some(delivery_key))
    }

    /// Commit a terminal capture completion in canonical storage before a
    /// scheduler releases its separate lease. Existing completion wins.
    pub fn complete_capture_without_proposals(
        store: &crate::memory::MemoryStore,
        authority: &crate::consolidation::EvolutionAuthority<'_>,
        delivery_key: &str,
    ) -> Result<(), LlmJobError> {
        commit_capture_batch(store, authority, Some(delivery_key), Vec::new(), &[], 0).map(|_| ())
    }

    fn run_selected(
        config: &SessionDigestConsolidationConfig,
        repository_id: &str,
        services: &mut LlmJobServices<'_>,
        delivery_key: Option<&str>,
    ) -> Result<SessionDigestConsolidationOutcome, LlmJobError> {
        if let Some(key) = delivery_key {
            if let Some(proposal_ids) =
                completed_capture(services.memory_store, services.authority, key)?
            {
                return Ok(SessionDigestConsolidationOutcome::Proposed {
                    proposal_ids,
                    source_fact_count: 0,
                });
            }
        }
        if !config.enabled {
            return skip(services, repository_id, ConsolidationSkipReason::Disabled);
        }
        let Some(provider) = config.provider else {
            return skip(
                services,
                repository_id,
                ConsolidationSkipReason::MissingProviderKey,
            );
        };
        if !config.usable_provider_key {
            return skip(
                services,
                repository_id,
                ConsolidationSkipReason::MissingProviderKey,
            );
        }

        let available_review_slots = services.runtime.available_review_slots(
            services.memory_store,
            repository_id,
            config.max_pending_review_proposals,
        )?;
        if available_review_slots == 0 {
            return skip(services, repository_id, ConsolidationSkipReason::QueueFull);
        }

        let facts = load_persisted_facts(
            services,
            repository_id,
            config.retention_window,
            config.max_source_facts,
            delivery_key,
        )?;
        if facts.is_empty() {
            if let Some(key) = delivery_key {
                commit_capture_batch(
                    services.memory_store,
                    services.authority,
                    Some(key),
                    Vec::new(),
                    &[],
                    config.max_pending_review_proposals,
                )?;
            }
            return skip(
                services,
                repository_id,
                ConsolidationSkipReason::NoEligibleCaptureFacts,
            );
        }

        let max_proposals = config.max_proposals_per_run.min(available_review_slots);
        let ctx = LlmJobContext {
            workspace_id: repository_id.to_string(),
            mode: ConsolidationJobMode::Background,
            budget_catalog: BudgetCatalog::from_config(services.runtime.config()),
        };
        let prompt = build_prompt(&facts, max_proposals)?;
        let completion = match complete_structured_with_provenance::<ModelResponse>(
            &ctx,
            services,
            ConsolidationJobKind::SessionDigestConsolidation,
            prompt,
            RESPONSE_SCHEMA,
        ) {
            Ok(completion) => completion,
            Err(LlmJobError::BudgetExceeded(_)) => {
                return skip(
                    services,
                    repository_id,
                    ConsolidationSkipReason::BudgetExceeded,
                )
            }
            Err(error) => return Err(error),
        };
        let candidates = validate_candidates(&ctx, services, completion.value, max_proposals)?;
        let source_memory_ids = facts
            .iter()
            .map(|fact| fact.source_memory_id.clone())
            .collect::<Vec<_>>();
        let source_capture_ids = facts
            .iter()
            .map(|fact| fact.source_capture_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();

        let mut proposals = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let proposed = proposed_state(
                &ctx,
                services.authority.branch,
                &candidate,
                &source_memory_ids,
            )?;
            let evidence = json!({
                "source_kind": "sanitized_persisted_session_capture",
                "source_capture_ids": source_capture_ids,
                "source_memory_ids": source_memory_ids,
                "evidence_summary": candidate.evidence_summary,
                "uncertainty": candidate.uncertainty,
                "provider": provider.as_str(),
                "model": services.driver.name(),
                "budget_outcome": "within"
            });
            let job_id = stable_id(SESSION_DIGEST_JOB_KIND, "job");
            let mut proposal = crate::consolidation::proposal::proposal_from_pending(
                &job_id,
                repository_id,
                crate::consolidation::PendingProposalSpec {
                    proposal_id: stable_id(SESSION_DIGEST_JOB_KIND, "proposal"),
                    target_memory_id: None,
                    proposal_kind: ProposalKind::CreateMemory,
                    prior_state: empty_state(),
                    proposed_state: proposed,
                    evidence,
                    provenance: Some(completion.provenance.clone()),
                },
            );
            crate::consolidation::bind_proposal_authority(&mut proposal, services.authority)?;
            proposals.push(proposal);
        }
        let proposal_ids = commit_capture_batch(
            services.memory_store,
            services.authority,
            delivery_key,
            proposals,
            &facts,
            config.max_pending_review_proposals,
        )?;

        Ok(SessionDigestConsolidationOutcome::Proposed {
            proposal_ids,
            source_fact_count: facts.len(),
        })
    }
}

fn completed_capture(
    store: &crate::memory::MemoryStore,
    authority: &crate::consolidation::EvolutionAuthority<'_>,
    delivery_key: &str,
) -> Result<Option<Vec<String>>, LlmJobError> {
    store
        .with_connection(|conn| completed_capture_on_connection(conn, authority, delivery_key))
        .map_err(Into::into)
}

fn completed_capture_on_connection(
    conn: &rusqlite::Connection,
    authority: &crate::consolidation::EvolutionAuthority<'_>,
    delivery_key: &str,
) -> Result<Option<Vec<String>>, LatticeError> {
    let row: Option<(String,String,String,String)> = conn.query_row(
        "SELECT repository_id,checkout_id,branch,proposal_ids FROM consolidation_capture_receipts WHERE delivery_key=?1",
        [delivery_key], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
    ).optional().map_err(source_storage_error)?;
    row.map(|(repo, checkout, branch, ids)| {
        if repo != authority.repository_id
            || checkout != authority.checkout_id
            || branch != authority.branch
        {
            return Err(LatticeError::Storage(
                "capture completion receipt authority mismatch".into(),
            ));
        }
        serde_json::from_str(&ids)
            .map_err(|e| LatticeError::Storage(format!("invalid capture completion receipt: {e}")))
    })
    .transpose()
}

fn commit_capture_batch(
    store: &crate::memory::MemoryStore,
    authority: &crate::consolidation::EvolutionAuthority<'_>,
    delivery_key: Option<&str>,
    proposals: Vec<crate::consolidation::ConsolidationProposal>,
    facts: &[PersistedFact],
    max_pending: usize,
) -> Result<Vec<String>, LlmJobError> {
    store.with_connection(|conn| {
        let tx=rusqlite::Transaction::new_unchecked(conn,TransactionBehavior::Immediate).map_err(source_storage_error)?;
        if let Some(key)=delivery_key {
            if let Some(ids)=completed_capture_on_connection(&tx,authority,key)? {
                tx.commit().map_err(source_storage_error)?;
                return Ok(ids);
            }
            let valid:bool=tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM session_digest_deliveries WHERE delivery_key=?1 AND repository_id=?2 AND checkout_id=?3 AND COALESCE(branch,'unknown')=?4 AND committed_count=candidate_count)",
                params![key,authority.repository_id,authority.checkout_id,authority.branch], |r|r.get(0),
            ).map_err(source_storage_error)?;
            if !valid { return Err(LatticeError::Storage("claimed capture was retired or changed authority before consolidation committed".into())); }
        }
        for fact in facts {
            let state_hash=crate::consolidation::proposal::state_hash_for_memory(store,&fact.source_memory_id)?;
            let evidence_current:bool=tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM memory_evidence e JOIN session_digest_capture_commits c ON c.memory_id=e.memory_id
                 WHERE e.memory_id=?1 AND e.kind='session_digest' AND e.reference=?2 AND e.detail=?3
                   AND c.delivery_key=?4 AND c.candidate_idempotency_key=?2)",
                params![fact.source_memory_id,fact.evidence_reference,fact.evidence_detail,fact.source_capture_id],|row|row.get(0),
            ).map_err(source_storage_error)?;
            if state_hash!=fact.expected_state_hash || !evidence_current {
                return Err(LatticeError::Storage("session capture source changed while the provider was running; retry from current evidence".into()));
            }
        }
        if !proposals.is_empty() {
            let pending=crate::consolidation::review_queue::pending_manual_review_count_up_to(&tx,authority.repository_id,max_pending)?;
            if pending.saturating_add(proposals.len())>max_pending {
                return Err(LatticeError::Storage("session consolidation review capacity changed while provider was running; capture remains pending".into()));
            }
        }
        let mut ids=Vec::with_capacity(proposals.len());
        for proposal in proposals {
            proposal.validate_creation_authority(store,authority)?;
            tx.execute(
                "INSERT INTO consolidation_jobs(job_id,workspace_id,kind,mode,status,enqueued_at,proposal_id) VALUES(?1,?2,?3,'background','proposed',?4,?5)",
                params![proposal.job_id,authority.repository_id,SESSION_DIGEST_JOB_KIND,crate::consolidation::now_unix_micros(),proposal.proposal_id],
            ).map_err(source_storage_error)?;
            proposal.insert_pending(&tx)?;
            ids.push(proposal.proposal_id);
        }
        if let Some(key)=delivery_key {
            tx.execute(
                "INSERT INTO consolidation_capture_receipts(delivery_key,repository_id,checkout_id,branch,proposal_ids,completed_at) VALUES(?1,?2,?3,?4,?5,?6)",
                params![key,authority.repository_id,authority.checkout_id,authority.branch,serde_json::to_string(&ids).map_err(|e|LatticeError::Storage(e.to_string()))?,crate::consolidation::now_unix_micros()],
            ).map_err(source_storage_error)?;
        }
        tx.commit().map_err(source_storage_error)?;
        Ok(ids)
    }).map_err(Into::into)
}

fn is_usable_provider_key(key: &str) -> bool {
    let key = key.trim();
    key.len() >= 8
        && key.len() <= MAX_PROVIDER_KEY_BYTES
        && !key.chars().any(char::is_control)
        && !key.chars().any(char::is_whitespace)
}

fn skip(
    services: &LlmJobServices<'_>,
    repository_id: &str,
    reason: ConsolidationSkipReason,
) -> Result<SessionDigestConsolidationOutcome, LlmJobError> {
    let job_id = services.runtime.record_content_free_skip(
        repository_id,
        SESSION_DIGEST_JOB_KIND,
        reason,
    )?;
    Ok(SessionDigestConsolidationOutcome::Skipped { job_id, reason })
}

#[derive(Clone, Debug)]
struct PersistedFact {
    source_capture_id: String,
    source_memory_id: String,
    claim: String,
    evidence: SessionDigestEvidence,
    expected_state_hash: [u8; 32],
    evidence_reference: String,
    evidence_detail: String,
}

fn load_persisted_facts(
    services: &LlmJobServices<'_>,
    repository_id: &str,
    retention_window: Duration,
    limit: usize,
    delivery_key: Option<&str>,
) -> Result<Vec<PersistedFact>, LlmJobError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64;
    let retention_seconds = retention_window.as_secs().min(i64::MAX as u64) as i64;
    let cutoff = now.saturating_sub(retention_seconds);
    let limit = i64::try_from(limit)
        .map_err(|_| LlmJobError::Storage("session-digest source limit overflowed".to_string()))?;

    services
        .memory_store
        .with_connection(|conn| {
            let delivery_filter = if delivery_key.is_some() { "d.delivery_key = ?6" } else { "?6 IS NULL" };
            let query = format!("SELECT d.delivery_key,
                            c.memory_id,
                            m.content,
                            c.candidate_idempotency_key,
                            e.reference,
                            e.detail
                     FROM session_digest_capture_commits c
                     INNER JOIN session_digest_deliveries d
                        ON d.delivery_key = c.delivery_key
                     INNER JOIN memories m ON m.id = c.memory_id
                     INNER JOIN memory_evidence e
                        ON e.memory_id = c.memory_id AND e.kind = 'session_digest'
                     LEFT JOIN session_capture_tombstones t
                        ON t.delivery_key = d.delivery_key
                     WHERE d.repository_id = ?1
                       AND {delivery_filter}
                       AND d.checkout_id = ?4
                       AND COALESCE(d.branch, 'unknown') = ?5
                       AND d.created_at >= ?2
                       AND d.committed_count = d.candidate_count
                       AND m.workspace_id = ?1
                       AND m.source_query = 'automatic_session_digest'
                       AND m.is_invalidated = 0
                       AND (m.applicable_checkout_id IS NULL OR m.applicable_checkout_id = ?4)
                       AND (m.scope = 'repo' OR (m.scope = 'branch' AND m.branch = ?5))
                       AND t.delivery_key IS NULL
                     ORDER BY d.created_at DESC, c.candidate_ordinal ASC
                     LIMIT ?3");
            let mut statement = conn.prepare(&query).map_err(source_storage_error)?;
            let rows = statement
                .query_map(params![repository_id, cutoff, limit, services.authority.checkout_id, services.authority.branch, delivery_key], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                })
                .map_err(source_storage_error)?;
            let mut facts = Vec::new();
            for row in rows {
                let (
                    source_capture_id,
                    source_memory_id,
                    persisted_claim,
                    committed_reference,
                    evidence_reference,
                    evidence_detail,
                ) = row.map_err(source_storage_error)?;
                if evidence_reference.as_deref() != Some(committed_reference.as_str()) {
                    return Err(LatticeError::Storage(
                        "persisted session-digest evidence reference mismatch".to_string(),
                    ));
                }
                let evidence: SessionDigestEvidence =
                    serde_json::from_str(evidence_detail.as_deref().ok_or_else(|| {
                        LatticeError::Storage(
                            "persisted session-digest evidence is missing typed detail".to_string(),
                        )
                    })?)
                    .map_err(|error| {
                        LatticeError::Storage(format!(
                            "Failed to decode persisted session-digest evidence: {error}"
                        ))
                    })?;
                if evidence.repository_id != repository_id
                    || evidence.checkout_id.as_deref() != Some(services.authority.checkout_id)
                    || evidence.branch.as_deref().unwrap_or("unknown") != services.authority.branch
                {
                    return Err(LatticeError::Storage(
                        "persisted session-digest evidence crossed repository, checkout, or branch authority"
                            .to_string(),
                    ));
                }
                let Some(claim) = prompt_claim(&persisted_claim, &evidence) else {
                    continue;
                };
                let expected_state_hash=crate::consolidation::proposal::state_hash_for_memory(services.memory_store,&source_memory_id)?;
                facts.push(PersistedFact {
                    expected_state_hash,
                    evidence_reference: committed_reference,
                    evidence_detail: evidence_detail.expect("typed evidence detail was validated above"),
                    source_capture_id,
                    source_memory_id,
                    claim,
                    evidence,
                });
            }
            Ok(facts)
        })
        .map_err(Into::into)
}

fn source_storage_error(error: rusqlite::Error) -> LatticeError {
    LatticeError::Storage(format!(
        "Failed to load persisted session-digest facts: {error}"
    ))
}

fn prompt_claim(persisted_claim: &str, evidence: &SessionDigestEvidence) -> Option<String> {
    if let Some(check) = &evidence.check {
        return Some(format!("Check {} {}.", check.label, check.outcome.as_str()));
    }
    if let Some(error) = &evidence.resolved_error {
        return Some(format!("Resolved {} failure.", error.category));
    }
    if !evidence.edited_paths.is_empty() {
        return Some(format!(
            "Session edited {} repository file(s).",
            evidence.edited_paths.len()
        ));
    }
    if evidence.summary_hash.is_some() {
        return sanitize_text(persisted_claim, MAX_CONSOLIDATED_CONTENT_BYTES);
    }
    None
}

#[derive(Serialize)]
struct PromptFact<'a> {
    source_memory_id: &'a str,
    claim: &'a str,
    edited_paths: &'a [String],
    check: &'a Option<crate::memory::SessionDigestCheckEvidence>,
    resolved_error: Option<PromptResolvedError<'a>>,
    summary_hash: &'a Option<String>,
}

#[derive(Serialize)]
struct PromptResolvedError<'a> {
    category: &'a str,
    summary: &'a Option<String>,
}

fn build_prompt(facts: &[PersistedFact], max_proposals: usize) -> Result<String, LlmJobError> {
    let prompt_facts = facts
        .iter()
        .map(|fact| PromptFact {
            source_memory_id: &fact.source_memory_id,
            claim: &fact.claim,
            edited_paths: &fact.evidence.edited_paths,
            check: &fact.evidence.check,
            resolved_error: fact.evidence.resolved_error.as_ref().map(|error| {
                PromptResolvedError {
                    category: &error.category,
                    summary: &error.summary,
                }
            }),
            summary_hash: &fact.evidence.summary_hash,
        })
        .collect::<Vec<_>>();
    let encoded = serde_json::to_string(&prompt_facts)
        .map_err(|error| LlmJobError::Storage(format!("Failed to encode prompt facts: {error}")))?;
    Ok(format!(
        "From the persisted sanitized session facts below, propose at most {max_proposals} durable repository-level decisions or constraints. Do not infer facts not supported by the typed evidence. Return strict JSON matching the supplied schema. Facts: {encoded}"
    ))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelResponse {
    proposals: Vec<ModelCandidate>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelCandidate {
    memory_class: ProposedMemoryClass,
    content: String,
    evidence_summary: String,
    uncertainty: String,
    confidence: f64,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProposedMemoryClass {
    Decision,
    Constraint,
}

impl ProposedMemoryClass {
    fn memory_class(self) -> MemoryClass {
        match self {
            Self::Decision => MemoryClass::Decision,
            Self::Constraint => MemoryClass::Constraint,
        }
    }

    fn assertion_type(self) -> MemoryAssertionType {
        match self {
            Self::Decision => MemoryAssertionType::Decision,
            Self::Constraint => MemoryAssertionType::Constraint,
        }
    }
}

#[derive(Clone, Debug)]
struct ValidatedCandidate {
    memory_class: ProposedMemoryClass,
    content: String,
    evidence_summary: String,
    uncertainty: String,
    confidence: f64,
}

fn validate_candidates(
    ctx: &LlmJobContext,
    services: &LlmJobServices<'_>,
    response: ModelResponse,
    max_proposals: usize,
) -> Result<Vec<ValidatedCandidate>, LlmJobError> {
    if response.proposals.is_empty() || response.proposals.len() > max_proposals {
        return Err(malformed_with_event(
            ctx,
            services,
            ConsolidationJobKind::SessionDigestConsolidation,
            "session-digest consolidation returned an invalid proposal count",
        ));
    }
    response
        .proposals
        .into_iter()
        .map(|candidate| {
            let content = sanitize_text(&candidate.content, MAX_CONSOLIDATED_CONTENT_BYTES);
            let evidence_summary =
                sanitize_text(&candidate.evidence_summary, MAX_EVIDENCE_SUMMARY_BYTES);
            let uncertainty = sanitize_text(&candidate.uncertainty, MAX_UNCERTAINTY_BYTES);
            if !candidate.confidence.is_finite()
                || !(0.0..=1.0).contains(&candidate.confidence)
                || content.is_none()
                || evidence_summary.is_none()
                || uncertainty.is_none()
            {
                return Err(malformed_with_event(
                    ctx,
                    services,
                    ConsolidationJobKind::SessionDigestConsolidation,
                    "session-digest consolidation returned an unsafe proposal",
                ));
            }
            Ok(ValidatedCandidate {
                memory_class: candidate.memory_class,
                content: content.expect("checked as present"),
                evidence_summary: evidence_summary.expect("checked as present"),
                uncertainty: uncertainty.expect("checked as present"),
                confidence: candidate.confidence,
            })
        })
        .collect()
}

fn proposed_state(
    ctx: &LlmJobContext,
    branch: &str,
    candidate: &ValidatedCandidate,
    source_memory_ids: &[String],
) -> Result<serde_json::Value, LlmJobError> {
    let refresh_key = refresh_key(candidate, source_memory_ids);
    let mut memory = create_memory(
        ctx,
        branch,
        candidate.content.clone(),
        MemoryType::Decision,
        refresh_key,
        SESSION_DIGEST_JOB_KIND.to_string(),
        candidate.confidence,
    );
    memory.scope = MemoryScope::Repo;
    memory.branch = None;

    let mut fields = structured_fields(
        candidate.memory_class.assertion_type(),
        MemoryVerificationStatus::InReview,
        candidate.uncertainty.clone(),
        candidate.evidence_summary.clone(),
    );
    fields.memory_class = candidate.memory_class.memory_class();
    fields.freshness_policy = MemoryFreshnessPolicy::ManualReview;
    fields
        .provenance
        .extend(source_memory_ids.iter().map(|memory_id| MemoryProvenance {
            source: "lattice.session_digest.llm_consolidation.v1".to_string(),
            reference: Some(memory_id.clone()),
            captured_at: None,
            note: None,
        }));
    memory_state(memory, fields, Vec::new())
}

fn refresh_key(candidate: &ValidatedCandidate, source_memory_ids: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"lattice.session-digest.llm-consolidation.v1\0");
    hasher.update(candidate.memory_class.memory_class().as_str().as_bytes());
    hasher.update(candidate.content.as_bytes());
    for source_memory_id in source_memory_ids {
        hasher.update((source_memory_id.len() as u64).to_be_bytes());
        hasher.update(source_memory_id.as_bytes());
    }
    format!("session_digest_llm::{:x}", hasher.finalize())
}

const RESPONSE_SCHEMA: &str = r#"{
  "proposals": [{
    "memory_class": "decision | constraint",
    "content": "bounded repository-level assertion",
    "evidence_summary": "bounded summary grounded in supplied facts",
    "uncertainty": "bounded uncertainty statement",
    "confidence": 0.0
  }]
}"#;
