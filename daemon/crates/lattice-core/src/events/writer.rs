//! Public append-only writer for event envelopes.
//!
//! The writer defaults to a 4096-byte inline ceiling so hot-path workflow events
//! stay in the main `events` table while larger payloads spill into
//! `event_payloads` without bloating primary-row reads.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::to_string;
use thiserror::Error;
use tracing::{debug, trace_span, warn};

use crate::events::kinds::EventKind;
use crate::events::store::MAX_EVENT_PAYLOAD_BYTES;
use crate::events::{
    Actor, BranchRef, CompactSummary, EventPayload, EventStore, EventStoreError, InsertEnvelopeRow,
    PayloadLocation, SessionId, StableRef, TaskId,
};
use crate::identity::{EventId, WorkspaceId};
use crate::{DateTime, Utc};

const DEFAULT_INLINE_CEILING_BYTES: usize = 4096;
const EVENT_SCHEMA_VERSION: i64 = 2;
const ULID_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
static ULID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushPolicy {
    Sync,
    Batched { interval_ms: u64 },
}

#[derive(Clone, Debug)]
pub struct PartialEnvelope {
    pub workspace_id: Option<WorkspaceId>,
    pub branch: BranchRef,
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub actor: Actor,
    pub kind: EventKind,
    pub references: Vec<StableRef>,
    pub summary: CompactSummary,
    pub payload: EventPayload,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdentityError {
    #[error("{field} must not be empty")]
    EmptyField { field: &'static str },
}

#[derive(Debug, Error)]
pub enum EventWriteError {
    #[error("event workspace `{actual}` does not match writer workspace `{expected}`")]
    WorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error("event payload is too large: {actual} bytes exceeds {ceiling} bytes")]
    PayloadTooLarge { ceiling: usize, actual: usize },
    #[error("event store write failed: {0}")]
    Storage(#[from] EventStoreError),
    #[error("event payload serialization failed: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("event identity is invalid: {0}")]
    Identity(#[from] IdentityError),
}

pub struct EventWriter {
    store: Arc<EventStore>,
    workspace_id: WorkspaceId,
    inline_ceiling_bytes: usize,
    flush_policy: FlushPolicy,
    monotonic_state: Mutex<MonotonicState>,
    payload_spill_cache: Mutex<HashMap<crate::events::PayloadHash, i64>>,
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedEventWrite {
    pub(crate) event_id: EventId,
    workspace_id: WorkspaceId,
    ts_unix_micros: i64,
}

#[derive(Default)]
struct MonotonicState {
    last_session_micros: HashMap<String, i64>,
}

impl EventWriter {
    pub fn new(
        store: Arc<EventStore>,
        workspace_id: WorkspaceId,
        inline_ceiling_bytes: usize,
    ) -> Self {
        Self {
            store,
            workspace_id,
            inline_ceiling_bytes: if inline_ceiling_bytes == 0 {
                DEFAULT_INLINE_CEILING_BYTES
            } else {
                inline_ceiling_bytes
            },
            flush_policy: default_flush_policy(),
            monotonic_state: Mutex::new(MonotonicState::default()),
            payload_spill_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_flush_policy(mut self, flush_policy: FlushPolicy) -> Self {
        self.flush_policy = flush_policy;
        self
    }

    pub fn inline_ceiling_bytes(&self) -> usize {
        self.inline_ceiling_bytes
    }

    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    pub fn store(&self) -> Arc<EventStore> {
        self.store.clone()
    }

    pub fn append(&self, envelope: PartialEnvelope) -> Result<EventId, EventWriteError> {
        self.append_with_flush_policy(envelope, self.flush_policy)
    }

    pub fn append_with_flush_policy(
        &self,
        envelope: PartialEnvelope,
        flush_policy: FlushPolicy,
    ) -> Result<EventId, EventWriteError> {
        let _span = trace_span!("event_writer.append", kind = envelope.kind.as_str()).entered();
        let prepared = self.prepare_write(&envelope)?;
        self.append_prepared_with_flush_policy(prepared, envelope, flush_policy)
    }

    pub(crate) fn prepare_write(
        &self,
        envelope: &PartialEnvelope,
    ) -> Result<PreparedEventWrite, EventWriteError> {
        let workspace_id = self.resolve_workspace(&envelope.workspace_id)?;
        let (_timestamp, ts_unix_micros) = self.next_timestamp(&envelope.session_id)?;
        Ok(PreparedEventWrite {
            event_id: EventId {
                workspace_id: workspace_id.clone(),
                ulid: next_ulid(ts_unix_micros),
            },
            workspace_id,
            ts_unix_micros,
        })
    }

    pub(crate) fn append_prepared_with_flush_policy(
        &self,
        prepared: PreparedEventWrite,
        envelope: PartialEnvelope,
        flush_policy: FlushPolicy,
    ) -> Result<EventId, EventWriteError> {
        let payload_bytes = serde_json::to_vec(&envelope.payload)?;
        if payload_bytes.len() > MAX_EVENT_PAYLOAD_BYTES {
            return Err(EventWriteError::PayloadTooLarge {
                ceiling: MAX_EVENT_PAYLOAD_BYTES,
                actual: payload_bytes.len(),
            });
        }
        let payload_hash = crate::events::hash_canonical_payload_bytes(&payload_bytes);
        let payload_location =
            self.persist_payload(&payload_hash, &payload_bytes, prepared.ts_unix_micros)?;
        let row = build_insert_row(
            &prepared.event_id,
            &prepared.workspace_id,
            &envelope,
            payload_hash.as_bytes().to_vec(),
            payload_location,
            payload_bytes,
            prepared.ts_unix_micros,
        )?;

        self.store.insert_envelope_row_cached(&row)?;
        self.flush_if_needed(flush_policy)?;
        Ok(prepared.event_id)
    }

    fn resolve_workspace(
        &self,
        workspace_id: &Option<WorkspaceId>,
    ) -> Result<WorkspaceId, EventWriteError> {
        match workspace_id {
            Some(value) if value != &self.workspace_id => {
                warn!(
                    writer_workspace_id = self.workspace_id.as_str(),
                    event_workspace_id = value.as_str(),
                    "event writer rejected workspace mismatch"
                );
                Err(EventWriteError::WorkspaceMismatch {
                    expected: self.workspace_id.clone(),
                    actual: value.clone(),
                })
            }
            Some(value) => Ok(value.clone()),
            None => Ok(self.workspace_id.clone()),
        }
    }

    fn next_timestamp(
        &self,
        session_id: &SessionId,
    ) -> Result<(DateTime<Utc>, i64), EventWriteError> {
        if session_id.value.is_empty() {
            return Err(IdentityError::EmptyField {
                field: "session_id",
            }
            .into());
        }

        let mut state =
            self.monotonic_state
                .lock()
                .map_err(|_| EventStoreError::EnvelopeInvalid {
                    reason: "event writer monotonic state lock was poisoned".to_string(),
                })?;
        let now_micros = current_unix_micros()?;
        let entry = state
            .last_session_micros
            .entry(session_id.value.clone())
            .or_insert(now_micros);
        let next_micros = if now_micros > *entry {
            now_micros
        } else {
            *entry + 1
        };
        *entry = next_micros;
        Ok((
            DateTime::from_unix_seconds(next_micros / 1_000_000),
            next_micros,
        ))
    }

    fn persist_payload(
        &self,
        payload_hash: &crate::events::PayloadHash,
        payload_bytes: &[u8],
        ts_unix_micros: i64,
    ) -> Result<PayloadLocation, EventWriteError> {
        if payload_bytes.len() <= self.inline_ceiling_bytes {
            let bytes_len = u32::try_from(payload_bytes.len()).map_err(|_| {
                EventWriteError::PayloadTooLarge {
                    ceiling: u32::MAX as usize,
                    actual: payload_bytes.len(),
                }
            })?;
            return Ok(PayloadLocation::Inline { bytes_len });
        }

        if let Some(row_id) = self.cached_payload_row_id(payload_hash)? {
            return Ok(PayloadLocation::Spilled { row_id });
        }

        let row_id = self.store.insert_or_get_payload(
            payload_hash.as_bytes(),
            payload_bytes,
            ts_unix_micros,
        )?;
        self.payload_spill_cache
            .lock()
            .map_err(|_| EventStoreError::EnvelopeInvalid {
                reason: "event writer payload cache lock was poisoned".to_string(),
            })?
            .insert(*payload_hash, row_id);
        debug!(
            bytes_len = payload_bytes.len(),
            inline_ceiling_bytes = self.inline_ceiling_bytes,
            payload_row_id = row_id,
            "event payload spilled to side table"
        );
        Ok(PayloadLocation::Spilled { row_id })
    }

    fn cached_payload_row_id(
        &self,
        payload_hash: &crate::events::PayloadHash,
    ) -> Result<Option<i64>, EventWriteError> {
        Ok(self
            .payload_spill_cache
            .lock()
            .map_err(|_| EventStoreError::EnvelopeInvalid {
                reason: "event writer payload cache lock was poisoned".to_string(),
            })?
            .get(payload_hash)
            .copied())
    }

    fn flush_if_needed(&self, flush_policy: FlushPolicy) -> Result<(), EventWriteError> {
        match flush_policy {
            FlushPolicy::Sync => self.store.checkpoint_wal().map_err(EventWriteError::from),
            FlushPolicy::Batched { interval_ms: _ } => Ok(()),
        }
    }
}

fn build_insert_row(
    event_id: &EventId,
    workspace_id: &str,
    envelope: &PartialEnvelope,
    payload_hash: Vec<u8>,
    payload_location: PayloadLocation,
    payload_bytes: Vec<u8>,
    ts_unix_micros: i64,
) -> Result<InsertEnvelopeRow, EventWriteError> {
    let (actor_kind, actor_detail) = actor_parts(&envelope.actor);
    validate_identity_fields(
        workspace_id,
        &envelope.branch,
        &envelope.session_id,
        &envelope.task_id,
    )?;
    let references_json = to_string(&envelope.references)?;
    let (payload_inline, payload_spill_id) = match payload_location {
        PayloadLocation::Inline { .. } => (Some(payload_bytes), None),
        PayloadLocation::Spilled { row_id } => (None, Some(row_id)),
    };

    Ok(InsertEnvelopeRow {
        event_uuid: event_id.ulid.clone(),
        workspace_id: workspace_id.to_string(),
        branch: envelope.branch.name.clone(),
        session_id: envelope.session_id.value.clone(),
        task_id: envelope
            .task_id
            .as_ref()
            .map(|task_id| task_id.value.clone()),
        actor_kind,
        actor_detail,
        kind: envelope.kind.as_str().to_string(),
        ts_unix_micros,
        payload_hash,
        summary: envelope.summary.as_str().to_string(),
        payload_inline,
        payload_spill_id,
        references_json,
        schema_version: EVENT_SCHEMA_VERSION,
    })
}

fn actor_parts(actor: &Actor) -> (String, Option<String>) {
    match actor {
        Actor::Assistant { model } => ("assistant".to_string(), Some(model.clone())),
        Actor::User => ("user".to_string(), None),
        Actor::Tool { name } => ("tool".to_string(), Some(name.clone())),
        Actor::Daemon => ("daemon".to_string(), None),
    }
}

fn validate_identity_fields(
    workspace_id: &str,
    branch: &BranchRef,
    session_id: &SessionId,
    task_id: &Option<TaskId>,
) -> Result<(), IdentityError> {
    if workspace_id.is_empty() {
        return Err(IdentityError::EmptyField {
            field: "workspace_id",
        });
    }
    if branch.name.is_empty() {
        return Err(IdentityError::EmptyField { field: "branch" });
    }
    if session_id.value.is_empty() {
        return Err(IdentityError::EmptyField {
            field: "session_id",
        });
    }
    if let Some(task_id) = task_id {
        if task_id.value.is_empty() {
            return Err(IdentityError::EmptyField { field: "task_id" });
        }
    }
    Ok(())
}

fn current_unix_micros() -> Result<i64, EventStoreError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| EventStoreError::EnvelopeInvalid {
            reason: format!("system clock precedes Unix epoch: {error}"),
        })?;
    i64::try_from(duration.as_micros()).map_err(|_| EventStoreError::EnvelopeInvalid {
        reason: "current Unix timestamp exceeds i64 microsecond range".to_string(),
    })
}

fn default_flush_policy() -> FlushPolicy {
    if cfg!(test) {
        FlushPolicy::Sync
    } else {
        FlushPolicy::Batched { interval_ms: 50 }
    }
}

fn next_ulid(ts_unix_micros: i64) -> String {
    let millis = ts_unix_micros.div_euclid(1_000) as u128;
    let sequence = u128::from(ULID_COUNTER.fetch_add(1, Ordering::SeqCst));
    encode_ulid((millis << 80) | sequence)
}

fn encode_ulid(mut value: u128) -> String {
    let mut output = [b'0'; 26];
    for slot in (0..26).rev() {
        let index = (value & 0x1f) as usize;
        output[slot] = ULID_ALPHABET[index];
        value >>= 5;
    }
    String::from_utf8(output.to_vec()).expect("ULID alphabet is valid UTF-8")
}
