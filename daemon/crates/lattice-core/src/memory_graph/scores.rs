use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::trace_span;

use super::{
    decode_duration_secs, decode_timestamp, encode_duration_secs, encode_timestamp,
    row_decode_error, warn_on_fk_violation, MemoryGraphParseError,
};
use crate::identity::{Identity, MemoryId};
use crate::memory_graph::encode_identity_text;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreKind {
    UsefulnessPrior,
    RecentUsefulness,
    RetrievalAccuracy,
    RegressionRisk,
}

impl ScoreKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UsefulnessPrior => "usefulness_prior",
            Self::RecentUsefulness => "recent_usefulness",
            Self::RetrievalAccuracy => "retrieval_accuracy",
            Self::RegressionRisk => "regression_risk",
        }
    }
}

impl FromStr for ScoreKind {
    type Err = MemoryGraphParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "usefulness_prior" => Ok(Self::UsefulnessPrior),
            "recent_usefulness" => Ok(Self::RecentUsefulness),
            "retrieval_accuracy" => Ok(Self::RetrievalAccuracy),
            "regression_risk" => Ok(Self::RegressionRisk),
            other => Err(MemoryGraphParseError::InvalidJsonColumn {
                column: "score_kind",
                reason: format!("unknown score kind `{other}`"),
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryScore {
    pub memory_id: MemoryId,
    pub score_kind: ScoreKind,
    pub value: f32,
    pub computed_at: DateTime<Utc>,
    pub computed_from_window: Duration,
    pub sample_size: u32,
}

#[derive(Debug, Error)]
pub enum MemoryScoreError {
    #[error(transparent)]
    Parse(#[from] MemoryGraphParseError),
    #[error("memory score SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

pub fn write_score(conn: &Connection, score: &MemoryScore) -> Result<(), MemoryScoreError> {
    let _span = trace_span!(
        "memory_graph.write_score",
        score_kind = score.score_kind.as_str()
    )
    .entered();
    let result = conn.execute(
        "INSERT INTO memory_scores
            (memory_id, score_kind, value, computed_at, computed_from_window_secs, sample_size)
         VALUES
            (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            encode_identity_text(&Identity::Memory(score.memory_id.clone())),
            score.score_kind.as_str(),
            score.value,
            encode_timestamp(score.computed_at),
            encode_duration_secs(score.computed_from_window)?,
            i64::from(score.sample_size),
        ],
    );
    if let Err(error) = result {
        warn_on_fk_violation("write_score", &error);
        return Err(MemoryScoreError::Sqlite(error));
    }
    Ok(())
}

pub fn latest_score(
    conn: &Connection,
    memory_id: &MemoryId,
    score_kind: ScoreKind,
) -> Result<Option<MemoryScore>, MemoryScoreError> {
    let results = score_history(conn, memory_id, score_kind)?;
    Ok(results.into_iter().next())
}

pub fn score_history(
    conn: &Connection,
    memory_id: &MemoryId,
    score_kind: ScoreKind,
) -> Result<Vec<MemoryScore>, MemoryScoreError> {
    let mut statement = conn.prepare(
        "SELECT memory_id, score_kind, value, computed_at, computed_from_window_secs, sample_size
         FROM memory_scores
         WHERE memory_id = ?1 AND score_kind = ?2
         ORDER BY computed_at DESC, rowid DESC",
    )?;
    let rows = statement.query_map(
        params![
            encode_identity_text(&Identity::Memory(memory_id.clone())),
            score_kind.as_str()
        ],
        |row| {
            let memory_id = match crate::memory_graph::decode_identity_text(
                row.get::<_, String>(0)?.as_str(),
                "memory_id",
            )
            .map_err(|error| {
                row_decode_error(
                    0,
                    rusqlite::types::Type::Text,
                    MemoryScoreError::Parse(error),
                )
            })? {
                Identity::Memory(value) => value,
                other => {
                    return Err(row_decode_error(
                        0,
                        rusqlite::types::Type::Text,
                        MemoryScoreError::Parse(MemoryGraphParseError::InvalidJsonColumn {
                            column: "memory_id",
                            reason: format!("expected memory identity, got {:?}", other.kind()),
                        }),
                    ));
                }
            };
            let sample_size_i64: i64 = row.get(5)?;
            let sample_size = u32::try_from(sample_size_i64).map_err(|error| {
                row_decode_error(
                    5,
                    rusqlite::types::Type::Integer,
                    MemoryScoreError::Parse(MemoryGraphParseError::InvalidJsonColumn {
                        column: "sample_size",
                        reason: error.to_string(),
                    }),
                )
            })?;
            Ok(MemoryScore {
                memory_id,
                score_kind: ScoreKind::from_str(row.get::<_, String>(1)?.as_str()).map_err(
                    |error| {
                        row_decode_error(
                            1,
                            rusqlite::types::Type::Text,
                            MemoryScoreError::Parse(error),
                        )
                    },
                )?,
                value: row.get(2)?,
                computed_at: decode_timestamp(row.get(3)?, "computed_at").map_err(|error| {
                    row_decode_error(
                        3,
                        rusqlite::types::Type::Integer,
                        MemoryScoreError::Parse(error),
                    )
                })?,
                computed_from_window: decode_duration_secs(
                    row.get(4)?,
                    "computed_from_window_secs",
                )
                .map_err(|error| {
                    row_decode_error(
                        4,
                        rusqlite::types::Type::Integer,
                        MemoryScoreError::Parse(error),
                    )
                })?,
                sample_size,
            })
        },
    )?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(convert_row_error)
}

fn convert_row_error(error: rusqlite::Error) -> MemoryScoreError {
    match error {
        rusqlite::Error::FromSqlConversionFailure(_, _, inner) => {
            match inner.downcast::<MemoryScoreError>() {
                Ok(memory_error) => *memory_error,
                Err(other) => MemoryScoreError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    other,
                )),
            }
        }
        other => MemoryScoreError::Sqlite(other),
    }
}
