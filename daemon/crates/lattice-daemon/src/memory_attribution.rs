//! Durable daemon-side bridge between memory retrievals, graph access rows,
//! terminal workflow outcomes, and adoption metrics.
//!
//! This module intentionally does not inspect tool text, session proximity, or
//! later calls to infer use. A caller must provide the exact `retrieval_id`
//! returned by [`MemoryAttributionBridge::record_retrieval`] and the exact
//! access ids it is resolving. Concrete runtime adapters are responsible for
//! checking persisted event ordering and for executing graph operations in a
//! graph-store transaction.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use lattice_core::events::{canonical_json_bytes, hash_canonical_payload_bytes, Actor};
use lattice_core::identity::{EventId, MemoryId};
use lattice_core::memory_graph::MemoryAccessId;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use thiserror::Error;

/// Explicit evidence supplied by a retrieval implementation. The inclusion
/// reason is required because a memory id alone does not explain why it was
/// surfaced to an assistant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RetrievedMemory {
    pub(crate) memory_id: MemoryId,
    pub(crate) inclusion_reason: String,
}

/// Trusted runtime input for one already-persisted retrieval event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RetrievalRecord {
    pub(crate) retrieval_event: EventId,
    pub(crate) tool_call_event: EventId,
    pub(crate) accessor: Actor,
    pub(crate) memories: Vec<RetrievedMemory>,
}

/// One graph access that must be inserted as part of a retrieval transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingMemoryAccess {
    pub(crate) access_id: MemoryAccessId,
    pub(crate) retrieval_id: String,
    pub(crate) retrieval_event: EventId,
    pub(crate) memory_id: MemoryId,
    pub(crate) accessor: Actor,
    pub(crate) inclusion_reason: String,
}

/// Opaque daemon-generated retrieval identity and its exact pending access
/// identities. These values, not memory ids or prose, are the only valid
/// inputs to a later resolution request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingAccessSet {
    pub(crate) retrieval_id: String,
    pub(crate) retrieval_event: EventId,
    pub(crate) access_ids: Vec<MemoryAccessId>,
}

/// An explicit terminal claim. `NotUsed` is retained in the graph but does
/// not produce an adoption-use metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccessDisposition {
    Used,
    NotUsed,
}

impl AccessDisposition {
    fn as_bool(self) -> bool {
        matches!(self, Self::Used)
    }
}

/// A single graph resolution passed to the graph adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AccessResolution {
    pub(crate) access_id: MemoryAccessId,
    pub(crate) was_used: bool,
    pub(crate) terminal_outcome_event: EventId,
}

/// Outcome of a resolution request. An idempotent retry does not increment
/// `newly_resolved_count` and cannot inflate use telemetry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolveAccessesResult {
    pub(crate) newly_resolved_count: usize,
    pub(crate) idempotent_count: usize,
}

/// Runtime adapter over the durable event log. Implementations must verify
/// that both ids exist, belong to the daemon workspace, and that the terminal
/// event is a newly appended terminal workflow event after the retrieval.
pub(crate) trait MemoryAttributionEvents: Send + Sync {
    fn validate_retrieval(
        &self,
        retrieval_event: &EventId,
        tool_call_event: &EventId,
    ) -> Result<(), String>;

    fn validate_terminal_outcome(
        &self,
        retrieval_event: &EventId,
        terminal_outcome_event: &EventId,
    ) -> Result<(), String>;
}

/// Runtime adapter over the graph database. Each method is required to be
/// idempotent for equal input and transactional for the supplied batch.
pub(crate) trait MemoryAttributionGraph: Send + Sync {
    fn record_pending_accesses(&self, accesses: &[PendingMemoryAccess]) -> Result<(), String>;

    fn resolve_accesses(&self, resolutions: &[AccessResolution]) -> Result<(), String>;
}

/// Runtime adapter over the operational adoption ledger. `metric_id` is
/// stable across recovery retries, so implementations must de-duplicate it.
pub(crate) trait MemoryAttributionMetrics: Send + Sync {
    fn record_retrieval(
        &self,
        metric_id: &str,
        retrieval_id: &str,
        retrieved_count: u64,
    ) -> Result<(), String>;

    fn record_use(
        &self,
        metric_id: &str,
        retrieval_id: &str,
        used_count: u64,
    ) -> Result<(), String>;
}

/// The daemon-owned durable attribution bridge. The SQLite index is separate
/// from the graph store because event and graph persistence are independent
/// stores. It makes replays and post-crash reconciliation deterministic.
pub(crate) struct MemoryAttributionBridge<'a> {
    workspace_id: String,
    index: Mutex<Connection>,
    events: &'a dyn MemoryAttributionEvents,
    graph: &'a dyn MemoryAttributionGraph,
    metrics: &'a dyn MemoryAttributionMetrics,
}

