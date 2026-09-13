//! Bounded, indexed inspection of contradiction and supersession edges.
//!
//! This is an explicit inspection surface. It deliberately retains semantically
//! stale, contradicted, and superseded memories while enforcing scope authority.

use crate::error::LatticeError;
use crate::verification::ScopeFilter;
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

const CONFLICT_VM_BUDGET_ENV: &str = "LATTICE_MEMORY_CONFLICT_VM_INSTRUCTIONS";
const DEFAULT_CONFLICT_VM_INSTRUCTIONS: u64 = 5_000_000;
const MAX_CONFLICT_VM_INSTRUCTIONS: u64 = 1_000_000_000;
const PROGRESS_GRANULARITY: u64 = 1_000;
pub const MAX_CONFLICT_PAGE_SIZE: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictAnchorQuery {
    Memory(String),
    File(String),
    Symbol(String),
    Doc(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictQueryRecord {
    pub source_memory_id: String,
    pub source_workspace_id: String,
    pub target_memory_id: String,
    pub target_workspace_id: String,
    pub link_type: String,
    pub link_strength: f32,
    pub created_by: String,
    pub created_at: u64,
    pub verification_status: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictQueryPage {
    pub records: Vec<ConflictQueryRecord>,
    pub total: usize,
}

pub fn query_conflicts(
    conn: &Connection,
    scope: &ScopeFilter,
    applicable_checkout_id: Option<&str>,
    anchor: &ConflictAnchorQuery,
    offset: usize,
    limit: usize,
) -> Result<ConflictQueryPage, LatticeError> {
    query_conflicts_with_budget(
        conn,
        scope,
        applicable_checkout_id,
        anchor,
        offset,
        limit,
        conflict_vm_instruction_budget()?,
    )
}

pub(crate) fn query_conflicts_with_budget(
    conn: &Connection,
    scope: &ScopeFilter,
    applicable_checkout_id: Option<&str>,
    anchor: &ConflictAnchorQuery,
    offset: usize,
    limit: usize,
    instruction_budget: u64,
) -> Result<ConflictQueryPage, LatticeError> {
    scope
        .validate()
        .map_err(|error| LatticeError::Storage(format!("Invalid memory scope filter: {error}")))?;
    if limit == 0 || limit > MAX_CONFLICT_PAGE_SIZE {
        return Err(LatticeError::Storage(format!(
            "memory conflict page limit must be between 1 and {MAX_CONFLICT_PAGE_SIZE}"
        )));
    }
    let end = offset.checked_add(limit).ok_or_else(|| {
        LatticeError::Storage("memory conflict cursor plus limit overflows usize".into())
    })?;
    let offset = i64::try_from(offset).map_err(|_| {
        LatticeError::Storage("memory conflict cursor exceeds SQLite integer range".into())
    })?;
    let _end = i64::try_from(end).map_err(|_| {
        LatticeError::Storage("memory conflict page end exceeds SQLite integer range".into())
    })?;
    let limit = i64::try_from(limit).expect("bounded page size fits i64");

    if matches!(anchor, ConflictAnchorQuery::Doc(_)) {
        let complete =
            super::retrieval::advance_doc_membership_backfill(conn).map_err(|error| {
                LatticeError::Storage(format!(
                    "failed to advance linked-doc conflict index backfill: {error}"
                ))
            })?;
        if !complete {
            return Err(LatticeError::Storage(
                "linked-doc conflict index backfill is still progressing in bounded pages; retry the inspection to advance the next page".into(),
            ));
        }
    }

    let (anchor_kind, anchor_value, anchor_parent) = match anchor {
        ConflictAnchorQuery::Memory(id) => ("memory", id.to_string(), id.to_string()),
        ConflictAnchorQuery::File(path) => ("file", normalize_path(path), normalize_path(path)),
        ConflictAnchorQuery::Symbol(symbol) => {
            let symbol = symbol.trim().to_lowercase();
            ("symbol", symbol.clone(), symbol)
        }
        ConflictAnchorQuery::Doc(doc) => {
            let doc = normalize_path(doc);
            let parent = doc
                .split_once('#')
                .map_or_else(|| doc.clone(), |(path, _)| path.to_string());
            ("doc", doc, parent)
        }
    };
    let values = query_values(
        scope,
        applicable_checkout_id,
        anchor_kind,
        anchor_value,
        anchor_parent,
    );
    conn.execute_batch("SAVEPOINT memory_conflict_inspection")
        .map_err(|error| {
            LatticeError::Storage(format!(
                "failed to begin memory conflict read snapshot: {error}"
            ))
        })?;
    let budget = ProgressBudget::install(conn, instruction_budget);
    let result = run_query(conn, anchor, values, offset, limit);
    let interrupted = budget.was_interrupted();
    drop(budget);
    let page = match result {
        Ok(page) => page,
        Err(error) => {
            let cleanup = conn.execute_batch(
                "ROLLBACK TO memory_conflict_inspection; RELEASE memory_conflict_inspection",
            );
            if let Err(cleanup) = cleanup {
                return Err(LatticeError::Storage(format!(
                    "memory conflict query failed ({error}); failed to release its read snapshot: {cleanup}"
                )));
            }
            if interrupted {
                return Err(LatticeError::Storage(format!(
                    "indexed memory conflict inspection exceeded the bounded SQLite work allowance ({instruction_budget} virtual-machine instructions); narrow the anchor or reduce conflict density"
                )));
            }
            return Err(error);
        }
    };
    conn.execute_batch("RELEASE memory_conflict_inspection")
        .map_err(|error| {
            LatticeError::Storage(format!(
                "failed to release memory conflict read snapshot: {error}"
            ))
        })?;
    Ok(page)
}

fn run_query(
    conn: &Connection,
    requested_anchor: &ConflictAnchorQuery,
    mut values: Vec<Value>,
    offset: i64,
    limit: i64,
) -> Result<ConflictQueryPage, LatticeError> {
    let prefix = query_prefix();
    values.push(Value::Integer(limit));
    values.push(Value::Integer(offset));
    let sql = format!(
        "{prefix}, page AS (
           SELECT * FROM deduplicated
           ORDER BY created_at DESC,source_id,target_id,link_type,reason LIMIT ?9 OFFSET ?10
         ), stats AS (
           SELECT (SELECT count(*) FROM anchors) anchor_count,
                  (SELECT min(endpoint) FROM unauthorized_endpoints) unauthorized_endpoint,
                  (SELECT count(*) FROM deduplicated) total
         )
         SELECT stats.anchor_count,stats.unauthorized_endpoint,stats.total,
                page.source_id,coalesce(source.workspace_id,'workspace-main'),
                page.target_id,coalesce(target.workspace_id,'workspace-main'),
                page.link_type,page.created_at,page.status,page.reason
         FROM stats LEFT JOIN page ON 1=1
         LEFT JOIN memories source ON source.id=page.source_id
         LEFT JOIN memories target ON target.id=page.target_id"
    );
    let mut statement = conn.prepare(&sql).map_err(|error| {
        LatticeError::Storage(format!(
            "failed to prepare indexed memory conflicts: {error}"
        ))
    })?;
    let mut rows = statement
        .query(params_from_iter(values.iter()))
        .map_err(|error| {
            LatticeError::Storage(format!("failed to query indexed memory conflicts: {error}"))
        })?;
    let mut records = Vec::new();
    let mut anchor_count = 0_i64;
    let mut unauthorized: Option<String> = None;
    let mut total = 0_i64;
    while let Some(row) = rows.next().map_err(|error| {
        LatticeError::Storage(format!("failed to read indexed memory conflict: {error}"))
    })? {
        anchor_count = row.get(0).map_err(sql_row_error)?;
        unauthorized = row.get(1).map_err(sql_row_error)?;
        total = row.get(2).map_err(sql_row_error)?;
        let source_memory_id: Option<String> = row.get(3).map_err(sql_row_error)?;
        let Some(source_memory_id) = source_memory_id else {
            continue;
        };
        records.push(ConflictQueryRecord {
            source_memory_id,
            source_workspace_id: row.get(4).map_err(sql_row_error)?,
            target_memory_id: row.get(5).map_err(sql_row_error)?,
            target_workspace_id: row.get(6).map_err(sql_row_error)?,
            link_type: row.get(7).map_err(sql_row_error)?,
            link_strength: 1.0,
            created_by: "system".into(),
            created_at: row.get::<_, i64>(8).map_err(sql_row_error)?.max(0) as u64,
            verification_status: row.get(9).map_err(sql_row_error)?,
            reason: row.get(10).map_err(sql_row_error)?,
        });
    }
    if anchor_count == 0 {
        if let ConflictAnchorQuery::Memory(id) = requested_anchor {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM memories WHERE id=?1)",
                    [id],
                    |row| row.get(0),
                )
                .map_err(|error| {
                    LatticeError::Storage(format!("failed to inspect memory anchor scope: {error}"))
                })?;
            return Err(LatticeError::Storage(if exists {
                format!("Memory `{id}` is outside the active scope filter")
            } else {
                format!("Memory `{id}` was not found")
            }));
        }
        return Err(LatticeError::Storage(
            "No in-scope memories matched the requested anchor".into(),
        ));
    }
    if let Some(id) = unauthorized {
        return Err(LatticeError::Storage(format!(
            "Conflict memory `{id}` is outside the active scope filter"
        )));
    }
    Ok(ConflictQueryPage {
        records,
        total: usize::try_from(total)
            .map_err(|_| LatticeError::Storage("memory conflict total exceeds usize".into()))?,
    })
}

