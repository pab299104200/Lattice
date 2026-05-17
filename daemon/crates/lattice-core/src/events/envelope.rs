use std::fmt;

use chrono::{DateTime, Utc};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::events::hashing::PayloadHash;
use crate::events::kinds::{EventKind, EventPayload};
use crate::events::DocSectionId;
use crate::identity::{ContextHandleId, EventId, FileId, MemoryId, SymbolId, WorkspaceId};

const COMPACT_SUMMARY_MAX_BYTES: usize = 512;

/// Validation error returned when event model invariants are violated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventModelError {
    SummaryTooLong {
        actual_bytes: usize,
        max_bytes: usize,
    },
    KindPayloadMismatch {
        expected: EventKind,
        actual: EventKind,
    },
}

impl fmt::Display for EventModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EventModelError::SummaryTooLong {
                actual_bytes,
                max_bytes,
            } => write!(
                formatter,
                "compact summary exceeds {} bytes (got {})",
                max_bytes, actual_bytes
            ),
            EventModelError::KindPayloadMismatch { expected, actual } => write!(
                formatter,
                "event payload kind mismatch: envelope kind `{}` does not match payload kind `{}`",
                expected.as_str(),
                actual.as_str()
            ),
        }
    }
}

impl std::error::Error for EventModelError {}

/// Stable branch reference for an event stream.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BranchRef {
    /// Branch or ref name associated with the event.
    pub name: String,
}

/// Stable session identifier for event grouping.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId {
    /// Session id that groups related assistant and user actions.
    pub value: String,
}

/// Stable task identifier for events linked to one task.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId {
    /// Task id supplied by the caller or orchestration layer.
    pub value: String,
}

/// Actor responsible for an event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Actor {
    /// Assistant-generated event attributed to a model id.
    Assistant {
        /// Model name that produced the event.
        model: String,
    },
    /// Direct user-authored event.
    User,
    /// Tool-emitted event attributed to a tool name.
    Tool {
        /// Tool name responsible for the event.
        name: String,
    },
    /// Daemon-generated internal event.
    Daemon,
}

/// Stable typed references carried by an event envelope.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StableRef {
    /// Reference to a stable file identity.
    FileRef(FileId),
    /// Reference to a stable symbol identity.
    SymbolRef(SymbolId),
    /// Reference to a stable document section identity.
    DocSectionRef(DocSectionId),
    /// Reference to another event in the append-only stream.
    EventRef(EventId),
    /// Reference to a durable memory record.
    MemoryRef(MemoryId),
    /// Reference to a follow-up context handle.
    ContextHandleRef(ContextHandleId),
}

/// Location metadata for the event payload body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PayloadLocation {
    /// Payload bytes were stored inline with the event row.
    Inline {
        /// Serialized payload size in bytes.
        bytes_len: u32,
    },
    /// Payload bytes were spilled into a secondary storage row.
    Spilled {
        /// Row id of the spilled payload body.
        row_id: i64,
    },
}

/// Compact human-readable summary with a hard byte ceiling.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CompactSummary(String);

impl CompactSummary {
    /// Construct a validated compact summary with a 512-byte ceiling.
    pub fn new(value: impl Into<String>) -> Result<Self, EventModelError> {
        let value = value.into();
        let actual_bytes = value.as_bytes().len();
        if actual_bytes > COMPACT_SUMMARY_MAX_BYTES {
            return Err(EventModelError::SummaryTooLong {
                actual_bytes,
                max_bytes: COMPACT_SUMMARY_MAX_BYTES,
            });
        }

        Ok(Self(value))
    }

    /// Return the summary as a borrowed string slice.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl Serialize for CompactSummary {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CompactSummary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CompactSummaryVisitor;

        impl<'de> Visitor<'de> for CompactSummaryVisitor {
            type Value = CompactSummary;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a compact summary string up to 512 bytes")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                CompactSummary::new(value).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                CompactSummary::new(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_string(CompactSummaryVisitor)
    }
}

/// Complete typed event record for the append-only event log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Stable event id for this record.
    pub event_id: EventId,
    /// Workspace id that owns the event.
    pub workspace_id: WorkspaceId,
    /// Branch or ref associated with the event.
    pub branch: BranchRef,
    /// Session id grouping related events.
    pub session_id: SessionId,
    /// Optional task id when the event belongs to a task.
    pub task_id: Option<TaskId>,
    /// Actor responsible for the event.
    pub actor: Actor,
    /// UTC timestamp when the event occurred.
    pub timestamp: DateTime<Utc>,
    /// Event kind used for routing and storage.
    pub kind: EventKind,
    /// Stable references attached to the event.
    pub references: Vec<StableRef>,
    /// Canonical payload hash for replay and dedupe work.
    pub payload_hash: PayloadHash,
    /// Compact summary safe for prompt-side snippets.
    pub summary: CompactSummary,
    /// Storage location of the full serialized payload.
    pub payload_location: PayloadLocation,
    /// Typed payload body for this event kind.
    pub payload: EventPayload,
}

impl EventEnvelope {
    /// Construct an event envelope while enforcing kind/payload parity.
    pub fn new(
        event_id: EventId,
        workspace_id: WorkspaceId,
        branch: BranchRef,
        session_id: SessionId,
        task_id: Option<TaskId>,
        actor: Actor,
        timestamp: DateTime<Utc>,
        kind: EventKind,
        references: Vec<StableRef>,
        payload_hash: PayloadHash,
        summary: CompactSummary,
        payload_location: PayloadLocation,
        payload: EventPayload,
    ) -> Result<Self, EventModelError> {
        let actual = payload.kind();
        if kind != actual {
            return Err(EventModelError::KindPayloadMismatch {
                expected: kind,
                actual,
            });
        }

        Ok(Self {
            event_id,
            workspace_id,
            branch,
            session_id,
            task_id,
            actor,
            timestamp,
            kind,
            references,
            payload_hash,
            summary,
            payload_location,
            payload,
        })
    }
}