impl<'a> MemoryAttributionBridge<'a> {
    pub(crate) fn open(
        index_path: &Path,
        workspace_id: impl Into<String>,
        events: &'a dyn MemoryAttributionEvents,
        graph: &'a dyn MemoryAttributionGraph,
        metrics: &'a dyn MemoryAttributionMetrics,
    ) -> Result<Self, MemoryAttributionError> {
        let workspace_id = workspace_id.into();
        if workspace_id.trim().is_empty() {
            return Err(MemoryAttributionError::EmptyWorkspace);
        }
        if let Some(parent) = index_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(index_path)?;
        initialize_index(&connection)?;
        Ok(Self {
            workspace_id,
            index: Mutex::new(connection),
            events,
            graph,
            metrics,
        })
    }

    /// Persists pending graph accesses before emitting retrieval telemetry.
    /// Repeating an equal persisted retrieval is safe; a changed candidate
    /// set for the same event is rejected rather than silently relabeled.
    pub(crate) fn record_retrieval(
        &self,
        record: RetrievalRecord,
    ) -> Result<PendingAccessSet, MemoryAttributionError> {
        self.validate_retrieval_record(&record)?;
        self.events
            .validate_retrieval(&record.retrieval_event, &record.tool_call_event)
            .map_err(MemoryAttributionError::Event)?;

        let retrieval_id = derive_retrieval_id(&record.retrieval_event)?;
        let pending = pending_accesses(&retrieval_id, &record)?;
        let mut index = self.lock_index()?;

        let existing = load_retrieval(&index, &retrieval_id)?;
        match existing {
            Some(existing) => {
                ensure_same_retrieval(&index, &retrieval_id, &existing, &record, &pending)?
            }
            None => insert_retrieval(&mut index, &retrieval_id, &record, &pending)?,
        }

        if !retrieval_graph_recorded(&index, &retrieval_id)? {
            if !pending.is_empty() {
                self.graph
                    .record_pending_accesses(&pending)
                    .map_err(MemoryAttributionError::Graph)?;
            }
            set_retrieval_graph_recorded(&index, &retrieval_id)?;
        }

        if !retrieval_metric_recorded(&index, &retrieval_id)? {
            self.metrics
                .record_retrieval(
                    &derive_metric_id("retrieval", &retrieval_id, None)?,
                    &retrieval_id,
                    pending.len() as u64,
                )
                .map_err(MemoryAttributionError::Metrics)?;
            set_retrieval_metric_recorded(&index, &retrieval_id)?;
        }

        Ok(PendingAccessSet {
            retrieval_id,
            retrieval_event: record.retrieval_event,
            access_ids: pending.into_iter().map(|access| access.access_id).collect(),
        })
    }

