use std::collections::VecDeque;
use std::sync::Arc;

use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};
use sha2::{Digest, Sha256};
use tracing::{debug_span, warn};

use crate::events::hashing::PayloadHash;
use crate::events::kinds::{EventKind, EventPayload};
use crate::events::query::{
    branch_ref, event_id, session_id, task_id, workspace_id, EventQuery, EventQueryError,
    QueryOrder, TailScope,
};
use crate::events::{Actor, CompactSummary, EventEnvelope, EventModelError, EventStore, StableRef};
use crate::{DateTime, Utc};

pub struct EventReader {
    store: Arc<EventStore>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventPage {
    pub events: Vec<EventEnvelope>,
    pub next_cursor_row_id: Option<i64>,
}

#[derive(Clone, Debug)]
struct QueryPlan {
    query: EventQuery,
    limit: usize,
}

#[derive(Clone, Copy, Debug)]
struct Cursor {
    row_id: i64,
}

struct EventRowData {
    row_id: i64,
    event_uuid: String,
    workspace_id: String,
    branch: String,
    session_id: String,
    task_id: Option<String>,
    actor_kind: String,
    actor_detail: Option<String>,
    kind: String,
    ts_unix_micros: i64,
    payload_hash: Vec<u8>,
    summary: String,
    payload_inline: Option<Vec<u8>>,
    payload_spill_id: Option<i64>,
    references_json: String,
    schema_version: i64,
    spilled_payload_hash: Option<Vec<u8>>,
    spilled_payload_bytes: Option<Vec<u8>>,
}

pub struct EventStream {
    store: Arc<EventStore>,
    plan: QueryPlan,
    cursor: Option<Cursor>,
    buffer: VecDeque<Result<EventEnvelope, EventQueryError>>,
    exhausted: bool,
}

impl EventReader {
    pub fn new(store: Arc<EventStore>) -> Self {
        Self { store }
    }

    pub fn execute(&self, query: EventQuery) -> Result<Vec<EventEnvelope>, EventQueryError> {
        let plan = QueryPlan::new(query)?;
        let _span =
            debug_span!("event_reader.execute", scope = %plan.query.trace_scope()).entered();
        self.fetch_page(&plan, None)
    }

    pub fn execute_page(
        &self,
        query: EventQuery,
        cursor_row_id: Option<i64>,
    ) -> Result<EventPage, EventQueryError> {
        let plan = QueryPlan::new(query)?;
        let _span = debug_span!(
            "event_reader.execute_page",
            scope = %plan.query.trace_scope(),
            cursor_row_id
        )
        .entered();
        let rows = self.fetch_page_rows(&plan, cursor_row_id.map(|row_id| Cursor { row_id }))?;
        let next_cursor_row_id = rows.last().map(|row| row.row_id);
        let events = rows
            .into_iter()
            .map(to_envelope)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(EventPage {
            next_cursor_row_id,
            events,
        })
    }

    pub fn stream(&self, query: EventQuery) -> EventStream {
        let stream_plan = QueryPlan::new(query);
        match stream_plan {
            Ok(plan) => EventStream {
                store: self.store.clone(),
                plan,
                cursor: None,
                buffer: VecDeque::new(),
                exhausted: false,
            },
            Err(error) => EventStream {
                store: self.store.clone(),
                plan: QueryPlan::fallback(),
                cursor: None,
                buffer: VecDeque::from([Err(error)]),
                exhausted: true,
            },
        }
    }

    pub fn tail<S>(&self, scope: S, n: usize) -> Result<Vec<EventEnvelope>, EventQueryError>
    where
        S: Into<TailScope>,
    {
        let limit = n.max(1);
        let query = match scope.into() {
            TailScope::Task(scope) => EventQuery::new().task(scope.task_id.value),
            TailScope::Session(scope) => EventQuery::new().session(scope.session_id.value),
        }
        .limit(limit)
        .order(QueryOrder::NewestFirst);
        self.execute(query)
    }

    fn fetch_page(
        &self,
        plan: &QueryPlan,
        cursor: Option<Cursor>,
    ) -> Result<Vec<EventEnvelope>, EventQueryError> {
        self.fetch_page_rows(plan, cursor)?
            .into_iter()
            .map(to_envelope)
            .collect()
    }

