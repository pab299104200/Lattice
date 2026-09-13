//! Consolidation runtime for proposal-only memory changes.
//!
//! This module implements the consolidation kernel described in
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 6. Consolidation Engine` and persists its job/proposal state according
//! to `## Storage Design`.
//!
//! "No consolidation job may silently rewrite high-scope memory without
//! preserving provenance and prior state."

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{field, info_span};

use crate::error::LatticeError;
use crate::events::EventWriter;
use crate::identity::EventId;
use crate::memory::{
    Memory, MemoryLinkRecord, MemoryStore, MemoryStructuredFields, MemoryVerificationStatus,
};

pub mod demotion;
pub mod duplicates;
pub mod episode;
pub mod llm;
pub mod proposal;
pub mod proposal_references;
pub mod queue;
pub mod refresh;
pub mod replay;
pub mod review_queue;
pub mod session;
pub mod stale_marker;
pub mod supersession;

#[cfg(test)]
mod deterministic_tests;
#[cfg(test)]
mod evolution_acceptance_tests;
#[cfg(test)]
mod integration_test_support;
#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod proposal_tests;
#[cfg(test)]
mod replay_tests;
#[cfg(test)]
mod review_queue_tests;
#[cfg(test)]
mod session_tests;

pub use demotion::DemotionScanner;
pub use duplicates::DuplicateDetector;
pub use episode::{EpisodeError, EpisodeOutcome, EpisodeTemplate, ToolName};
pub use proposal::{
    drain_event_outbox, empty_state, state_hash_for_memory, ApplyOutcome, ConsolidationProposal,
    ConsolidationProposalRecord, EvolutionAuthority, OutboxDrainReport, ProposalDecision,
    ProposalKind, ProposalTarget, RejectOutcome,
};
pub use queue::{
    BoundedJobQueue, ConsolidationJobMode, ConsolidationJobRecord, ConsolidationJobSpec,
    ConsolidationJobStatus, ConsolidationSkipReason, EnqueueOutcome, PendingProposalSpec,
};
pub use refresh::RefreshScanner;
pub use replay::{
    Clock, FixedReplayClock, ReplayDriver, ReplayError, ReplayMode, ReplayReport, ReverseError,
    ReverseOutcome,
};
pub use review_queue::{
    ReviewDecisionOutcome, ReviewItem, ReviewQueue, ReviewQueueError, ReviewQueueFilter,
};
pub use session::{SessionConsolidationConfig, SessionConsolidationOutcome, SessionConsolidator};
pub use stale_marker::StaleMarker;
pub use supersession::SupersessionCandidates;

const CONSOLIDATION_SCHEMA_SQL: &str = include_str!("schema.sql");
const DEFAULT_CONSOLIDATION_QUEUE_DEPTH: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanReport {
    pub proposals_enqueued: u32,
    pub skipped: u32,
    pub elapsed: Duration,
}