    /// Resolves only explicitly cited pending access ids. It never credits an
    /// unannotated successful tool result or a merely later event.
    pub(crate) fn resolve_accesses(
        &self,
        retrieval_id: &str,
        terminal_outcome_event: EventId,
        disposition: AccessDisposition,
        cited_access_ids: &[MemoryAccessId],
    ) -> Result<ResolveAccessesResult, MemoryAttributionError> {
        if retrieval_id.trim().is_empty() {
            return Err(MemoryAttributionError::EmptyRetrievalId);
        }
        ensure_unique_access_ids(cited_access_ids)?;

        let mut index = self.lock_index()?;
        let retrieval = load_retrieval(&index, retrieval_id)?.ok_or_else(|| {
            MemoryAttributionError::UnknownRetrieval {
                retrieval_id: retrieval_id.to_string(),
            }
        })?;
        if retrieval.workspace_id != self.workspace_id {
            return Err(MemoryAttributionError::WorkspaceMismatch {
                field: "retrieval_id",
                expected: self.workspace_id.clone(),
                actual: retrieval.workspace_id,
            });
        }
        if terminal_outcome_event.workspace_id != self.workspace_id {
            return Err(MemoryAttributionError::WorkspaceMismatch {
                field: "terminal_outcome_event",
                expected: self.workspace_id.clone(),
                actual: terminal_outcome_event.workspace_id,
            });
        }
        self.events
            .validate_terminal_outcome(&retrieval.retrieval_event, &terminal_outcome_event)
            .map_err(MemoryAttributionError::Event)?;

        let access_rows = load_accesses(&index, retrieval_id)?;
        let by_id = access_rows
            .into_iter()
            .map(|access| (access.access_id.0.clone(), access))
            .collect::<BTreeMap<_, _>>();
        let mut newly_pending = Vec::new();
        let mut idempotent_count = 0;
        for access_id in cited_access_ids {
            let row = by_id.get(access_id.as_str()).ok_or_else(|| {
                MemoryAttributionError::AccessNotInRetrieval {
                    retrieval_id: retrieval_id.to_string(),
                    access_id: access_id.to_string(),
                }
            })?;
            match (&row.resolution, row.metric_recorded) {
                (None, _) => newly_pending.push(row.clone()),
                (Some(resolution), _)
                    if resolution.matches(disposition, &terminal_outcome_event) =>
                {
                    idempotent_count += 1;
                }
                (Some(_), _) => {
                    return Err(MemoryAttributionError::ResolutionConflict {
                        access_id: access_id.to_string(),
                    })
                }
            }
        }

        let resolutions = newly_pending
            .iter()
            .map(|row| AccessResolution {
                access_id: row.access_id.clone(),
                was_used: disposition.as_bool(),
                terminal_outcome_event: terminal_outcome_event.clone(),
            })
            .collect::<Vec<_>>();
        if !resolutions.is_empty() {
            self.graph
                .resolve_accesses(&resolutions)
                .map_err(MemoryAttributionError::Graph)?;
            set_access_resolutions(&mut index, retrieval_id, &resolutions)?;
        }

        // Metrics are intentionally after the graph commit. Read the durable
        // state again so a crash after graph resolution but before a metric can
        // replay the exact stable metric id without changing graph outcome.
        if disposition == AccessDisposition::Used {
            for access_id in cited_access_ids {
                let row = load_access(&index, retrieval_id, access_id.as_str())?
                    .expect("cited access was validated against the durable index");
                if !row.metric_recorded {
                    let metric_id = derive_metric_id(
                        "use",
                        retrieval_id,
                        Some((&row.access_id, &terminal_outcome_event)),
                    )?;
                    self.metrics
                        .record_use(&metric_id, retrieval_id, 1)
                        .map_err(MemoryAttributionError::Metrics)?;
                    set_access_metric_recorded(&index, retrieval_id, row.access_id.as_str())?;
                }
            }
        }

        Ok(ResolveAccessesResult {
            newly_resolved_count: resolutions.len(),
            idempotent_count,
        })
    }

    fn validate_retrieval_record(
        &self,
        record: &RetrievalRecord,
    ) -> Result<(), MemoryAttributionError> {
        check_workspace(
            "retrieval_event",
            &self.workspace_id,
            &record.retrieval_event.workspace_id,
        )?;
        check_workspace(
            "tool_call_event",
            &self.workspace_id,
            &record.tool_call_event.workspace_id,
        )?;
        let mut memory_ids = BTreeSet::new();
        for memory in &record.memories {
            check_workspace(
                "memory_id",
                &self.workspace_id,
                &memory.memory_id.workspace_id,
            )?;
            if memory.inclusion_reason.trim().is_empty() {
                return Err(MemoryAttributionError::EmptyInclusionReason {
                    memory_id: memory.memory_id.ulid.clone(),
                });
            }
            if !memory_ids.insert(memory.memory_id.ulid.clone()) {
                return Err(MemoryAttributionError::DuplicateMemory {
                    memory_id: memory.memory_id.ulid.clone(),
                });
            }
        }
        Ok(())
    }

    fn lock_index(&self) -> Result<std::sync::MutexGuard<'_, Connection>, MemoryAttributionError> {
        self.index
            .lock()
            .map_err(|_| MemoryAttributionError::IndexLockPoisoned)
    }
}