    fn fetch_page_rows(
        &self,
        plan: &QueryPlan,
        cursor: Option<Cursor>,
    ) -> Result<Vec<EventRowData>, EventQueryError> {
        let conn = self.store.lock_conn()?;
        query_rows(&conn, plan, cursor)
    }
}

impl Iterator for EventStream {
    type Item = Result<EventEnvelope, EventQueryError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(item) = self.buffer.pop_front() {
            return Some(item);
        }
        if self.exhausted {
            return None;
        }

        let conn = match self.store.lock_conn() {
            Ok(conn) => conn,
            Err(error) => {
                self.exhausted = true;
                return Some(Err(EventQueryError::Storage(error)));
            }
        };
        let rows = match query_rows(&conn, &self.plan, self.cursor) {
            Ok(rows) => rows,
            Err(error) => {
                self.exhausted = true;
                return Some(Err(error));
            }
        };
        if rows.is_empty() {
            self.exhausted = true;
            return None;
        }

        self.cursor = rows.last().map(cursor_from_row);
        self.exhausted = rows.len() < self.plan.limit;
        self.buffer = rows.into_iter().map(to_envelope).collect();
        self.buffer.pop_front()
    }
}

impl QueryPlan {
    fn new(query: EventQuery) -> Result<Self, EventQueryError> {
        query.validate()?;
        Ok(Self {
            limit: query.limit_or_default(),
            query,
        })
    }

    fn fallback() -> Self {
        Self {
            query: EventQuery::new().task("__invalid__"),
            limit: 1,
        }
    }
}

fn query_rows(
    conn: &Connection,
    plan: &QueryPlan,
    cursor: Option<Cursor>,
) -> Result<Vec<EventRowData>, EventQueryError> {
    let (sql, params) = build_sql(plan, cursor);
    let mut statement = conn.prepare(&sql).map_err(sqlite_error)?;
    let rows = statement
        .query_map(params_from_iter(params), read_row)
        .map_err(sqlite_error)?;
    let mut events = Vec::new();
    for row in rows {
        events.push(row.map_err(sqlite_error)?);
    }
    Ok(events)
}

fn build_sql(plan: &QueryPlan, cursor: Option<Cursor>) -> (String, Vec<Value>) {
    let mut sql = String::from(
        "SELECT e.event_id, e.event_uuid, e.workspace_id, e.branch, e.session_id, e.task_id, \
         e.actor_kind, e.actor_detail, e.kind, e.ts_unix_micros, e.payload_hash, e.summary, \
         e.payload_inline, e.payload_spill_id, e.references_json, e.schema_version, \
         p.payload_hash, p.bytes \
         FROM events e \
         LEFT JOIN event_payloads p ON p.row_id = e.payload_spill_id \
         WHERE ",
    );
    let mut params = Vec::new();
    push_scope_clause(&mut sql, &mut params, &plan.query);
    push_kind_clause(&mut sql, &mut params, &plan.query.kinds);
    push_time_clause(
        &mut sql,
        &mut params,
        "e.ts_unix_micros > ?",
        plan.query.after,
    );
    push_time_clause(
        &mut sql,
        &mut params,
        "e.ts_unix_micros < ?",
        plan.query.before,
    );
    push_cursor_clause(&mut sql, &mut params, plan.query.order_or_default(), cursor);
    push_order_clause(&mut sql, plan.query.order_or_default());
    sql.push_str(" LIMIT ?");
    params.push(Value::Integer(plan.limit as i64));
    (sql, params)
}

fn push_scope_clause(sql: &mut String, params: &mut Vec<Value>, query: &EventQuery) {
    if let Some(task_id) = &query.task_id {
        sql.push_str("e.task_id = ?");
        params.push(Value::Text(task_id.clone()));
        if let Some(session_id) = &query.session_id {
            sql.push_str(" AND e.session_id = ?");
            params.push(Value::Text(session_id.clone()));
        }
        if let Some(workspace_id) = &query.workspace_id {
            sql.push_str(" AND e.workspace_id = ?");
            params.push(Value::Text(workspace_id.clone()));
        }
        if let Some(branch) = &query.branch {
            sql.push_str(" AND e.branch = ?");
            params.push(Value::Text(branch.clone()));
        }
    } else if let Some(session_id) = &query.session_id {
        sql.push_str("e.session_id = ?");
        params.push(Value::Text(session_id.clone()));
        if let Some(workspace_id) = &query.workspace_id {
            sql.push_str(" AND e.workspace_id = ?");
            params.push(Value::Text(workspace_id.clone()));
        }
        if let Some(branch) = &query.branch {
            sql.push_str(" AND e.branch = ?");
            params.push(Value::Text(branch.clone()));
        }
    } else {
        sql.push_str("e.workspace_id = ? AND e.branch = ?");
        params.push(Value::Text(query.workspace_id.clone().unwrap_or_default()));
        params.push(Value::Text(query.branch.clone().unwrap_or_default()));
    }
}