impl ScanReport {
    pub fn from_counts(proposals_enqueued: u32, skipped: u32, started: Instant) -> Self {
        Self {
            proposals_enqueued,
            skipped,
            elapsed: started.elapsed(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error(transparent)]
    Storage(#[from] LatticeError),
    #[error("proposal routing failed: {0}")]
    Proposal(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConsolidationMemoryState {
    pub memory: Memory,
    pub structured_fields: MemoryStructuredFields,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified_graph_snapshot_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<crate::DateTime<crate::Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_links: Vec<MemoryLinkRecord>,
}

pub(crate) fn capture_memory_state(
    store: &MemoryStore,
    memory: &Memory,
) -> Result<ConsolidationMemoryState, LatticeError> {
    let structured_fields = store.get_structured_fields(&memory.id)?.unwrap_or_default();
    let memory_links = store.list_memory_links_from(&memory.id)?;
    Ok(ConsolidationMemoryState {
        memory: memory.clone(),
        structured_fields,
        last_verified_at: store.get_last_verified_at(&memory.id)?,
        last_verified_graph_snapshot_id: store.get_last_verified_graph_snapshot_id(&memory.id)?,
        expires_at: store.get_expires_at(&memory.id)?,
        memory_links,
    })
}

pub(crate) fn encode_memory_state(state: &ConsolidationMemoryState) -> Value {
    serde_json::to_value(state).unwrap_or_else(|_| json!({}))
}

/// Build canonical proposal snapshots for an episode create or refresh.
/// Episode-specific provenance belongs in proposal evidence; memory state uses
/// the same complete representation as apply-time CAS and replay.
pub fn episode_memory_states(
    store: &MemoryStore,
    existing: Option<&Memory>,
    proposed: Memory,
) -> Result<(Value, Value), LatticeError> {
    let captured = existing
        .map(|memory| capture_memory_state(store, memory))
        .transpose()?;
    let prior = captured
        .as_ref()
        .map(encode_memory_state)
        .unwrap_or_else(empty_state);
    let mut next = if let Some(state) = captured {
        state
    } else {
        let mut structured_fields = MemoryStructuredFields::default();
        structured_fields.memory_class = crate::memory::MemoryClass::WorkflowOutcome;
        ConsolidationMemoryState {
            memory: proposed.clone(),
            structured_fields,
            last_verified_at: None,
            last_verified_graph_snapshot_id: None,
            expires_at: None,
            memory_links: Vec::new(),
        }
    };
    let content_changed = next.memory.content != proposed.content;
    next.memory = proposed;
    if content_changed {
        next.memory.verification_status = crate::memory::MemoryVerificationStatus::Unverified;
        next.structured_fields.verification_status =
            crate::memory::MemoryVerificationStatus::Unverified;
        next.last_verified_at = None;
        next.last_verified_graph_snapshot_id = None;
    }
    Ok((prior, encode_memory_state(&next)))
}

pub(crate) fn mark_state_stale(
    state: &ConsolidationMemoryState,
    reason: String,
) -> ConsolidationMemoryState {
    let mut next = state.clone();
    next.memory.is_stale = true;
    next.memory.stale_reason = Some(reason);
    next.structured_fields.verification_status = MemoryVerificationStatus::Stale;
    next
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsolidationConfig {
    pub max_queue_depth: usize,
    pub llm_budget_catalog: Option<llm::BudgetCatalog>,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            max_queue_depth: DEFAULT_CONSOLIDATION_QUEUE_DEPTH,
            llm_budget_catalog: None,
        }
    }
}

pub fn initialize_schema(conn: &Connection) -> Result<(), LatticeError> {
    conn.execute_batch(CONSOLIDATION_SCHEMA_SQL).map_err(|e| {
        LatticeError::Storage(format!("Failed to initialize consolidation schema: {e}"))
    })?;
    migrate_consolidation_schema(conn)?;
    crate::verification::initialize_schema(conn)?;
    Ok(())
}

fn migrate_consolidation_schema(conn: &Connection) -> Result<(), LatticeError> {
    if !column_exists(conn, "consolidation_event_outbox", "transition")? {
        conn.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE consolidation_event_outbox RENAME TO consolidation_event_outbox_legacy;
             CREATE TABLE consolidation_event_outbox (
               outbox_id TEXT PRIMARY KEY, proposal_id TEXT NOT NULL,
               transition TEXT NOT NULL CHECK (transition IN ('applied','reverted')),
               workspace_id TEXT NOT NULL, event_uuid TEXT NOT NULL UNIQUE,
               event_ts_unix_micros INTEGER NOT NULL,
               envelope_json TEXT NOT NULL CHECK (json_valid(envelope_json)),
               created_at INTEGER NOT NULL, delivered_at INTEGER,
               attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0), last_error TEXT,
               FOREIGN KEY (proposal_id) REFERENCES consolidation_proposals(proposal_id) ON DELETE CASCADE);
             INSERT INTO consolidation_event_outbox
               (outbox_id,proposal_id,transition,workspace_id,event_uuid,event_ts_unix_micros,envelope_json,created_at,delivered_at,attempt_count,last_error)
             SELECT outbox_id,proposal_id,'applied',workspace_id,event_uuid,event_ts_unix_micros,envelope_json,created_at,delivered_at,attempt_count,last_error
             FROM consolidation_event_outbox_legacy;
             DROP TABLE consolidation_event_outbox_legacy;
             COMMIT;",
        ).map_err(|e| LatticeError::Storage(format!("Failed to migrate consolidation outbox transition contract: {e}")))?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_consolidation_event_outbox_pending
           ON consolidation_event_outbox(delivered_at, workspace_id, attempt_count, created_at, outbox_id);
         CREATE INDEX IF NOT EXISTS idx_consolidation_event_outbox_proposal_transition
           ON consolidation_event_outbox(proposal_id, transition);",
    ).map_err(|e| LatticeError::Storage(format!("Failed to initialize consolidation outbox indexes: {e}")))?;
    if !column_exists(conn, "consolidation_proposals", "provenance_json")? {
        conn.execute(
            "ALTER TABLE consolidation_proposals ADD COLUMN provenance_json TEXT CHECK (provenance_json IS NULL OR json_valid(provenance_json))",
            [],
        )
        .map_err(|e| LatticeError::Storage(format!("Failed to add proposal provenance column: {e}")))?;
    }
    if !column_exists(conn, "consolidation_proposals", "decision_reason")? {
        conn.execute(
            "ALTER TABLE consolidation_proposals ADD COLUMN decision_reason TEXT",
            [],
        )
        .map_err(|e| {
            LatticeError::Storage(format!(
                "Failed to add proposal decision_reason column: {e}"
            ))
        })?;
    }
    if !decision_supports_reverted(conn)? {
        conn.execute_batch(
            "ALTER TABLE consolidation_proposals RENAME TO consolidation_proposals_legacy;
             CREATE TABLE consolidation_proposals (
                 proposal_id TEXT PRIMARY KEY,
                 job_id TEXT NOT NULL,
                 target_memory_id TEXT,
                 proposal_kind TEXT NOT NULL,
                 prior_state TEXT NOT NULL CHECK (json_valid(prior_state)),
                 proposed_state TEXT NOT NULL CHECK (json_valid(proposed_state)),
                 evidence TEXT NOT NULL CHECK (json_valid(evidence)),
                 provenance_json TEXT CHECK (provenance_json IS NULL OR json_valid(provenance_json)),
                 decided_at INTEGER,
                 decision TEXT NOT NULL CHECK (decision IN ('pending', 'applied', 'rejected', 'reverted')),
                 decided_by TEXT,
                 decision_reason TEXT,
                 FOREIGN KEY (job_id) REFERENCES consolidation_jobs(job_id)
             );
             INSERT INTO consolidation_proposals (
                 proposal_id, job_id, target_memory_id, proposal_kind, prior_state,
                 proposed_state, evidence, provenance_json, decided_at, decision, decided_by,
                 decision_reason
             )
             SELECT
                 proposal_id, job_id, target_memory_id, proposal_kind, prior_state,
                 proposed_state, evidence, provenance_json, decided_at, decision, decided_by,
                 decision_reason
             FROM consolidation_proposals_legacy;
             DROP TABLE consolidation_proposals_legacy;",
        )
        .map_err(|e| LatticeError::Storage(format!("Failed to add reverted proposal decision: {e}")))?;
    }
    Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, LatticeError> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| LatticeError::Storage(format!("Failed to inspect table {table}: {e}")))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| LatticeError::Storage(format!("Failed to inspect table {table}: {e}")))?;
    for name in columns {
        if name.map_err(|e| LatticeError::Storage(e.to_string()))? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn decision_supports_reverted(conn: &Connection) -> Result<bool, LatticeError> {
    let table_sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'consolidation_proposals'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| {
            LatticeError::Storage(format!(
                "Failed to inspect consolidation_proposals table SQL: {e}"
            ))
        })?;
    Ok(table_sql
        .as_deref()
        .is_some_and(|sql| sql.contains("'reverted'")))
}

pub struct ConsolidationJobRuntime {
    conn: Arc<Mutex<Connection>>,
    queue: BoundedJobQueue,
    config: ConsolidationConfig,
}

pub fn persist_pending_proposal(
    memory_store: &MemoryStore,
    authority: &EvolutionAuthority<'_>,
    kind: &str,
    mode: ConsolidationJobMode,
    proposal: &ConsolidationProposal,
) -> Result<ConsolidationProposalRecord, LatticeError> {
    let mut proposal = proposal.clone();
    bind_proposal_authority(&mut proposal, authority)?;
    memory_store.with_connection(|conn| {
    let tx = conn.unchecked_transaction().map_err(|e| LatticeError::Storage(format!("Failed to begin pending proposal transaction: {e}")))?;
    proposal.validate_creation_authority(memory_store, authority)?;
    let changed = tx.execute(
        "INSERT INTO consolidation_jobs
            (job_id, workspace_id, kind, mode, status, enqueued_at, proposal_id)
         VALUES (?1, ?2, ?3, ?4, 'proposed', ?5, ?6)
         ON CONFLICT(job_id) DO UPDATE SET status='proposed',proposal_id=excluded.proposal_id
         WHERE consolidation_jobs.workspace_id=excluded.workspace_id AND consolidation_jobs.kind=excluded.kind
           AND consolidation_jobs.mode=excluded.mode AND consolidation_jobs.status='queued'",
        params![
            proposal.job_id,
            authority.repository_id,
            kind,
            mode.as_str(),
            now_unix_micros(),
            proposal.proposal_id,
        ],
    )
    .map_err(|e| LatticeError::Storage(format!("Failed to persist consolidation job: {e}")))?;
    if changed != 1 { return Err(LatticeError::Storage(format!("Consolidation job '{}' conflicts with an existing non-identical job", proposal.job_id))); }
    proposal.insert_pending(&tx)?;
    tx.commit().map_err(|e| LatticeError::Storage(format!("Failed to commit pending proposal: {e}")))?;
    ConsolidationProposal::load_record(conn, &proposal.proposal_id)?.ok_or_else(|| {
        LatticeError::Storage(format!(
            "Proposal '{}' was not readable after persistence",
            proposal.proposal_id
        ))
    })
    })
}

impl ConsolidationJobRuntime {
    pub fn new(conn: Connection, config: ConsolidationConfig) -> Result<Self, LatticeError> {
        initialize_schema(&conn)?;
        let conn = Arc::new(Mutex::new(conn));
        let queue = BoundedJobQueue::new(config.max_queue_depth, conn.clone());
        Ok(Self {
            conn,
            queue,
            config,
        })
    }

