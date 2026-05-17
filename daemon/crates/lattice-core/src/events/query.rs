//! Typed query builder for bounded event-log reads.
//!
//! The reader defaults to a 1000-row limit and rejects requests above 10_000 rows.
//! That ceiling keeps event-log reads bounded on hot paths: callers can page or use
//! the streaming reader for larger replays, but no single query may degenerate into
//! a full-log scan or unbounded in-memory buffer.

use thiserror::Error;

use crate::events::reader::EventReader;
use crate::events::{
    BranchRef, EventEnvelope, EventKind, EventStoreError, PayloadHash, SessionId, TaskId,
};
use crate::identity::{EventId, WorkspaceId};
use crate::{DateTime, Utc};

pub const DEFAULT_EVENT_QUERY_LIMIT: usize = 1_000;
pub const EVENT_QUERY_LIMIT_CEILING: usize = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryOrder {
    OldestFirst,
    NewestFirst,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskScope {
    pub task_id: TaskId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionScope {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TailScope {
    Task(TaskScope),
    Session(SessionScope),
}

impl From<TaskScope> for TailScope {
    fn from(value: TaskScope) -> Self {
        Self::Task(value)
    }
}

impl From<SessionScope> for TailScope {
    fn from(value: SessionScope) -> Self {
        Self::Session(value)
    }
}

#[derive(Debug, Error)]
pub enum EventQueryError {
    #[error("event queries require task, session, or workspace+branch scope")]
    Unscoped,
    #[error("event query limit {requested} exceeds ceiling {ceiling}")]
    LimitTooLarge { requested: usize, ceiling: usize },
    #[error("event store read failed: {0}")]
    Storage(#[from] EventStoreError),
    #[error("event payload deserialization failed: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("event payload deserialization failed for `{event_id:?}`: {source}")]
    PayloadDecode {
        event_id: EventId,
        source: serde_json::Error,
    },
    #[error(
        "event payload corruption detected for `{event_id:?}`: expected {expected}, actual {actual}"
    )]
    PayloadCorruption {
        event_id: EventId,
        expected: PayloadHash,
        actual: PayloadHash,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EventQuery {
    pub(crate) task_id: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) workspace_id: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) kinds: Vec<EventKind>,
    pub(crate) after: Option<DateTime<Utc>>,
    pub(crate) before: Option<DateTime<Utc>>,
    pub(crate) limit: Option<usize>,
    pub(crate) order: Option<QueryOrder>,
}

impl EventQuery {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn task(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    pub fn session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn workspace(mut self, workspace_id: impl Into<String>) -> Self {
        self.workspace_id = Some(workspace_id.into());
        self
    }

    pub fn branch(mut self, branch_ref: impl Into<String>) -> Self {
        self.branch = Some(branch_ref.into());
        self
    }

    pub fn kind(mut self, kind: EventKind) -> Self {
        self.kinds = vec![kind];
        self
    }

    pub fn kinds(mut self, kinds: &[EventKind]) -> Self {
        self.kinds = kinds.to_vec();
        self
    }

    pub fn after(mut self, timestamp: DateTime<Utc>) -> Self {
        self.after = Some(timestamp);
        self
    }

    pub fn before(mut self, timestamp: DateTime<Utc>) -> Self {
        self.before = Some(timestamp);
        self
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn order(mut self, order: QueryOrder) -> Self {
        self.order = Some(order);
        self
    }

    pub fn execute(self, reader: &EventReader) -> Result<Vec<EventEnvelope>, EventQueryError> {
        reader.execute(self)
    }

    pub(crate) fn order_or_default(&self) -> QueryOrder {
        self.order.unwrap_or(QueryOrder::OldestFirst)
    }

    pub(crate) fn limit_or_default(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_EVENT_QUERY_LIMIT).max(1)
    }

    pub(crate) fn validate(&self) -> Result<(), EventQueryError> {
        let has_scope = self.task_id.is_some()
            || self.session_id.is_some()
            || (self.workspace_id.is_some() && self.branch.is_some());
        if !has_scope {
            return Err(EventQueryError::Unscoped);
        }

        let limit = self.limit_or_default();
        if limit > EVENT_QUERY_LIMIT_CEILING {
            return Err(EventQueryError::LimitTooLarge {
                requested: limit,
                ceiling: EVENT_QUERY_LIMIT_CEILING,
            });
        }
        Ok(())
    }

    pub(crate) fn trace_scope(&self) -> String {
        if let Some(task_id) = &self.task_id {
            return format!("task:{task_id}");
        }
        if let Some(session_id) = &self.session_id {
            return format!("session:{session_id}");
        }
        if let (Some(workspace_id), Some(branch)) = (&self.workspace_id, &self.branch) {
            return format!("workspace:{workspace_id}@{branch}");
        }
        "unscoped".to_string()
    }
}

pub(crate) fn event_id(workspace_id: &str, ulid: &str) -> EventId {
    EventId {
        workspace_id: workspace_id.to_string(),
        ulid: ulid.to_string(),
    }
}

pub(crate) fn branch_ref(name: &str) -> BranchRef {
    BranchRef {
        name: name.to_string(),
    }
}

pub(crate) fn session_id(value: &str) -> SessionId {
    SessionId {
        value: value.to_string(),
    }
}

pub(crate) fn task_id(value: &str) -> TaskId {
    TaskId {
        value: value.to_string(),
    }
}

pub(crate) fn workspace_id(value: &str) -> WorkspaceId {
    value.to_string()
}
