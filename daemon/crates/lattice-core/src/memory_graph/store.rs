use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Transaction};
use serde_json::json;
use thiserror::Error;

use super::{
    classify_stream, decode_json_column, insert_evidence, replay::capture_replay_snapshot,
    row_decode_error, warn_on_fk_violation, AssertionType, EvidenceAnchor, EvidenceReference,
    FreshnessPolicy, InvalidationTrigger, MemoryAccessRecord, MemoryClass, MemoryEvidence,
    MemoryEvidenceError, MemoryEvidenceId, MemoryGraphParseError, MemoryLinkReference,
    MemoryRecord, MemoryReplayError, MemoryScope, ScopeError, ScopeFilter, TestId,
    ValidityCondition, VerificationStatus,
};
use crate::events::{
    Actor, BranchRef, CompactSummary, EventKind, EventPayload, EventWriteError, EventWriter,
    FlushPolicy, MemoryCreatedPayload, MemoryInvalidatedPayload, MemoryUpdatedPayload,
    PartialEnvelope, SessionId, StableRef,
};
use crate::identity::{
    decode_identity, encode_identity, EventId, FileId, Identity, MemoryId, SectionId, SymbolId,
};
use crate::{DateTime, Utc};

const MEMORY_SCHEMA_VERSION: i64 = 1;
const ULID_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
static MEMORY_ULID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    pub fn new(value: impl Into<String>) -> Result<Self, MemoryStoreError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(MemoryStoreError::Identity(
                crate::events::IdentityError::EmptyField {
                    field: "idempotency_key",
                },
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MemoryScopeState {
    pub scope: MemoryScope,
    pub scope_session_id: Option<String>,
    pub scope_branch: Option<String>,
    pub scope_workspace_id: Option<String>,
    pub scope_user_id: Option<String>,
    pub scope_org_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MemoryDraft {
    pub content: String,
    pub class: MemoryClass,
    pub assertion_type: AssertionType,
    pub scope: MemoryScopeState,
    pub confidence: f64,
    pub confidence_reason: String,
    pub freshness_policy: FreshnessPolicy,
    pub validity_conditions: Vec<ValidityCondition>,
    pub invalidation_triggers: Vec<InvalidationTrigger>,
    pub provenance_event_ids: Vec<EventId>,
    pub evidence_references: Vec<EvidenceReference>,
    pub linked_files: Vec<FileId>,
    pub linked_symbols: Vec<SymbolId>,
    pub linked_docs: Vec<SectionId>,
    pub linked_tests: Vec<TestId>,
    pub linked_memories: Vec<MemoryLinkReference>,
    pub contradiction_links: Vec<MemoryLinkReference>,
    pub supersession_links: Vec<MemoryLinkReference>,
    pub access_history: Vec<MemoryAccessRecord>,
    pub created_by: String,
    pub updated_by: String,
    pub superseded_by: Option<MemoryId>,
    pub schema_version: i64,
    pub initial_evidence: Vec<MemoryEvidence>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MemoryPatch {
    pub content: Option<String>,
    pub confidence: Option<f64>,
    pub freshness_policy: Option<FreshnessPolicy>,
    pub validity_conditions: Option<Vec<ValidityCondition>>,
    pub invalidation_triggers: Option<Vec<InvalidationTrigger>>,
    pub scope: Option<MemoryScopeState>,
}

#[derive(Debug, Error)]
pub enum MemoryStoreError {
    #[error("memory `{0}` was not found")]
    NotFound(MemoryId),
    #[error("scope violation: {0:?}")]
    ScopeViolation(#[from] ScopeError),
    #[error("memory store constraint violation: {detail}")]
    ConstraintViolation { detail: String },
    #[error("memory store SQLite error: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error(transparent)]
    Identity(#[from] crate::events::IdentityError),
    #[error(transparent)]
    EventEmit(#[from] EventWriteError),
    #[error(transparent)]
    Replay(#[from] MemoryReplayError),
}

pub struct MemoryStore {
    conn: Arc<Mutex<Connection>>,
    event_writer: Arc<EventWriter>,
}

impl MemoryStore {
    pub fn open(conn: Arc<Mutex<Connection>>, event_writer: Arc<EventWriter>) -> Self {
        Self { conn, event_writer }
    }

    pub fn create(
        &self,
        draft: MemoryDraft,
        idempotency_key: IdempotencyKey,
    ) -> Result<MemoryRecord, MemoryStoreError> {
        validate_scope_state(&draft.scope)?;
        validate_confidence(draft.confidence)?;
        let mut conn = lock_connection(&self.conn)?;
        if let Some(memory_id) = lookup_idempotency(conn.deref(), &idempotency_key)? {
            return load_memory(conn.deref(), &memory_id);
        }

        let now = now_unix_seconds()?;
        let initial_evidence = draft.initial_evidence.clone();
        let memory_id = MemoryId {
            workspace_id: workspace_id_for_draft(&draft)?,
            ulid: next_ulid(now),
        };
        let tx = conn.transaction()?;
        let mut record = record_from_draft(memory_id.clone(), draft, now);
        insert_memory(&tx, &record)?;
        bind_idempotency(&tx, &idempotency_key, &memory_id)?;
        let prepared = self.prepare_memory_event_write(&record, "memory created")?;
        let event_id = prepared.event_id.clone();
        record.provenance_event_ids.push(event_id.clone());
        record.evidence_references.push(EvidenceReference {
            target: StableRef::EventRef(event_id.clone()),
            event_id: Some(event_id.clone()),
            summary: "memory created".to_string(),
        });
        replace_memory(&tx, &record)?;
        for evidence in collect_initial_evidence(
            &record.memory_id,
            &event_id,
            &record,
            &initial_evidence,
            now,
        )? {
            insert_evidence(&tx, &evidence).map_err(map_evidence_error)?;
        }
        let snapshot_json = capture_replay_snapshot(tx.deref(), &record)?;
        self.append_memory_created(&record, &idempotency_key, Some(snapshot_json), prepared)?;
        tx.commit()?;
        load_memory(conn.deref(), &memory_id)
    }

    pub fn get(
        &self,
        memory_id: &MemoryId,
        scope: &ScopeFilter,
    ) -> Result<MemoryRecord, MemoryStoreError> {
        ensure_scope_filter(scope)?;
        let conn = lock_connection(&self.conn)?;
        let record = load_memory(conn.deref(), memory_id)?;
        record_scope_filter(&record).enforce_subset(scope)?;
        Ok(record)
    }

    pub fn list_by_class(
        &self,
        class: MemoryClass,
        scope: &ScopeFilter,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, MemoryStoreError> {
        self.list_filtered(
            "class = ?",
            Value::Text(class.as_str().to_string()),
            scope,
            limit,
        )
    }

    pub fn list_by_stream(
        &self,
        stream: super::MemoryStream,
        scope: &ScopeFilter,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, MemoryStoreError> {
        let records = self.list_filtered("1 = 1", Value::Null, scope, limit)?;
        Ok(records
            .into_iter()
            .filter(|record| classify_stream(record.class, record.assertion_type) == stream)
            .collect())
    }

    pub fn list_by_status(
        &self,
        status: VerificationStatus,
        scope: &ScopeFilter,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, MemoryStoreError> {
        self.list_filtered(
            "verification_status = ?",
            Value::Text(status.as_str().to_string()),
            scope,
            limit,
        )
    }

    pub fn update(
        &self,
        memory_id: &MemoryId,
        patch: MemoryPatch,
        evidence: Option<MemoryEvidence>,
        idempotency_key: IdempotencyKey,
    ) -> Result<MemoryRecord, MemoryStoreError> {
        let mut conn = lock_connection(&self.conn)?;
        if let Some(bound_memory_id) = lookup_idempotency(conn.deref(), &idempotency_key)? {
            if bound_memory_id != *memory_id {
                return Err(MemoryStoreError::ConstraintViolation {
                    detail: format!(
                        "idempotency key `{}` is already bound to `{}`",
                        idempotency_key.as_str(),
                        bound_memory_id
                    ),
                });
            }
            return load_memory(conn.deref(), memory_id);
        }

        let tx = conn.transaction()?;
        let existing = load_memory(tx.deref(), memory_id)?;
        let now = now_unix_seconds()?;
        let mut updated = apply_patch(existing.clone(), patch)?;
        updated.updated_at = now;
        bind_idempotency(&tx, &idempotency_key, memory_id)?;
        replace_memory(&tx, &updated)?;
        let prepared = self.prepare_memory_event_write(&updated, "memory updated")?;
        let event_id = prepared.event_id.clone();
        updated.provenance_event_ids.push(event_id.clone());
        if let Some(evidence) = evidence {
            let evidence_row = stamp_evidence(evidence, memory_id.clone(), event_id.clone(), now)?;
            insert_evidence(&tx, &evidence_row).map_err(map_evidence_error)?;
            updated.evidence_references.push(EvidenceReference {
                target: StableRef::EventRef(event_id.clone()),
                event_id: Some(event_id.clone()),
                summary: "memory updated".to_string(),
            });
        }
        replace_memory(&tx, &updated)?;
        let snapshot_json = capture_replay_snapshot(tx.deref(), &updated)?;
        self.append_memory_updated(
            &existing,
            &updated,
            "memory updated",
            Some(snapshot_json),
            prepared,
        )?;
        tx.commit()?;
        load_memory(conn.deref(), memory_id)
    }

    pub fn delete(&self, memory_id: &MemoryId, reason: &str) -> Result<(), MemoryStoreError> {
        let mut conn = lock_connection(&self.conn)?;
        let tx = conn.transaction()?;
        let mut record = load_memory(tx.deref(), memory_id)?;
        let now = now_unix_seconds()?;
        record.verification_status = VerificationStatus::Invalidated;
        record.updated_at = now;
        let prepared = self.prepare_memory_event_write(&record, reason)?;
        let event_id = prepared.event_id.clone();
        record.provenance_event_ids.push(event_id.clone());
        record.last_verified_state = Some(json!({
            "status": VerificationStatus::Invalidated.as_str(),
            "reason": reason,
            "event_id": encode_identity(&Identity::Event(event_id.clone())),
        }));
        replace_memory(&tx, &record)?;
        upsert_tombstone(&tx, memory_id, reason, now, &event_id)?;
        let evidence =
            event_reference_evidence(memory_id.clone(), event_id, now, "memory invalidated")?;
        insert_evidence(&tx, &evidence).map_err(map_evidence_error)?;
        let snapshot_json = capture_replay_snapshot(tx.deref(), &record)?;
        self.append_memory_invalidated(&record, reason, None, Some(snapshot_json), prepared)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn load_for_transition(
        &self,
        memory_id: &MemoryId,
    ) -> Result<MemoryRecord, MemoryStoreError> {
        let conn = lock_connection(&self.conn)?;
        load_memory(conn.deref(), memory_id)
    }

    pub(crate) fn write_transition(
        &self,
        prior: &MemoryRecord,
        mut updated: MemoryRecord,
        reason: &str,
        evidence: Option<MemoryEvidence>,
    ) -> Result<MemoryRecord, MemoryStoreError> {
        let mut conn = lock_connection(&self.conn)?;
        let tx = conn.transaction()?;
        let now = now_unix_seconds()?;
        updated.updated_at = now;
        replace_memory(&tx, &updated)?;
        let summary = format!(
            "{} -> {}: {}",
            prior.verification_status.as_str(),
            updated.verification_status.as_str(),
            reason
        );
        let prepared = self.prepare_memory_event_write(
            &updated,
            if updated.verification_status == VerificationStatus::Invalidated {
                reason
            } else {
                &summary
            },
        )?;
        let event_id = prepared.event_id.clone();
        updated.provenance_event_ids.push(event_id.clone());
        updated.last_verified_state = Some(json!({
            "status": updated.verification_status.as_str(),
            "reason": reason,
            "event_id": encode_identity(&Identity::Event(event_id.clone())),
        }));
        if let Some(evidence) = evidence {
            let evidence_row =
                stamp_evidence(evidence, updated.memory_id.clone(), event_id.clone(), now)?;
            insert_evidence(&tx, &evidence_row).map_err(map_evidence_error)?;
        }
        let event_evidence = event_reference_evidence(
            updated.memory_id.clone(),
            event_id.clone(),
            now,
            &format!(
                "verification transition: {}",
                updated.verification_status.as_str()
            ),
        )?;
        insert_evidence(&tx, &event_evidence).map_err(map_evidence_error)?;
        updated.evidence_references.push(EvidenceReference {
            target: StableRef::EventRef(event_id.clone()),
            event_id: Some(event_id.clone()),
            summary: format!(
                "verification transition: {}",
                updated.verification_status.as_str()
            ),
        });
        replace_memory(&tx, &updated)?;
        if updated.verification_status == VerificationStatus::Invalidated {
            upsert_tombstone(&tx, &updated.memory_id, reason, now, &event_id)?;
        }
        let snapshot_json = capture_replay_snapshot(tx.deref(), &updated)?;
        match updated.verification_status {
            VerificationStatus::Invalidated => self.append_memory_invalidated(
                &updated,
                reason,
                None,
                Some(snapshot_json),
                prepared,
            )?,
            _ => self.append_memory_updated(
                prior,
                &updated,
                &summary,
                Some(snapshot_json),
                prepared,
            )?,
        };
        tx.commit()?;
        load_memory(conn.deref(), &updated.memory_id)
    }

    fn list_filtered(
        &self,
        filter_sql: &str,
        filter_value: Value,
        scope: &ScopeFilter,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, MemoryStoreError> {
        ensure_scope_filter(scope)?;
        let conn = lock_connection(&self.conn)?;
        let predicate = scope.to_sql_predicate();
        let mut params = Vec::new();
        let sql = if filter_sql == "1 = 1" {
            format!(
                "SELECT {} FROM memories WHERE {} ORDER BY created_at DESC, memory_id ASC LIMIT ?",
                select_columns(),
                predicate.where_clause
            )
        } else {
            params.push(filter_value);
            format!(
                "SELECT {} FROM memories WHERE {} AND {} ORDER BY created_at DESC, memory_id ASC LIMIT ?",
                select_columns(),
                filter_sql,
                predicate.where_clause
            )
        };
        params.extend(predicate.bind_values);
        params.push(Value::Integer(limit_i64(limit)?));
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(params), decode_memory_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(convert_row_error)
    }

    fn append_memory_created(
        &self,
        record: &MemoryRecord,
        idempotency_key: &IdempotencyKey,
        replay_snapshot_json: Option<String>,
        prepared: crate::events::writer::PreparedEventWrite,
    ) -> Result<EventId, MemoryStoreError> {
        let payload = EventPayload::MemoryCreated(MemoryCreatedPayload {
            memory_id: record.memory_id.clone(),
            class: record.class.as_str().to_string(),
            stream: classify_stream(record.class, record.assertion_type)
                .as_str()
                .to_string(),
            scope: record.scope.as_str().to_string(),
            idempotency_key: idempotency_key.as_str().to_string(),
            source_event_id: record.provenance_event_ids.last().cloned(),
            evidence_event_ids: collect_evidence_event_ids(record),
            symbol_ids: record.linked_symbols.clone(),
            doc_section_ids: record.linked_docs.clone(),
            replay_snapshot_json,
        });
        self.append_lifecycle_event(
            record,
            payload,
            "memory created",
            FlushPolicy::Batched { interval_ms: 50 },
            prepared,
        )
    }

    fn append_memory_updated(
        &self,
        prior: &MemoryRecord,
        record: &MemoryRecord,
        summary: &str,
        replay_snapshot_json: Option<String>,
        prepared: crate::events::writer::PreparedEventWrite,
    ) -> Result<EventId, MemoryStoreError> {
        let payload = EventPayload::MemoryUpdated(MemoryUpdatedPayload {
            memory_id: record.memory_id.clone(),
            previous_event_id: prior.provenance_event_ids.last().cloned(),
            evidence_event_ids: collect_evidence_event_ids(record),
            superseded_memory_id: record.superseded_by.clone(),
            update_summary: summary.to_string(),
            replay_snapshot_json,
        });
        self.append_lifecycle_event(
            record,
            payload,
            summary,
            FlushPolicy::Batched { interval_ms: 50 },
            prepared,
        )
    }

    fn append_memory_invalidated(
        &self,
        record: &MemoryRecord,
        reason: &str,
        invalidated_by_event_id: Option<EventId>,
        replay_snapshot_json: Option<String>,
        prepared: crate::events::writer::PreparedEventWrite,
    ) -> Result<EventId, MemoryStoreError> {
        let payload = EventPayload::MemoryInvalidated(MemoryInvalidatedPayload {
            memory_id: record.memory_id.clone(),
            invalidated_by_event_id,
            contradicting_memory_ids: record
                .contradiction_links
                .iter()
                .map(|link| link.memory_id.clone())
                .collect(),
            reason: reason.to_string(),
            replay_snapshot_json,
        });
        self.append_lifecycle_event(record, payload, reason, FlushPolicy::Sync, prepared)
    }

    fn append_lifecycle_event(
        &self,
        record: &MemoryRecord,
        payload: EventPayload,
        summary: &str,
        flush_policy: FlushPolicy,
        prepared: crate::events::writer::PreparedEventWrite,
    ) -> Result<EventId, MemoryStoreError> {
        let envelope = PartialEnvelope {
            workspace_id: None,
            branch: BranchRef {
                name: record
                    .scope_branch
                    .clone()
                    .unwrap_or_else(|| "memory-graph".to_string()),
            },
            session_id: SessionId {
                value: record
                    .scope_session_id
                    .clone()
                    .unwrap_or_else(|| "memory-graph".to_string()),
            },
            task_id: None,
            actor: Actor::Daemon,
            kind: payload.kind(),
            references: lifecycle_refs(record),
            summary: CompactSummary::new(summary.to_string()).map_err(|error| {
                MemoryStoreError::ConstraintViolation {
                    detail: error.to_string(),
                }
            })?,
            payload,
        };
        self.event_writer
            .append_prepared_with_flush_policy(prepared, envelope, flush_policy)
            .map_err(MemoryStoreError::EventEmit)
    }

    fn prepare_memory_event_write(
        &self,
        record: &MemoryRecord,
        summary: &str,
    ) -> Result<crate::events::writer::PreparedEventWrite, MemoryStoreError> {
        let envelope = PartialEnvelope {
            workspace_id: None,
            branch: BranchRef {
                name: record
                    .scope_branch
                    .clone()
                    .unwrap_or_else(|| "memory-graph".to_string()),
            },
            session_id: SessionId {
                value: record
                    .scope_session_id
                    .clone()
                    .unwrap_or_else(|| "memory-graph".to_string()),
            },
            task_id: None,
            actor: Actor::Daemon,
            kind: EventKind::MemoryUpdated,
            references: lifecycle_refs(record),
            summary: CompactSummary::new(summary.to_string()).map_err(|error| {
                MemoryStoreError::ConstraintViolation {
                    detail: error.to_string(),
                }
            })?,
            payload: EventPayload::MemoryUpdated(MemoryUpdatedPayload {
                memory_id: record.memory_id.clone(),
                previous_event_id: record.provenance_event_ids.last().cloned(),
                evidence_event_ids: collect_evidence_event_ids(record),
                superseded_memory_id: record.superseded_by.clone(),
                update_summary: summary.to_string(),
                replay_snapshot_json: None,
            }),
        };
        self.event_writer
            .prepare_write(&envelope)
            .map_err(MemoryStoreError::EventEmit)
    }
}

fn lock_connection(
    conn: &Arc<Mutex<Connection>>,
) -> Result<std::sync::MutexGuard<'_, Connection>, MemoryStoreError> {
    conn.lock()
        .map_err(|_| MemoryStoreError::ConstraintViolation {
            detail: "memory store connection lock was poisoned".to_string(),
        })
}

fn record_from_draft(memory_id: MemoryId, draft: MemoryDraft, now: i64) -> MemoryRecord {
    MemoryRecord {
        memory_id,
        content: draft.content,
        class: draft.class,
        assertion_type: draft.assertion_type,
        scope: draft.scope.scope,
        scope_session_id: draft.scope.scope_session_id,
        scope_branch: draft.scope.scope_branch,
        scope_workspace_id: draft.scope.scope_workspace_id,
        scope_user_id: draft.scope.scope_user_id,
        scope_org_id: draft.scope.scope_org_id,
        verification_status: VerificationStatus::Unverified,
        confidence: draft.confidence,
        confidence_reason: draft.confidence_reason,
        freshness_policy: draft.freshness_policy,
        validity_conditions: draft.validity_conditions,
        invalidation_triggers: draft.invalidation_triggers,
        provenance_event_ids: draft.provenance_event_ids,
        evidence_references: draft.evidence_references,
        linked_files: draft.linked_files,
        linked_symbols: draft.linked_symbols,
        linked_docs: draft.linked_docs,
        linked_tests: draft.linked_tests,
        linked_memories: draft.linked_memories,
        contradiction_links: draft.contradiction_links,
        supersession_links: draft.supersession_links,
        access_history: draft.access_history,
        last_verified_event_id: None,
        last_verified_state: None,
        usefulness_score: 0.0,
        usefulness_score_updated_at: now,
        created_at: now,
        created_by: draft.created_by,
        updated_at: now,
        updated_by: draft.updated_by,
        superseded_by: draft.superseded_by,
        schema_version: if draft.schema_version == 0 {
            MEMORY_SCHEMA_VERSION
        } else {
            draft.schema_version
        },
    }
}

fn apply_patch(
    mut record: MemoryRecord,
    patch: MemoryPatch,
) -> Result<MemoryRecord, MemoryStoreError> {
    if let Some(content) = patch.content {
        record.content = content;
    }
    if let Some(confidence) = patch.confidence {
        validate_confidence(confidence)?;
        record.confidence = confidence;
    }
    if let Some(freshness_policy) = patch.freshness_policy {
        record.freshness_policy = freshness_policy;
    }
    if let Some(validity_conditions) = patch.validity_conditions {
        record.validity_conditions = validity_conditions;
    }
    if let Some(invalidation_triggers) = patch.invalidation_triggers {
        record.invalidation_triggers = invalidation_triggers;
    }
    if let Some(scope) = patch.scope {
        validate_scope_state(&scope)?;
        record.scope = scope.scope;
        record.scope_session_id = scope.scope_session_id;
        record.scope_branch = scope.scope_branch;
        record.scope_workspace_id = scope.scope_workspace_id;
        record.scope_user_id = scope.scope_user_id;
        record.scope_org_id = scope.scope_org_id;
    }
    Ok(record)
}

fn collect_initial_evidence(
    memory_id: &MemoryId,
    event_id: &EventId,
    _record: &MemoryRecord,
    initial_evidence: &[MemoryEvidence],
    now: i64,
) -> Result<Vec<MemoryEvidence>, MemoryStoreError> {
    let mut evidence = Vec::new();
    evidence.push(event_reference_evidence(
        memory_id.clone(),
        event_id.clone(),
        now,
        "memory created",
    )?);
    for (index, existing) in initial_evidence.iter().cloned().enumerate() {
        let mut stored = existing;
        stored.memory_id = memory_id.clone();
        if stored.evidence_id.as_str().is_empty() {
            stored.evidence_id = MemoryEvidenceId(format!("{}-seed-{index}", memory_id.ulid));
        }
        evidence.push(stored);
    }
    Ok(evidence)
}

fn stamp_evidence(
    mut evidence: MemoryEvidence,
    memory_id: MemoryId,
    event_id: EventId,
    now: i64,
) -> Result<MemoryEvidence, MemoryStoreError> {
    evidence.memory_id = memory_id;
    evidence.event_id = Some(event_id);
    evidence.captured_at = chrono_timestamp(now)?;
    Ok(evidence)
}

fn event_reference_evidence(
    memory_id: MemoryId,
    event_id: EventId,
    now: i64,
    suffix: &str,
) -> Result<MemoryEvidence, MemoryStoreError> {
    let normalized_suffix = suffix.replace(' ', "-");
    Ok(MemoryEvidence {
        evidence_id: MemoryEvidenceId(format!(
            "{}-{}-{}",
            memory_id.ulid, event_id.ulid, normalized_suffix
        )),
        memory_id,
        event_id: Some(event_id.clone()),
        anchor: EvidenceAnchor::EventReference(event_id),
        captured_at: chrono_timestamp(now)?,
        captured_by: Actor::Daemon,
    })
}

fn chrono_timestamp(unix_seconds: i64) -> Result<DateTime<Utc>, MemoryStoreError> {
    Ok(DateTime::from_unix_seconds(unix_seconds))
}

fn insert_memory(tx: &Transaction<'_>, record: &MemoryRecord) -> Result<(), MemoryStoreError> {
    tx.execute(
        "INSERT INTO memories
            (memory_id, content, class, assertion_type, scope, scope_session_id, scope_branch,
             scope_workspace_id, scope_user_id, scope_org_id, verification_status, confidence,
             confidence_reason, freshness_policy_json, validity_conditions_json,
             invalidation_triggers_json, provenance_event_ids_json, evidence_references_json,
             linked_files_json, linked_symbols_json, linked_docs_json, linked_tests_json,
             linked_memories_json, contradiction_links_json, supersession_links_json,
             access_history_json, last_verified_event_id, last_verified_state, usefulness_score,
             usefulness_score_updated_at, created_at, created_by, updated_at, updated_by,
             superseded_by, schema_version)
         VALUES
            (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
             ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34,
             ?35, ?36)",
        params_from_iter(memory_params(record)),
    )
    .map(|_| ())
    .map_err(map_sqlite_error)
}

fn replace_memory(tx: &Transaction<'_>, record: &MemoryRecord) -> Result<(), MemoryStoreError> {
    tx.execute(
        "UPDATE memories SET
            content = ?2,
            class = ?3,
            assertion_type = ?4,
            scope = ?5,
            scope_session_id = ?6,
            scope_branch = ?7,
            scope_workspace_id = ?8,
            scope_user_id = ?9,
            scope_org_id = ?10,
            verification_status = ?11,
            confidence = ?12,
            confidence_reason = ?13,
            freshness_policy_json = ?14,
            validity_conditions_json = ?15,
            invalidation_triggers_json = ?16,
            provenance_event_ids_json = ?17,
            evidence_references_json = ?18,
            linked_files_json = ?19,
            linked_symbols_json = ?20,
            linked_docs_json = ?21,
            linked_tests_json = ?22,
            linked_memories_json = ?23,
            contradiction_links_json = ?24,
            supersession_links_json = ?25,
            access_history_json = ?26,
            last_verified_event_id = ?27,
            last_verified_state = ?28,
            usefulness_score = ?29,
            usefulness_score_updated_at = ?30,
            created_at = ?31,
            created_by = ?32,
            updated_at = ?33,
            updated_by = ?34,
            superseded_by = ?35,
            schema_version = ?36
         WHERE memory_id = ?1",
        params_from_iter(memory_params(record)),
    )
    .map(|_| ())
    .map_err(map_sqlite_error)
}

fn bind_idempotency(
    tx: &Transaction<'_>,
    idempotency_key: &IdempotencyKey,
    memory_id: &MemoryId,
) -> Result<(), MemoryStoreError> {
    tx.execute(
        "INSERT OR IGNORE INTO memory_idempotency (idempotency_key, memory_id) VALUES (?1, ?2)",
        params![
            idempotency_key.as_str(),
            encode_identity(&Identity::Memory(memory_id.clone()))
        ],
    )?;
    let Some(bound) = lookup_idempotency(tx.deref(), idempotency_key)? else {
        return Err(MemoryStoreError::ConstraintViolation {
            detail: format!(
                "idempotency key `{}` was not persisted",
                idempotency_key.as_str()
            ),
        });
    };
    if bound != *memory_id {
        return Err(MemoryStoreError::ConstraintViolation {
            detail: format!(
                "idempotency key `{}` is already bound to `{}`",
                idempotency_key.as_str(),
                bound
            ),
        });
    }
    Ok(())
}

fn lookup_idempotency(
    conn: &Connection,
    idempotency_key: &IdempotencyKey,
) -> Result<Option<MemoryId>, MemoryStoreError> {
    conn.query_row(
        "SELECT memory_id FROM memory_idempotency WHERE idempotency_key = ?1",
        params![idempotency_key.as_str()],
        |row| {
            let encoded: String = row.get(0)?;
            match decode_identity(&encoded)
                .map_err(|error| row_decode_error(0, rusqlite::types::Type::Text, error))?
            {
                Identity::Memory(memory_id) => Ok(memory_id),
                other => Err(row_decode_error(
                    0,
                    rusqlite::types::Type::Text,
                    MemoryGraphParseError::InvalidJsonColumn {
                        column: "memory_id",
                        reason: format!("expected memory identity, got {:?}", other.kind()),
                    },
                )),
            }
        },
    )
    .optional()
    .map_err(convert_row_error)
}

fn upsert_tombstone(
    tx: &Transaction<'_>,
    memory_id: &MemoryId,
    reason: &str,
    deleted_at: i64,
    event_id: &EventId,
) -> Result<(), MemoryStoreError> {
    tx.execute(
        "INSERT INTO memory_tombstones (memory_id, reason, deleted_at, deleted_by_event_id)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(memory_id) DO UPDATE SET
            reason = excluded.reason,
            deleted_at = excluded.deleted_at,
            deleted_by_event_id = excluded.deleted_by_event_id",
        params![
            encode_identity(&Identity::Memory(memory_id.clone())),
            reason,
            deleted_at,
            encode_identity(&Identity::Event(event_id.clone()))
        ],
    )?;
    Ok(())
}

fn load_memory(conn: &Connection, memory_id: &MemoryId) -> Result<MemoryRecord, MemoryStoreError> {
    let mut statement = conn.prepare(&format!(
        "SELECT {} FROM memories WHERE memory_id = ?1",
        select_columns()
    ))?;
    statement
        .query_row(
            params![encode_identity(&Identity::Memory(memory_id.clone()))],
            decode_memory_row,
        )
        .map_err(convert_row_error)
        .map_err(|error| match error {
            MemoryStoreError::Storage(rusqlite::Error::QueryReturnedNoRows) => {
                MemoryStoreError::NotFound(memory_id.clone())
            }
            other => other,
        })
}

fn decode_memory_row(row: &rusqlite::Row<'_>) -> Result<MemoryRecord, rusqlite::Error> {
    let memory_id = decode_memory_identity(&row.get::<_, String>(0)?)?;
    Ok(MemoryRecord {
        memory_id,
        content: row.get(1)?,
        class: parse_enum(row.get::<_, String>(2)?, 2)?,
        assertion_type: parse_enum(row.get::<_, String>(3)?, 3)?,
        scope: parse_enum(row.get::<_, String>(4)?, 4)?,
        scope_session_id: row.get(5)?,
        scope_branch: row.get(6)?,
        scope_workspace_id: row.get(7)?,
        scope_user_id: row.get(8)?,
        scope_org_id: row.get(9)?,
        verification_status: parse_enum(row.get::<_, String>(10)?, 10)?,
        confidence: row.get(11)?,
        confidence_reason: row.get(12)?,
        freshness_policy: decode_json(row, 13, "freshness_policy_json")?,
        validity_conditions: decode_json(row, 14, "validity_conditions_json")?,
        invalidation_triggers: decode_json(row, 15, "invalidation_triggers_json")?,
        provenance_event_ids: decode_json(row, 16, "provenance_event_ids_json")?,
        evidence_references: decode_json(row, 17, "evidence_references_json")?,
        linked_files: decode_json(row, 18, "linked_files_json")?,
        linked_symbols: decode_json(row, 19, "linked_symbols_json")?,
        linked_docs: decode_json(row, 20, "linked_docs_json")?,
        linked_tests: decode_json(row, 21, "linked_tests_json")?,
        linked_memories: decode_json(row, 22, "linked_memories_json")?,
        contradiction_links: decode_json(row, 23, "contradiction_links_json")?,
        supersession_links: decode_json(row, 24, "supersession_links_json")?,
        access_history: decode_json(row, 25, "access_history_json")?,
        last_verified_event_id: row.get(26)?,
        last_verified_state: row
            .get::<_, Option<String>>(27)?
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(|error| row_decode_error(27, rusqlite::types::Type::Text, error))?,
        usefulness_score: row.get(28)?,
        usefulness_score_updated_at: row.get(29)?,
        created_at: row.get(30)?,
        created_by: row.get(31)?,
        updated_at: row.get(32)?,
        updated_by: row.get(33)?,
        superseded_by: row
            .get::<_, Option<String>>(34)?
            .map(|value| decode_memory_identity(&value))
            .transpose()?,
        schema_version: row.get(35)?,
    })
}

fn decode_memory_identity(encoded: &str) -> Result<MemoryId, rusqlite::Error> {
    match decode_identity(encoded)
        .map_err(|error| row_decode_error(0, rusqlite::types::Type::Text, error))?
    {
        Identity::Memory(memory_id) => Ok(memory_id),
        other => Err(row_decode_error(
            0,
            rusqlite::types::Type::Text,
            MemoryGraphParseError::InvalidJsonColumn {
                column: "memory_id",
                reason: format!("expected memory identity, got {:?}", other.kind()),
            },
        )),
    }
}

fn parse_enum<T>(value: String, column_index: usize) -> Result<T, rusqlite::Error>
where
    T: std::str::FromStr<Err = MemoryGraphParseError>,
{
    value
        .parse()
        .map_err(|error| row_decode_error(column_index, rusqlite::types::Type::Text, error))
}

fn decode_json<T>(
    row: &rusqlite::Row<'_>,
    column_index: usize,
    column: &'static str,
) -> Result<T, rusqlite::Error>
where
    T: for<'de> serde::Deserialize<'de>,
{
    let value: String = row.get(column_index)?;
    decode_json_column(column, &value)
        .map_err(|error| row_decode_error(column_index, rusqlite::types::Type::Text, error))
}

pub(crate) fn memory_params_for_replay(record: &MemoryRecord) -> [Value; 36] {
    memory_params(record)
}

fn memory_params(record: &MemoryRecord) -> [Value; 36] {
    [
        Value::Text(encode_identity(&Identity::Memory(record.memory_id.clone()))),
        Value::Text(record.content.clone()),
        Value::Text(record.class.as_str().to_string()),
        Value::Text(record.assertion_type.as_str().to_string()),
        Value::Text(record.scope.as_str().to_string()),
        optional_text(record.scope_session_id.clone()),
        optional_text(record.scope_branch.clone()),
        optional_text(record.scope_workspace_id.clone()),
        optional_text(record.scope_user_id.clone()),
        optional_text(record.scope_org_id.clone()),
        Value::Text(record.verification_status.as_str().to_string()),
        Value::Real(record.confidence),
        Value::Text(record.confidence_reason.clone()),
        Value::Text(serde_json::to_string(&record.freshness_policy).expect("freshness encodes")),
        Value::Text(serde_json::to_string(&record.validity_conditions).expect("validity encodes")),
        Value::Text(serde_json::to_string(&record.invalidation_triggers).expect("triggers encode")),
        Value::Text(
            serde_json::to_string(&record.provenance_event_ids).expect("provenance encodes"),
        ),
        Value::Text(serde_json::to_string(&record.evidence_references).expect("evidence encodes")),
        Value::Text(serde_json::to_string(&record.linked_files).expect("files encode")),
        Value::Text(serde_json::to_string(&record.linked_symbols).expect("symbols encode")),
        Value::Text(serde_json::to_string(&record.linked_docs).expect("docs encode")),
        Value::Text(serde_json::to_string(&record.linked_tests).expect("tests encode")),
        Value::Text(
            serde_json::to_string(&record.linked_memories).expect("linked memories encode"),
        ),
        Value::Text(
            serde_json::to_string(&record.contradiction_links).expect("contradictions encode"),
        ),
        Value::Text(
            serde_json::to_string(&record.supersession_links).expect("supersessions encode"),
        ),
        Value::Text(serde_json::to_string(&record.access_history).expect("access history encodes")),
        optional_i64(record.last_verified_event_id),
        optional_json(record.last_verified_state.clone()),
        Value::Real(record.usefulness_score),
        Value::Integer(record.usefulness_score_updated_at),
        Value::Integer(record.created_at),
        Value::Text(record.created_by.clone()),
        Value::Integer(record.updated_at),
        Value::Text(record.updated_by.clone()),
        record
            .superseded_by
            .clone()
            .map(|memory_id| Value::Text(encode_identity(&Identity::Memory(memory_id))))
            .unwrap_or(Value::Null),
        Value::Integer(record.schema_version),
    ]
}

fn select_columns() -> &'static str {
    "memory_id, content, class, assertion_type, scope, scope_session_id, scope_branch, \
     scope_workspace_id, scope_user_id, scope_org_id, verification_status, confidence, \
     confidence_reason, freshness_policy_json, validity_conditions_json, \
     invalidation_triggers_json, provenance_event_ids_json, evidence_references_json, \
     linked_files_json, linked_symbols_json, linked_docs_json, linked_tests_json, \
     linked_memories_json, contradiction_links_json, supersession_links_json, access_history_json, \
     last_verified_event_id, last_verified_state, usefulness_score, usefulness_score_updated_at, \
     created_at, created_by, updated_at, updated_by, superseded_by, schema_version"
}

fn validate_scope_state(scope: &MemoryScopeState) -> Result<(), MemoryStoreError> {
    let valid = match scope.scope {
        MemoryScope::Session => {
            scope.scope_session_id.is_some()
                && scope.scope_branch.is_none()
                && scope.scope_workspace_id.is_none()
                && scope.scope_user_id.is_none()
                && scope.scope_org_id.is_none()
        }
        MemoryScope::Branch => {
            scope.scope_session_id.is_none()
                && scope.scope_branch.is_some()
                && scope.scope_workspace_id.is_some()
                && scope.scope_user_id.is_none()
                && scope.scope_org_id.is_none()
        }
        MemoryScope::Repo => {
            scope.scope_session_id.is_none()
                && scope.scope_branch.is_none()
                && scope.scope_workspace_id.is_some()
                && scope.scope_user_id.is_none()
                && scope.scope_org_id.is_none()
        }
        MemoryScope::User => {
            scope.scope_session_id.is_none()
                && scope.scope_branch.is_none()
                && scope.scope_workspace_id.is_none()
                && scope.scope_user_id.is_some()
                && scope.scope_org_id.is_none()
        }
        MemoryScope::Organization => {
            scope.scope_session_id.is_none()
                && scope.scope_branch.is_none()
                && scope.scope_workspace_id.is_none()
                && scope.scope_user_id.is_none()
                && scope.scope_org_id.is_some()
        }
    };
    if valid {
        Ok(())
    } else {
        Err(MemoryStoreError::ConstraintViolation {
            detail: format!("scope fields do not match `{}`", scope.scope.as_str()),
        })
    }
}

fn validate_confidence(confidence: f64) -> Result<(), MemoryStoreError> {
    if (0.0..=1.0).contains(&confidence) {
        Ok(())
    } else {
        Err(MemoryStoreError::ConstraintViolation {
            detail: format!("confidence `{confidence}` must be between 0 and 1"),
        })
    }
}

fn workspace_id_for_draft(draft: &MemoryDraft) -> Result<String, MemoryStoreError> {
    workspace_id_for_scope(
        &draft.scope,
        &draft.provenance_event_ids,
        &draft.linked_files,
        &draft.linked_symbols,
        &draft.linked_docs,
    )
}

fn workspace_id_for_scope(
    scope: &MemoryScopeState,
    provenance_event_ids: &[EventId],
    linked_files: &[FileId],
    linked_symbols: &[SymbolId],
    linked_docs: &[SectionId],
) -> Result<String, MemoryStoreError> {
    match scope.scope {
        MemoryScope::Branch | MemoryScope::Repo => {
            scope
                .scope_workspace_id
                .clone()
                .ok_or_else(|| MemoryStoreError::ConstraintViolation {
                    detail: "branch and repo scopes require scope_workspace_id".to_string(),
                })
        }
        MemoryScope::Session | MemoryScope::User | MemoryScope::Organization => {
            provenance_event_ids
                .first()
                .map(|event_id| event_id.workspace_id.clone())
                .or_else(|| {
                    linked_files
                        .first()
                        .map(|file_id| file_id.workspace_id.clone())
                })
                .or_else(|| {
                    linked_symbols
                        .first()
                        .map(|symbol_id| symbol_id.file.workspace_id.clone())
                })
                .or_else(|| {
                    linked_docs
                        .first()
                        .map(|section_id| section_id.doc.workspace_id.clone())
                })
                .ok_or_else(|| MemoryStoreError::ConstraintViolation {
                    detail: format!(
                    "scope `{}` needs provenance, files, symbols, or docs to derive workspace id",
                    scope.scope.as_str()
                ),
                })
        }
    }
}

fn record_scope_filter(record: &MemoryRecord) -> ScopeFilter {
    match record.scope {
        MemoryScope::Session => {
            ScopeFilter::session(record.scope_session_id.clone().unwrap_or_default())
        }
        MemoryScope::Branch => ScopeFilter::branch(
            record.scope_workspace_id.clone().unwrap_or_default(),
            record.scope_branch.clone().unwrap_or_default(),
        ),
        MemoryScope::Repo => {
            ScopeFilter::repo(record.scope_workspace_id.clone().unwrap_or_default())
        }
        MemoryScope::User => ScopeFilter::user(record.scope_user_id.clone().unwrap_or_default()),
        MemoryScope::Organization => {
            ScopeFilter::organization(record.scope_org_id.clone().unwrap_or_default())
        }
    }
}

fn ensure_scope_filter(scope: &ScopeFilter) -> Result<(), MemoryStoreError> {
    if scope.is_empty() {
        Err(MemoryStoreError::ScopeViolation(ScopeError::Underspecified))
    } else {
        Ok(())
    }
}

fn lifecycle_refs(record: &MemoryRecord) -> Vec<StableRef> {
    let mut refs = vec![StableRef::MemoryRef(record.memory_id.clone())];
    refs.extend(
        record
            .linked_symbols
            .iter()
            .cloned()
            .map(StableRef::SymbolRef),
    );
    refs.extend(
        record
            .linked_docs
            .iter()
            .cloned()
            .map(StableRef::DocSectionRef),
    );
    refs
}

fn collect_evidence_event_ids(record: &MemoryRecord) -> Vec<EventId> {
    record
        .evidence_references
        .iter()
        .filter_map(|reference| reference.event_id.clone())
        .collect()
}

fn optional_text(value: Option<String>) -> Value {
    value.map(Value::Text).unwrap_or(Value::Null)
}

fn optional_i64(value: Option<i64>) -> Value {
    value.map(Value::Integer).unwrap_or(Value::Null)
}

fn optional_json(value: Option<serde_json::Value>) -> Value {
    value
        .map(|json| Value::Text(serde_json::to_string(&json).expect("json encodes")))
        .unwrap_or(Value::Null)
}

fn convert_row_error(error: rusqlite::Error) -> MemoryStoreError {
    match error {
        rusqlite::Error::FromSqlConversionFailure(_, _, inner) => {
            match inner.downcast::<MemoryGraphParseError>() {
                Ok(parse_error) => MemoryStoreError::ConstraintViolation {
                    detail: parse_error.to_string(),
                },
                Err(other) => MemoryStoreError::Storage(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    other,
                )),
            }
        }
        other => MemoryStoreError::Storage(other),
    }
}

fn map_evidence_error(error: MemoryEvidenceError) -> MemoryStoreError {
    MemoryStoreError::ConstraintViolation {
        detail: error.to_string(),
    }
}

fn map_sqlite_error(error: rusqlite::Error) -> MemoryStoreError {
    warn_on_fk_violation("memory_store.write", &error);
    match error {
        rusqlite::Error::SqliteFailure(_, Some(detail)) => {
            MemoryStoreError::ConstraintViolation { detail }
        }
        other => MemoryStoreError::Storage(other),
    }
}

fn limit_i64(limit: usize) -> Result<i64, MemoryStoreError> {
    i64::try_from(limit).map_err(|_| MemoryStoreError::ConstraintViolation {
        detail: format!("limit `{limit}` exceeds i64 range"),
    })
}

fn now_unix_seconds() -> Result<i64, MemoryStoreError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| MemoryStoreError::ConstraintViolation {
            detail: format!("system clock precedes Unix epoch: {error}"),
        })?;
    i64::try_from(duration.as_secs()).map_err(|_| MemoryStoreError::ConstraintViolation {
        detail: "system clock exceeds i64 seconds".to_string(),
    })
}

fn next_ulid(unix_seconds: i64) -> String {
    let millis = u128::from(unix_seconds.max(0) as u64) * 1_000;
    let sequence = u128::from(MEMORY_ULID_COUNTER.fetch_add(1, Ordering::SeqCst));
    let value = (millis << 80) | sequence;
    let mut output = [b'0'; 26];
    for (slot, byte) in output.iter_mut().enumerate().rev() {
        let shift = (25 - slot) * 5;
        let index = ((value >> shift) & 0x1f) as usize;
        *byte = ULID_ALPHABET[index];
    }
    String::from_utf8(output.to_vec()).expect("ULID alphabet is valid UTF-8")
}
