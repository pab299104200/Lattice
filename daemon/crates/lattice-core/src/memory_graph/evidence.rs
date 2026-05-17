use std::fmt;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::trace_span;

use super::{
    decode_actor, decode_timestamp, encode_actor, encode_timestamp, row_decode_error,
    warn_on_fk_violation, MemoryGraphParseError, TestId,
};
use crate::events::{Actor, DocSectionId};
use crate::identity::{EventId, Identity, MemoryId, SymbolId};
use crate::memory_graph::{decode_event_id, encode_event_id, encode_identity_text};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryEvidenceId(pub String);

impl MemoryEvidenceId {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceAnchor {
    FileSpan {
        file: crate::identity::FileId,
        byte_start: u64,
        byte_end: u64,
        sha256: [u8; 32],
    },
    SymbolRef(SymbolId),
    DocSection {
        id: DocSectionId,
        sha256: [u8; 32],
    },
    TestResult {
        test: TestId,
        passed: bool,
        run_event: EventId,
    },
    EventReference(EventId),
}

impl EvidenceAnchor {
    fn kind(&self) -> &'static str {
        match self {
            Self::FileSpan { .. } => "file_span",
            Self::SymbolRef(_) => "symbol_ref",
            Self::DocSection { .. } => "doc_section",
            Self::TestResult { .. } => "test_result",
            Self::EventReference(_) => "event_reference",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEvidence {
    pub evidence_id: MemoryEvidenceId,
    pub memory_id: MemoryId,
    pub event_id: Option<EventId>,
    pub anchor: EvidenceAnchor,
    pub captured_at: DateTime<Utc>,
    pub captured_by: Actor,
}

#[derive(Debug, Error)]
pub enum MemoryEvidenceError {
    #[error("failed to serialize evidence anchor: {0}")]
    AnchorEncode(serde_json::Error),
    #[error("failed to decode evidence anchor JSON: {0}")]
    AnchorDecode(serde_json::Error),
    #[error("anchor kind mismatch: column says `{kind}`, JSON says `{json_kind}`")]
    AnchorKindMismatch {
        kind: String,
        json_kind: &'static str,
    },
    #[error(transparent)]
    Parse(#[from] MemoryGraphParseError),
    #[error("memory evidence SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

pub fn insert_evidence(
    conn: &Connection,
    evidence: &MemoryEvidence,
) -> Result<(), MemoryEvidenceError> {
    let _span = trace_span!(
        "memory_graph.insert_evidence",
        anchor_kind = evidence.anchor.kind()
    )
    .entered();
    let (captured_by_kind, captured_by_detail) = encode_actor(&evidence.captured_by);
    let memory_id = encode_identity_text(&Identity::Memory(evidence.memory_id.clone()));
    let event_id = evidence.event_id.as_ref().map(encode_event_id);
    let anchor_json =
        serde_json::to_string(&evidence.anchor).map_err(MemoryEvidenceError::AnchorEncode)?;
    let result = conn.execute(
        "INSERT INTO memory_evidence
            (evidence_id, memory_id, event_id, anchor_kind, anchor_json, captured_at,
             captured_by_kind, captured_by_detail)
         VALUES
            (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            evidence.evidence_id.as_str(),
            memory_id,
            event_id,
            evidence.anchor.kind(),
            anchor_json,
            encode_timestamp(evidence.captured_at),
            captured_by_kind,
            captured_by_detail,
        ],
    );
    if let Err(error) = result {
        warn_on_fk_violation("insert_evidence", &error);
        return Err(MemoryEvidenceError::Sqlite(error));
    }
    Ok(())
}

pub fn get_evidence_for(
    conn: &Connection,
    memory_id: &MemoryId,
) -> Result<Vec<MemoryEvidence>, MemoryEvidenceError> {
    let encoded_memory_id = encode_identity_text(&Identity::Memory(memory_id.clone()));
    let mut statement = conn.prepare(
        "SELECT evidence_id, memory_id, event_id, anchor_kind, anchor_json, captured_at,
                captured_by_kind, captured_by_detail
         FROM memory_evidence
         WHERE memory_id = ?1
         ORDER BY captured_at ASC, evidence_id ASC",
    )?;
    let rows = statement.query_map(params![encoded_memory_id], |row| {
        let stored_memory_id = match crate::memory_graph::decode_identity_text(
            row.get::<_, String>(1)?.as_str(),
            "memory_id",
        )
        .map_err(|error| {
            row_decode_error(
                1,
                rusqlite::types::Type::Text,
                MemoryEvidenceError::Parse(error),
            )
        })? {
            Identity::Memory(value) => value,
            other => {
                return Err(row_decode_error(
                    1,
                    rusqlite::types::Type::Text,
                    MemoryEvidenceError::Parse(MemoryGraphParseError::InvalidJsonColumn {
                        column: "memory_id",
                        reason: format!("expected memory identity, got {:?}", other.kind()),
                    }),
                ));
            }
        };
        let event_id = row
            .get::<_, Option<String>>(2)?
            .map(|value| decode_event_id(&value, "event_id"))
            .transpose()
            .map_err(|error| {
                row_decode_error(
                    2,
                    rusqlite::types::Type::Text,
                    MemoryEvidenceError::Parse(error),
                )
            })?;
        let anchor_kind: String = row.get(3)?;
        let anchor_json: String = row.get(4)?;
        let anchor: EvidenceAnchor = serde_json::from_str(&anchor_json)
            .map_err(MemoryEvidenceError::AnchorDecode)
            .map_err(|error| row_decode_error(4, rusqlite::types::Type::Text, error))?;
        if anchor.kind() != anchor_kind {
            return Err(row_decode_error(
                4,
                rusqlite::types::Type::Text,
                MemoryEvidenceError::AnchorKindMismatch {
                    kind: anchor_kind,
                    json_kind: anchor.kind(),
                },
            ));
        }
        Ok(MemoryEvidence {
            evidence_id: MemoryEvidenceId(row.get(0)?),
            memory_id: stored_memory_id,
            event_id,
            anchor,
            captured_at: decode_timestamp(row.get(5)?, "captured_at").map_err(|error| {
                row_decode_error(
                    5,
                    rusqlite::types::Type::Integer,
                    MemoryEvidenceError::Parse(error),
                )
            })?,
            captured_by: decode_actor(
                row.get::<_, String>(6)?.as_str(),
                row.get(7)?,
                "captured_by",
            )
            .map_err(|error| {
                row_decode_error(
                    6,
                    rusqlite::types::Type::Text,
                    MemoryEvidenceError::Parse(error),
                )
            })?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(convert_row_error)
}

fn convert_row_error(error: rusqlite::Error) -> MemoryEvidenceError {
    match error {
        rusqlite::Error::FromSqlConversionFailure(_, _, inner) => match inner
            .downcast::<MemoryEvidenceError>()
        {
            Ok(memory_error) => *memory_error,
            Err(other) => MemoryEvidenceError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                other,
            )),
        },
        other => MemoryEvidenceError::Sqlite(other),
    }
}

impl fmt::Display for MemoryEvidenceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