fn push_kind_clause(sql: &mut String, params: &mut Vec<Value>, kinds: &[EventKind]) {
    if kinds.is_empty() {
        return;
    }
    sql.push_str(" AND e.kind IN (");
    for (index, kind) in kinds.iter().enumerate() {
        if index > 0 {
            sql.push_str(", ");
        }
        sql.push('?');
        params.push(Value::Text(kind.as_str().to_string()));
    }
    sql.push(')');
}

fn push_time_clause(
    sql: &mut String,
    params: &mut Vec<Value>,
    clause: &str,
    timestamp: Option<DateTime<Utc>>,
) {
    if let Some(timestamp) = timestamp {
        sql.push_str(" AND ");
        sql.push_str(clause);
        params.push(Value::Integer(timestamp.unix_seconds() * 1_000_000));
    }
}

fn push_cursor_clause(
    sql: &mut String,
    params: &mut Vec<Value>,
    order: QueryOrder,
    cursor: Option<Cursor>,
) {
    let Some(cursor) = cursor else {
        return;
    };
    let comparator = match order {
        QueryOrder::OldestFirst => ">",
        QueryOrder::NewestFirst => "<",
    };
    sql.push_str(" AND e.event_id ");
    sql.push_str(comparator);
    sql.push_str(" ?");
    params.push(Value::Integer(cursor.row_id));
}

