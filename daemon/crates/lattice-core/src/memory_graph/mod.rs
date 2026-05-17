//! Typed memory graph schema for the cognitive workspace memory redesign.
//!
//! This module is the Rust model surface for
//! `docs/architecture/2026-05-16-memory-graph-schema.md`, especially
//! `## Classes`, `## Required fields`, `## Scopes`,
//! `## Verification statuses`, `## Freshness, validity, invalidation`,
//! `## CounterMemory semantics`, `## Links`, `## Evidence`,
//! `## Accesses`, `## Scores`, and `## Schema parity table`.

use std::time::Duration;

use crate::events::Actor;
use crate::identity::{decode_identity, encode_identity, EventId, Identity};
use crate::{DateTime, Utc};
use rusqlite::Connection;
use tracing::warn;

pub mod accesses;
pub mod classes;
pub mod evidence;
pub mod links;
pub mod migration;
pub(crate) mod migration_mapping;
pub mod replay;
pub mod scope;
pub mod scores;
pub mod store;
pub mod streams;
pub mod transitions;

#[cfg(test)]
mod contradiction_tests;
#[cfg(test)]
mod links_tests;
#[cfg(test)]
mod migration_tests;
#[cfg(test)]
mod replay_tests;
#[cfg(test)]
mod scope_tests;
#[cfg(test)]
mod store_tests;
#[cfg(test)]
mod supersession_tests;
#[cfg(test)]
mod tests_common;

pub use accesses::{
    list_accesses_for, mark_used, record_access, MemoryAccess, MemoryAccessError, MemoryAccessId,
};
pub use classes::{
    decode_json_column, AssertionType, EvidenceReference, FreshnessKind, FreshnessPolicy,
    InvalidationTrigger, MemoryAccessRecord, MemoryClass, MemoryGraphParseError,
    MemoryLinkReference, MemoryRecord, MemoryScope, TestId, TriggerKind, ValidityCondition,
    ValidityPredicate, VerificationStatus,
};
pub use evidence::{
    get_evidence_for, insert_evidence, EvidenceAnchor, MemoryEvidence, MemoryEvidenceError,
    MemoryEvidenceId,
};
pub use links::{
    get_links_by_type, get_links_from, get_links_to, insert_link, MemoryLink, MemoryLinkError,
    MemoryLinkId, MemoryLinkTarget, MemoryLinkTargetKind, MemoryLinkType,
};
pub use migration::{MemoryMigrator, MigrationError, MigrationPlan, MigrationReport};
pub use replay::{
    capture_replay_snapshot, decode_replay_snapshot, replay_events, MemoryReplayError,
    MemoryReplaySnapshot,
};
pub use scope::{ScopeClause, ScopeError, ScopeFilter, ScopePredicate};
pub use scores::{
    latest_score, score_history, write_score, MemoryScore, MemoryScoreError, ScoreKind,
};
pub use store::{IdempotencyKey, MemoryDraft, MemoryPatch, MemoryStore, MemoryStoreError};
pub use streams::{classify_stream, default_policy, MemoryStream, RankingProfileRef, StreamPolicy};
pub use transitions::{transition_status, MemoryTransitionError};

#[derive(Debug, thiserror::Error)]
pub enum MemoryGraphError {
    #[error(transparent)]
    Parse(#[from] MemoryGraphParseError),
    #[error(transparent)]
    Link(#[from] MemoryLinkError),
    #[error(transparent)]
    Evidence(#[from] MemoryEvidenceError),
    #[error(transparent)]
    Access(#[from] MemoryAccessError),
    #[error(transparent)]
    Score(#[from] MemoryScoreError),
}

pub fn initialize_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(include_str!("schema.sql"))
}

pub(crate) fn encode_actor(actor: &Actor) -> (&str, Option<&str>) {
    match actor {
        Actor::Assistant { model } => ("assistant", Some(model.as_str())),
        Actor::User => ("user", None),
        Actor::Tool { name } => ("tool", Some(name.as_str())),
        Actor::Daemon => ("daemon", None),
    }
}

pub(crate) fn decode_actor(
    kind: &str,
    detail: Option<String>,
    column_prefix: &'static str,
) -> Result<Actor, MemoryGraphParseError> {
    let detail_for_error = detail.clone();
    match (kind, detail) {
        ("assistant", Some(model)) => Ok(Actor::Assistant { model }),
        ("user", None) => Ok(Actor::User),
        ("tool", Some(name)) => Ok(Actor::Tool { name }),
        ("daemon", None) => Ok(Actor::Daemon),
        _ => Err(MemoryGraphParseError::InvalidJsonColumn {
            column: column_prefix,
            reason: format!(
                "invalid actor columns: kind `{kind}` detail {:?}",
                detail_for_error.as_deref()
            ),
        }),
    }
}

pub(crate) fn encode_identity_text(identity: &Identity) -> String {
    encode_identity(identity)
}

pub(crate) fn decode_identity_text(
    encoded: &str,
    column: &'static str,
) -> Result<Identity, MemoryGraphParseError> {
    decode_identity(encoded).map_err(|error| MemoryGraphParseError::InvalidJsonColumn {
        column,
        reason: error.to_string(),
    })
}

pub(crate) fn encode_event_id(event_id: &EventId) -> String {
    encode_identity_text(&Identity::Event(event_id.clone()))
}

pub(crate) fn decode_event_id(
    encoded: &str,
    column: &'static str,
) -> Result<EventId, MemoryGraphParseError> {
    match decode_identity_text(encoded, column)? {
        Identity::Event(event_id) => Ok(event_id),
        other => Err(MemoryGraphParseError::InvalidJsonColumn {
            column,
            reason: format!("expected event identity, got {:?}", other.kind()),
        }),
    }
}

pub(crate) fn encode_timestamp(value: DateTime<Utc>) -> i64 {
    value.unix_seconds()
}

pub(crate) fn decode_timestamp(
    unix_seconds: i64,
    column: &'static str,
) -> Result<DateTime<Utc>, MemoryGraphParseError> {
    let _ = column;
    Ok(DateTime::from_unix_seconds(unix_seconds))
}

pub(crate) fn encode_duration_secs(value: Duration) -> Result<i64, MemoryGraphParseError> {
    i64::try_from(value.as_secs()).map_err(|_| MemoryGraphParseError::InvalidJsonColumn {
        column: "computed_from_window_secs",
        reason: format!("duration {} seconds exceeds i64", value.as_secs()),
    })
}

pub(crate) fn decode_duration_secs(
    seconds: i64,
    column: &'static str,
) -> Result<Duration, MemoryGraphParseError> {
    let secs = u64::try_from(seconds).map_err(|_| MemoryGraphParseError::InvalidJsonColumn {
        column,
        reason: format!("negative duration `{seconds}`"),
    })?;
    Ok(Duration::from_secs(secs))
}

pub(crate) fn warn_on_fk_violation(operation: &str, error: &rusqlite::Error) {
    if let rusqlite::Error::SqliteFailure(code, _) = error {
        if code.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY {
            warn!(operation, sqlite_error = %error, "memory graph foreign key violation");
        }
    }
}

pub(crate) fn row_decode_error(
    column_index: usize,
    value_type: rusqlite::types::Type,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column_index, value_type, Box::new(error))
}
