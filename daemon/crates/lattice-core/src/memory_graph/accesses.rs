use std::fmt;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::trace_span;

use super::{
    decode_actor, decode_timestamp, encode_actor, encode_timestamp, row_decode_error,
    warn_on_fk_violation, MemoryGraphParseError,
};
use crate::events::Actor;
use crate::identity::{EventId, Identity, MemoryId};
use crate::memory_graph::{decode_event_id, encode_event_id, encode_identity_text};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryAccessId(pub String);

impl MemoryAccessId {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryAccess {
    pub access_id: MemoryAccessId,
    pub memory_id: MemoryId,
    pub accessed_at: DateTime<Utc>,
    pub accessed_in_event: EventId,
    pub accessor: Actor,
    pub inclusion_reason: String,
    pub was_used: Option<bool>,
    pub downstream_outcome_event: Option<EventId>,
}

#[derive(Debug, Error)]
pub enum MemoryAccessError {
    #[error("memory access `{access_id}` does not exist")]
    NotFound { access_id: String },
    #[error(transparent)]
    Parse(#[from] MemoryGraphParseError),
    #[error("memory access SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

pub fn record_access(conn: &Connection, access: &MemoryAccess) -> Result<(), MemoryAccessError> {
    let _span = trace_span!("memory_graph.record_access").entered();
    let (accessor_kind, accessor_detail) = encode_actor(&access.accessor);
    let memory_id = encode_identity_text(&Identity::Memory(access.memory_id.clone()));
    let result = conn.execute(
        "INSERT INTO memory_accesses
            (access_id, memory_id, accessed_at, accessed_in_event, accessor_kind, accessor_detail,
             inclusion_reason, was_used, downstream_outcome_event)
         VALUES
            (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            access.access_id.as_str(),
            memory_id,
            encode_timestamp(access.accessed_at),
            encode_event_id(&access.accessed_in_event),
            accessor_kind,
            accessor_detail,
            access.inclusion_reason,
            access.was_used.map(bool_to_i64),
            access
                .downstream_outcome_event
                .as_ref()
                .map(encode_event_id),
        ],
    );
    if let Err(error) = result {
        warn_on_fk_violation("record_access", &error);
        return Err(MemoryAccessError::Sqlite(error));
    }
    Ok(())
}

pub fn list_accesses_for(
    conn: &Connection,
    memory_id: &MemoryId,
) -> Result<Vec<MemoryAccess>, MemoryAccessError> {
    let encoded_memory_id = encode_identity_text(&Identity::Memory(memory_id.clone()));
    let mut statement = conn.prepare(
        "SELECT access_id, memory_id, accessed_at, accessed_in_event, accessor_kind, accessor_detail,
                inclusion_reason, was_used, downstream_outcome_event
         FROM memory_accesses
         WHERE memory_id = ?1
         ORDER BY accessed_at DESC, access_id DESC",
    )?;
    let rows = statement.query_map(params![encoded_memory_id], |row| {
        let memory_id = match crate::memory_graph::decode_identity_text(
            row.get::<_, String>(1)?.as_str(),
            "memory_id",
        )
        .map_err(|error| {
            row_decode_error(
                1,
                rusqlite::types::Type::Text,
                MemoryAccessError::Parse(error),
            )
        })? {
            Identity::Memory(value) => value,
            other => {
                return Err(row_decode_error(
                    1,
                    rusqlite::types::Type::Text,
                    MemoryAccessError::Parse(MemoryGraphParseError::InvalidJsonColumn {
                        column: "memory_id",
                        reason: format!("expected memory identity, got {:?}", other.kind()),
                    }),
                ));
            }
        };
        Ok(MemoryAccess {
            access_id: MemoryAccessId(row.get(0)?),
            memory_id,
            accessed_at: decode_timestamp(row.get(2)?, "accessed_at").map_err(|error| {
                row_decode_error(
                    2,
                    rusqlite::types::Type::Integer,
                    MemoryAccessError::Parse(error),
                )
            })?,
            accessed_in_event: decode_event_id(&row.get::<_, String>(3)?, "accessed_in_event")
                .map_err(|error| {
                    row_decode_error(
                        3,
                        rusqlite::types::Type::Text,
                        MemoryAccessError::Parse(error),
                    )
                })?,
            accessor: decode_actor(row.get::<_, String>(4)?.as_str(), row.get(5)?, "accessor")
                .map_err(|error| {
                    row_decode_error(
                        4,
                        rusqlite::types::Type::Text,
                        MemoryAccessError::Parse(error),
                    )
                })?,
            inclusion_reason: row.get(6)?,
            was_used: row.get::<_, Option<i64>>(7)?.map(|value| value != 0),
            downstream_outcome_event: row
                .get::<_, Option<String>>(8)?
                .map(|value| decode_event_id(&value, "downstream_outcome_event"))
                .transpose()
                .map_err(|error| {
                    row_decode_error(
                        8,
                        rusqlite::types::Type::Text,
                        MemoryAccessError::Parse(error),
                    )
                })?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(convert_row_error)
}

pub fn mark_used(
    conn: &Connection,
    access_id: &MemoryAccessId,
    was_used: bool,
    downstream_outcome_event: Option<&EventId>,
) -> Result<(), MemoryAccessError> {
    let _span = trace_span!("memory_graph.mark_used").entered();
    let updated = conn.execute(
        "UPDATE memory_accesses
         SET was_used = ?2, downstream_outcome_event = ?3
         WHERE access_id = ?1",
        params![
            access_id.as_str(),
            bool_to_i64(was_used),
            downstream_outcome_event.map(encode_event_id),
        ],
    )?;
    if updated == 0 {
        return Err(MemoryAccessError::NotFound {
            access_id: access_id.to_string(),
        });
    }
    Ok(())
}

fn convert_row_error(error: rusqlite::Error) -> MemoryAccessError {
    match error {
        rusqlite::Error::FromSqlConversionFailure(_, _, inner) => {
            match inner.downcast::<MemoryAccessError>() {
                Ok(memory_error) => *memory_error,
                Err(other) => MemoryAccessError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    other,
                )),
            }
        }
        other => MemoryAccessError::Sqlite(other),
    }
}

impl fmt::Display for MemoryAccessId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

fn bool_to_i64(value: bool) -> i64 {
    if value {
        1
    } else {
        0
    }
}