#[derive(Debug, Error)]
pub(crate) enum MemoryAttributionError {
    #[error("memory attribution requires a non-empty runtime workspace id")]
    EmptyWorkspace,
    #[error("memory attribution requires a non-empty retrieval id")]
    EmptyRetrievalId,
    #[error("{field} workspace `{actual}` does not match runtime workspace `{expected}`")]
    WorkspaceMismatch {
        field: &'static str,
        expected: String,
        actual: String,
    },
    #[error("retrieved memory `{memory_id}` requires a non-empty inclusion reason")]
    EmptyInclusionReason { memory_id: String },
    #[error("retrieval includes memory `{memory_id}` more than once")]
    DuplicateMemory { memory_id: String },
    #[error("resolution cites access `{access_id}` more than once")]
    DuplicateAccess { access_id: String },
    #[error("unknown memory retrieval `{retrieval_id}`")]
    UnknownRetrieval { retrieval_id: String },
    #[error("access `{access_id}` is not part of retrieval `{retrieval_id}`")]
    AccessNotInRetrieval {
        retrieval_id: String,
        access_id: String,
    },
    #[error("access `{access_id}` was already resolved with a different outcome")]
    ResolutionConflict { access_id: String },
    #[error("retrieval `{retrieval_id}` was replayed with a different persisted payload")]
    RetrievalConflict { retrieval_id: String },
    #[error("event validation failed: {0}")]
    Event(String),
    #[error("memory graph write failed: {0}")]
    Graph(String),
    #[error("adoption metrics write failed: {0}")]
    Metrics(String),
    #[error("memory attribution index lock poisoned")]
    IndexLockPoisoned,
    #[error("memory attribution index I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("memory attribution index SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("cannot derive deterministic attribution id: {0}")]
    Identifier(#[from] serde_json::Error),
}

#[derive(Clone)]
struct StoredRetrieval {
    workspace_id: String,
    retrieval_event: EventId,
    tool_call_event: EventId,
}

#[derive(Clone)]
struct StoredAccess {
    access_id: MemoryAccessId,
    memory_id: MemoryId,
    inclusion_reason: String,
    resolution: Option<StoredResolution>,
    metric_recorded: bool,
}

#[derive(Clone)]
struct StoredResolution {
    was_used: bool,
    terminal_outcome_event: EventId,
}

impl StoredResolution {
    fn matches(&self, disposition: AccessDisposition, event: &EventId) -> bool {
        self.was_used == disposition.as_bool() && self.terminal_outcome_event == *event
    }
}

fn initialize_index(connection: &Connection) -> Result<(), rusqlite::Error> {
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS memory_attribution_retrievals (
             retrieval_id TEXT PRIMARY KEY NOT NULL,
             workspace_id TEXT NOT NULL,
             retrieval_event_workspace_id TEXT NOT NULL,
             retrieval_event_ulid TEXT NOT NULL,
             tool_call_event_workspace_id TEXT NOT NULL,
             tool_call_event_ulid TEXT NOT NULL,
             graph_recorded INTEGER NOT NULL DEFAULT 0 CHECK (graph_recorded IN (0, 1)),
             retrieval_metric_recorded INTEGER NOT NULL DEFAULT 0 CHECK (retrieval_metric_recorded IN (0, 1))
         );
         CREATE TABLE IF NOT EXISTS memory_attribution_accesses (
             retrieval_id TEXT NOT NULL REFERENCES memory_attribution_retrievals(retrieval_id) ON DELETE CASCADE,
             access_id TEXT NOT NULL,
             memory_workspace_id TEXT NOT NULL,
             memory_ulid TEXT NOT NULL,
             inclusion_reason TEXT NOT NULL,
             was_used INTEGER NULL CHECK (was_used IN (0, 1)),
             outcome_workspace_id TEXT NULL,
             outcome_ulid TEXT NULL,
             metric_recorded INTEGER NOT NULL DEFAULT 0 CHECK (metric_recorded IN (0, 1)),
             PRIMARY KEY (retrieval_id, access_id),
             CHECK ((was_used IS NULL AND outcome_workspace_id IS NULL AND outcome_ulid IS NULL)
                 OR (was_used IS NOT NULL AND outcome_workspace_id IS NOT NULL AND outcome_ulid IS NOT NULL))
         );
         CREATE INDEX IF NOT EXISTS memory_attribution_accesses_retrieval_idx
             ON memory_attribution_accesses(retrieval_id, access_id);",
    )
}

fn insert_retrieval(
    connection: &mut Connection,
    retrieval_id: &str,
    record: &RetrievalRecord,
    accesses: &[PendingMemoryAccess],
) -> Result<(), rusqlite::Error> {
    let transaction = connection.transaction()?;
    transaction.execute(
        "INSERT INTO memory_attribution_retrievals
             (retrieval_id, workspace_id, retrieval_event_workspace_id, retrieval_event_ulid,
              tool_call_event_workspace_id, tool_call_event_ulid)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            retrieval_id,
            record.retrieval_event.workspace_id,
            record.retrieval_event.workspace_id,
            record.retrieval_event.ulid,
            record.tool_call_event.workspace_id,
            record.tool_call_event.ulid,
        ],
    )?;
    for access in accesses {
        transaction.execute(
            "INSERT INTO memory_attribution_accesses
                 (retrieval_id, access_id, memory_workspace_id, memory_ulid, inclusion_reason)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                retrieval_id,
                access.access_id.as_str(),
                access.memory_id.workspace_id,
                access.memory_id.ulid,
                access.inclusion_reason,
            ],
        )?;
    }
    transaction.commit()
}