fn push_order_clause(sql: &mut String, order: QueryOrder) {
    match order {
        QueryOrder::OldestFirst => {
            sql.push_str(" ORDER BY e.event_id ASC");
        }
        QueryOrder::NewestFirst => {
            sql.push_str(" ORDER BY e.event_id DESC");
        }
    }
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRowData> {
    Ok(EventRowData {
        row_id: row.get(0)?,
        event_uuid: row.get(1)?,
        workspace_id: row.get(2)?,
        branch: row.get(3)?,
        session_id: row.get(4)?,
        task_id: row.get(5)?,
        actor_kind: row.get(6)?,
        actor_detail: row.get(7)?,
        kind: row.get(8)?,
        ts_unix_micros: row.get(9)?,
        payload_hash: row.get(10)?,
        summary: row.get(11)?,
        payload_inline: row.get(12)?,
        payload_spill_id: row.get(13)?,
        references_json: row.get(14)?,
        schema_version: row.get(15)?,
        spilled_payload_hash: row.get(16)?,
        spilled_payload_bytes: row.get(17)?,
    })
}

fn to_envelope(row: EventRowData) -> Result<EventEnvelope, EventQueryError> {
    let expected = payload_hash_from_db(&row.payload_hash)?;
    validate_row_metadata(&row, expected)?;
    let payload_bytes = payload_bytes(&row)?;
    let actual = hash_bytes(payload_bytes);
    if expected != actual {
        let stable_event_id = event_id(&row.workspace_id, &row.event_uuid);
        warn!(
            event_type = "corruption",
            event_id = ?stable_event_id,
            expected = %expected,
            actual = %actual,
            "event payload corruption: hash mismatch while reading event log"
        );
        return Err(EventQueryError::PayloadCorruption {
            event_id: stable_event_id,
            expected,
            actual,
        });
    }

    let stable_event_id = event_id(&row.workspace_id, &row.event_uuid);
    let payload: EventPayload = serde_json::from_slice(payload_bytes).map_err(|source| {
        warn!(
            event_type = "corruption",
            event_id = ?stable_event_id,
            error = %source,
            "event payload corruption: payload JSON did not decode"
        );
        EventQueryError::PayloadDecode {
            event_id: stable_event_id.clone(),
            source,
        }
    })?;
    let references: Vec<StableRef> = serde_json::from_str(&row.references_json)?;
    let actor = parse_actor(&row.actor_kind, row.actor_detail.as_deref())?;
    let kind = row.kind.parse::<EventKind>().map_err(|error| {
        warn!(
            event_type = "corruption",
            event_id = ?stable_event_id,
            kind = row.kind.as_str(),
            error = %error,
            "event payload corruption: invalid event kind"
        );
        storage_invalid(error)
    })?;
    let payload_location = payload_location(&row);
    let summary = CompactSummary::new(row.summary).map_err(model_invalid)?;
    EventEnvelope::new(
        stable_event_id,
        workspace_id(&row.workspace_id),
        branch_ref(&row.branch),
        session_id(&row.session_id),
        row.task_id.as_deref().map(task_id),
        actor,
        DateTime::from_unix_seconds(row.ts_unix_micros / 1_000_000),
        kind,
        references,
        expected,
        summary,
        payload_location,
        payload,
    )
    .map_err(model_invalid)
}

fn payload_bytes(row: &EventRowData) -> Result<&[u8], EventQueryError> {
    if let Some(bytes) = row.payload_inline.as_deref() {
        return Ok(bytes);
    }
    if let Some(bytes) = row.spilled_payload_bytes.as_deref() {
        return Ok(bytes);
    }
    warn!(
        event_type = "recovery",
        event_uuid = row.event_uuid.as_str(),
        payload_spill_id = row.payload_spill_id.unwrap_or_default(),
        "event log recovery: spilled payload row is missing"
    );
    Err(storage_invalid(format!(
        "spilled payload for event `{}` row {} is missing",
        row.event_uuid,
        row.payload_spill_id.unwrap_or_default()
    )))
}

fn payload_location(row: &EventRowData) -> crate::events::PayloadLocation {
    if let Some(bytes) = &row.payload_inline {
        return crate::events::PayloadLocation::Inline {
            bytes_len: bytes.len() as u32,
        };
    }
    crate::events::PayloadLocation::Spilled {
        row_id: row.payload_spill_id.unwrap_or_default(),
    }
}

fn payload_hash_from_db(bytes: &[u8]) -> Result<PayloadHash, EventQueryError> {
    let array: [u8; 32] = bytes.try_into().map_err(|_| {
        storage_invalid(format!(
            "payload_hash stored {} bytes; expected 32 bytes",
            bytes.len()
        ))
    })?;
    Ok(PayloadHash::new(array))
}

fn hash_bytes(bytes: &[u8]) -> PayloadHash {
    let digest = Sha256::digest(bytes);
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(&digest);
    PayloadHash::new(hash)
}

fn parse_actor(kind: &str, detail: Option<&str>) -> Result<Actor, EventQueryError> {
    match (kind, detail) {
        ("assistant", Some(model)) => Ok(Actor::Assistant {
            model: model.to_string(),
        }),
        ("assistant", None) => Err(storage_invalid("assistant actor is missing model detail")),
        ("user", _) => Ok(Actor::User),
        ("tool", Some(name)) => Ok(Actor::Tool {
            name: name.to_string(),
        }),
        ("tool", None) => Err(storage_invalid("tool actor is missing name detail")),
        ("daemon", _) => Ok(Actor::Daemon),
        (other, _) => Err(storage_invalid(format!("unknown actor kind `{other}`"))),
    }
}

fn cursor_from_row(row: &EventRowData) -> Cursor {
    Cursor { row_id: row.row_id }
}

fn storage_invalid(reason: impl Into<String>) -> EventQueryError {
    EventQueryError::Storage(crate::events::EventStoreError::EnvelopeInvalid {
        reason: reason.into(),
    })
}

fn validate_row_metadata(row: &EventRowData, expected: PayloadHash) -> Result<(), EventQueryError> {
    if row.schema_version <= 0 {
        return Err(storage_invalid(format!(
            "event `{}` has invalid schema_version {}",
            row.event_uuid, row.schema_version
        )));
    }
    if let Some(bytes) = row.spilled_payload_hash.as_deref() {
        let spilled_hash = payload_hash_from_db(bytes)?;
        if spilled_hash != expected {
            warn!(
                event_type = "corruption",
                event_uuid = row.event_uuid.as_str(),
                expected = %expected,
                actual = %spilled_hash,
                "event payload corruption: spill row hash does not match envelope hash"
            );
            return Err(storage_invalid(format!(
                "spill row hash for event `{}` does not match envelope hash",
                row.event_uuid
            )));
        }
    }
    Ok(())
}

fn model_invalid(error: EventModelError) -> EventQueryError {
    storage_invalid(error.to_string())
}

fn sqlite_error(error: rusqlite::Error) -> EventQueryError {
    EventQueryError::Storage(crate::events::EventStoreError::Sqlite(error))
}
