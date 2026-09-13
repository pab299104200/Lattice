//! Replay-safe consolidation rebuilds and mechanical proposal reversal.
//!
//! Spec excerpts from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 6. Consolidation Engine` and
//! `## Non-Negotiable Product Properties`:
//!
//! > "Consolidation modes: ... replay mode for rebuilding memory state from the event log."
//!
//! > "Every background consolidation pass is recoverable, replayable, and observable."

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::{field, info_span};

use crate::consolidation::llm::LlmDriver;
use crate::consolidation::proposal::{
    apply_memory_state, empty_state, state_hash_for_memory, ConsolidationProposal, ProposalDecision,
};
use crate::events::{EventPayload, EventReader, EventStore, EventStoreError};
use crate::identity::EventId;
use crate::memory::Memory;
use crate::memory::MemoryStore;
use crate::LatticeError;

pub type PromptHash = [u8; 32];
pub type ResponseBytes = Vec<u8>;

pub trait Clock: Send + Sync {
    fn pin(&self, unix_micros: i64);
    fn now_unix_micros(&self) -> i64;
}

#[derive(Default)]
pub struct FixedReplayClock(std::sync::Mutex<i64>);

impl Clock for FixedReplayClock {
    fn pin(&self, unix_micros: i64) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = unix_micros;
        }
    }

    fn now_unix_micros(&self) -> i64 {
        self.0.lock().map(|guard| *guard).unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayMode {
    FromGenesis,
    FromSnapshot { snapshot_id: i64 },
    FromEventId(EventId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayReport {
    pub mode: ReplayMode,
    pub baseline_events_applied: usize,
    pub events_replayed: usize,
    pub events_skipped: usize,
    pub consolidation_failures_seen: usize,
    pub divergences: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReverseOutcome {
    Reverted { memory_id: String },
    AlreadyReverted,
}

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("event store query failed: {0}")]
    EventStore(String),
    #[error("payload decode failed for event row {row_id}: {reason}")]
    PayloadDecode { row_id: i64, reason: String },
    #[error("cached LLM response missing for prompt hash {prompt_hash:?}")]
    CachedResponseMissing { prompt_hash: PromptHash },
    #[error("cached LLM response hash mismatch for prompt hash {prompt_hash:?}")]
    CachedResponseHashMismatch { prompt_hash: PromptHash },
    #[error("post-apply state divergence for memory {memory_id}: expected {expected:?}, actual {actual:?}")]
    DivergenceDetected {
        memory_id: String,
        expected: [u8; 32],
        actual: [u8; 32],
    },
    #[error("proposal {proposal_id} referenced by replay event was not found")]
    MissingProposal { proposal_id: String },
    #[error("memory replay failed: {0}")]
    Memory(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ReverseError {
    #[error("proposal {proposal_id} was not found")]
    ProposalNotFound { proposal_id: String },
    #[error("proposal {proposal_id} is not applied and cannot be reverted")]
    NotApplied { proposal_id: String },
    #[error("proposal {proposal_id} prior state is corrupt")]
    PriorStateCorrupt { proposal_id: String },
    #[error("proposal {proposal_id} restore failed: {reason}")]
    RestoreFailed { proposal_id: String, reason: String },
    #[error("proposal {proposal_id} rollback failed after reverse error: {reason}")]
    RollbackFailed { proposal_id: String, reason: String },
    #[error("proposal {proposal_id} event emission failed: {reason}")]
    EventWriteFailed { proposal_id: String, reason: String },
}

pub struct ReplayDriver<'a> {
    pub event_reader: &'a EventReader,
    /// Canonical authority for proposals, deletion fences, and explicit reversal.
    pub authority_store: &'a MemoryStore,
    /// Disposable derived store rebuilt by replay.
    replay_store: MemoryStore,
    pub llm_response_cache: HashMap<PromptHash, ResponseBytes>,
    pub live_llm_driver: Option<&'a dyn LlmDriver>,
    event_store: Arc<EventStore>,
    event_writer: &'a crate::events::EventWriter,
    clock: &'a dyn Clock,
}

impl<'a> ReplayDriver<'a> {
    pub fn new(
        event_reader: &'a EventReader,
        event_store: Arc<EventStore>,
        authority_store: &'a MemoryStore,
        event_writer: &'a crate::events::EventWriter,
        clock: &'a dyn Clock,
    ) -> Result<Self, LatticeError> {
        Ok(Self {
            event_reader,
            authority_store,
            replay_store: MemoryStore::open_in_memory()?,
            llm_response_cache: HashMap::new(),
            live_llm_driver: None,
            event_store,
            event_writer,
            clock,
        })
    }

    pub fn with_cached_responses(
        mut self,
        provenance_index: impl IntoIterator<Item = (PromptHash, ResponseBytes)>,
    ) -> Self {
        self.llm_response_cache.extend(provenance_index);
        self
    }

    pub fn with_live_llm_driver(mut self, driver: &'a dyn LlmDriver) -> Self {
        self.live_llm_driver = Some(driver);
        self
    }

    /// Read a memory reconstructed in the driver's disposable replay result.
    /// This never reads or mutates the canonical authority store.
    pub fn derived_memory(&self, memory_id: &str) -> Result<Option<Memory>, LatticeError> {
        self.replay_store.get_by_id(memory_id)
    }

    /// Read all memories reconstructed in the disposable replay result.
    pub fn derived_memories(&self) -> Result<Vec<Memory>, LatticeError> {
        self.replay_store.list_all()
    }

    pub fn replay(&self, mode: ReplayMode) -> Result<ReplayReport, ReplayError> {
        let (baseline_row_id, replay_start_row_id) = self.mode_boundaries(&mode)?;
        let span = info_span!(
            "replay",
            mode = ?mode,
            events_replayed = field::Empty,
            divergences = field::Empty
        );
        let _entered = span.enter();
        self.replay_store
            .clear_all()
            .map_err(|e| ReplayError::Memory(e.to_string()))?;
        let mut baseline_events_applied = 0usize;
        let mut events_replayed = 0usize;
        let mut events_skipped = 0usize;
        let mut consolidation_failures_seen = 0usize;
        let divergences = 0usize;

        let mut cursor = 0_i64;
        let high_water = self
            .event_store
            .latest_event_row_id()
            .map_err(store_error)?;
        'pages: loop {
            if cursor >= high_water {
                break;
            }
            let rows = self
                .event_store
                .query_events_after_row_id(cursor, 512)
                .map_err(store_error)?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                if row.event_id > high_water {
                    break 'pages;
                }
                cursor = row.event_id;
                if row.workspace_id != self.event_writer.workspace_id().as_str() {
                    continue;
                }
                let payload = decode_payload(&self.event_store, &row)?;
                if row.event_id <= baseline_row_id {
                    if let EventPayload::MemoryConsolidated(ref consolidated) = payload {
                        if self.apply_replayed_event(&row.workspace_id, consolidated)? {
                            baseline_events_applied += 1;
                        } else {
                            events_skipped += 1;
                        }
                    }
                    continue;
                }
                if row.event_id < replay_start_row_id {
                    continue;
                }
                self.clock.pin(row.ts_unix_micros);
                match payload {
                    EventPayload::MemoryConsolidated(consolidated) => {
                        if self.apply_replayed_event(&row.workspace_id, &consolidated)? {
                            events_replayed += 1;
                        } else {
                            events_skipped += 1;
                        }
                    }
                    EventPayload::ConsolidationFailed(_) => {
                        consolidation_failures_seen += 1;
                        events_replayed += 1;
                    }
                    _ => {}
                }
            }
        }

        span.record("events_replayed", events_replayed);
        span.record("divergences", divergences);
        Ok(ReplayReport {
            mode,
            baseline_events_applied,
            events_replayed,
            events_skipped,
            consolidation_failures_seen,
            divergences,
        })
    }

    pub fn reverse(&self, proposal_id: &str) -> Result<ReverseOutcome, ReverseError> {
        let span = info_span!("reverse_proposal", proposal_id);
        let _entered = span.enter();
        let outcome = self.authority_store.with_connection(|conn| {
            let tx = conn.unchecked_transaction().map_err(|e| LatticeError::Storage(e.to_string()))?;
            let record = ConsolidationProposal::load_record(&tx, proposal_id)?
                .ok_or_else(|| LatticeError::Storage(format!("proposal {proposal_id} was not found")))?;
            if record.workspace_id != self.event_writer.workspace_id().as_str() {
                return Err(LatticeError::Storage(format!("proposal {proposal_id} belongs to repository '{}'", record.workspace_id)));
            }
            if record.evidence.get("checkout_id").and_then(Value::as_str).is_some_and(|id| !id.is_empty()) {
                return Err(LatticeError::Storage(format!("proposal {proposal_id} is checkout-bound and requires an explicit checkout authority to reverse")));
            }
            if record.decision == ProposalDecision::Reverted { return Ok(ReverseOutcome::AlreadyReverted); }
            if record.decision != ProposalDecision::Applied { return Err(LatticeError::Storage(format!("proposal {proposal_id} is not applied"))); }
            let memory_id = proposal_memory_id(&record.proposed_state, &record.target_memory_id)
                .map_err(|_| LatticeError::Storage(format!("proposal {proposal_id} prior state is corrupt")))?;
            let fenced: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM memory_deletion_receipts WHERE memory_id=?1) OR EXISTS(SELECT 1 FROM session_capture_tombstone_provenance p JOIN session_capture_tombstones t ON t.delivery_key=p.delivery_key WHERE p.derived_kind='proposal' AND p.derived_id=?2)",
                rusqlite::params![memory_id, proposal_id], |row| row.get(0),
            ).map_err(|e| LatticeError::Storage(format!("Failed to read reversal deletion fence: {e}")))?;
            if fenced {
                return Err(LatticeError::Storage(format!("proposal {proposal_id} cannot be reversed because its knowledge was deliberately deleted")));
            }
            let proposed_state: crate::consolidation::ConsolidationMemoryState = serde_json::from_value(record.proposed_state.clone())
                .map_err(|e| LatticeError::Storage(format!("proposal {proposal_id} lacks a canonical applied-state snapshot: {e}")))?;
            let expected_store = MemoryStore::open_in_memory()?;
            for link in &proposed_state.memory_links {
                if link.target_memory_id != memory_id {
                    let target = self.authority_store.get_by_id(&link.target_memory_id)?
                        .ok_or_else(|| LatticeError::Storage(format!("proposal {proposal_id} applied-state snapshot references missing memory '{}'", link.target_memory_id)))?;
                    expected_store.store(target)?;
                }
            }
            apply_memory_state(&expected_store, &proposed_state)?;
            let expected_current = state_hash_for_memory(&expected_store, &memory_id)?;
            let actual_current = state_hash_for_memory(self.authority_store, &memory_id)?;
            if actual_current != expected_current {
                return Err(LatticeError::Storage(format!("proposal {proposal_id} cannot be reversed because memory '{memory_id}' changed after apply")));
            }
            let reverse_proposal = ConsolidationProposal {
                proposal_id: record.proposal_id.clone(), job_id: record.job_id.clone(),
                target: record.target_memory_id.clone().map(crate::consolidation::ProposalTarget::ExistingMemory).unwrap_or(crate::consolidation::ProposalTarget::NewMemory),
                proposal_kind: record.proposal_kind, prior_state: record.proposed_state.clone(),
                proposed_state: record.prior_state.clone(), evidence: record.evidence.clone(), provenance: record.provenance.clone(),
            };
            apply_reverse_state(self.authority_store, &record.prior_state, &memory_id)?;
            let changed = tx.execute("UPDATE consolidation_proposals SET decision='reverted',decided_at=?1 WHERE proposal_id=?2 AND decision='applied'", rusqlite::params![crate::consolidation::now_unix_micros(),proposal_id])
                .map_err(|e| LatticeError::Storage(format!("Failed to commit proposal reversal decision: {e}")))?;
            if changed != 1 { return Err(LatticeError::Storage(format!("proposal {proposal_id} lost the reversal race"))); }
            let hash = crate::consolidation::proposal::state_hash_from_json(&record.prior_state)?;
            reverse_proposal.enqueue_event(&tx, self.event_writer.workspace_id(), "replay", "reverted", &memory_id, hash, "replay", Some("proposal reversed by replay driver"))?;
            tx.commit().map_err(|e| LatticeError::Storage(e.to_string()))?;
            Ok(ReverseOutcome::Reverted { memory_id })
        }).map_err(|e| ReverseError::RestoreFailed { proposal_id: proposal_id.to_string(), reason: e.to_string() })?;
        crate::consolidation::drain_event_outbox(self.authority_store, self.event_writer, 64)
            .map_err(|e| ReverseError::EventWriteFailed {
                proposal_id: proposal_id.to_string(),
                reason: e.to_string(),
            })?;
        Ok(outcome)
    }

    fn apply_replayed_event(
        &self,
        trusted_workspace_id: &str,
        payload: &crate::events::MemoryConsolidatedPayload,
    ) -> Result<bool, ReplayError> {
        if payload.consolidated_memory_id.workspace_id != trusted_workspace_id {
            return Err(ReplayError::Memory(format!(
                "consolidation event row belongs to repository '{trusted_workspace_id}' but payload belongs to '{}'",
                payload.consolidated_memory_id.workspace_id
            )));
        }
        let Some(proposal_id) = payload.proposal_id.as_deref() else {
            return Ok(false);
        };
        let transition = match payload.transition.as_deref() {
            Some("applied") => "applied",
            Some("reverted") => "reverted",
            _ => return Ok(false),
        };
        {
            let tombstoned = self.authority_store.with_connection(|conn| {
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM session_capture_tombstone_provenance p JOIN session_capture_tombstones t ON t.delivery_key=p.delivery_key WHERE p.derived_kind='proposal' AND p.derived_id=?1)",
                    [proposal_id], |row| row.get::<_, bool>(0),
                ).map_err(|e| LatticeError::Storage(format!("Failed to read proposal replay fence: {e}")))
            }).map_err(|e| ReplayError::Memory(e.to_string()))?;
            if tombstoned {
                return Ok(false);
            }
        }
        let purged = self
            .authority_store
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM memory_deletion_receipts WHERE memory_id=?1)",
                    [&payload.consolidated_memory_id.ulid],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(|e| {
                    LatticeError::Storage(format!("Failed to read memory replay fence: {e}"))
                })
            })
            .map_err(|e| ReplayError::Memory(e.to_string()))?;
        if purged {
            return Ok(false);
        }
        if let Some(proposal_id) = payload.proposal_id.as_deref() {
            let record = self
                .authority_store
                .with_connection(|conn| ConsolidationProposal::load_record(conn, proposal_id))
                .map_err(|e| ReplayError::Memory(e.to_string()))?;
            let Some(record) = record else {
                return Err(ReplayError::MissingProposal {
                    proposal_id: proposal_id.to_string(),
                });
            };
            if record.workspace_id != payload.consolidated_memory_id.workspace_id {
                return Err(ReplayError::Memory(format!(
                    "proposal {proposal_id} belongs to repository '{}' but event belongs to '{}'",
                    record.workspace_id, payload.consolidated_memory_id.workspace_id
                )));
            }
            let decision_allows_transition = match transition {
                "applied" => {
                    record.decision == ProposalDecision::Applied
                        || record.decision == ProposalDecision::Reverted
                }
                "reverted" => record.decision == ProposalDecision::Reverted,
                _ => false,
            };
            if !decision_allows_transition {
                return Err(ReplayError::Memory(format!(
                    "proposal {proposal_id} is '{}' and cannot authorize a '{transition}' replay event", record.decision.as_str()
                )));
            }
            if let Some(provenance) = &record.provenance {
                verify_cached_response(
                    &self.llm_response_cache,
                    provenance.prompt_sha256.clone(),
                    provenance.response_sha256.clone(),
                )?;
            }
            let state = if transition == "applied" {
                &record.proposed_state
            } else {
                &record.prior_state
            };
            apply_reverse_state(
                &self.replay_store,
                state,
                &payload.consolidated_memory_id.ulid,
            )
            .map_err(|e| ReplayError::Memory(e.to_string()))?;
            let actual_hash = match self
                .replay_store
                .get_by_id(&payload.consolidated_memory_id.ulid)
                .map_err(|e| ReplayError::Memory(e.to_string()))?
            {
                Some(_) => {
                    state_hash_for_memory(&self.replay_store, &payload.consolidated_memory_id.ulid)
                        .map_err(|e| ReplayError::Memory(e.to_string()))?
                }
                None => crate::consolidation::proposal::state_hash_from_json(state)
                    .map_err(|e| ReplayError::Memory(e.to_string()))?,
            };
            if actual_hash != payload.post_apply_state_hash {
                return Err(ReplayError::Memory(format!(
                    "proposal {proposal_id} replay state hash mismatch"
                )));
            }
        }
        Ok(true)
    }

    fn mode_boundaries(&self, mode: &ReplayMode) -> Result<(i64, i64), ReplayError> {
        match mode {
            ReplayMode::FromGenesis => Ok((0, 1)),
            ReplayMode::FromSnapshot { snapshot_id } => Ok((*snapshot_id, snapshot_id + 1)),
            ReplayMode::FromEventId(event_id) => {
                let row_id = self
                    .event_store
                    .row_id_for_event_uuid(&event_id.ulid)
                    .map_err(store_error)?
                    .unwrap_or_default();
                Ok((row_id.saturating_sub(1), row_id))
            }
        }
    }
}