fn load_retrieval(
    connection: &Connection,
    retrieval_id: &str,
) -> Result<Option<StoredRetrieval>, rusqlite::Error> {
    connection
        .query_row(
            "SELECT workspace_id, retrieval_event_workspace_id, retrieval_event_ulid,
                    tool_call_event_workspace_id, tool_call_event_ulid
             FROM memory_attribution_retrievals WHERE retrieval_id = ?1",
            params![retrieval_id],
            |row| {
                Ok(StoredRetrieval {
                    workspace_id: row.get(0)?,
                    retrieval_event: EventId {
                        workspace_id: row.get(1)?,
                        ulid: row.get(2)?,
                    },
                    tool_call_event: EventId {
                        workspace_id: row.get(3)?,
                        ulid: row.get(4)?,
                    },
                })
            },
        )
        .optional()
}

fn ensure_same_retrieval(
    connection: &Connection,
    retrieval_id: &str,
    existing: &StoredRetrieval,
    record: &RetrievalRecord,
    requested: &[PendingMemoryAccess],
) -> Result<(), MemoryAttributionError> {
    if existing.retrieval_event != record.retrieval_event
        || existing.tool_call_event != record.tool_call_event
    {
        return Err(MemoryAttributionError::RetrievalConflict {
            retrieval_id: retrieval_id.to_string(),
        });
    }
    let requested = requested
        .iter()
        .map(|access| {
            (
                access.access_id.as_str().to_string(),
                (access.memory_id.clone(), access.inclusion_reason.clone()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let persisted = load_accesses(connection, retrieval_id)?
        .into_iter()
        .map(|access| {
            (
                access.access_id.as_str().to_string(),
                (access.memory_id, access.inclusion_reason),
            )
        })
        .collect::<BTreeMap<_, _>>();
    if requested != persisted {
        return Err(MemoryAttributionError::RetrievalConflict {
            retrieval_id: retrieval_id.to_string(),
        });
    }
    Ok(())
}

fn retrieval_graph_recorded(
    connection: &Connection,
    retrieval_id: &str,
) -> Result<bool, rusqlite::Error> {
    read_retrieval_flag(connection, retrieval_id, "graph_recorded")
}

fn retrieval_metric_recorded(
    connection: &Connection,
    retrieval_id: &str,
) -> Result<bool, rusqlite::Error> {
    read_retrieval_flag(connection, retrieval_id, "retrieval_metric_recorded")
}

fn read_retrieval_flag(
    connection: &Connection,
    retrieval_id: &str,
    column: &'static str,
) -> Result<bool, rusqlite::Error> {
    let query = match column {
        "graph_recorded" => "SELECT graph_recorded FROM memory_attribution_retrievals WHERE retrieval_id = ?1",
        "retrieval_metric_recorded" => "SELECT retrieval_metric_recorded FROM memory_attribution_retrievals WHERE retrieval_id = ?1",
        _ => unreachable!("only internal retrieval flag columns are supported"),
    };
    connection.query_row(query, params![retrieval_id], |row| {
        Ok(row.get::<_, i64>(0)? != 0)
    })
}

fn set_retrieval_graph_recorded(
    connection: &Connection,
    retrieval_id: &str,
) -> Result<(), rusqlite::Error> {
    connection.execute(
        "UPDATE memory_attribution_retrievals SET graph_recorded = 1 WHERE retrieval_id = ?1",
        params![retrieval_id],
    )?;
    Ok(())
}

fn set_retrieval_metric_recorded(
    connection: &Connection,
    retrieval_id: &str,
) -> Result<(), rusqlite::Error> {
    connection.execute(
        "UPDATE memory_attribution_retrievals
         SET retrieval_metric_recorded = 1 WHERE retrieval_id = ?1",
        params![retrieval_id],
    )?;
    Ok(())
}

fn load_accesses(
    connection: &Connection,
    retrieval_id: &str,
) -> Result<Vec<StoredAccess>, rusqlite::Error> {
    let mut statement = connection.prepare(
        "SELECT access_id, memory_workspace_id, memory_ulid, inclusion_reason, was_used,
                outcome_workspace_id, outcome_ulid, metric_recorded
         FROM memory_attribution_accesses
         WHERE retrieval_id = ?1 ORDER BY access_id ASC",
    )?;
    let rows = statement.query_map(params![retrieval_id], |row| {
        let was_used = row.get::<_, Option<i64>>(4)?;
        let outcome_workspace_id = row.get::<_, Option<String>>(5)?;
        let outcome_ulid = row.get::<_, Option<String>>(6)?;
        let resolution = match (was_used, outcome_workspace_id, outcome_ulid) {
            (Some(was_used), Some(workspace_id), Some(ulid)) => Some(StoredResolution {
                was_used: was_used != 0,
                terminal_outcome_event: EventId { workspace_id, ulid },
            }),
            (None, None, None) => None,
            _ => return Err(rusqlite::Error::InvalidQuery),
        };
        Ok(StoredAccess {
            access_id: MemoryAccessId(row.get(0)?),
            memory_id: MemoryId {
                workspace_id: row.get(1)?,
                ulid: row.get(2)?,
            },
            inclusion_reason: row.get(3)?,
            resolution,
            metric_recorded: row.get::<_, i64>(7)? != 0,
        })
    })?;
    rows.collect()
}

fn load_access(
    connection: &Connection,
    retrieval_id: &str,
    access_id: &str,
) -> Result<Option<StoredAccess>, rusqlite::Error> {
    Ok(load_accesses(connection, retrieval_id)?
        .into_iter()
        .find(|row| row.access_id.as_str() == access_id))
}

fn set_access_resolutions(
    connection: &mut Connection,
    retrieval_id: &str,
    resolutions: &[AccessResolution],
) -> Result<(), rusqlite::Error> {
    let transaction = connection.transaction()?;
    for resolution in resolutions {
        let updated = transaction.execute(
            "UPDATE memory_attribution_accesses
             SET was_used = ?3, outcome_workspace_id = ?4, outcome_ulid = ?5
             WHERE retrieval_id = ?1 AND access_id = ?2
               AND was_used IS NULL AND outcome_workspace_id IS NULL AND outcome_ulid IS NULL",
            params![
                retrieval_id,
                resolution.access_id.as_str(),
                i64::from(resolution.was_used),
                resolution.terminal_outcome_event.workspace_id,
                resolution.terminal_outcome_event.ulid,
            ],
        )?;
        if updated != 1 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
    }
    transaction.commit()
}

fn set_access_metric_recorded(
    connection: &Connection,
    retrieval_id: &str,
    access_id: &str,
) -> Result<(), rusqlite::Error> {
    connection.execute(
        "UPDATE memory_attribution_accesses SET metric_recorded = 1
         WHERE retrieval_id = ?1 AND access_id = ?2",
        params![retrieval_id, access_id],
    )?;
    Ok(())
}

fn pending_accesses(
    retrieval_id: &str,
    record: &RetrievalRecord,
) -> Result<Vec<PendingMemoryAccess>, MemoryAttributionError> {
    record
        .memories
        .iter()
        .map(|memory| {
            Ok(PendingMemoryAccess {
                access_id: MemoryAccessId(derive_access_id(
                    &record.retrieval_event,
                    &memory.memory_id,
                )?),
                retrieval_id: retrieval_id.to_string(),
                retrieval_event: record.retrieval_event.clone(),
                memory_id: memory.memory_id.clone(),
                accessor: record.accessor.clone(),
                inclusion_reason: memory.inclusion_reason.clone(),
            })
        })
        .collect()
}

fn check_workspace(
    field: &'static str,
    expected: &str,
    actual: &str,
) -> Result<(), MemoryAttributionError> {
    if expected != actual {
        return Err(MemoryAttributionError::WorkspaceMismatch {
            field,
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

fn ensure_unique_access_ids(access_ids: &[MemoryAccessId]) -> Result<(), MemoryAttributionError> {
    let mut seen = BTreeSet::new();
    for access_id in access_ids {
        if !seen.insert(access_id.as_str()) {
            return Err(MemoryAttributionError::DuplicateAccess {
                access_id: access_id.to_string(),
            });
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct AttributionIdInput<'a> {
    kind: &'a str,
    workspace_id: &'a str,
    retrieval_event_ulid: &'a str,
    memory_ulid: Option<&'a str>,
    terminal_event_ulid: Option<&'a str>,
}

fn derive_retrieval_id(event: &EventId) -> Result<String, serde_json::Error> {
    derive_attribution_id("retrieval", event, None, None)
}

fn derive_access_id(event: &EventId, memory: &MemoryId) -> Result<String, serde_json::Error> {
    derive_attribution_id("access", event, Some(memory.ulid.as_str()), None)
}

fn derive_metric_id(
    kind: &'static str,
    retrieval_id: &str,
    access_and_event: Option<(&MemoryAccessId, &EventId)>,
) -> Result<String, serde_json::Error> {
    let input = match access_and_event {
        Some((access_id, event)) => AttributionIdInput {
            kind,
            workspace_id: &event.workspace_id,
            retrieval_event_ulid: retrieval_id,
            memory_ulid: Some(access_id.as_str()),
            terminal_event_ulid: Some(&event.ulid),
        },
        None => AttributionIdInput {
            kind,
            workspace_id: "",
            retrieval_event_ulid: retrieval_id,
            memory_ulid: None,
            terminal_event_ulid: None,
        },
    };
    let bytes = canonical_json_bytes(&input)?;
    Ok(format!(
        "memory_metric:{}",
        hash_canonical_payload_bytes(&bytes).to_hex()
    ))
}

fn derive_attribution_id(
    kind: &'static str,
    event: &EventId,
    memory_ulid: Option<&str>,
    terminal_event_ulid: Option<&str>,
) -> Result<String, serde_json::Error> {
    let bytes = canonical_json_bytes(&AttributionIdInput {
        kind,
        workspace_id: &event.workspace_id,
        retrieval_event_ulid: &event.ulid,
        memory_ulid,
        terminal_event_ulid,
    })?;
    Ok(format!(
        "memory_{kind}:{}",
        hash_canonical_payload_bytes(&bytes).to_hex()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    #[derive(Default)]
    struct FakeEvents {
        retrievals: Mutex<Vec<(EventId, EventId)>>,
        terminals: Mutex<Vec<(EventId, EventId)>>,
    }

    impl FakeEvents {
        fn allow_retrieval(&self, retrieval: EventId, call: EventId) {
            self.retrievals.lock().unwrap().push((retrieval, call));
        }

        fn allow_terminal(&self, retrieval: EventId, terminal: EventId) {
            self.terminals.lock().unwrap().push((retrieval, terminal));
        }
    }

    impl MemoryAttributionEvents for FakeEvents {
        fn validate_retrieval(&self, retrieval: &EventId, call: &EventId) -> Result<(), String> {
            self.retrievals
                .lock()
                .unwrap()
                .contains(&(retrieval.clone(), call.clone()))
                .then_some(())
                .ok_or_else(|| "retrieval event was not persisted for this tool call".to_string())
        }

        fn validate_terminal_outcome(
            &self,
            retrieval: &EventId,
            terminal: &EventId,
        ) -> Result<(), String> {
            self.terminals
                .lock()
                .unwrap()
                .contains(&(retrieval.clone(), terminal.clone()))
                .then_some(())
                .ok_or_else(|| "terminal event is absent or not downstream".to_string())
        }
    }

    #[derive(Default)]
    struct FakeGraph {
        pending: Mutex<Vec<Vec<PendingMemoryAccess>>>,
        resolutions: Mutex<Vec<Vec<AccessResolution>>>,
    }

    impl MemoryAttributionGraph for FakeGraph {
        fn record_pending_accesses(&self, accesses: &[PendingMemoryAccess]) -> Result<(), String> {
            self.pending.lock().unwrap().push(accesses.to_vec());
            Ok(())
        }

        fn resolve_accesses(&self, resolutions: &[AccessResolution]) -> Result<(), String> {
            self.resolutions.lock().unwrap().push(resolutions.to_vec());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeMetrics {
        retrievals: Mutex<Vec<(String, String, u64)>>,
        uses: Mutex<Vec<(String, String, u64)>>,
    }

    impl MemoryAttributionMetrics for FakeMetrics {
        fn record_retrieval(
            &self,
            metric_id: &str,
            retrieval_id: &str,
            retrieved_count: u64,
        ) -> Result<(), String> {
            self.retrievals.lock().unwrap().push((
                metric_id.to_string(),
                retrieval_id.to_string(),
                retrieved_count,
            ));
            Ok(())
        }

        fn record_use(
            &self,
            metric_id: &str,
            retrieval_id: &str,
            used_count: u64,
        ) -> Result<(), String> {
            self.uses.lock().unwrap().push((
                metric_id.to_string(),
                retrieval_id.to_string(),
                used_count,
            ));
            Ok(())
        }
    }

    fn event(id: &str) -> EventId {
        EventId {
            workspace_id: "workspace-a".to_string(),
            ulid: id.to_string(),
        }
    }

    fn memory(id: &str) -> MemoryId {
        MemoryId {
            workspace_id: "workspace-a".to_string(),
            ulid: id.to_string(),
        }
    }

    fn temporary_index_path() -> std::path::PathBuf {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "lattice-memory-attribution-{}-{}-{id}.sqlite",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ))
    }

    fn retrieval(
        retrieval_event: EventId,
        tool_call_event: EventId,
        memories: Vec<RetrievedMemory>,
    ) -> RetrievalRecord {
        RetrievalRecord {
            retrieval_event,
            tool_call_event,
            accessor: Actor::Daemon,
            memories,
        }
    }

    #[test]
    fn retrieval_is_graph_first_and_replay_is_idempotent() {
        let events = FakeEvents::default();
        let graph = FakeGraph::default();
        let metrics = FakeMetrics::default();
        let retrieval_event = event("retrieval-1");
        let call_event = event("call-1");
        events.allow_retrieval(retrieval_event.clone(), call_event.clone());
        let path = temporary_index_path();
        let bridge =
            MemoryAttributionBridge::open(&path, "workspace-a", &events, &graph, &metrics).unwrap();
        let request = retrieval(
            retrieval_event.clone(),
            call_event,
            vec![RetrievedMemory {
                memory_id: memory("memory-1"),
                inclusion_reason: "direct scope match".to_string(),
            }],
        );

        let first = bridge.record_retrieval(request.clone()).unwrap();
        let second = bridge.record_retrieval(request).unwrap();

        assert_eq!(first, second);
        assert_eq!(graph.pending.lock().unwrap().len(), 1);
        assert_eq!(metrics.retrievals.lock().unwrap().len(), 1);
        assert_eq!(metrics.retrievals.lock().unwrap()[0].2, 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn explicit_exact_outcome_resolves_only_cited_access_and_is_idempotent() {
        let events = FakeEvents::default();
        let graph = FakeGraph::default();
        let metrics = FakeMetrics::default();
        let retrieval_event = event("retrieval-2");
        let call_event = event("call-2");
        let terminal = event("outcome-2");
        events.allow_retrieval(retrieval_event.clone(), call_event.clone());
        events.allow_terminal(retrieval_event.clone(), terminal.clone());
        let path = temporary_index_path();
        let bridge =
            MemoryAttributionBridge::open(&path, "workspace-a", &events, &graph, &metrics).unwrap();
        let pending = bridge
            .record_retrieval(retrieval(
                retrieval_event,
                call_event,
                vec![
                    RetrievedMemory {
                        memory_id: memory("memory-a"),
                        inclusion_reason: "reason a".to_string(),
                    },
                    RetrievedMemory {
                        memory_id: memory("memory-b"),
                        inclusion_reason: "reason b".to_string(),
                    },
                ],
            ))
            .unwrap();

        let first = bridge
            .resolve_accesses(
                &pending.retrieval_id,
                terminal.clone(),
                AccessDisposition::Used,
                &pending.access_ids[..1],
            )
            .unwrap();
        let replay = bridge
            .resolve_accesses(
                &pending.retrieval_id,
                terminal,
                AccessDisposition::Used,
                &pending.access_ids[..1],
            )
            .unwrap();

        assert_eq!(first.newly_resolved_count, 1);
        assert_eq!(replay.newly_resolved_count, 0);
        assert_eq!(replay.idempotent_count, 1);
        assert_eq!(graph.resolutions.lock().unwrap().len(), 1);
        assert_eq!(metrics.uses.lock().unwrap().len(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn empty_retrieval_records_a_miss_without_graph_accesses() {
        let events = FakeEvents::default();
        let graph = FakeGraph::default();
        let metrics = FakeMetrics::default();
        let retrieval_event = event("retrieval-empty");
        let call_event = event("call-empty");
        events.allow_retrieval(retrieval_event.clone(), call_event.clone());
        let path = temporary_index_path();
        let bridge =
            MemoryAttributionBridge::open(&path, "workspace-a", &events, &graph, &metrics).unwrap();

        let pending = bridge
            .record_retrieval(retrieval(retrieval_event, call_event, Vec::new()))
            .unwrap();

        assert!(pending.access_ids.is_empty());
        assert!(graph.pending.lock().unwrap().is_empty());
        assert_eq!(metrics.retrievals.lock().unwrap()[0].2, 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn foreign_and_uncited_accesses_are_rejected_before_resolution() {
        let events = FakeEvents::default();
        let graph = FakeGraph::default();
        let metrics = FakeMetrics::default();
        let retrieval_event = event("retrieval-foreign");
        let call_event = event("call-foreign");
        let terminal = event("outcome-foreign");
        events.allow_retrieval(retrieval_event.clone(), call_event.clone());
        events.allow_terminal(retrieval_event.clone(), terminal.clone());
        let path = temporary_index_path();
        let bridge =
            MemoryAttributionBridge::open(&path, "workspace-a", &events, &graph, &metrics).unwrap();
        let pending = bridge
            .record_retrieval(retrieval(
                retrieval_event,
                call_event,
                vec![RetrievedMemory {
                    memory_id: memory("memory-c"),
                    inclusion_reason: "exact evidence".to_string(),
                }],
            ))
            .unwrap();

        let error = bridge
            .resolve_accesses(
                &pending.retrieval_id,
                terminal,
                AccessDisposition::Used,
                &[MemoryAccessId("other-retrieval-access".to_string())],
            )
            .unwrap_err();
        assert!(matches!(
            error,
            MemoryAttributionError::AccessNotInRetrieval { .. }
        ));
        assert!(graph.resolutions.lock().unwrap().is_empty());
        assert!(metrics.uses.lock().unwrap().is_empty());
        let _ = std::fs::remove_file(path);
    }
}