fn sql_row_error(error: rusqlite::Error) -> LatticeError {
    LatticeError::Storage(format!("failed to decode indexed memory conflict: {error}"))
}

fn query_values(
    scope: &ScopeFilter,
    checkout_id: Option<&str>,
    kind: &str,
    value: String,
    parent: String,
) -> Vec<Value> {
    vec![
        scope.session_id.clone().map_or(Value::Null, Value::Text),
        Value::Text(scope.workspace_id.clone()),
        scope
            .branch
            .as_ref()
            .map(|branch| branch.name.clone())
            .map_or(Value::Null, Value::Text),
        scope
            .organization_id
            .clone()
            .map_or(Value::Null, Value::Text),
        checkout_id
            .map(str::to_string)
            .map_or(Value::Null, Value::Text),
        Value::Text(kind.into()),
        Value::Text(value),
        Value::Text(parent),
    ]
}

fn query_prefix() -> String {
    let scoped = |alias: &str| {
        format!(
        "(({alias}.scope='session' AND ?1 IS NOT NULL AND {alias}.session_id=?1)
          OR ({alias}.scope='branch' AND ?3 IS NOT NULL AND {alias}.workspace_id=?2 AND {alias}.branch=?3)
          OR ({alias}.scope='repo' AND {alias}.workspace_id=?2)
          OR ({alias}.scope='organization' AND ?4 IS NOT NULL AND {alias}.scope_organization_id=?4))
         AND ({alias}.applicable_checkout_id IS NULL OR (?5 IS NOT NULL AND {alias}.applicable_checkout_id=?5))
         AND {alias}.is_invalidated=0"
    )
    };
    let anchor_scope = scoped("m");
    let source_scope = scoped("source");
    let target_scope = scoped("target");
    format!(
        "WITH anchor_candidates(id) AS (
           SELECT id FROM memories WHERE ?6='memory' AND id=?7
           UNION SELECT memory_id FROM memory_retrieval_paths WHERE ?6='file' AND path=?7
           UNION SELECT memory_id FROM memory_retrieval_symbols WHERE ?6='symbol' AND symbol=?7
           UNION SELECT memory_id FROM memory_retrieval_docs WHERE ?6='doc' AND doc IN (?7,?8)
         ), anchors(id) AS (
           SELECT m.id FROM memories m JOIN anchor_candidates a ON a.id=m.id WHERE {anchor_scope}
         ), raw_edges(source_id,target_id,link_type,created_at,status,reason) AS (
           SELECT l.source_memory_id,l.target_memory_id,l.link_type,l.created_at,l.verification_status,l.reason
             FROM memory_links l WHERE l.link_type IN ('contradicts','supersedes')
             AND (l.source_memory_id IN (SELECT id FROM anchors) OR l.target_memory_id IN (SELECT id FROM anchors))
           UNION ALL SELECT a.id,j.value,'contradicts',m.created_at,'contradicted','structured contradicts edge'
             FROM anchors a JOIN memories m ON m.id=a.id JOIN json_each(m.contradicts_memory_ids) j
           UNION ALL SELECT j.value,a.id,'contradicts',m.created_at,'contradicted','structured contradicted_by edge'
             FROM anchors a JOIN memories m ON m.id=a.id JOIN json_each(m.contradicted_by_memory_ids) j
           UNION ALL SELECT a.id,m.supersedes_memory_id,'supersedes',m.created_at,'superseded','structured supersedes edge'
             FROM anchors a JOIN memories m ON m.id=a.id WHERE m.supersedes_memory_id IS NOT NULL
           UNION ALL SELECT m.superseded_by_memory_id,a.id,'supersedes',m.created_at,'superseded','structured superseded_by edge'
             FROM anchors a JOIN memories m ON m.id=a.id WHERE m.superseded_by_memory_id IS NOT NULL
           UNION ALL SELECT m.superseded_by_memory_id,m.id,'supersedes',m.created_at,'superseded','structured superseded_by edge'
             FROM memories m WHERE m.superseded_by_memory_id IN (SELECT id FROM anchors)
           UNION ALL SELECT m.id,m.supersedes_memory_id,'supersedes',m.created_at,'superseded','structured supersedes edge'
             FROM memories m WHERE m.supersedes_memory_id IN (SELECT id FROM anchors)
         ), ranked AS (
           SELECT *,row_number() OVER (PARTITION BY source_id,target_id,link_type,reason ORDER BY created_at DESC) AS duplicate_rank
             FROM raw_edges
         ), deduplicated AS (
           SELECT r.source_id,r.target_id,r.link_type,r.created_at,r.status,r.reason FROM ranked r
             JOIN memories source ON source.id=r.source_id JOIN memories target ON target.id=r.target_id
             WHERE r.duplicate_rank=1 AND {source_scope} AND {target_scope}
         ), unauthorized_endpoints(endpoint) AS (
           SELECT r.source_id FROM raw_edges r LEFT JOIN memories source ON source.id=r.source_id
             WHERE source.id IS NULL OR NOT ({source_scope})
           UNION SELECT r.target_id FROM raw_edges r LEFT JOIN memories target ON target.id=r.target_id
             WHERE target.id IS NULL OR NOT ({target_scope})
         )"
    )
}