fn apply_reverse_state(
    memory_store: &MemoryStore,
    state: &Value,
    memory_id: &str,
) -> Result<(), LatticeError> {
    if state == &empty_state() {
        if memory_store.get_by_id(memory_id)?.is_some() {
            memory_store.invalidate(memory_id)?;
        }
        return Ok(());
    }
    if let Some(decoded) = decode_state(state)? {
        return apply_memory_state(memory_store, &decoded);
    }
    let memory: crate::memory::Memory = serde_json::from_value(state.clone())
        .map_err(|e| LatticeError::Storage(format!("Failed to decode replay memory: {e}")))?;
    memory_store.store(memory)?;
    Ok(())
}

fn verify_cached_response(
    cache: &HashMap<PromptHash, ResponseBytes>,
    prompt_hash: PromptHash,
    expected_response_hash: [u8; 32],
) -> Result<(), ReplayError> {
    let Some(bytes) = cache.get(&prompt_hash) else {
        return Err(ReplayError::CachedResponseMissing { prompt_hash });
    };
    let digest = Sha256::digest(bytes);
    let mut actual = [0_u8; 32];
    actual.copy_from_slice(&digest);
    if actual != expected_response_hash {
        return Err(ReplayError::CachedResponseHashMismatch { prompt_hash });
    }
    Ok(())
}

