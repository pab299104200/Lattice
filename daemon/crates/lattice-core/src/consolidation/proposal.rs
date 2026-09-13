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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvolutionAuthority<'a> {
    pub repository_id: &'a str,
    pub checkout_id: &'a str,
    pub branch: &'a str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutboxDrainReport {
    pub delivered: usize,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredEventEnvelope {
    workspace_id: String,
    branch: BranchRef,
    session_id: SessionId,
    actor: Actor,
    kind: EventKind,
    references: Vec<StableRef>,
    summary: CompactSummary,
    payload: EventPayload,
}

impl StoredEventEnvelope {
    fn into_partial(self) -> PartialEnvelope {
        PartialEnvelope {
            workspace_id: Some(self.workspace_id),
            branch: self.branch,
            session_id: self.session_id,
            task_id: None,
            actor: self.actor,
            kind: self.kind,
            references: self.references,
            summary: self.summary,
            payload: self.payload,
        }
    }
}

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
    pub(crate) fn validate_creation_authority(
        &self,
        memory_store: &MemoryStore,
        authority: &EvolutionAuthority<'_>,
    ) -> Result<(), LatticeError> {
        let mut ids = self
            .evidence
            .get("source_memory_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        if let Some(id) = self.target.memory_id() {
            ids.push(id);
        }
        if let Some(id) = self
            .evidence
            .get("superseded_by_memory_id")
            .and_then(Value::as_str)
        {
            ids.push(id);
        }
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            let memory = memory_store.get_by_id(id)?.ok_or_else(|| {
                LatticeError::Storage(format!(
                    "proposal '{}' references missing memory '{id}'",
                    self.proposal_id
                ))
            })?;
            verify_memory_authority(memory_store, &memory, authority, "proposal source")?;
        }
        if let Some(state) = proposed_memory_state(&self.proposed_state)? {
            if state.memory.workspace_id.as_deref() != Some(authority.repository_id)
                || (state.memory.scope == crate::memory::MemoryScope::Branch
                    && state.memory.branch.as_deref() != Some(authority.branch))
            {
                return Err(LatticeError::Storage(format!(
                    "proposal '{}' proposed state is outside explicit authority",
                    self.proposal_id
                )));
            }
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn apply(
        &self,
        _conn: &Connection,
        memory_store: &MemoryStore,
        event_writer: &EventWriter,
        decided_by: &str,
        decision_reason: Option<&str>,
    ) -> Result<ApplyOutcome, LatticeError> {
        let mut proposal = self.clone();
        if let Some(evidence) = proposal.evidence.as_object_mut() {
            evidence
                .entry("repository_id")
                .or_insert_with(|| Value::String(event_writer.workspace_id().clone()));
            evidence
                .entry("checkout_id")
                .or_insert_with(|| Value::String(String::new()));
        }
        let authority = EvolutionAuthority {
            repository_id: event_writer.workspace_id(),
            checkout_id: "",
            branch: "main",
        };
        let outcome = memory_store.with_connection(|conn| {
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| LatticeError::Storage(e.to_string()))?;
            let outcome = proposal.apply_transactional(
                &tx,
                memory_store,
                &authority,
                decided_by,
                decision_reason,
            )?;
            tx.commit()
                .map_err(|e| LatticeError::Storage(e.to_string()))?;
            Ok(outcome)
        })?;
        drain_event_outbox(memory_store, event_writer, 64)?;
        Ok(outcome)
    }

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
        super::proposal_references::validate_proposal_payload(
            &self.prior_state,
            &self.proposed_state,
            &self.evidence,
        )?;
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

    pub fn apply_transactional(
        &self,
        conn: &Connection,
        memory_store: &MemoryStore,
        authority: &EvolutionAuthority<'_>,
        decided_by: &str,
        decision_reason: Option<&str>,
    ) -> Result<ApplyOutcome, LatticeError> {
        let same_connection =
            memory_store.with_connection(|memory_conn| Ok(std::ptr::eq(memory_conn, conn)))?;
        if !same_connection || conn.is_autocommit() {
            return Err(LatticeError::Storage(
                "proposal apply requires an active transaction on the MemoryStore connection"
                    .into(),
            ));
        }
        let span = info_span!(
            "consolidation.proposal.apply",
            workspace_id = authority.repository_id,
            job_id = self.job_id.as_str(),
            proposal_id = self.proposal_id.as_str(),
            outcome = field::Empty
        );
        let _entered = span.enter();
        self.verify_proposal_authority(conn, authority)?;
        let decision = load_decision(conn, &self.proposal_id)?;
        if decision != ProposalDecision::Pending {
            span.record("outcome", "already_decided");
            return Ok(ApplyOutcome::AlreadyDecided { decision });
        }

        self.verify_target_unchanged(memory_store, authority)?;
        self.verify_replacement_unchanged(memory_store, authority)?;

        let memory = self.materialize(memory_store)?;
        let post_apply_state_hash = state_hash_for_memory(memory_store, &memory.id)?;
        let memory_id = memory.id.clone();
        self.enqueue_event(
            conn,
            authority.repository_id,
            authority.branch,
            "applied",
            &memory_id,
            post_apply_state_hash,
            decided_by,
            decision_reason,
        )?;
        if !mark_decided(
            conn,
            &self.proposal_id,
            ProposalDecision::Applied,
            decided_by,
            decision_reason,
        )? {
            return Err(LatticeError::Storage(format!(
                "Proposal '{}' lost the decision race",
                self.proposal_id
            )));
        }
        if !mark_job_status(conn, &self.job_id, "applied", Some(&self.proposal_id), None)? {
            return Err(LatticeError::Storage(format!(
                "Proposal '{}' references a missing consolidation job",
                self.proposal_id
            )));
        }
        span.record("outcome", "applied");
        Ok(ApplyOutcome::Applied { memory_id })
    }

    fn verify_target_unchanged(
        &self,
        memory_store: &MemoryStore,
        authority: &EvolutionAuthority<'_>,
    ) -> Result<(), LatticeError> {
        let Some(target_id) = self.target.memory_id() else {
            return Ok(());
        };
        let current = memory_store.get_by_id(target_id)?.ok_or_else(|| {
            LatticeError::Storage(format!(
                "Proposal '{}' is stale because target memory '{}' no longer exists",
                self.proposal_id, target_id
            ))
        })?;
        verify_memory_authority(memory_store, &current, authority, "source")?;
        let prior_state = proposed_memory_state(&self.prior_state)?.ok_or_else(|| {
            LatticeError::Storage(format!(
                "Proposal '{}' lacks the canonical target snapshot required for apply-time CAS; re-propose it",
                self.proposal_id
            ))
        })?;
        let proposed = match proposed_memory_state(&self.proposed_state)? {
            Some(state) => state.memory,
            None => proposed_memory(&self.proposed_state)?,
        };
        if proposed.id != target_id {
            return Err(LatticeError::Storage(format!(
                "Proposal '{}' targets memory '{}' but proposes state for memory '{}'",
                self.proposal_id, target_id, proposed.id
            )));
        }
        let current_state = crate::consolidation::capture_memory_state(memory_store, &current)?;
        if current_state != prior_state {
            return Err(LatticeError::Storage(format!(
                "Proposal '{}' is stale because target memory '{}' changed after proposal creation",
                self.proposal_id, target_id
            )));
        }
        Ok(())
    }

    fn verify_replacement_unchanged(
        &self,
        memory_store: &MemoryStore,
        authority: &EvolutionAuthority<'_>,
    ) -> Result<(), LatticeError> {
        if self.proposal_kind != ProposalKind::Supersede {
            return Ok(());
        }
        let replacement_id = self
            .evidence
            .get("superseded_by_memory_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                LatticeError::Storage(
                    "supersede proposal lacks replacement identity evidence".into(),
                )
            })?;
        let expected: [u8; 32] = serde_json::from_value(
            self.evidence
                .get("replacement_state_hash")
                .cloned()
                .ok_or_else(|| {
                    LatticeError::Storage(
                        "supersede proposal lacks replacement state hash evidence".into(),
                    )
                })?,
        )
        .map_err(json_error)?;
        let current = memory_store.get_by_id(replacement_id)?.ok_or_else(|| {
            LatticeError::Storage(format!(
                "Proposal '{}' is stale because replacement memory '{}' no longer exists",
                self.proposal_id, replacement_id
            ))
        })?;
        verify_memory_authority(memory_store, &current, authority, "replacement")?;
        if state_hash_for_memory(memory_store, &current.id)? != expected {
            return Err(LatticeError::Storage(format!("Proposal '{}' is stale because replacement memory '{}' changed after proposal creation", self.proposal_id, replacement_id)));
        }
        Ok(())
    }

    fn verify_proposal_authority(
        &self,
        conn: &Connection,
        authority: &EvolutionAuthority<'_>,
    ) -> Result<(), LatticeError> {
        let workspace: String = conn.query_row("SELECT j.workspace_id FROM consolidation_proposals p JOIN consolidation_jobs j ON j.job_id=p.job_id WHERE p.proposal_id=?1", [&self.proposal_id], |r| r.get(0))
            .map_err(|e| LatticeError::Storage(format!("Failed to load proposal authority: {e}")))?;
        if workspace != authority.repository_id {
            return Err(LatticeError::Storage(format!(
                "Proposal '{}' belongs to repository authority '{}'",
                self.proposal_id, workspace
            )));
        }
        let evidence_repository = self
            .evidence
            .get("repository_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                LatticeError::Storage(format!(
                    "Proposal '{}' lacks repository authority evidence",
                    self.proposal_id
                ))
            })?;
        let evidence_checkout = self
            .evidence
            .get("checkout_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                LatticeError::Storage(format!(
                    "Proposal '{}' lacks checkout authority evidence",
                    self.proposal_id
                ))
            })?;
        if evidence_repository != authority.repository_id
            || evidence_checkout != authority.checkout_id
        {
            return Err(LatticeError::Storage(format!(
                "Proposal '{}' authority does not match repository/checkout decision authority",
                self.proposal_id
            )));
        }
        Ok(())
    }

    pub fn reject_transactional(
        &self,
        conn: &Connection,
        authority: &EvolutionAuthority<'_>,
        decided_by: &str,
        decision_reason: Option<&str>,
    ) -> Result<RejectOutcome, LatticeError> {
        if conn.is_autocommit() {
            return Err(LatticeError::Storage(
                "proposal rejection requires an active transaction".into(),
            ));
        }
        let span = info_span!(
            "consolidation.proposal.reject",
            workspace_id = authority.repository_id,
            job_id = self.job_id.as_str(),
            proposal_id = self.proposal_id.as_str(),
            outcome = field::Empty
        );
        let _entered = span.enter();
        self.verify_proposal_authority(conn, authority)?;
        let decision = load_decision(conn, &self.proposal_id)?;
        if decision != ProposalDecision::Pending {
            span.record("outcome", "already_decided");
            return Ok(RejectOutcome::AlreadyDecided { decision });
        }

        if !mark_decided(
            conn,
            &self.proposal_id,
            ProposalDecision::Rejected,
            decided_by,
            decision_reason,
        )? {
            return Err(LatticeError::Storage(format!(
                "Proposal '{}' lost the decision race",
                self.proposal_id
            )));
        }
        if !mark_job_status(
            conn,
            &self.job_id,
            "rejected",
            Some(&self.proposal_id),
            None,
        )? {
            return Err(LatticeError::Storage(format!(
                "Proposal '{}' references a missing consolidation job",
                self.proposal_id
            )));
        }
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

    pub(crate) fn enqueue_event(
        &self,
        conn: &Connection,
        workspace_id: &str,
        branch: &str,
        decision_domain: &str,
        memory: &str,
        post_apply_state_hash: [u8; 32],
        decided_by: &str,
        decision_reason: Option<&str>,
    ) -> Result<(), LatticeError> {
        let memory_id = memory_identity(workspace_id, memory);
        let payload = EventPayload::MemoryConsolidated(MemoryConsolidatedPayload {
            source_memory_ids: source_memory_ids(workspace_id, &self.evidence),
            consolidated_memory_id: memory_id.clone(),
            source_event_ids: Vec::new(),
            consolidation_summary: consolidation_summary(self),
            proposal_id: Some(self.proposal_id.clone()),
            transition: Some(decision_domain.to_string()),
            prior_state_json: None,
            proposed_state_json: None,
            decided_by: Some(decided_by.to_string()),
            decision_reason: decision_reason.map(str::to_string),
            post_apply_state_hash,
        });
        let envelope = StoredEventEnvelope {
            workspace_id: workspace_id.to_string(),
            branch: BranchRef {
                name: branch.to_string(),
            },
            session_id: SessionId {
                value: format!("consolidation-{}", self.job_id),
            },
            actor: Actor::Daemon,
            kind: EventKind::MemoryConsolidated,
            references: vec![StableRef::MemoryRef(memory_id)],
            summary: CompactSummary::new(consolidation_summary(self))
                .map_err(|e| LatticeError::Storage(e.to_string()))?,
            payload,
        };
        let event_uuid = deterministic_event_uuid(&self.proposal_id, decision_domain);
        let ts = crate::consolidation::now_unix_micros();
        let envelope_json = serde_json::to_string(&envelope).map_err(json_error)?;
        conn.execute("INSERT INTO consolidation_event_outbox(outbox_id,proposal_id,transition,workspace_id,event_uuid,event_ts_unix_micros,envelope_json,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?6)", params![format!("{decision_domain}:{}",self.proposal_id), self.proposal_id, decision_domain, workspace_id, event_uuid, ts, envelope_json])
            .map_err(|e| LatticeError::Storage(format!("Failed to enqueue consolidation event: {e}")))?;
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
) -> Result<bool, LatticeError> {
    let changed = conn
        .execute(
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
    Ok(changed == 1)
}

pub(crate) fn mark_job_status(
    conn: &Connection,
    job_id: &str,
    status: &str,
    proposal_id: Option<&str>,
    error_kind: Option<&str>,
) -> Result<bool, LatticeError> {
    let changed = conn.execute(
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
    Ok(changed == 1)
}

fn verify_memory_authority(
    memory_store: &MemoryStore,
    memory: &Memory,
    authority: &EvolutionAuthority<'_>,
    role: &str,
) -> Result<(), LatticeError> {
    if memory.workspace_id.as_deref() != Some(authority.repository_id) {
        return Err(LatticeError::Storage(format!(
            "{role} memory '{}' is outside repository authority '{}'",
            memory.id, authority.repository_id
        )));
    }
    if memory.scope == crate::memory::MemoryScope::Branch
        && memory.branch.as_deref() != Some(authority.branch)
    {
        return Err(LatticeError::Storage(format!(
            "{role} memory '{}' is outside branch authority '{}'",
            memory.id, authority.branch
        )));
    }
    let checkout: Option<String> = memory_store.with_connection(|conn| {
        conn.query_row(
            "SELECT applicable_checkout_id FROM memories WHERE id=?1 AND is_invalidated=0",
            [&memory.id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| {
            LatticeError::Storage(format!("Failed to load {role} checkout authority: {e}"))
        })
        .and_then(|value| {
            value.ok_or_else(|| {
                LatticeError::Storage(format!(
                    "{role} memory '{}' is deleted or invalidated",
                    memory.id
                ))
            })
        })
    })?;
    if checkout
        .as_deref()
        .is_some_and(|required| required != authority.checkout_id)
    {
        return Err(LatticeError::Storage(format!(
            "{role} memory '{}' is outside checkout authority '{}'",
            memory.id, authority.checkout_id
        )));
    }
    Ok(())
}

fn deterministic_event_uuid(proposal_id: &str, decision_domain: &str) -> String {
    let digest =
        Sha256::digest(format!("lattice:consolidation:{decision_domain}:{proposal_id}").as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    crate::events::writer::encode_ulid(u128::from_be_bytes(bytes) & ((1_u128 << 126) - 1))
}

/// Publish a bounded batch of transactionally committed consolidation events.
/// Failed rows stay pending and are safe to retry after restart.
pub fn drain_event_outbox(
    memory_store: &MemoryStore,
    event_writer: &EventWriter,
    limit: usize,
) -> Result<OutboxDrainReport, LatticeError> {
    let limit = limit.clamp(1, 256) as i64;
    let mut rows: Vec<(String, String, String, i64, String)> = memory_store.with_connection(|conn| {
        let mut statement = conn.prepare("SELECT o.outbox_id,o.workspace_id,o.event_uuid,o.event_ts_unix_micros,o.envelope_json FROM consolidation_event_outbox o WHERE o.delivered_at IS NULL AND o.workspace_id=?1 AND (o.transition!='reverted' OR NOT EXISTS (SELECT 1 FROM consolidation_event_outbox predecessor WHERE predecessor.proposal_id=o.proposal_id AND predecessor.transition='applied')) ORDER BY o.attempt_count,o.created_at,o.outbox_id LIMIT ?2")
            .map_err(|e| LatticeError::Storage(format!("Failed to prepare consolidation outbox drain: {e}")))?;
        let rows = statement.query_map(params![event_writer.workspace_id(), limit + 1], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))
            .map_err(|e| LatticeError::Storage(format!("Failed to query consolidation outbox: {e}")))?;
        rows.collect::<Result<Vec<_>,_>>().map_err(|e| LatticeError::Storage(format!("Failed to decode consolidation outbox: {e}")))
    })?;
    rows.truncate(limit as usize);
    let mut delivered = 0;
    let mut last_error = None;
    for (outbox_id, workspace_id, event_uuid, ts, json) in rows {
        debug_assert_eq!(workspace_id, event_writer.workspace_id().as_str());
        let stored: StoredEventEnvelope = match serde_json::from_str(&json) {
            Ok(stored) => stored,
            Err(error) => {
                let message = format!("Failed to decode consolidation outbox envelope: {error}");
                memory_store.with_connection(|conn| conn.execute("UPDATE consolidation_event_outbox SET attempt_count=attempt_count+1,last_error=?1 WHERE outbox_id=?2", params![message,outbox_id]).map(|_|()).map_err(|e| LatticeError::Storage(format!("Failed to record consolidation outbox decode failure: {e}"))))?;
                last_error = Some(message);
                continue;
            }
        };
        match event_writer.append_idempotent(&event_uuid, ts, stored.into_partial()) {
            Ok(_) => {
                memory_store.with_connection(|conn| {
                    conn.execute(
                        "DELETE FROM consolidation_event_outbox WHERE outbox_id=?1",
                        [&outbox_id],
                    )
                    .map(|_| ())
                    .map_err(|e| {
                        LatticeError::Storage(format!(
                            "Failed to retire delivered consolidation event: {e}"
                        ))
                    })
                })?;
                delivered += 1;
            }
            Err(error) => {
                let message = error.to_string();
                memory_store.with_connection(|conn| conn.execute("UPDATE consolidation_event_outbox SET attempt_count=attempt_count+1,last_error=?1 WHERE outbox_id=?2", params![message,outbox_id]).map(|_|()).map_err(|e| LatticeError::Storage(format!("Failed to record consolidation event failure: {e}"))))?;
                last_error = Some(error.to_string());
            }
        }
    }
    if let Some(error) = last_error {
        return Err(LatticeError::Storage(format!(
            "One or more consolidation events remain pending: {error}"
        )));
    }
    let has_more = memory_store.with_connection(|conn| {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM consolidation_event_outbox WHERE delivered_at IS NULL AND workspace_id=?1)",
            [event_writer.workspace_id()],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|e| LatticeError::Storage(format!("Failed to inspect remaining consolidation outbox work: {e}")))
    })?;
    Ok(OutboxDrainReport {
        delivered,
        has_more,
    })
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
    workspace_id: &str,
    pending: crate::consolidation::PendingProposalSpec,
) -> ConsolidationProposal {
    let mut evidence = pending.evidence;
    if let Some(object) = evidence.as_object_mut() {
        object
            .entry("repository_id")
            .or_insert_with(|| Value::String(workspace_id.to_string()));
        object
            .entry("checkout_id")
            .or_insert_with(|| Value::String(String::new()));
    }
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
        evidence,
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

pub fn state_hash_for_memory(
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
