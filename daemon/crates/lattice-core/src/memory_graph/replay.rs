use rusqlite::{params, params_from_iter, Connection};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

use crate::events::{EventEnvelope, EventPayload};
use crate::identity::{encode_identity, Identity};

use super::{
    get_evidence_for, get_links_from, insert_evidence, insert_link, MemoryEvidence,
    MemoryEvidenceError, MemoryGraphParseError, MemoryLink, MemoryLinkError, MemoryRecord,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryReplaySnapshot {
    pub memory: MemoryRecord,
    pub evidence: Vec<MemoryEvidence>,
    pub outgoing_links: Vec<MemoryLink>,
}

#[derive(Debug, Error)]
pub enum MemoryReplayError {
    #[error("event `{event_id}` is missing memory replay snapshot data")]
    MissingSnapshot { event_id: String },
    #[error("event `{event_id}` replay snapshot failed to decode: {source}")]
    SnapshotDecode {
        event_id: String,
        source: serde_json::Error,
    },
    #[error(transparent)]
    Storage(#[from] rusqlite::Error),
    #[error(transparent)]
    Evidence(#[from] MemoryEvidenceError),
    #[error(transparent)]
    Link(#[from] MemoryLinkError),
    #[error(transparent)]
    Parse(#[from] MemoryGraphParseError),
}

pub fn capture_replay_snapshot(
    conn: &Connection,
    memory: &MemoryRecord,
) -> Result<String, MemoryReplayError> {
    let snapshot = MemoryReplaySnapshot {
        memory: memory.clone(),
        evidence: get_evidence_for(conn, &memory.memory_id)?,
        outgoing_links: get_links_from(conn, &memory.memory_id)?,
    };
    serde_json::to_string(&snapshot).map_err(|source| MemoryReplayError::SnapshotDecode {
        event_id: memory.memory_id.to_string(),
        source,
    })
}

pub fn decode_replay_snapshot(
    event: &EventEnvelope,
    snapshot_json: &Option<String>,
) -> Result<MemoryReplaySnapshot, MemoryReplayError> {
    let Some(snapshot_json) = snapshot_json else {
        return Err(MemoryReplayError::MissingSnapshot {
            event_id: event.event_id.to_string(),
        });
    };
    serde_json::from_str(snapshot_json).map_err(|source| MemoryReplayError::SnapshotDecode {
        event_id: event.event_id.to_string(),
        source,
    })
}

pub fn replay_events(conn: &Connection, events: &[EventEnvelope]) -> Result<(), MemoryReplayError> {
    for event in events {
        let result = match &event.payload {
            EventPayload::MemoryCreated(payload) => apply_snapshot(
                conn,
                &decode_replay_snapshot(event, &payload.replay_snapshot_json)?,
            ),
            EventPayload::MemoryUpdated(payload) => apply_snapshot(
                conn,
                &decode_replay_snapshot(event, &payload.replay_snapshot_json)?,
            ),
            EventPayload::MemoryInvalidated(payload) => apply_snapshot(
                conn,
                &decode_replay_snapshot(event, &payload.replay_snapshot_json)?,
            ),
            _ => Ok(()),
        };
        if let Err(error) = result {
            warn!(event_id = %event.event_id, error = %error, "memory replay failed");
            return Err(error);
        }
    }
    Ok(())
}

fn apply_snapshot(
    conn: &Connection,
    snapshot: &MemoryReplaySnapshot,
) -> Result<(), MemoryReplayError> {
    let tx = conn.unchecked_transaction()?;
    upsert_memory(&tx, &snapshot.memory)?;

    tx.execute(
        "DELETE FROM memory_evidence WHERE memory_id = ?1",
        params![encode_identity(&Identity::Memory(
            snapshot.memory.memory_id.clone()
        ))],
    )?;
    for evidence in &snapshot.evidence {
        insert_evidence(&tx, evidence)?;
    }

    tx.execute(
        "DELETE FROM memory_links WHERE source_memory_id = ?1",
        params![encode_identity(&Identity::Memory(
            snapshot.memory.memory_id.clone()
        ))],
    )?;
    for link in &snapshot.outgoing_links {
        insert_link(&tx, link)?;
    }
    tx.commit()?;
    Ok(())
}

fn upsert_memory(conn: &Connection, record: &MemoryRecord) -> Result<(), rusqlite::Error> {
    conn.execute(
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
             ?35, ?36)
         ON CONFLICT(memory_id) DO UPDATE SET
            content = excluded.content,
            class = excluded.class,
            assertion_type = excluded.assertion_type,
            scope = excluded.scope,
            scope_session_id = excluded.scope_session_id,
            scope_branch = excluded.scope_branch,
            scope_workspace_id = excluded.scope_workspace_id,
            scope_user_id = excluded.scope_user_id,
            scope_org_id = excluded.scope_org_id,
            verification_status = excluded.verification_status,
            confidence = excluded.confidence,
            confidence_reason = excluded.confidence_reason,
            freshness_policy_json = excluded.freshness_policy_json,
            validity_conditions_json = excluded.validity_conditions_json,
            invalidation_triggers_json = excluded.invalidation_triggers_json,
            provenance_event_ids_json = excluded.provenance_event_ids_json,
            evidence_references_json = excluded.evidence_references_json,
            linked_files_json = excluded.linked_files_json,
            linked_symbols_json = excluded.linked_symbols_json,
            linked_docs_json = excluded.linked_docs_json,
            linked_tests_json = excluded.linked_tests_json,
            linked_memories_json = excluded.linked_memories_json,
            contradiction_links_json = excluded.contradiction_links_json,
            supersession_links_json = excluded.supersession_links_json,
            access_history_json = excluded.access_history_json,
            last_verified_event_id = excluded.last_verified_event_id,
            last_verified_state = excluded.last_verified_state,
            usefulness_score = excluded.usefulness_score,
            usefulness_score_updated_at = excluded.usefulness_score_updated_at,
            created_at = excluded.created_at,
            created_by = excluded.created_by,
            updated_at = excluded.updated_at,
            updated_by = excluded.updated_by,
            superseded_by = excluded.superseded_by,
            schema_version = excluded.schema_version",
        params_from_iter(super::store::memory_params_for_replay(record)),
    )?;
    Ok(())
}
