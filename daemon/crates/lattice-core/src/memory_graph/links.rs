use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::trace_span;

use super::{
    decode_actor, decode_event_id, decode_identity_text, decode_timestamp, encode_actor,
    encode_event_id, encode_identity_text, encode_timestamp, row_decode_error,
    warn_on_fk_violation, MemoryGraphParseError, TestId, VerificationStatus,
};
use crate::events::{Actor, DocSectionId};
use crate::identity::{FileId, SymbolId};
use crate::identity::{Identity, MemoryId};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryLinkId(pub String);

impl MemoryLinkId {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLinkType {
    Supports,
    Contradicts,
    Supersedes,
    Refines,
    Generalizes,
    Specializes,
    CoOccursWith,
    DerivedFrom,
    AppliesTo,
    ValidatedBy,
    InvalidatedBy,
}

impl MemoryLinkType {
    pub const VALUES: &'static [&'static str] = &[
        "supports",
        "contradicts",
        "supersedes",
        "refines",
        "generalizes",
        "specializes",
        "co_occurs_with",
        "derived_from",
        "applies_to",
        "validated_by",
        "invalidated_by",
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            MemoryLinkType::Supports => "supports",
            MemoryLinkType::Contradicts => "contradicts",
            MemoryLinkType::Supersedes => "supersedes",
            MemoryLinkType::Refines => "refines",
            MemoryLinkType::Generalizes => "generalizes",
            MemoryLinkType::Specializes => "specializes",
            MemoryLinkType::CoOccursWith => "co_occurs_with",
            MemoryLinkType::DerivedFrom => "derived_from",
            MemoryLinkType::AppliesTo => "applies_to",
            MemoryLinkType::ValidatedBy => "validated_by",
            MemoryLinkType::InvalidatedBy => "invalidated_by",
        }
    }
}

impl FromStr for MemoryLinkType {
    type Err = MemoryGraphParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "supports" => Ok(Self::Supports),
            "contradicts" => Ok(Self::Contradicts),
            "supersedes" => Ok(Self::Supersedes),
            "refines" => Ok(Self::Refines),
            "generalizes" => Ok(Self::Generalizes),
            "specializes" => Ok(Self::Specializes),
            "co_occurs_with" => Ok(Self::CoOccursWith),
            "derived_from" => Ok(Self::DerivedFrom),
            "applies_to" => Ok(Self::AppliesTo),
            "validated_by" => Ok(Self::ValidatedBy),
            "invalidated_by" => Ok(Self::InvalidatedBy),
            other => Err(MemoryGraphParseError::InvalidJsonColumn {
                column: "link_type",
                reason: format!("unknown memory link type `{other}`"),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemoryLinkTargetKind {
    Memory,
    File,
    Symbol,
    DocSection,
    Test,
}

impl MemoryLinkTargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::File => "file",
            Self::Symbol => "symbol",
            Self::DocSection => "doc_section",
            Self::Test => "test",
        }
    }
}