    pub fn config(&self) -> &ConsolidationConfig {
        &self.config
    }

    pub fn submit(
        &mut self,
        job_spec: ConsolidationJobSpec,
    ) -> Result<EnqueueOutcome, LatticeError> {
        self.queue.enqueue(job_spec)
    }

    pub(crate) fn submit_inline(
        &mut self,
        job_spec: ConsolidationJobSpec,
        memory_store: &MemoryStore,
        authority: &EvolutionAuthority<'_>,
    ) -> Result<(EnqueueOutcome, Option<ConsolidationProposal>), LatticeError> {
        let proposal = persist_canonical_proposal(memory_store, &job_spec, authority)?;
        Ok((
            EnqueueOutcome::Queued {
                depth: self.queue.depth(),
            },
            Some(proposal),
        ))
    }

    pub fn run_due(
        &mut self,
        memory_store: &MemoryStore,
        authority: &EvolutionAuthority<'_>,
    ) -> Result<Vec<ConsolidationProposal>, LatticeError> {
        let mut proposals = Vec::new();
        while let Some(job) = self.queue.pop() {
            proposals.push(persist_canonical_proposal(memory_store, &job, authority)?);
        }
        Ok(proposals)
    }

    pub fn execute_ready(
        &mut self,
        memory_store: &MemoryStore,
        event_writer: &EventWriter,
        decided_by: &crate::identity::OperatorId,
        authority: &EvolutionAuthority<'_>,
    ) -> Result<Vec<ConsolidationProposal>, LatticeError> {
        let mut proposals = Vec::new();
        while let Some(job) = self.queue.pop() {
            match self.run_job(
                job,
                Some((memory_store, event_writer, decided_by, authority)),
            ) {
                Ok(Some(proposal)) => proposals.push(proposal),
                Ok(None) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(proposals)
    }

    pub fn decide(
        &self,
        proposal_id: &str,
        decision: ProposalDecision,
        memory_store: &MemoryStore,
        event_writer: &EventWriter,
        decided_by: &str,
        authority: &EvolutionAuthority<'_>,
    ) -> Result<Option<ApplyOutcome>, LatticeError> {
        let runtime_has_proposal = {
            let conn = self.lock_conn()?;
            ConsolidationProposal::load(&conn, proposal_id)?.is_some()
        };
        if authority.repository_id != event_writer.workspace_id() {
            return Err(LatticeError::Storage(
                "decision authority repository does not match event writer".into(),
            ));
        }
        let outcome = memory_store.with_connection(|conn| {
            let tx = conn.unchecked_transaction().map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to begin consolidation decision transaction: {e}"
                ))
            })?;
            let proposal = match ConsolidationProposal::load(&tx, proposal_id)? {
                Some(proposal) => proposal,
                None if runtime_has_proposal => {
                    return Err(LatticeError::Storage(format!(
                    "Proposal '{proposal_id}' is stored outside the canonical MemoryStore database"
                )))
                }
                None => return Ok(None),
            };
            let outcome = match decision {
                ProposalDecision::Pending | ProposalDecision::Reverted => None,
                ProposalDecision::Applied => Some(proposal.apply_transactional(
                    &tx,
                    memory_store,
                    authority,
                    decided_by,
                    None,
                )?),
                ProposalDecision::Rejected => {
                    proposal.reject_transactional(&tx, authority, decided_by, None)?;
                    None
                }
            };
            tx.commit().map_err(|e| {
                LatticeError::Storage(format!("Failed to commit consolidation decision: {e}"))
            })?;
            Ok(outcome)
        })?;
        drain_event_outbox(memory_store, event_writer, 64)?;
        Ok(outcome)
    }

    pub fn replay_from(&mut self, event_id: EventId) -> Result<EnqueueOutcome, LatticeError> {
        let job = ConsolidationJobSpec {
            job_id: format!("replay-{}", event_id.ulid),
            workspace_id: event_id.workspace_id,
            kind: "replay_from_event".to_string(),
            mode: ConsolidationJobMode::Replay,
            proposal: None,
        };
        self.submit(job)
    }

    pub fn depth(&self) -> usize {
        self.queue.depth()
    }

    /// Return the number of repo/organization review proposals that a bounded
    /// background producer may still submit. A full in-memory work queue also
    /// closes admission so expensive producers can check before doing work.
    pub(crate) fn available_review_slots(
        &self,
        memory_store: &MemoryStore,
        workspace_id: &str,
        max_pending_review_proposals: usize,
    ) -> Result<usize, LatticeError> {
        if !self.queue.has_capacity() || max_pending_review_proposals == 0 {
            return Ok(0);
        }
        if max_pending_review_proposals > 4096 {
            return Err(LatticeError::Storage(
                "manual-review proposal admission limit must be at most 4096".into(),
            ));
        }
        let pending = memory_store.with_connection(|conn| {
            review_queue::pending_manual_review_count_up_to(
                conn,
                workspace_id,
                max_pending_review_proposals,
            )
        })?;
        Ok(max_pending_review_proposals.saturating_sub(pending))
    }

    /// Persist an intentionally content-free skip record. The closed reason
    /// enum is the only variable diagnostic accepted by this API.
    pub(crate) fn record_content_free_skip(
        &self,
        workspace_id: &str,
        kind: &str,
        reason: ConsolidationSkipReason,
    ) -> Result<String, LatticeError> {
        self.queue
            .persist_content_free_skip(workspace_id, kind, reason)
    }

    pub fn drain_for_shutdown(&mut self) -> Vec<ConsolidationJobSpec> {
        self.queue.drain_for_shutdown()
    }

    fn run_job(
        &self,
        job: ConsolidationJobSpec,
        auto_apply: Option<(
            &MemoryStore,
            &EventWriter,
            &crate::identity::OperatorId,
            &EvolutionAuthority<'_>,
        )>,
    ) -> Result<Option<ConsolidationProposal>, LatticeError> {
        let proposal_id = job
            .proposal
            .as_ref()
            .map(|proposal| proposal.proposal_id.clone())
            .unwrap_or_default();
        let span = info_span!(
            "consolidation.job.run",
            workspace_id = job.workspace_id.as_str(),
            job_id = job.job_id.as_str(),
            proposal_id = proposal_id.as_str(),
            outcome = field::Empty
        );
        let _entered = span.enter();
        if let Some((memory_store, event_writer, decided_by, authority)) = auto_apply {
            if job.workspace_id != authority.repository_id {
                return Err(LatticeError::Storage(format!(
                    "consolidation job '{}' workspace does not match explicit decision authority",
                    job.job_id
                )));
            }
            let proposal = persist_canonical_proposal(memory_store, &job, authority)?;
            if !ReviewQueue::should_gate(&proposal) {
                let _ = self.decide(
                    &proposal.proposal_id,
                    ProposalDecision::Applied,
                    memory_store,
                    event_writer,
                    decided_by.value.as_str(),
                    authority,
                )?;
            }
            return Ok(Some(proposal));
        }
        let conn = self.lock_conn()?;
        mark_running(&conn, &job.job_id)?;
        let Some(pending) = job.proposal else {
            proposal::mark_job_status(&conn, &job.job_id, "failed", None, Some("no_proposal"))?;
            span.record("outcome", "failed");
            return Ok(None);
        };
        let proposal = proposal::proposal_from_pending(&job.job_id, &job.workspace_id, pending);
        proposal.insert_pending(&conn)?;
        proposal::mark_job_status(
            &conn,
            &job.job_id,
            "proposed",
            Some(&proposal.proposal_id),
            None,
        )?;
        span.record("outcome", "proposed");
        Ok(Some(proposal))
    }

    pub(crate) fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, LatticeError> {
        self.conn.lock().map_err(|_| {
            LatticeError::Storage("consolidation runtime storage lock was poisoned".to_string())
        })
    }
}