fn normalize_path(value: &str) -> String {
    value.trim().replace('\\', "/").to_lowercase()
}

fn conflict_vm_instruction_budget() -> Result<u64, LatticeError> {
    let Some(raw) = std::env::var_os(CONFLICT_VM_BUDGET_ENV) else {
        return Ok(DEFAULT_CONFLICT_VM_INSTRUCTIONS);
    };
    let value = raw.to_string_lossy().parse::<u64>().map_err(|_| {
        LatticeError::Storage(format!("{CONFLICT_VM_BUDGET_ENV} must be an integer"))
    })?;
    if value == 0 || value > MAX_CONFLICT_VM_INSTRUCTIONS {
        return Err(LatticeError::Storage(format!(
            "{CONFLICT_VM_BUDGET_ENV} must be between 1 and {MAX_CONFLICT_VM_INSTRUCTIONS}"
        )));
    }
    Ok(value)
}

struct ProgressBudget<'a> {
    conn: &'a Connection,
    interrupted: Arc<AtomicBool>,
}

impl<'a> ProgressBudget<'a> {
    fn install(conn: &'a Connection, instructions: u64) -> Self {
        let callbacks = Arc::new(AtomicU64::new(0));
        let interrupted = Arc::new(AtomicBool::new(false));
        let callback_count = Arc::clone(&callbacks);
        let callback_interrupted = Arc::clone(&interrupted);
        let callback_budget = instructions.max(1).div_ceil(PROGRESS_GRANULARITY);
        conn.progress_handler(
            PROGRESS_GRANULARITY as i32,
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

impl Drop for ProgressBudget<'_> {
    fn drop(&mut self) {
        self.conn.progress_handler(0, None::<fn() -> bool>);
    }
}