impl FromStr for MemoryLinkTargetKind {
    type Err = MemoryGraphParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "memory" => Ok(Self::Memory),
            "file" => Ok(Self::File),
            "symbol" => Ok(Self::Symbol),
            "doc_section" => Ok(Self::DocSection),
            "test" => Ok(Self::Test),
            other => Err(MemoryGraphParseError::InvalidJsonColumn {
                column: "target_kind",
                reason: format!("unknown memory link target kind `{other}`"),
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLinkTarget {
    Memory(MemoryId),
    File(FileId),
    Symbol(SymbolId),
    DocSection(DocSectionId),
    Test(TestId),
}

impl MemoryLinkTarget {
    fn kind(&self) -> MemoryLinkTargetKind {
        match self {
            Self::Memory(_) => MemoryLinkTargetKind::Memory,
            Self::File(_) => MemoryLinkTargetKind::File,
            Self::Symbol(_) => MemoryLinkTargetKind::Symbol,
            Self::DocSection(_) => MemoryLinkTargetKind::DocSection,
            Self::Test(_) => MemoryLinkTargetKind::Test,
        }
    }

    fn encode_id(&self) -> Result<String, MemoryLinkError> {
        match self {
            Self::Memory(value) => Ok(encode_identity_text(&Identity::Memory(value.clone()))),
            Self::File(value) => Ok(encode_identity_text(&Identity::File(value.clone()))),
            Self::Symbol(value) => Ok(encode_identity_text(&Identity::Symbol(value.clone()))),
            Self::DocSection(value) => Ok(encode_identity_text(&Identity::Section(value.clone()))),
            Self::Test(value) => {
                serde_json::to_string(value).map_err(MemoryLinkError::TargetEncode)
            }
        }
    }

    fn decode(kind: &str, encoded: &str) -> Result<Self, MemoryLinkError> {
        match MemoryLinkTargetKind::from_str(kind)? {
            MemoryLinkTargetKind::Memory => match decode_identity_text(encoded, "target_id")? {
                Identity::Memory(value) => Ok(Self::Memory(value)),
                other => Err(MemoryLinkError::TargetKindMismatch {
                    expected: "memory",
                    actual: format!("{:?}", other.kind()),
                }),
            },
            MemoryLinkTargetKind::File => match decode_identity_text(encoded, "target_id")? {
                Identity::File(value) => Ok(Self::File(value)),
                other => Err(MemoryLinkError::TargetKindMismatch {
                    expected: "file",
                    actual: format!("{:?}", other.kind()),
                }),
            },
            MemoryLinkTargetKind::Symbol => match decode_identity_text(encoded, "target_id")? {
                Identity::Symbol(value) => Ok(Self::Symbol(value)),
                other => Err(MemoryLinkError::TargetKindMismatch {
                    expected: "symbol",
                    actual: format!("{:?}", other.kind()),
                }),
            },
            MemoryLinkTargetKind::DocSection => match decode_identity_text(encoded, "target_id")? {
                Identity::Section(value) => Ok(Self::DocSection(value)),
                other => Err(MemoryLinkError::TargetKindMismatch {
                    expected: "doc_section",
                    actual: format!("{:?}", other.kind()),
                }),
            },
            MemoryLinkTargetKind::Test => serde_json::from_str(encoded)
                .map(Self::Test)
                .map_err(MemoryLinkError::TargetDecode),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryLink {
    pub link_id: MemoryLinkId,
    pub source: MemoryId,
    pub target: MemoryLinkTarget,
    pub link_type: MemoryLinkType,
    pub strength: f32,
    pub reason: String,
    pub evidence_event_id: Option<crate::identity::EventId>,
    pub created_by: Actor,
    pub created_at: DateTime<Utc>,
    pub verification_status: VerificationStatus,
}

impl MemoryLink {
    pub fn try_new(
        link_id: MemoryLinkId,
        source: MemoryId,
        target: MemoryLinkTarget,
        link_type: MemoryLinkType,
        strength: f32,
        reason: String,
        evidence_event_id: Option<crate::identity::EventId>,
        created_by: Actor,
        created_at: DateTime<Utc>,
        verification_status: VerificationStatus,
    ) -> Result<Self, MemoryLinkError> {
        validate_strength(strength)?;
        Ok(Self {
            link_id,
            source,
            target,
            link_type,
            strength,
            reason,
            evidence_event_id,
            created_by,
            created_at,
            verification_status,
        })
    }
}

#[derive(Debug, Error)]
pub enum MemoryLinkError {
    #[error("memory link strength must be within [0, 1], got {strength}")]
    InvalidStrength { strength: f32 },
    #[error("failed to encode test link target: {0}")]
    TargetEncode(serde_json::Error),
    #[error("failed to decode test link target: {0}")]
    TargetDecode(serde_json::Error),
    #[error("link target kind mismatch: expected {expected}, got {actual}")]
    TargetKindMismatch {
        expected: &'static str,
        actual: String,
    },
    #[error(transparent)]
    Parse(#[from] MemoryGraphParseError),
    #[error("memory link SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

pub fn insert_link(conn: &Connection, link: &MemoryLink) -> Result<(), MemoryLinkError> {
    validate_strength(link.strength)?;
    let _span = trace_span!(
        "memory_graph.insert_link",
        link_type = link.link_type.as_str()
    )
    .entered();
    let (created_by_kind, created_by_detail) = encode_actor(&link.created_by);
    let target_kind = link.target.kind();
    let target_id = link.target.encode_id()?;
    let evidence_event_id = link.evidence_event_id.as_ref().map(encode_event_id);
    let source_memory_id = encode_identity_text(&Identity::Memory(link.source.clone()));
    let result = conn.execute(
        "INSERT INTO memory_links
            (link_id, source_memory_id, target_kind, target_id, link_type, strength, reason,
             evidence_event_id, created_by_kind, created_by_detail, created_at, verification_status)
         VALUES
            (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            link.link_id.as_str(),
            source_memory_id,
            target_kind.as_str(),
            target_id,
            link.link_type.as_str(),
            link.strength,
            link.reason,
            evidence_event_id,
            created_by_kind,
            created_by_detail,
            encode_timestamp(link.created_at),
            link.verification_status.as_str(),
        ],
    );
    if let Err(error) = result {
        warn_on_fk_violation("insert_link", &error);
        return Err(MemoryLinkError::Sqlite(error));
    }
    Ok(())
}

pub fn get_links_from(
    conn: &Connection,
    source: &MemoryId,
) -> Result<Vec<MemoryLink>, MemoryLinkError> {
    let source_memory_id = encode_identity_text(&Identity::Memory(source.clone()));
    query_links(
        conn,
        "SELECT link_id, source_memory_id, target_kind, target_id, link_type, strength, reason,
                evidence_event_id, created_by_kind, created_by_detail, created_at, verification_status
         FROM memory_links
         WHERE source_memory_id = ?1
         ORDER BY created_at ASC, link_id ASC",
        params![source_memory_id],
    )
}

pub fn get_links_to(
    conn: &Connection,
    target: &MemoryLinkTarget,
) -> Result<Vec<MemoryLink>, MemoryLinkError> {
    query_links(
        conn,
        "SELECT link_id, source_memory_id, target_kind, target_id, link_type, strength, reason,
                evidence_event_id, created_by_kind, created_by_detail, created_at, verification_status
         FROM memory_links
         WHERE target_kind = ?1 AND target_id = ?2
         ORDER BY created_at ASC, link_id ASC",
        params![target.kind().as_str(), target.encode_id()?],
    )
}

pub fn get_links_by_type(
    conn: &Connection,
    link_type: MemoryLinkType,
) -> Result<Vec<MemoryLink>, MemoryLinkError> {
    query_links(
        conn,
        "SELECT link_id, source_memory_id, target_kind, target_id, link_type, strength, reason,
                evidence_event_id, created_by_kind, created_by_detail, created_at, verification_status
         FROM memory_links
         WHERE link_type = ?1
         ORDER BY created_at ASC, link_id ASC",
        params![link_type.as_str()],
    )
}

fn query_links<P>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<MemoryLink>, MemoryLinkError>
where
    P: rusqlite::Params,
{
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map(params, |row| {
        let source = match decode_identity_text(&row.get::<_, String>(1)?, "source_memory_id")
            .map_err(|error| {
                row_decode_error(
                    1,
                    rusqlite::types::Type::Text,
                    MemoryLinkError::Parse(error),
                )
            })? {
            Identity::Memory(value) => value,
            other => {
                return Err(row_decode_error(
                    1,
                    rusqlite::types::Type::Text,
                    MemoryLinkError::TargetKindMismatch {
                        expected: "memory",
                        actual: format!("{:?}", other.kind()),
                    },
                ));
            }
        };
        let evidence_event_id = row
            .get::<_, Option<String>>(7)?
            .map(|value| decode_event_id(&value, "evidence_event_id"))
            .transpose()
            .map_err(|error| {
                row_decode_error(
                    7,
                    rusqlite::types::Type::Text,
                    MemoryLinkError::Parse(error),
                )
            })?;
        let verification_status = VerificationStatus::from_str(row.get::<_, String>(11)?.as_str())
            .map_err(|error| {
                row_decode_error(
                    11,
                    rusqlite::types::Type::Text,
                    MemoryLinkError::Parse(error),
                )
            })?;
        Ok(MemoryLink {
            link_id: MemoryLinkId(row.get(0)?),
            source,
            target: MemoryLinkTarget::decode(
                row.get::<_, String>(2)?.as_str(),
                row.get::<_, String>(3)?.as_str(),
            )
            .map_err(|error| row_decode_error(3, rusqlite::types::Type::Text, error))?,
            link_type: MemoryLinkType::from_str(row.get::<_, String>(4)?.as_str()).map_err(
                |error| {
                    row_decode_error(
                        4,
                        rusqlite::types::Type::Text,
                        MemoryLinkError::Parse(error),
                    )
                },
            )?,
            strength: row.get(5)?,
            reason: row.get(6)?,
            evidence_event_id,
            created_by: decode_actor(row.get::<_, String>(8)?.as_str(), row.get(9)?, "created_by")
                .map_err(|error| {
                    row_decode_error(
                        8,
                        rusqlite::types::Type::Text,
                        MemoryLinkError::Parse(error),
                    )
                })?,
            created_at: decode_timestamp(row.get(10)?, "created_at").map_err(|error| {
                row_decode_error(
                    10,
                    rusqlite::types::Type::Integer,
                    MemoryLinkError::Parse(error),
                )
            })?,
            verification_status,
        })
    })?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| match error {
            rusqlite::Error::FromSqlConversionFailure(_, _, inner) => {
                match inner.downcast::<MemoryLinkError>() {
                    Ok(memory_error) => *memory_error,
                    Err(other) => {
                        MemoryLinkError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            other,
                        ))
                    }
                }
            }
            other => MemoryLinkError::Sqlite(other),
        })
}

fn validate_strength(strength: f32) -> Result<(), MemoryLinkError> {
    if (0.0..=1.0).contains(&strength) {
        Ok(())
    } else {
        Err(MemoryLinkError::InvalidStrength { strength })
    }
}

impl fmt::Display for MemoryLinkId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