fn mark_running(conn: &Connection, job_id: &str) -> Result<(), LatticeError> {
    conn.execute(
        "UPDATE consolidation_jobs
         SET status = 'running', started_at = ?1
         WHERE job_id = ?2",
        params![now_unix_micros(), job_id],
    )
    .map_err(|e| LatticeError::Storage(format!("Failed to start consolidation job: {e}")))?;
    Ok(())
}

fn bind_proposal_authority(
    proposal: &mut ConsolidationProposal,
    authority: &EvolutionAuthority<'_>,
) -> Result<(), LatticeError> {
    let evidence = proposal.evidence.as_object_mut().ok_or_else(|| {
        LatticeError::Storage("consolidation proposal evidence must be a JSON object".into())
    })?;
    evidence.insert(
        "repository_id".into(),
        serde_json::Value::String(authority.repository_id.into()),
    );
    evidence.insert(
        "checkout_id".into(),
        serde_json::Value::String(authority.checkout_id.into()),
    );
    evidence.insert(
        "branch".into(),
        serde_json::Value::String(authority.branch.into()),
    );
    Ok(())
}

fn persist_canonical_proposal(
    memory_store: &MemoryStore,
    job: &ConsolidationJobSpec,
    authority: &EvolutionAuthority<'_>,
) -> Result<ConsolidationProposal, LatticeError> {
    if job.workspace_id != authority.repository_id {
        return Err(LatticeError::Storage(format!(
            "consolidation job '{}' workspace does not match explicit authority",
            job.job_id
        )));
    }
    let pending = job.proposal.clone().ok_or_else(|| {
        LatticeError::Storage(format!(
            "consolidation job '{}' has no proposal",
            job.job_id
        ))
    })?;
    let mut proposal = proposal::proposal_from_pending(&job.job_id, &job.workspace_id, pending);
    bind_proposal_authority(&mut proposal, authority)?;
    memory_store.with_connection(|conn| {
        let tx = conn.unchecked_transaction().map_err(|e| LatticeError::Storage(format!("Failed to begin canonical proposal transaction: {e}")))?;
        proposal.validate_creation_authority(memory_store, authority)?;
        let changed = tx.execute("INSERT INTO consolidation_jobs(job_id,workspace_id,kind,mode,status,enqueued_at,proposal_id) VALUES(?1,?2,?3,?4,'proposed',?5,?6) ON CONFLICT(job_id) DO UPDATE SET status='proposed',proposal_id=excluded.proposal_id WHERE consolidation_jobs.workspace_id=excluded.workspace_id AND consolidation_jobs.kind=excluded.kind AND consolidation_jobs.mode=excluded.mode AND consolidation_jobs.status='queued'", params![job.job_id,job.workspace_id,job.kind,job.mode.as_str(),now_unix_micros(),proposal.proposal_id])
            .map_err(|e| LatticeError::Storage(format!("Failed to persist canonical consolidation job: {e}")))?;
        if changed != 1 { return Err(LatticeError::Storage(format!("Consolidation job '{}' conflicts with an existing non-identical job", job.job_id))); }
        proposal.insert_pending(&tx)?;
        tx.commit().map_err(|e| LatticeError::Storage(format!("Failed to commit canonical proposal transaction: {e}")))
    })?;
    Ok(proposal)
}

pub(crate) fn now_unix_micros() -> i64 {
    queue::now_unix_micros()
}

#[cfg(test)]
pub(crate) fn test_authority() -> EvolutionAuthority<'static> {
    EvolutionAuthority {
        repository_id: "workspace-main",
        checkout_id: "checkout-main",
        branch: "main",
    }
}