fn decode_state(
    value: &Value,
) -> Result<Option<crate::consolidation::ConsolidationMemoryState>, LatticeError> {
    if value.get("memory").is_none() || value.get("structured_fields").is_none() {
        return Ok(None);
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|e| LatticeError::Storage(format!("Failed to decode replay state: {e}")))
}

fn proposal_memory_id(
    proposed_state: &Value,
    target_memory_id: &Option<String>,
) -> Result<String, LatticeError> {
    if let Some(state) = decode_state(proposed_state)? {
        return Ok(state.memory.id);
    }
    if let Some(memory_id) = proposed_state.get("id").and_then(Value::as_str) {
        return Ok(memory_id.to_string());
    }
    target_memory_id.clone().ok_or_else(|| {
        LatticeError::Storage("proposal state does not include a reversible memory id".to_string())
    })
}

fn decode_payload(
    store: &EventStore,
    row: &crate::events::EventEnvelopeRow,
) -> Result<EventPayload, ReplayError> {
    let bytes = match (&row.payload_inline, row.payload_spill_id) {
        (Some(bytes), _) => bytes.clone(),
        (None, Some(spill_id)) => {
            store
                .get_payload_row(spill_id)
                .map_err(store_error)?
                .ok_or_else(|| ReplayError::PayloadDecode {
                    row_id: row.event_id,
                    reason: "spilled payload row missing".to_string(),
                })?
                .bytes
        }
        (None, None) => {
            return Err(ReplayError::PayloadDecode {
                row_id: row.event_id,
                reason: "payload bytes missing".to_string(),
            });
        }
    };
    serde_json::from_slice(&bytes).map_err(|e| ReplayError::PayloadDecode {
        row_id: row.event_id,
        reason: e.to_string(),
    })
}

fn store_error(error: EventStoreError) -> ReplayError {
    ReplayError::EventStore(error.to_string())
}
