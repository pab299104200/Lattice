//! Transactional attribution journal for memory retrieval and feedback.
//!
//! The journal persists validated event facts independently of the event log so
//! compaction cannot make an unresolved retrieval impossible to attribute.

use super::MemoryStore;
use crate::error::LatticeError;
use crate::identity::EventId;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

pub const MAX_ATTRIBUTION_ACCESSES: usize = 256;
pub const MAX_ATTRIBUTION_STRING_BYTES: usize = 4096;
pub const MAX_ATTRIBUTION_PRUNE_BATCH: usize = 1024;
pub const MAX_ATTRIBUTION_RETAINED: usize = 100_000;
pub const MAX_ATTRIBUTION_METRIC_BATCH: usize = 256;
const ATTRIBUTION_PRUNE_VM_INSTRUCTIONS: u64 = 2_000_000;
const PRUNE_PROGRESS_GRANULARITY: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionEventKind {
    ToolCalled,
    MemoryRetrieved,
    WorkflowSucceeded,
    WorkflowFailed,
}

impl AttributionEventKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::ToolCalled => "tool_called",
            Self::MemoryRetrieved => "memory_retrieved",
            Self::WorkflowSucceeded => "workflow_succeeded",
            Self::WorkflowFailed => "workflow_failed",
        }
    }

    fn parse(value: &str) -> Result<Self, LatticeError> {
        match value {
            "tool_called" => Ok(Self::ToolCalled),
            "memory_retrieved" => Ok(Self::MemoryRetrieved),
            "workflow_succeeded" => Ok(Self::WorkflowSucceeded),
            "workflow_failed" => Ok(Self::WorkflowFailed),
            _ => Err(storage_error(format!(
                "unknown attribution event kind `{value}`"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributionEventFact {
    pub event_id: EventId,
    pub kind: AttributionEventKind,
    pub checkout_id: String,
    pub session_id: String,
    pub branch: Option<String>,
    pub sequence: u64,
    pub observed_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributionAccessInput {
    pub access_id: String,
    pub local_memory_id: String,
    pub inclusion_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributionRetrievalInput {
    pub retrieval_id: String,
    pub repository_id: String,
    pub checkout_id: String,
    pub session_id: String,
    pub branch: Option<String>,
    pub tool_event: AttributionEventFact,
    pub retrieval_event: AttributionEventFact,
    pub accessor: String,
    pub metric_client: String,
    pub metric_channel: String,
    pub accesses: Vec<AttributionAccessInput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributionRecordOutcome {
    pub access_ids: Vec<String>,
    pub metric_pending: bool,
    pub replayed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionDisposition {
    Applied,
    Rejected,
    Ignored,
}

impl AttributionDisposition {
    fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Rejected => "rejected",
            Self::Ignored => "ignored",
        }
    }

    fn parse(value: &str) -> Result<Self, LatticeError> {
        match value {
            "applied" => Ok(Self::Applied),
            "rejected" => Ok(Self::Rejected),
            "ignored" => Ok(Self::Ignored),
            _ => Err(storage_error(format!(
                "unknown attribution disposition `{value}`"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributionResolutionStatus {
    NewlyResolved,
    Idempotent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributionResolutionOutcome {
    pub status: AttributionResolutionStatus,
    pub retrieval_metric_pending: bool,
    pub access_metric_pending_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAttributionRetrieval {
    pub input: AttributionRetrievalInput,
    pub terminal_event: Option<AttributionEventFact>,
    pub disposition: Option<AttributionDisposition>,
    pub cited_access_ids: Vec<String>,
    pub retrieval_metric_pending: bool,
    pub access_metric_pending_ids: Vec<String>,
    pub resolved_accesses: Vec<ResolvedAttributionAccess>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAttributionAccess {
    pub access_id: String,
    pub local_memory_id: String,
    pub was_used: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAttributionMetrics {
    pub items: Vec<PendingAttributionMetric>,
    pub next_cursor: Option<PendingAttributionCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendingAttributionCursor {
    Retrieval {
        retrieval_id: String,
    },
    Access {
        retrieval_id: String,
        access_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingAttributionMetric {
    Retrieval {
        retrieval_id: String,
        repository_id: String,
        checkout_id: String,
        session_id: String,
        branch: Option<String>,
        accessor: String,
        metric_client: String,
        metric_channel: String,
        retrieved_count: usize,
        retrieval_event_id: EventId,
    },
    Access {
        retrieval_id: String,
        access_id: String,
        local_memory_id: String,
        was_used: bool,
        disposition: AttributionDisposition,
        terminal_event_id: EventId,
        metric_client: String,
        metric_channel: String,
        session_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributionPrunePolicy {
    pub max_resolved_age_secs: u64,
    pub max_pending_age_secs: u64,
    pub max_metric_pending_age_secs: u64,
    pub max_resolved_retrievals: usize,
    pub batch_limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributionPruneOutcome {
    pub retrievals_pruned: usize,
    pub expired_receipts_pruned: usize,
    pub metric_dead_letters: usize,
}

impl MemoryStore {
    pub fn record_attribution_retrieval(
        &self,
        input: &AttributionRetrievalInput,
    ) -> Result<AttributionRecordOutcome, LatticeError> {
        self.with_connection(|conn| record_retrieval(conn, input))
    }

    pub fn resolve_attribution(
        &self,
        retrieval_id: &str,
        terminal_event: &AttributionEventFact,
        disposition: AttributionDisposition,
        cited_access_ids: &[String],
    ) -> Result<AttributionResolutionOutcome, LatticeError> {
        self.with_connection(|conn| {
            resolve_retrieval(
                conn,
                retrieval_id,
                terminal_event,
                disposition,
                cited_access_ids,
            )
        })
    }

    pub fn load_attribution_retrieval(
        &self,
        retrieval_id: &str,
    ) -> Result<Option<StoredAttributionRetrieval>, LatticeError> {
        self.with_connection(|conn| load_retrieval(conn, retrieval_id))
    }

    pub fn mark_attribution_retrieval_metric_recorded(
        &self,
        retrieval_id: &str,
    ) -> Result<(), LatticeError> {
        self.with_connection(|conn| mark_retrieval_metric(conn, retrieval_id))
    }

    pub fn pending_attribution_metrics(
        &self,
        after: Option<&PendingAttributionCursor>,
        limit: usize,
    ) -> Result<PendingAttributionMetrics, LatticeError> {
        self.with_connection(|conn| pending_metrics(conn, after, limit))
    }

    pub fn mark_attribution_access_metrics_recorded(
        &self,
        access_ids: &[String],
    ) -> Result<(), LatticeError> {
        self.with_connection(|conn| mark_access_metrics(conn, access_ids))
    }

    pub fn prune_attribution_journal(
        &self,
        now: u64,
        policy: AttributionPrunePolicy,
    ) -> Result<AttributionPruneOutcome, LatticeError> {
        self.with_connection(|conn| prune(conn, now, policy))
    }
}

fn record_retrieval(
    conn: &Connection,
    input: &AttributionRetrievalInput,
) -> Result<AttributionRecordOutcome, LatticeError> {
    validate_input(input)?;
    let payload_hash = payload_hash(input)?;
    let tx = conn
        .unchecked_transaction()
        .map_err(sql_error("begin attribution retrieval"))?;
    if let Some(expired_hash) = tx
        .query_row(
            "SELECT payload_hash FROM memory_attribution_expired WHERE retrieval_id=?1",
            [&input.retrieval_id],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(sql_error("check expired attribution retrieval"))?
    {
        return Err(storage_error(if expired_hash == payload_hash {
            format!(
                "attribution retrieval `{}` expired and cannot be replayed",
                input.retrieval_id
            )
        } else {
            format!(
                "attribution retrieval `{}` was reused after expiry with changed facts",
                input.retrieval_id
            )
        }));
    }
    if let Some(existing_hash) = tx
        .query_row(
            "SELECT payload_hash FROM memory_attribution_retrievals WHERE retrieval_id=?1",
            [&input.retrieval_id],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(sql_error("load attribution replay"))?
    {
        if existing_hash != payload_hash {
            return Err(storage_error(format!(
                "attribution retrieval `{}` was retried with changed facts",
                input.retrieval_id
            )));
        }
        let outcome = replay_outcome(&tx, &input.retrieval_id)?;
        tx.commit()
            .map_err(sql_error("commit attribution replay"))?;
        return Ok(outcome);
    }
    validate_access_memories(&tx, input)?;
    tx.execute(
        "INSERT INTO memory_attribution_retrievals(
           retrieval_id,repository_id,checkout_id,session_id,branch,
           tool_event_id,tool_event_kind,tool_event_sequence,tool_event_observed_at,retrieval_event_id,
           retrieval_event_kind,retrieval_event_sequence,retrieval_event_observed_at,
           accessor,retrieved_count,metric_client,metric_channel,payload_hash,created_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?13)",
        params![
            input.retrieval_id,
            input.repository_id,
            input.checkout_id,
            input.session_id,
            input.branch,
            input.tool_event.event_id.to_string(),
            input.tool_event.kind.as_str(),
            to_i64(input.tool_event.sequence, "tool event sequence")?,
            to_i64(input.tool_event.observed_at, "tool event timestamp")?,
            input.retrieval_event.event_id.to_string(),
            input.retrieval_event.kind.as_str(),
            to_i64(input.retrieval_event.sequence, "retrieval event sequence")?,
            to_i64(
                input.retrieval_event.observed_at,
                "retrieval event timestamp"
            )?,
            input.accessor, input.accesses.len() as i64, input.metric_client, input.metric_channel, payload_hash
        ],
    )
    .map_err(sql_error("insert attribution retrieval"))?;
    for access in &input.accesses {
        tx.execute(
            "INSERT INTO memory_accesses(access_id,memory_id,accessed_at,inclusion_reason,was_used)
             VALUES(?1,?2,?3,?4,NULL)",
            params![
                access.access_id,
                access.local_memory_id,
                to_i64(input.retrieval_event.observed_at, "access timestamp")?,
                access.inclusion_reason
            ],
        )
        .map_err(sql_error("insert attributed memory access"))?;
        tx.execute(
            "INSERT INTO memory_attribution_accesses(access_id,retrieval_id,memory_id,inclusion_reason)
             VALUES(?1,?2,?3,?4)",
            params![access.access_id,input.retrieval_id,access.local_memory_id,access.inclusion_reason],
        ).map_err(sql_error("insert attribution access metadata"))?;
    }
    tx.commit()
        .map_err(sql_error("commit attribution retrieval"))?;
    Ok(AttributionRecordOutcome {
        access_ids: input
            .accesses
            .iter()
            .map(|access| access.access_id.clone())
            .collect(),
        metric_pending: true,
        replayed: false,
    })
}

fn resolve_retrieval(
    conn: &Connection,
    retrieval_id: &str,
    terminal: &AttributionEventFact,
    disposition: AttributionDisposition,
    cited: &[String],
) -> Result<AttributionResolutionOutcome, LatticeError> {
    validate_bounded("retrieval_id", retrieval_id)?;
    validate_terminal_kind(terminal)?;
    if cited.len() > MAX_ATTRIBUTION_ACCESSES {
        return Err(storage_error("attribution cited-access count exceeds 256"));
    }
    let cited = cited.iter().cloned().collect::<BTreeSet<_>>();
    if cited.len() > MAX_ATTRIBUTION_ACCESSES {
        return Err(storage_error(
            "attribution cited-access ids contain too many distinct values",
        ));
    }
    let cited_json = serde_json::to_string(&cited).map_err(|error| {
        storage_error(format!("failed to encode attribution citations: {error}"))
    })?;
    let tx = conn
        .unchecked_transaction()
        .map_err(sql_error("begin attribution resolution"))?;
    let stored = load_retrieval_tx(&tx, retrieval_id)?.ok_or_else(|| {
        storage_error(format!(
            "attribution retrieval `{retrieval_id}` was not found or has expired"
        ))
    })?;
    validate_terminal(&stored.input, terminal)?;
    let accessed = stored
        .input
        .accesses
        .iter()
        .map(|access| access.access_id.as_str())
        .collect::<BTreeSet<_>>();
    if let Some(invalid) = cited.iter().find(|id| !accessed.contains(id.as_str())) {
        return Err(storage_error(format!(
            "cited access `{invalid}` was not part of retrieval `{retrieval_id}`"
        )));
    }
    if let Some(existing) = stored.disposition {
        if existing != disposition
            || stored
                .cited_access_ids
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                != cited
        {
            return Err(storage_error(format!("attribution retrieval `{retrieval_id}` was resolved with conflicting terminal facts")));
        }
        tx.commit()
            .map_err(sql_error("commit attribution resolution replay"))?;
        return Ok(resolution_outcome(
            stored,
            AttributionResolutionStatus::Idempotent,
        ));
    }
    let changed = tx
        .execute(
            "UPDATE memory_attribution_retrievals SET terminal_event_id=?2,terminal_event_kind=?3,
         terminal_event_sequence=?4,terminal_event_observed_at=?5,disposition=?6,
         cited_access_ids_json=?7,resolved_at=?5 WHERE retrieval_id=?1 AND disposition IS NULL",
            params![
                retrieval_id,
                terminal.event_id.to_string(),
                terminal.kind.as_str(),
                to_i64(terminal.sequence, "terminal event sequence")?,
                to_i64(terminal.observed_at, "terminal event timestamp")?,
                disposition.as_str(),
                cited_json
            ],
        )
        .map_err(sql_error("resolve attribution retrieval"))?;
    if changed != 1 {
        return Err(storage_error(format!(
            "attribution retrieval `{retrieval_id}` resolution lost its compare-and-set race"
        )));
    }
    for access in &stored.input.accesses {
        let was_used =
            disposition == AttributionDisposition::Applied && cited.contains(&access.access_id);
        let changed = tx
            .execute(
                "UPDATE memory_accesses SET was_used=?2 WHERE access_id=?1 AND was_used IS NULL",
                params![access.access_id, i64::from(was_used)],
            )
            .map_err(sql_error("resolve attributed memory access"))?;
        if changed != 1 {
            return Err(storage_error(format!(
                "attribution access `{}` was concurrently resolved or removed",
                access.access_id
            )));
        }
    }
    let resolved = load_retrieval_tx(&tx, retrieval_id)?
        .ok_or_else(|| storage_error("resolved attribution retrieval disappeared"))?;
    tx.commit()
        .map_err(sql_error("commit attribution resolution"))?;
    Ok(resolution_outcome(
        resolved,
        AttributionResolutionStatus::NewlyResolved,
    ))
}

fn validate_input(input: &AttributionRetrievalInput) -> Result<(), LatticeError> {
    for (name, value) in [
        ("retrieval_id", input.retrieval_id.as_str()),
        ("repository_id", input.repository_id.as_str()),
        ("checkout_id", input.checkout_id.as_str()),
        ("session_id", input.session_id.as_str()),
        ("accessor", input.accessor.as_str()),
        ("metric_client", input.metric_client.as_str()),
        ("metric_channel", input.metric_channel.as_str()),
    ] {
        validate_bounded(name, value)?;
    }
    if let Some(branch) = &input.branch {
        validate_bounded("branch", branch)?;
    }
    validate_event(&input.tool_event, AttributionEventKind::ToolCalled)?;
    validate_event(
        &input.retrieval_event,
        AttributionEventKind::MemoryRetrieved,
    )?;
    if input.tool_event.event_id.workspace_id != input.repository_id
        || input.tool_event.checkout_id != input.checkout_id
        || input.tool_event.session_id != input.session_id
        || input.tool_event.branch != input.branch
        || input.retrieval_event.event_id.workspace_id != input.repository_id
        || input.retrieval_event.checkout_id != input.checkout_id
        || input.retrieval_event.session_id != input.session_id
        || input.retrieval_event.branch != input.branch
    {
        return Err(storage_error(
            "attribution retrieval event authority does not match repository/session/branch input",
        ));
    }
    if input.retrieval_event.sequence <= input.tool_event.sequence
        || input.retrieval_event.observed_at < input.tool_event.observed_at
    {
        return Err(storage_error(
            "memory_retrieved event must follow tool_called event order",
        ));
    }
    if input.accesses.is_empty() || input.accesses.len() > MAX_ATTRIBUTION_ACCESSES {
        return Err(storage_error(
            "attribution retrieval must contain between 1 and 256 accesses",
        ));
    }
    let mut access_ids = BTreeSet::new();
    let mut memory_ids = BTreeSet::new();
    for access in &input.accesses {
        validate_bounded("access_id", &access.access_id)?;
        validate_bounded("local_memory_id", &access.local_memory_id)?;
        validate_bounded("inclusion_reason", &access.inclusion_reason)?;
        if !access_ids.insert(&access.access_id) {
            return Err(storage_error(
                "attribution retrieval contains duplicate access ids",
            ));
        }
        memory_ids.insert(&access.local_memory_id);
    }
    if memory_ids.len() > MAX_ATTRIBUTION_ACCESSES {
        return Err(storage_error(
            "attribution retrieval contains too many memory ids",
        ));
    }
    Ok(())
}

fn validate_event(
    event: &AttributionEventFact,
    expected: AttributionEventKind,
) -> Result<(), LatticeError> {
    validate_bounded("event workspace", &event.event_id.workspace_id)?;
    validate_bounded("event id", &event.event_id.ulid)?;
    validate_bounded("event checkout", &event.checkout_id)?;
    validate_bounded("event session", &event.session_id)?;
    if let Some(branch) = &event.branch {
        validate_bounded("event branch", branch)?;
    }
    if event.kind != expected {
        return Err(storage_error(format!(
            "attribution event kind {:?} is invalid for this operation",
            event.kind
        )));
    }
    Ok(())
}

fn validate_terminal_kind(event: &AttributionEventFact) -> Result<(), LatticeError> {
    validate_bounded("event workspace", &event.event_id.workspace_id)?;
    validate_bounded("event id", &event.event_id.ulid)?;
    validate_bounded("event checkout", &event.checkout_id)?;
    validate_bounded("event session", &event.session_id)?;
    if let Some(branch) = &event.branch {
        validate_bounded("event branch", branch)?;
    }
    if !matches!(
        event.kind,
        AttributionEventKind::WorkflowSucceeded | AttributionEventKind::WorkflowFailed
    ) {
        return Err(storage_error(
            "terminal attribution event must be workflow_succeeded or workflow_failed",
        ));
    }
    Ok(())
}

fn validate_terminal(
    input: &AttributionRetrievalInput,
    terminal: &AttributionEventFact,
) -> Result<(), LatticeError> {
    if terminal.event_id.workspace_id != input.repository_id
        || terminal.checkout_id != input.checkout_id
        || terminal.branch != input.branch
    {
        return Err(storage_error(
            "terminal attribution event repository/checkout/branch authority does not match retrieval",
        ));
    }
    if terminal.sequence <= input.retrieval_event.sequence
        || terminal.observed_at < input.retrieval_event.observed_at
    {
        return Err(storage_error(
            "terminal attribution event must follow retrieval event order",
        ));
    }
    Ok(())
}

fn validate_access_memories(
    tx: &Transaction<'_>,
    input: &AttributionRetrievalInput,
) -> Result<(), LatticeError> {
    for access in &input.accesses {
        let valid: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM memories WHERE id=?1 AND is_invalidated=0 AND (applicable_checkout_id IS NULL OR applicable_checkout_id=?2) AND workspace_id=?4 AND ((scope='session' AND session_id=?3) OR (scope='branch' AND branch=?5) OR scope='repo'))",params![access.local_memory_id,input.checkout_id,input.session_id,input.repository_id,input.branch],|row|row.get(0)).map_err(sql_error("validate attributed memory authority"))?;
        if !valid {
            return Err(storage_error(format!("memory `{}` does not exist in the retrieval repository/checkout/session/branch authority",access.local_memory_id)));
        }
    }
    Ok(())
}

fn replay_outcome(
    tx: &Transaction<'_>,
    retrieval_id: &str,
) -> Result<AttributionRecordOutcome, LatticeError> {
    let mut statement=tx.prepare("SELECT access_id FROM memory_attribution_accesses WHERE retrieval_id=?1 ORDER BY access_id").map_err(sql_error("prepare attribution replay accesses"))?;
    let access_ids = statement
        .query_map([retrieval_id], |row| row.get(0))
        .map_err(sql_error("query attribution replay accesses"))?
        .collect::<Result<Vec<String>, _>>()
        .map_err(sql_error("read attribution replay access"))?;
    let pending:bool=tx.query_row("SELECT retrieval_metric_recorded=0 FROM memory_attribution_retrievals WHERE retrieval_id=?1",[retrieval_id],|row|row.get(0)).map_err(sql_error("read attribution replay metric state"))?;
    Ok(AttributionRecordOutcome {
        access_ids,
        metric_pending: pending,
        replayed: true,
    })
}

fn load_retrieval(
    conn: &Connection,
    retrieval_id: &str,
) -> Result<Option<StoredAttributionRetrieval>, LatticeError> {
    validate_bounded("retrieval_id", retrieval_id)?;
    load_retrieval_conn(conn, retrieval_id)
}
fn load_retrieval_tx(
    tx: &Transaction<'_>,
    retrieval_id: &str,
) -> Result<Option<StoredAttributionRetrieval>, LatticeError> {
    load_retrieval_conn(tx, retrieval_id)
}

fn load_retrieval_conn(
    conn: &Connection,
    retrieval_id: &str,
) -> Result<Option<StoredAttributionRetrieval>, LatticeError> {
    let row=conn.query_row("SELECT repository_id,checkout_id,session_id,branch,tool_event_id,tool_event_kind,tool_event_sequence,tool_event_observed_at,retrieval_event_id,retrieval_event_kind,retrieval_event_sequence,retrieval_event_observed_at,accessor,metric_client,metric_channel,terminal_event_id,terminal_event_kind,terminal_event_sequence,terminal_event_observed_at,disposition,cited_access_ids_json,retrieval_metric_recorded FROM memory_attribution_retrievals WHERE retrieval_id=?1",[retrieval_id],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,String>(4)?,row.get::<_,String>(5)?,row.get::<_,i64>(6)?,row.get::<_,i64>(7)?,row.get::<_,String>(8)?,row.get::<_,String>(9)?,row.get::<_,i64>(10)?,row.get::<_,i64>(11)?,row.get::<_,String>(12)?,row.get::<_,String>(13)?,row.get::<_,String>(14)?,row.get::<_,Option<String>>(15)?,row.get::<_,Option<String>>(16)?,row.get::<_,Option<i64>>(17)?,row.get::<_,Option<i64>>(18)?,row.get::<_,Option<String>>(19)?,row.get::<_,Option<String>>(20)?,row.get::<_,i64>(21)?))).optional().map_err(sql_error("load attribution retrieval"))?;
    let Some((
        repository_id,
        checkout_id,
        session_id,
        branch,
        tool_id,
        tool_kind,
        tool_sequence,
        tool_time,
        event_id,
        event_kind,
        event_sequence,
        event_time,
        accessor,
        metric_client,
        metric_channel,
        terminal_id,
        terminal_kind,
        terminal_sequence,
        terminal_time,
        disposition,
        cited_json,
        metric_recorded,
    )) = row
    else {
        return Ok(None);
    };
    let mut statement=conn.prepare("SELECT a.access_id,a.memory_id,a.inclusion_reason,a.metric_recorded,m.was_used FROM memory_attribution_accesses a JOIN memory_accesses m ON m.access_id=a.access_id WHERE a.retrieval_id=?1 ORDER BY a.access_id").map_err(sql_error("prepare attribution accesses"))?;
    let rows = statement
        .query_map([retrieval_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })
        .map_err(sql_error("query attribution accesses"))?;
    let mut accesses = Vec::new();
    let mut pending = Vec::new();
    let mut resolved_accesses = Vec::new();
    for row in rows {
        let (access_id, memory_id, reason, recorded, was_used) =
            row.map_err(sql_error("read attribution access"))?;
        if recorded == 0 {
            pending.push(access_id.clone())
        };
        accesses.push(AttributionAccessInput {
            access_id: access_id.clone(),
            local_memory_id: memory_id.clone(),
            inclusion_reason: reason,
        });
        resolved_accesses.push(ResolvedAttributionAccess {
            access_id,
            local_memory_id: memory_id,
            was_used: was_used.map(|value| value != 0),
        });
    }
    let terminal_event = match (terminal_id, terminal_kind, terminal_sequence, terminal_time) {
        (Some(id), Some(kind), Some(sequence), Some(time)) => Some(AttributionEventFact {
            event_id: parse_event_id(&id)?,
            kind: AttributionEventKind::parse(&kind)?,
            checkout_id: checkout_id.clone(),
            session_id: session_id.clone(),
            branch: branch.clone(),
            sequence: sequence.max(0) as u64,
            observed_at: time.max(0) as u64,
        }),
        _ => None,
    };
    let cited_access_ids = match cited_json {
        Some(value) => serde_json::from_str::<BTreeSet<String>>(&value)
            .map_err(|error| {
                storage_error(format!("failed to decode attribution citations: {error}"))
            })?
            .into_iter()
            .collect(),
        None => Vec::new(),
    };
    Ok(Some(StoredAttributionRetrieval {
        input: AttributionRetrievalInput {
            retrieval_id: retrieval_id.into(),
            repository_id: repository_id.clone(),
            checkout_id: checkout_id.clone(),
            session_id: session_id.clone(),
            branch: branch.clone(),
            tool_event: AttributionEventFact {
                event_id: parse_event_id(&tool_id)?,
                kind: AttributionEventKind::parse(&tool_kind)?,
                checkout_id: checkout_id.clone(),
                session_id: session_id.clone(),
                branch: branch.clone(),
                sequence: tool_sequence.max(0) as u64,
                observed_at: tool_time.max(0) as u64,
            },
            retrieval_event: AttributionEventFact {
                event_id: parse_event_id(&event_id)?,
                kind: AttributionEventKind::parse(&event_kind)?,
                checkout_id,
                session_id,
                branch,
                sequence: event_sequence.max(0) as u64,
                observed_at: event_time.max(0) as u64,
            },
            accessor,
            metric_client,
            metric_channel,
            accesses,
        },
        terminal_event,
        disposition: disposition
            .map(|value| AttributionDisposition::parse(&value))
            .transpose()?,
        cited_access_ids,
        retrieval_metric_pending: metric_recorded == 0,
        access_metric_pending_ids: pending,
        resolved_accesses,
    }))
}

fn mark_retrieval_metric(conn: &Connection, id: &str) -> Result<(), LatticeError> {
    let changed=conn.execute("UPDATE memory_attribution_retrievals SET retrieval_metric_recorded=1 WHERE retrieval_id=?1",[id]).map_err(sql_error("mark attribution retrieval metric"))?;
    if changed != 1 {
        return Err(storage_error(format!(
            "attribution retrieval `{id}` was not found"
        )));
    }
    Ok(())
}

fn pending_metrics(
    conn: &Connection,
    after: Option<&PendingAttributionCursor>,
    limit: usize,
) -> Result<PendingAttributionMetrics, LatticeError> {
    if limit == 0 || limit > MAX_ATTRIBUTION_METRIC_BATCH {
        return Err(storage_error(
            "pending attribution metric limit must be between 1 and 256",
        ));
    }
    let (retrieval_after, access_retrieval_after, access_after, include_retrievals) = match after {
        None => ("", "", "", true),
        Some(PendingAttributionCursor::Retrieval { retrieval_id }) => {
            validate_bounded("attribution metric retrieval cursor", retrieval_id)?;
            (retrieval_id.as_str(), "", "", true)
        }
        Some(PendingAttributionCursor::Access {
            retrieval_id,
            access_id,
        }) => {
            validate_bounded("attribution metric retrieval cursor", retrieval_id)?;
            validate_bounded("attribution metric access cursor", access_id)?;
            ("", retrieval_id.as_str(), access_id.as_str(), false)
        }
    };
    let sql = "SELECT kind,sort_key,retrieval_id,repository_id,checkout_id,session_id,branch,accessor,
              metric_client,metric_channel,retrieved_count,event_id,access_id,memory_id,was_used,disposition
       FROM (
         SELECT * FROM (SELECT 'retrieval' kind,'0r:'||retrieval_id sort_key,retrieval_id,repository_id,checkout_id,
                session_id,branch,accessor,metric_client,metric_channel,retrieved_count,retrieval_event_id event_id,
                NULL access_id,NULL memory_id,NULL was_used,NULL disposition
         FROM memory_attribution_retrievals WHERE retrieval_metric_recorded=0 AND ?6=1 AND retrieval_id>?3 ORDER BY retrieval_id LIMIT ?2)
         UNION ALL
         SELECT * FROM (SELECT 'access','1a:'||a.retrieval_id||':'||a.access_id,a.retrieval_id,r.repository_id,
                r.checkout_id,r.session_id,r.branch,r.accessor,r.metric_client,r.metric_channel,r.retrieved_count,
                r.terminal_event_id,a.access_id,a.memory_id,m.was_used,r.disposition
         FROM memory_attribution_accesses a JOIN memory_attribution_retrievals r USING(retrieval_id)
              JOIN memory_accesses m USING(access_id)
         WHERE a.metric_recorded=0 AND r.disposition IS NOT NULL AND m.was_used IS NOT NULL
           AND (a.retrieval_id>?4 OR (a.retrieval_id=?4 AND a.access_id>?5))
         ORDER BY a.retrieval_id,a.access_id LIMIT ?2))
       ORDER BY sort_key LIMIT ?2".replace("\n+","\n");
    let mut statement = conn
        .prepare(&sql)
        .map_err(sql_error("prepare pending attribution metrics"))?;
    let rows = statement
        .query_map(
            params![
                "",
                limit as i64,
                retrieval_after,
                access_retrieval_after,
                access_after,
                i64::from(include_retrievals)
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, Option<i64>>(14)?,
                    row.get::<_, Option<String>>(15)?,
                ))
            },
        )
        .map_err(sql_error("query pending attribution metrics"))?;
    let mut items = Vec::new();
    let mut next_cursor = None;
    for row in rows {
        let (
            kind,
            key,
            retrieval_id,
            repository_id,
            checkout_id,
            session_id,
            branch,
            accessor,
            metric_client,
            metric_channel,
            retrieved_count,
            event_id,
            access_id,
            memory_id,
            was_used,
            disposition,
        ) = row.map_err(sql_error("read pending attribution metric"))?;
        let item = if kind == "retrieval" {
            next_cursor = Some(PendingAttributionCursor::Retrieval {
                retrieval_id: retrieval_id.clone(),
            });
            PendingAttributionMetric::Retrieval {
                retrieval_id,
                repository_id,
                checkout_id,
                session_id,
                branch,
                accessor,
                metric_client,
                metric_channel,
                retrieved_count: retrieved_count.max(0) as usize,
                retrieval_event_id: parse_event_id(&event_id)?,
            }
        } else {
            let access_id =
                access_id.ok_or_else(|| storage_error("pending access metric lacks access id"))?;
            next_cursor = Some(PendingAttributionCursor::Access {
                retrieval_id: retrieval_id.clone(),
                access_id: access_id.clone(),
            });
            PendingAttributionMetric::Access {
                retrieval_id,
                access_id,
                local_memory_id: memory_id
                    .ok_or_else(|| storage_error("pending access metric lacks memory id"))?,
                was_used: was_used.is_some_and(|v| v != 0),
                disposition: AttributionDisposition::parse(
                    &disposition
                        .ok_or_else(|| storage_error("pending access metric lacks disposition"))?,
                )?,
                terminal_event_id: parse_event_id(&event_id)?,
                metric_client,
                metric_channel,
                session_id,
            }
        };
        let _ = key;
        items.push(item);
    }
    if items.len() < limit {
        next_cursor = None
    };
    Ok(PendingAttributionMetrics { items, next_cursor })
}

fn mark_access_metrics(conn: &Connection, ids: &[String]) -> Result<(), LatticeError> {
    if ids.len() > MAX_ATTRIBUTION_ACCESSES {
        return Err(storage_error("too many attribution access metric ids"));
    };
    let tx = conn
        .unchecked_transaction()
        .map_err(sql_error("begin access metric update"))?;
    for id in ids {
        validate_bounded("access_id", id)?;
        let changed = tx
            .execute(
                "UPDATE memory_attribution_accesses SET metric_recorded=1 WHERE access_id=?1",
                [id],
            )
            .map_err(sql_error("mark attribution access metric"))?;
        if changed != 1 {
            return Err(storage_error(format!(
                "attribution access `{id}` was not found"
            )));
        }
    }
    tx.commit()
        .map_err(sql_error("commit access metric update"))
}

fn prune(
    conn: &Connection,
    now: u64,
    policy: AttributionPrunePolicy,
) -> Result<AttributionPruneOutcome, LatticeError> {
    if policy.batch_limit == 0 || policy.batch_limit > MAX_ATTRIBUTION_PRUNE_BATCH {
        return Err(storage_error(
            "attribution prune batch must be between 1 and 1024",
        ));
    }
    if policy.max_resolved_retrievals == 0
        || policy.max_resolved_retrievals > MAX_ATTRIBUTION_RETAINED
        || policy.max_pending_age_secs == 0
        || policy.max_metric_pending_age_secs < policy.max_pending_age_secs
    {
        return Err(storage_error(
            "attribution prune requires retained count 1..100000 and metric-pending age >= pending age > 0",
        ));
    }
    let tx = conn
        .unchecked_transaction()
        .map_err(sql_error("begin attribution prune"))?;
    let outcome = prune_in_transaction(&tx, now, policy)?;
    tx.commit().map_err(sql_error("commit attribution prune"))?;
    Ok(outcome)
}

pub(crate) fn prune_in_transaction(
    tx: &Transaction<'_>,
    now: u64,
    policy: AttributionPrunePolicy,
) -> Result<AttributionPruneOutcome, LatticeError> {
    prune_in_transaction_with_budget(tx, now, policy, ATTRIBUTION_PRUNE_VM_INSTRUCTIONS)
}

pub(crate) fn prune_in_transaction_with_budget(
    tx: &Transaction<'_>,
    now: u64,
    policy: AttributionPrunePolicy,
    instruction_budget: u64,
) -> Result<AttributionPruneOutcome, LatticeError> {
    if policy.batch_limit == 0
        || policy.batch_limit > MAX_ATTRIBUTION_PRUNE_BATCH
        || policy.max_resolved_retrievals == 0
        || policy.max_resolved_retrievals > MAX_ATTRIBUTION_RETAINED
        || policy.max_pending_age_secs == 0
        || policy.max_metric_pending_age_secs < policy.max_pending_age_secs
    {
        return Err(storage_error("invalid attribution prune policy"));
    }
    let cutoff = now.saturating_sub(policy.max_resolved_age_secs);
    let pending_cutoff = now.saturating_sub(policy.max_pending_age_secs);
    let outbox_cutoff = now.saturating_sub(policy.max_metric_pending_age_secs);
    let mut statement=tx.prepare("SELECT retrieval_id,payload_hash,metric_pending FROM (
      SELECT * FROM (SELECT retrieval_id,payload_hash,coalesce(resolved_at,created_at) age,
        (retrieval_metric_recorded=0 OR EXISTS(SELECT 1 FROM memory_attribution_accesses a WHERE a.retrieval_id=r.retrieval_id AND a.metric_recorded=0)) metric_pending
       FROM memory_attribution_retrievals r
       WHERE disposition IS NOT NULL AND resolved_at<?1
         AND ((retrieval_metric_recorded=1 AND NOT EXISTS(SELECT 1 FROM memory_attribution_accesses a WHERE a.retrieval_id=r.retrieval_id AND a.metric_recorded=0)) OR created_at<?3)
       ORDER BY resolved_at,retrieval_id LIMIT ?4)
      UNION
      SELECT * FROM (SELECT retrieval_id,payload_hash,created_at age,
        (retrieval_metric_recorded=0 OR EXISTS(SELECT 1 FROM memory_attribution_accesses a WHERE a.retrieval_id=r.retrieval_id AND a.metric_recorded=0)) metric_pending
       FROM memory_attribution_retrievals r
       WHERE disposition IS NULL AND created_at<?2
         AND ((retrieval_metric_recorded=1 AND NOT EXISTS(SELECT 1 FROM memory_attribution_accesses a WHERE a.retrieval_id=r.retrieval_id AND a.metric_recorded=0)) OR created_at<?3)
       ORDER BY created_at,retrieval_id LIMIT ?4)
      UNION
      SELECT * FROM (SELECT retrieval_id,payload_hash,resolved_at age,
        (retrieval_metric_recorded=0 OR EXISTS(SELECT 1 FROM memory_attribution_accesses a WHERE a.retrieval_id=r.retrieval_id AND a.metric_recorded=0)) metric_pending
       FROM memory_attribution_retrievals r
       WHERE disposition IS NOT NULL
         AND ((retrieval_metric_recorded=1 AND NOT EXISTS(SELECT 1 FROM memory_attribution_accesses a WHERE a.retrieval_id=r.retrieval_id AND a.metric_recorded=0)) OR created_at<?3)
       ORDER BY resolved_at DESC,retrieval_id DESC LIMIT ?4 OFFSET ?5))
      ORDER BY age,retrieval_id LIMIT ?4").map_err(sql_error("prepare attribution prune"))?;
    let budget = PruneProgressBudget::install(tx, instruction_budget);
    let candidate_result = statement
        .query_map(
            params![
                to_i64(cutoff, "prune cutoff")?,
                to_i64(pending_cutoff, "pending prune cutoff")?,
                to_i64(outbox_cutoff, "outbox prune cutoff")?,
                policy.batch_limit as i64,
                policy.max_resolved_retrievals as i64
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )
        .map_err(sql_error("query attribution prune"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error("read attribution prune"));
    let interrupted = budget.was_interrupted();
    drop(budget);
    let candidates = match candidate_result {
        Ok(candidates) => candidates,
        Err(_error) if interrupted => {
            return Err(storage_error(format!(
                "attribution retention exceeded the bounded SQLite work allowance ({instruction_budget} virtual-machine instructions); drain the metric outbox or retry maintenance"
            )))
        }
        Err(error) => return Err(error),
    };
    drop(statement);
    for (id, hash, metric_pending) in &candidates {
        let reason = if *metric_pending {
            "metric_delivery_expired"
        } else {
            "retention_expired"
        };
        tx.execute("INSERT INTO memory_attribution_expired(retrieval_id,payload_hash,expired_at,reason) VALUES(?1,?2,?3,?4) ON CONFLICT(retrieval_id) DO NOTHING",params![id,hash,to_i64(now,"prune time")?,reason]).map_err(sql_error("write attribution expiry receipt"))?;
        tx.execute("DELETE FROM memory_accesses WHERE access_id IN (SELECT access_id FROM memory_attribution_accesses WHERE retrieval_id=?1)",[id]).map_err(sql_error("delete pruned attribution accesses"))?;
        tx.execute(
            "DELETE FROM memory_attribution_retrievals WHERE retrieval_id=?1",
            [id],
        )
        .map_err(sql_error("delete pruned attribution retrieval"))?;
    }
    let receipt_cutoff = cutoff.saturating_sub(policy.max_resolved_age_secs);
    let expired=tx.execute("DELETE FROM memory_attribution_expired WHERE retrieval_id IN (SELECT retrieval_id FROM memory_attribution_expired WHERE expired_at<?1 ORDER BY expired_at,retrieval_id LIMIT ?2)",params![to_i64(receipt_cutoff,"expiry receipt cutoff")?,policy.batch_limit as i64]).map_err(sql_error("prune attribution expiry receipts"))?;
    Ok(AttributionPruneOutcome {
        retrievals_pruned: candidates.len(),
        expired_receipts_pruned: expired,
        metric_dead_letters: candidates.iter().filter(|(_, _, pending)| *pending).count(),
    })
}

struct PruneProgressBudget<'a> {
    conn: &'a Connection,
    interrupted: Arc<AtomicBool>,
}

impl<'a> PruneProgressBudget<'a> {
    fn install(conn: &'a Connection, instructions: u64) -> Self {
        let callbacks = Arc::new(AtomicU64::new(0));
        let interrupted = Arc::new(AtomicBool::new(false));
        let callback_count = Arc::clone(&callbacks);
        let callback_interrupted = Arc::clone(&interrupted);
        let callback_budget = instructions.max(1).div_ceil(PRUNE_PROGRESS_GRANULARITY);
        conn.progress_handler(
            PRUNE_PROGRESS_GRANULARITY as i32,
            Some(move || {
                let exceeded = callback_count
                    .fetch_add(1, Ordering::Relaxed)
                    .saturating_add(1)
                    >= callback_budget;
                if exceeded {
                    callback_interrupted.store(true, Ordering::Release);
                }
                exceeded
            }),
        );
        Self { conn, interrupted }
    }

    fn was_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::Acquire)
    }
}

impl Drop for PruneProgressBudget<'_> {
    fn drop(&mut self) {
        self.conn.progress_handler(0, None::<fn() -> bool>);
    }
}

fn resolution_outcome(
    stored: StoredAttributionRetrieval,
    status: AttributionResolutionStatus,
) -> AttributionResolutionOutcome {
    AttributionResolutionOutcome {
        status,
        retrieval_metric_pending: stored.retrieval_metric_pending,
        access_metric_pending_ids: stored.access_metric_pending_ids,
    }
}
fn payload_hash(input: &AttributionRetrievalInput) -> Result<Vec<u8>, LatticeError> {
    let bytes = serde_json::to_vec(input).map_err(|error| {
        storage_error(format!("failed to encode attribution retrieval: {error}"))
    })?;
    Ok(Sha256::digest(bytes).to_vec())
}
fn parse_event_id(value: &str) -> Result<EventId, LatticeError> {
    match crate::identity::decode_identity(value)
        .map_err(|error| storage_error(format!("invalid stored attribution event id: {error}")))?
    {
        crate::identity::Identity::Event(id) => Ok(id),
        _ => Err(storage_error(
            "stored attribution identity is not an event id",
        )),
    }
}
fn validate_bounded(name: &str, value: &str) -> Result<(), LatticeError> {
    if value.is_empty() {
        return Err(storage_error(format!("{name} must not be empty")));
    }
    if value.len() > MAX_ATTRIBUTION_STRING_BYTES {
        return Err(storage_error(format!(
            "{name} exceeds {MAX_ATTRIBUTION_STRING_BYTES} UTF-8 bytes"
        )));
    }
    Ok(())
}
fn to_i64(value: u64, name: &str) -> Result<i64, LatticeError> {
    i64::try_from(value).map_err(|_| storage_error(format!("{name} exceeds SQLite integer range")))
}
fn sql_error(context: &'static str) -> impl FnOnce(rusqlite::Error) -> LatticeError {
    move |error| storage_error(format!("{context}: {error}"))
}
fn storage_error(message: impl Into<String>) -> LatticeError {
    LatticeError::Storage(message.into())
}
