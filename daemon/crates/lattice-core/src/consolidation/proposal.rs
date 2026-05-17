use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tracing::{field, info_span};

use super::{encode_memory_state, llm::LlmProvenance};
use crate::consolidation::ConsolidationMemoryState;
use crate::error::LatticeError;
use crate::events::{
    canonical_json_bytes, Actor, BranchRef, CompactSummary, EventKind, EventPayload, EventWriter,
    MemoryConsolidatedPayload, PartialEnvelope, SessionId, StableRef,
};
use crate::identity::MemoryId;
use crate::memory::{Memory, MemoryStore};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConsolidationProposal {
    pub proposal_id: String,
    pub job_id: String,
    pub target: ProposalTarget,
    pub proposal_kind: ProposalKind,
    pub prior_state: Value,
    pub proposed_state: Value,
    pub evidence: Value,
    pub provenance: Option<LlmProvenance>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConsolidationProposalRecord {
    pub proposal_id: String,
    pub job_id: String,
    pub workspace_id: String,
    pub enqueued_at: i64,
    pub target_memory_id: Option<String>,
    pub proposal_kind: ProposalKind,
    pub prior_state: Value,
    pub proposed_state: Value,
    pub evidence: Value,
    pub provenance: Option<LlmProvenance>,
    pub decided_at: Option<i64>,
    pub decision: ProposalDecision,
    pub decided_by: Option<String>,
    pub decision_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConsolidationProposalRow {
    pub proposal_id: String,
    pub job_id: String,
    pub workspace_id: String,
    pub enqueued_at: i64,
    pub target_memory_id: Option<String>,
    pub proposal_kind: String,
    pub prior_state: String,
    pub proposed_state: String,
    pub evidence: String,
    pub provenance_json: Option<String>,
    pub decided_at: Option<i64>,
    pub decision: String,
    pub decided_by: Option<String>,
    pub decision_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalTarget {
    NewMemory,
    ExistingMemory(String),
}

impl ProposalTarget {
    pub fn memory_id(&self) -> Option<&str> {
        match self {
            Self::NewMemory => None,
            Self::ExistingMemory(memory_id) => Some(memory_id.as_str()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    CreateMemory,
    UpdateMemory,
    MarkVerified,
    MarkStale,
    MarkInvalidated,
    MarkExpired,
    Supersede,
    Demote,
    Refresh,
}

impl ProposalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CreateMemory => "create_memory",
            Self::UpdateMemory => "update_memory",
            Self::MarkVerified => "mark_verified",
            Self::MarkStale => "mark_stale",
            Self::MarkInvalidated => "mark_invalidated",
            Self::MarkExpired => "mark_expired",
            Self::Supersede => "supersede",
            Self::Demote => "demote",
            Self::Refresh => "refresh",
        }
    }

    pub fn from_str(value: &str) -> Result<Self, LatticeError> {
        match value {
            "create_memory" => Ok(Self::CreateMemory),
            "update_memory" => Ok(Self::UpdateMemory),
            "mark_verified" => Ok(Self::MarkVerified),
            "mark_stale" => Ok(Self::MarkStale),
            "mark_invalidated" => Ok(Self::MarkInvalidated),
            "mark_expired" => Ok(Self::MarkExpired),
            "supersede" => Ok(Self::Supersede),
            "demote" => Ok(Self::Demote),
            "refresh" => Ok(Self::Refresh),
            _ => Err(LatticeError::Storage(format!(
                "Unknown consolidation proposal kind '{value}'"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalDecision {
    Pending,
    Applied,
    Rejected,
    Reverted,
}

impl ProposalDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applied => "applied",
            Self::Rejected => "rejected",
            Self::Reverted => "reverted",
        }
    }

    pub fn from_str(value: &str) -> Result<Self, LatticeError> {
        match value {
            "pending" => Ok(Self::Pending),
            "applied" => Ok(Self::Applied),
            "rejected" => Ok(Self::Rejected),
            "reverted" => Ok(Self::Reverted),
            _ => Err(LatticeError::Storage(format!(
                "Unknown consolidation proposal decision '{value}'"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied { memory_id: String },
    AlreadyDecided { decision: ProposalDecision },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RejectOutcome {
    Rejected,
    AlreadyDecided { decision: ProposalDecision },
}

impl ConsolidationProposal {
    pub fn load_record(
        conn: &Connection,
        proposal_id: &str,
    ) -> Result<Option<ConsolidationProposalRecord>, LatticeError> {
        conn.query_row(
            "SELECT p.proposal_id,
                    p.job_id,
                    j.workspace_id,
                    j.enqueued_at,
                    p.target_memory_id,
                    p.proposal_kind,
                    p.prior_state,
                    p.proposed_state,
                    p.evidence,
                    p.provenance_json,
                    p.decided_at,
                    p.decision,
                    p.decided_by,
                    p.decision_reason
             FROM consolidation_proposals p
             INNER JOIN consolidation_jobs j ON j.job_id = p.job_id
             WHERE p.proposal_id = ?1",
            params![proposal_id],
            |row| {
                Ok(ProposalRecordRow {
                    proposal_id: row.get(0)?,
                    job_id: row.get(1)?,
                    workspace_id: row.get(2)?,
                    enqueued_at: row.get(3)?,
                    target_memory_id: row.get(4)?,
                    proposal_kind: row.get(5)?,
                    prior_state: row.get(6)?,
                    proposed_state: row.get(7)?,
                    evidence: row.get(8)?,
                    provenance_json: row.get(9)?,
                    decided_at: row.get(10)?,
                    decision: row.get(11)?,
                    decided_by: row.get(12)?,
                    decision_reason: row.get(13)?,
                })
            },
        )
        .optional()
        .map_err(|e| LatticeError::Storage(format!("Failed to load proposal record: {e}")))?
        .map(ProposalRecordRow::into_record)
        .transpose()
    }

    pub fn insert_pending(&self, conn: &Connection) -> Result<(), LatticeError> {
        let prior_state = serde_json::to_string(&self.prior_state).map_err(json_error)?;
        let proposed_state = serde_json::to_string(&self.proposed_state).map_err(json_error)?;
        let evidence = serde_json::to_string(&self.evidence).map_err(json_error)?;
        let provenance = self
            .provenance
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(json_error)?;
        conn.execute(
            "INSERT INTO consolidation_proposals
                (proposal_id, job_id, target_memory_id, proposal_kind, prior_state,
                 proposed_state, evidence, provenance_json, decision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending')",
            params![
                self.proposal_id,
                self.job_id,
                self.target.memory_id(),
                self.proposal_kind.as_str(),
                prior_state,
                proposed_state,
                evidence,
                provenance
            ],
        )
        .map_err(|e| LatticeError::Storage(format!("Failed to insert proposal: {e}")))?;
        Ok(())
    }

    pub fn load(conn: &Connection, proposal_id: &str) -> Result<Option<Self>, LatticeError> {
        let row = conn
            .query_row(
                "SELECT p.proposal_id,
                        p.job_id,
                        p.target_memory_id,
                        p.proposal_kind,
                        p.prior_state,
                        p.proposed_state,
                        p.evidence,
                        p.provenance_json
                 FROM consolidation_proposals p
                 WHERE p.proposal_id = ?1",
                params![proposal_id],
                |row| {
                    Ok(ProposalRow {
                        proposal_id: row.get(0)?,
                        job_id: row.get(1)?,
                        target_memory_id: row.get(2)?,
                        proposal_kind: row.get(3)?,
                        prior_state: row.get(4)?,
                        proposed_state: row.get(5)?,
                        evidence: row.get(6)?,
                        provenance_json: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(|e| LatticeError::Storage(format!("Failed to load proposal: {e}")))?;
        row.map(ProposalRow::into_proposal).transpose()
    }

    pub fn apply(
        &self,
        conn: &Connection,
        memory_store: &MemoryStore,
        event_writer: &EventWriter,
        decided_by: &str,
        decision_reason: Option<&str>,
    ) -> Result<ApplyOutcome, LatticeError> {
        let span = info_span!(
            "consolidation.proposal.apply",
            workspace_id = event_writer.workspace_id().as_str(),
            job_id = self.job_id.as_str(),
            proposal_id = self.proposal_id.as_str(),
            outcome = field::Empty
        );
        let _entered = span.enter();
        let decision = load_decision(conn, &self.proposal_id)?;
        if decision != ProposalDecision::Pending {
            span.record("outcome", "already_decided");
            return Ok(ApplyOutcome::AlreadyDecided { decision });
        }

        let memory = self.materialize(memory_store)?;
        let post_apply_state_hash = state_hash_for_memory(memory_store, &memory.id)?;
        let memory_id = memory.id.clone();
        self.emit_event(
            event_writer,
            &memory_id,
            post_apply_state_hash,
            decided_by,
            decision_reason,
        )?;
        mark_decided(
            conn,
            &self.proposal_id,
            ProposalDecision::Applied,
            decided_by,
            decision_reason,
        )?;
        mark_job_status(conn, &self.job_id, "applied", Some(&self.proposal_id), None)?;
        span.record("outcome", "applied");
        Ok(ApplyOutcome::Applied { memory_id })
    }

    pub fn reject(
        &self,
        conn: &Connection,
        event_writer: &EventWriter,
        decided_by: &str,
        decision_reason: Option<&str>,
    ) -> Result<RejectOutcome, LatticeError> {
        let span = info_span!(
            "consolidation.proposal.reject",
            workspace_id = event_writer.workspace_id().as_str(),
            job_id = self.job_id.as_str(),
            proposal_id = self.proposal_id.as_str(),
            outcome = field::Empty
        );
        let _entered = span.enter();
        let decision = load_decision(conn, &self.proposal_id)?;
        if decision != ProposalDecision::Pending {
            span.record("outcome", "already_decided");
            return Ok(RejectOutcome::AlreadyDecided { decision });
        }

        mark_decided(
            conn,
            &self.proposal_id,
            ProposalDecision::Rejected,
            decided_by,
            decision_reason,
        )?;
        mark_job_status(
            conn,
            &self.job_id,
            "rejected",
            Some(&self.proposal_id),
            None,
        )?;
        span.record("outcome", "rejected");
        Ok(RejectOutcome::Rejected)
    }

    fn materialize(&self, memory_store: &MemoryStore) -> Result<Memory, LatticeError> {
        match self.proposal_kind {
            ProposalKind::CreateMemory
            | ProposalKind::UpdateMemory
            | ProposalKind::MarkVerified
            | ProposalKind::MarkInvalidated
            | ProposalKind::MarkExpired
            | ProposalKind::Refresh => self.materialize_memory_state(memory_store),
            ProposalKind::MarkStale => self.materialize_stale(memory_store),
            ProposalKind::Supersede => self.materialize_supersession(memory_store),
            ProposalKind::Demote => self.materialize_memory_state(memory_store),
        }
    }

    fn materialize_memory_state(&self, memory_store: &MemoryStore) -> Result<Memory, LatticeError> {
        if let Some(state) = proposed_memory_state(&self.proposed_state)? {
            apply_memory_state(memory_store, &state)?;
            return Ok(state.memory);
        }

        let memory = proposed_memory(&self.proposed_state)?;
        memory_store.store(memory.clone())?;
        Ok(memory)
    }

    fn materialize_stale(&self, memory_store: &MemoryStore) -> Result<Memory, LatticeError> {
        if proposed_memory_state(&self.proposed_state)?.is_some() {
            return self.materialize_memory_state(memory_store);
        }
        let target_id = required_target_id(&self.target)?;
        let reason = self
            .proposed_state
            .get("stale_reason")
            .and_then(Value::as_str)
            .unwrap_or("marked stale by consolidation proposal");
        memory_store.mark_stale_by_id(target_id, reason)?;
        memory_store
            .get_by_id(target_id)?
            .ok_or_else(|| missing_memory(target_id))
    }

    fn materialize_supersession(&self, memory_store: &MemoryStore) -> Result<Memory, LatticeError> {
        if proposed_memory_state(&self.proposed_state)?.is_some() {
            return self.materialize_memory_state(memory_store);
        }
        let target_id = required_target_id(&self.target)?;
        let superseded_by = self
            .proposed_state
            .get("superseded_by_memory_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                LatticeError::Storage("supersede proposal missing superseded_by_memory_id".into())
            })?;
        memory_store.mark_memory_superseded(target_id, superseded_by)?;
        memory_store
            .get_by_id(target_id)?
            .ok_or_else(|| missing_memory(target_id))
    }

    pub(crate) fn emit_event(
        &self,
        event_writer: &EventWriter,
        memory: &str,
        post_apply_state_hash: [u8; 32],
        decided_by: &str,
        decision_reason: Option<&str>,
    ) -> Result<(), LatticeError> {
        let memory_id = memory_identity(event_writer.workspace_id(), memory);
        let payload = EventPayload::MemoryConsolidated(MemoryConsolidatedPayload {
            source_memory_ids: source_memory_ids(event_writer.workspace_id(), &self.evidence),
            consolidated_memory_id: memory_id.clone(),
            source_event_ids: Vec::new(),
            consolidation_summary: consolidation_summary(self),
            proposal_id: Some(self.proposal_id.clone()),
            prior_state_json: Some(self.prior_state.to_string()),
            proposed_state_json: Some(self.proposed_state.to_string()),
            decided_by: Some(decided_by.to_string()),
            decision_reason: decision_reason.map(str::to_string),
            post_apply_state_hash,
        });
        let envelope = PartialEnvelope {
            workspace_id: Some(event_writer.workspace_id().clone()),
            branch: BranchRef {
                name: "main".to_string(),
            },
            session_id: SessionId {
                value: format!("consolidation-{}", self.job_id),
            },
            task_id: None,
            actor: Actor::Daemon,
            kind: EventKind::MemoryConsolidated,
            references: vec![StableRef::MemoryRef(memory_id)],
            summary: CompactSummary::new(consolidation_summary(self))
                .map_err(|e| LatticeError::Storage(e.to_string()))?,
            payload,
        };
        event_writer.append(envelope).map_err(|e| {
            LatticeError::Storage(format!("Failed to write consolidation event: {e}"))
        })?;
        Ok(())
    }
}

struct ProposalRow {
    proposal_id: String,
    job_id: String,
    target_memory_id: Option<String>,
    proposal_kind: String,
    prior_state: String,
    proposed_state: String,
    evidence: String,
    provenance_json: Option<String>,
}

struct ProposalRecordRow {
    proposal_id: String,
    job_id: String,
    workspace_id: String,
    enqueued_at: i64,
    target_memory_id: Option<String>,
    proposal_kind: String,
    prior_state: String,
    proposed_state: String,
    evidence: String,
    provenance_json: Option<String>,
    decided_at: Option<i64>,
    decision: String,
    decided_by: Option<String>,
    decision_reason: Option<String>,
}

impl ProposalRow {
    fn into_proposal(self) -> Result<ConsolidationProposal, LatticeError> {
        Ok(ConsolidationProposal {
            proposal_id: self.proposal_id,
            job_id: self.job_id,
            target: match self.target_memory_id {
                Some(memory_id) => ProposalTarget::ExistingMemory(memory_id),
                None => ProposalTarget::NewMemory,
            },
            proposal_kind: ProposalKind::from_str(&self.proposal_kind)?,
            prior_state: serde_json::from_str(&self.prior_state).map_err(json_error)?,
            proposed_state: serde_json::from_str(&self.proposed_state).map_err(json_error)?,
            evidence: serde_json::from_str(&self.evidence).map_err(json_error)?,
            provenance: self
                .provenance_json
                .map(|json| serde_json::from_str(&json).map_err(json_error))
                .transpose()?,
        })
    }
}

impl ProposalRecordRow {
    fn into_record(self) -> Result<ConsolidationProposalRecord, LatticeError> {
        Ok(ConsolidationProposalRecord {
            proposal_id: self.proposal_id,
            job_id: self.job_id,
            workspace_id: self.workspace_id,
            enqueued_at: self.enqueued_at,
            target_memory_id: self.target_memory_id,
            proposal_kind: ProposalKind::from_str(&self.proposal_kind)?,
            prior_state: serde_json::from_str(&self.prior_state).map_err(json_error)?,
            proposed_state: serde_json::from_str(&self.proposed_state).map_err(json_error)?,
            evidence: serde_json::from_str(&self.evidence).map_err(json_error)?,
            provenance: self
                .provenance_json
                .map(|json| serde_json::from_str(&json).map_err(json_error))
                .transpose()?,
            decided_at: self.decided_at,
            decision: ProposalDecision::from_str(&self.decision)?,
            decided_by: self.decided_by,
            decision_reason: self.decision_reason,
        })
    }
}

pub fn load_row(
    conn: &Connection,
    proposal_id: &str,
) -> Result<Option<ConsolidationProposalRow>, LatticeError> {
    conn.query_row(
        "SELECT p.proposal_id,
                p.job_id,
                j.workspace_id,
                j.enqueued_at,
                p.target_memory_id,
                p.proposal_kind,
                p.prior_state,
                p.proposed_state,
                p.evidence,
                p.provenance_json,
                p.decided_at,
                p.decision,
                p.decided_by,
                p.decision_reason
         FROM consolidation_proposals p
         INNER JOIN consolidation_jobs j ON j.job_id = p.job_id
         WHERE p.proposal_id = ?1",
        params![proposal_id],
        |row| {
            Ok(ConsolidationProposalRow {
                proposal_id: row.get(0)?,
                job_id: row.get(1)?,
                workspace_id: row.get(2)?,
                enqueued_at: row.get(3)?,
                target_memory_id: row.get(4)?,
                proposal_kind: row.get(5)?,
                prior_state: row.get(6)?,
                proposed_state: row.get(7)?,
                evidence: row.get(8)?,
                provenance_json: row.get(9)?,
                decided_at: row.get(10)?,
                decision: row.get(11)?,
                decided_by: row.get(12)?,
                decision_reason: row.get(13)?,
            })
        },
    )
    .optional()
    .map_err(|e| LatticeError::Storage(format!("Failed to load proposal row: {e}")))
}

fn load_decision(conn: &Connection, proposal_id: &str) -> Result<ProposalDecision, LatticeError> {
    let decision: String = conn
        .query_row(
            "SELECT decision FROM consolidation_proposals WHERE proposal_id = ?1",
            params![proposal_id],
            |row| row.get(0),
        )
        .map_err(|e| LatticeError::Storage(format!("Failed to load proposal decision: {e}")))?;
    ProposalDecision::from_str(&decision)
}

fn mark_decided(
    conn: &Connection,
    proposal_id: &str,
    decision: ProposalDecision,
    decided_by: &str,
    decision_reason: Option<&str>,
) -> Result<(), LatticeError> {
    conn.execute(
        "UPDATE consolidation_proposals
         SET decision = ?1, decided_at = ?2, decided_by = ?3, decision_reason = ?4
         WHERE proposal_id = ?5 AND decision = 'pending'",
        params![
            decision.as_str(),
            crate::consolidation::now_unix_micros(),
            decided_by,
            decision_reason,
            proposal_id
        ],
    )
    .map_err(|e| LatticeError::Storage(format!("Failed to update proposal decision: {e}")))?;
    Ok(())
}

pub(crate) fn mark_job_status(
    conn: &Connection,
    job_id: &str,
    status: &str,
    proposal_id: Option<&str>,
    error_kind: Option<&str>,
) -> Result<(), LatticeError> {
    conn.execute(
        "UPDATE consolidation_jobs
         SET status = ?1,
             finished_at = CASE WHEN ?1 IN ('proposed', 'applied', 'rejected', 'failed') THEN ?2 ELSE finished_at END,
             proposal_id = COALESCE(?3, proposal_id),
             error_kind = ?4
         WHERE job_id = ?5",
        params![
            status,
            crate::consolidation::now_unix_micros(),
            proposal_id,
            error_kind,
            job_id
        ],
    )
    .map_err(|e| LatticeError::Storage(format!("Failed to update consolidation job: {e}")))?;
    Ok(())
}

fn proposed_memory(value: &Value) -> Result<Memory, LatticeError> {
    serde_json::from_value(value.clone()).map_err(json_error)
}

fn proposed_memory_state(
    value: &Value,
) -> Result<Option<crate::consolidation::ConsolidationMemoryState>, LatticeError> {
    if value.get("memory").is_none() || value.get("structured_fields").is_none() {
        return Ok(None);
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(json_error)
}

fn required_target_id(target: &ProposalTarget) -> Result<&str, LatticeError> {
    target.memory_id().ok_or_else(|| {
        LatticeError::Storage("proposal kind requires an existing target memory".to_string())
    })
}

fn missing_memory(memory_id: &str) -> LatticeError {
    LatticeError::Storage(format!("Memory '{memory_id}' not found or invalidated"))
}

fn source_memory_ids(workspace_id: &str, evidence: &Value) -> Vec<MemoryId> {
    evidence
        .get("source_memory_ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|id| memory_identity(workspace_id, id))
        .collect()
}

fn memory_identity(workspace_id: &str, memory_id: &str) -> MemoryId {
    MemoryId {
        workspace_id: workspace_id.to_string(),
        ulid: memory_id.to_string(),
    }
}

fn consolidation_summary(proposal: &ConsolidationProposal) -> String {
    let target = proposal.target.memory_id().unwrap_or("new memory");
    format!(
        "consolidation proposal {} applied to {target}",
        proposal.proposal_id
    )
}

fn json_error(error: serde_json::Error) -> LatticeError {
    LatticeError::Storage(format!("Failed to process proposal JSON: {error}"))
}

pub(crate) fn proposal_from_pending(
    job_id: &str,
    pending: crate::consolidation::PendingProposalSpec,
) -> ConsolidationProposal {
    ConsolidationProposal {
        proposal_id: pending.proposal_id,
        job_id: job_id.to_string(),
        target: match pending.target_memory_id {
            Some(memory_id) => ProposalTarget::ExistingMemory(memory_id),
            None => ProposalTarget::NewMemory,
        },
        proposal_kind: pending.proposal_kind,
        prior_state: pending.prior_state,
        proposed_state: pending.proposed_state,
        evidence: pending.evidence,
        provenance: pending.provenance,
    }
}

pub fn empty_state() -> Value {
    json!({})
}

pub(crate) fn apply_memory_state(
    memory_store: &MemoryStore,
    state: &ConsolidationMemoryState,
) -> Result<(), LatticeError> {
    memory_store.store(state.memory.clone())?;
    memory_store.update_structured_fields(&state.memory.id, &state.structured_fields)?;
    memory_store.delete_memory_links_from(&state.memory.id)?;
    for link in &state.memory_links {
        memory_store.insert_memory_link(link)?;
    }
    match state.last_verified_at {
        Some(last_verified_at) => {
            memory_store.set_last_verified_at(&state.memory.id, last_verified_at)?
        }
        None => memory_store.clear_last_verified_at(&state.memory.id)?,
    }
    match state.last_verified_graph_snapshot_id {
        Some(snapshot_id) => {
            memory_store.set_last_verified_graph_snapshot_id(&state.memory.id, snapshot_id)?
        }
        None => memory_store.clear_last_verified_graph_snapshot_id(&state.memory.id)?,
    }
    match state.expires_at {
        Some(expires_at) => memory_store.set_expires_at(&state.memory.id, expires_at)?,
        None => memory_store.clear_expires_at(&state.memory.id)?,
    }
    Ok(())
}

pub(crate) fn state_hash_for_memory(
    memory_store: &MemoryStore,
    memory_id: &str,
) -> Result<[u8; 32], LatticeError> {
    let state = memory_store
        .get_by_id(memory_id)?
        .ok_or_else(|| missing_memory(memory_id))?;
    let state = crate::consolidation::capture_memory_state(memory_store, &state)?;
    state_hash_from_json(&encode_memory_state(&state))
}

pub(crate) fn state_hash_from_json(value: &Value) -> Result<[u8; 32], LatticeError> {
    let canonical = canonical_json_bytes(value).map_err(|e| {
        LatticeError::Storage(format!("Failed to canonicalize state hash payload: {e}"))
    })?;
    let digest = Sha256::digest(&canonical);
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(&digest);
    Ok(hash)
}
