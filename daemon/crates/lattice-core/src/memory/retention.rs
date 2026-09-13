//! Recall-based memory lifecycle. Reads and attempted delivery do not renew retention.
use crate::{error::LatticeError, working_memory::WorkingMemoryState};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
pub const DAY: u64 = 86_400;
pub const MAX_REPLAY_AGE_SECS: u64 = 7 * DAY;
pub const CHECKPOINT_REFERENCE_BACKFILL_ROWS_PER_PASS: usize = 64;
pub const CHECKPOINT_REFERENCE_BACKFILL_BYTES_PER_PASS: usize = 4 * 1024 * 1024;
pub const MAX_LEGACY_CHECKPOINT_PAYLOAD_BYTES: usize = 1024 * 1024;
pub const MAX_CHECKPOINT_MEMORY_REFERENCES: usize = 4096;
pub const RETENTION_DEPENDENCY_DELETE_BATCH: usize = 32;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetentionPolicy {
    pub stale_after_secs: u64,
    pub purge_after_secs: u64,
    pub sweep_interval_secs: u64,
    pub receipt_retention_secs: u64,
    pub max_receipts: usize,
    pub batch_size: usize,
}
impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            stale_after_secs: 90 * DAY,
            purge_after_secs: 180 * DAY,
            sweep_interval_secs: 3600,
            receipt_retention_secs: 30 * DAY,
            max_receipts: 100_000,
            batch_size: 256,
        }
    }
}
impl RetentionPolicy {
    pub fn from_env() -> Result<Self, LatticeError> {
        let mut p = Self::default();
        for (n, v) in [
            ("LATTICE_MEMORY_STALE_SECS", &mut p.stale_after_secs),
            ("LATTICE_MEMORY_PURGE_SECS", &mut p.purge_after_secs),
            ("LATTICE_MEMORY_SWEEP_SECS", &mut p.sweep_interval_secs),
            ("LATTICE_MEMORY_RECEIPT_SECS", &mut p.receipt_retention_secs),
        ] {
            if let Some(x) = std::env::var_os(n) {
                *v = x
                    .to_string_lossy()
                    .parse()
                    .map_err(|_| err(format!("{n} must be an integer")))?
            }
        }
        if let Ok(x) = std::env::var("LATTICE_MEMORY_RETENTION_BATCH") {
            p.batch_size = x.parse().map_err(|_| err("invalid retention batch"))?
        }
        if let Ok(x) = std::env::var("LATTICE_MEMORY_MAX_RECEIPTS") {
            p.max_receipts = x.parse().map_err(|_| err("invalid receipt limit"))?
        }
        p.validate()?;
        Ok(p)
    }
    pub fn validate(&self) -> Result<(), LatticeError> {
        if self.stale_after_secs == 0
            || self.stale_after_secs >= self.purge_after_secs
            || self.sweep_interval_secs == 0
            || self.receipt_retention_secs < MAX_REPLAY_AGE_SECS
            || self.max_receipts == 0
            || self.max_receipts > 100_000
            || self.batch_size == 0
            || self.batch_size > 4096
        {
            return Err(err("retention requires 0 < stale < purge, receipt retention >= replay age, positive limits, batch <= 4096, and max receipts <= 100000"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryBinding<'a> {
    pub delivery_id: &'a str,
    pub repository_id: &'a str,
    pub session_id: &'a str,
    pub payload_hash: &'a str,
}
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct RetentionHealth {
    pub active: u64,
    pub retention_stale: u64,
    pub purge_due: u64,
    pub receipts: u64,
    pub receipts_complete: bool,
    pub last_attempted_sweep: Option<u64>,
    pub last_successful_sweep: Option<u64>,
    pub last_error: Option<String>,
    pub logical_payload_bytes_purged: u64,
    pub physical_bytes_reclaimed: u64,
    pub reclamation_pending: bool,
    pub next_deadline: Option<u64>,
}
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct SweepReport {
    pub stale: usize,
    pub purged: usize,
    pub receipts_expired: usize,
    pub attribution_retrievals_pruned: usize,
    pub attribution_expired_receipts_pruned: usize,
    pub attribution_metric_dead_letters: usize,
    pub logical_payload_bytes_purged: u64,
    pub physical_bytes_reclaimed: u64,
    pub skipped: bool,
}
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct ReclamationReport {
    pub pages_reclaimed: u64,
    pub bytes_reclaimed: u64,
    pub wal_pages_pending: u64,
}

/// Reclaim a bounded number of SQLite free pages and report actual page-count
/// reduction. This never runs an unbounded full `VACUUM`.
pub fn reclaim_free_pages(
    c: &Connection,
    max_pages: u32,
) -> Result<ReclamationReport, LatticeError> {
    if max_pages == 0 || max_pages > 4096 {
        return Err(err("reclamation page budget must be 1..4096"));
    }
    let mode: i64 = sql(c.query_row("PRAGMA auto_vacuum", [], |r| r.get(0)))?;
    if mode != 2 {
        return Err(err(
            "memory store requires offline conversion to incremental auto-vacuum",
        ));
    }
    let before: u64 = sql(c.query_row("PRAGMA page_count", [], |r| r.get(0)))?;
    let page_size: u64 = sql(c.query_row("PRAGMA page_size", [], |r| r.get(0)))?;
    for _ in 0..max_pages {
        let free: u64 = sql(c.query_row("PRAGMA freelist_count", [], |r| r.get(0)))?;
        if free == 0 {
            break;
        }
        sql(c.execute_batch("PRAGMA incremental_vacuum(1)"))?;
    }
    let after: u64 = sql(c.query_row("PRAGMA page_count", [], |r| r.get(0)))?;
    let (_, log, checkpointed): (i64, i64, i64) =
        sql(c.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        }))?;
    let pages = before.saturating_sub(after);
    let bytes = pages.saturating_mul(page_size);
    sql(c.execute(
        "UPDATE memory_retention_control SET physical_bytes_reclaimed=physical_bytes_reclaimed+?1",
        [bytes],
    ))?;
    Ok(ReclamationReport {
        pages_reclaimed: pages,
        bytes_reclaimed: bytes,
        wal_pages_pending: log.saturating_sub(checkpointed).max(0) as u64,
    })
}
pub const DEFAULT_RECALL_PREDICATE: &str = "retention_stale = 0 AND purge_pending = 0";
pub fn initialize(c: &Connection, now: u64) -> Result<(), LatticeError> {
    let result = sql((|| {
        let tx = rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
        if !has_column(&tx, "memories", "last_recalled_at")? {
            tx.execute_batch("ALTER TABLE memories ADD COLUMN last_recalled_at INTEGER;ALTER TABLE memories ADD COLUMN retention_stale INTEGER NOT NULL DEFAULT 0;ALTER TABLE memories ADD COLUMN retention_grace_until INTEGER NOT NULL DEFAULT 0;")?;
            tx.execute(
                "UPDATE memories SET retention_grace_until=?1",
                [now.saturating_add(30 * DAY)],
            )?;
        }
        if !has_column(&tx, "memories", "purge_pending")? {
            tx.execute_batch("ALTER TABLE memories ADD COLUMN purge_pending INTEGER NOT NULL DEFAULT 0 CHECK(purge_pending IN(0,1));")?;
        }
        tx.execute_batch("CREATE INDEX IF NOT EXISTS idx_memory_idle_clock ON memories(COALESCE(last_recalled_at,created_at),id);CREATE TABLE IF NOT EXISTS memory_retention_control(id INTEGER PRIMARY KEY CHECK(id=1),next_sweep INTEGER NOT NULL,last_attempt INTEGER,last_success INTEGER,last_error TEXT,logical_payload_bytes_purged INTEGER NOT NULL DEFAULT 0,physical_bytes_reclaimed INTEGER NOT NULL DEFAULT 0,restore_floor INTEGER NOT NULL DEFAULT 0);INSERT OR IGNORE INTO memory_retention_control(id,next_sweep)VALUES(1,0);CREATE TABLE IF NOT EXISTS memory_deletion_receipts(memory_id TEXT PRIMARY KEY,deleted_at INTEGER NOT NULL);CREATE INDEX IF NOT EXISTS idx_memory_receipts_age ON memory_deletion_receipts(deleted_at,memory_id);CREATE TABLE IF NOT EXISTS memory_deliveries(delivery_id TEXT PRIMARY KEY,repository_id TEXT NOT NULL,session_id TEXT NOT NULL,payload_hash TEXT NOT NULL,memory_set_hash TEXT NOT NULL,attempted_at INTEGER NOT NULL,acknowledged_at INTEGER);CREATE TABLE IF NOT EXISTS memory_delivery_items(delivery_id TEXT NOT NULL REFERENCES memory_deliveries(delivery_id)ON DELETE CASCADE,memory_id TEXT NOT NULL REFERENCES memories(id)ON DELETE CASCADE,PRIMARY KEY(delivery_id,memory_id));CREATE INDEX IF NOT EXISTS idx_memory_delivery_age ON memory_deliveries(attempted_at);CREATE TRIGGER IF NOT EXISTS prevent_purged_memory_restore BEFORE INSERT ON memories WHEN EXISTS(SELECT 1 FROM memory_deletion_receipts WHERE memory_id=NEW.id)BEGIN SELECT RAISE(ABORT,'purged memory replay forbidden');END;CREATE TABLE IF NOT EXISTS working_memory_checkpoint_memory_refs(checkpoint_id INTEGER NOT NULL REFERENCES working_memory_checkpoints(checkpoint_id)ON DELETE CASCADE,memory_id TEXT NOT NULL,PRIMARY KEY(checkpoint_id,memory_id));CREATE INDEX IF NOT EXISTS idx_checkpoint_memory_ref ON working_memory_checkpoint_memory_refs(memory_id,checkpoint_id);")?;
        tx.execute_batch("CREATE INDEX IF NOT EXISTS idx_memory_retention_fresh_idle ON memories(COALESCE(last_recalled_at,created_at),id) WHERE retention_stale=0;CREATE INDEX IF NOT EXISTS idx_memory_retention_stale_idle ON memories(COALESCE(last_recalled_at,created_at),id) WHERE retention_stale=1;CREATE INDEX IF NOT EXISTS idx_memory_purge_pending ON memories(id) WHERE purge_pending=1;CREATE INDEX IF NOT EXISTS idx_memory_delivery_items_memory ON memory_delivery_items(memory_id,delivery_id);CREATE INDEX IF NOT EXISTS idx_memory_attribution_accesses_memory ON memory_attribution_accesses(memory_id,access_id);CREATE INDEX IF NOT EXISTS idx_memory_scope_filter_events_memory ON memory_scope_filter_events(memory_id,event_id);CREATE TRIGGER IF NOT EXISTS prevent_purge_pending_mutation BEFORE UPDATE ON memories WHEN OLD.purge_pending=1 BEGIN SELECT RAISE(ABORT,'purge-pending memory is immutable');END;CREATE TABLE IF NOT EXISTS working_memory_checkpoint_ref_backfill(id INTEGER PRIMARY KEY CHECK(id=1),next_checkpoint_id INTEGER NOT NULL DEFAULT 0,complete INTEGER NOT NULL DEFAULT 0 CHECK(complete IN(0,1)),blocked_checkpoint_id INTEGER,blocked_payload_bytes INTEGER,last_error TEXT);INSERT OR IGNORE INTO working_memory_checkpoint_ref_backfill(id,complete)SELECT 1,CASE WHEN EXISTS(SELECT 1 FROM working_memory_checkpoints)THEN 0 ELSE 1 END;CREATE TRIGGER IF NOT EXISTS working_memory_checkpoint_payload_limit_insert BEFORE INSERT ON working_memory_checkpoints WHEN CASE WHEN octet_length(NEW.state_json)>1048576 THEN 1 ELSE (SELECT COUNT(*) FROM json_tree(NEW.state_json) WHERE type='text')>4096 END BEGIN SELECT RAISE(ABORT,'working-memory checkpoint exceeds retention payload/reference limit');END;CREATE TRIGGER IF NOT EXISTS working_memory_checkpoint_payload_limit_update BEFORE UPDATE OF state_json ON working_memory_checkpoints WHEN CASE WHEN octet_length(NEW.state_json)>1048576 THEN 1 ELSE (SELECT COUNT(*) FROM json_tree(NEW.state_json) WHERE type='text')>4096 END BEGIN SELECT RAISE(ABORT,'working-memory checkpoint exceeds retention payload/reference limit');END;")?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS memory_receipt_count_state(id INTEGER PRIMARY KEY CHECK(id=1),cursor TEXT NOT NULL DEFAULT '',counted INTEGER NOT NULL DEFAULT 0,complete INTEGER NOT NULL DEFAULT 0 CHECK(complete IN(0,1)));INSERT OR IGNORE INTO memory_receipt_count_state(id,complete)SELECT 1,CASE WHEN EXISTS(SELECT 1 FROM memory_deletion_receipts)THEN 0 ELSE 1 END;CREATE TRIGGER IF NOT EXISTS memory_receipt_count_insert AFTER INSERT ON memory_deletion_receipts BEGIN UPDATE memory_receipt_count_state SET counted=counted+CASE WHEN complete=1 OR NEW.memory_id<=cursor THEN 1 ELSE 0 END WHERE id=1;END;CREATE TRIGGER IF NOT EXISTS memory_receipt_count_delete AFTER DELETE ON memory_deletion_receipts BEGIN UPDATE memory_receipt_count_state SET counted=MAX(0,counted-CASE WHEN complete=1 OR OLD.memory_id<=cursor THEN 1 ELSE 0 END) WHERE id=1;END;")?;
        for (name, definition) in [
            ("last_attempt", "INTEGER"),
            ("last_error", "TEXT"),
            ("logical_payload_bytes_purged", "INTEGER NOT NULL DEFAULT 0"),
            ("physical_bytes_reclaimed", "INTEGER NOT NULL DEFAULT 0"),
            ("restore_floor", "INTEGER NOT NULL DEFAULT 0"),
            ("stale_scan_cursor", "TEXT NOT NULL DEFAULT ''"),
            ("purge_scan_cursor", "TEXT NOT NULL DEFAULT ''"),
        ] {
            if !has_column(&tx, "memory_retention_control", name)? {
                tx.execute_batch(&format!(
                    "ALTER TABLE memory_retention_control ADD COLUMN {name} {definition}"
                ))?;
            }
        }
        tx.commit()
    })());
    if let Err(error) = &result {
        let _ = c.execute(
            "UPDATE memory_retention_control SET last_attempt=?1,last_error=?2 WHERE id=1",
            params![now, error.to_string()],
        );
    }
    result
}
fn set_hash(ids: &BTreeSet<String>) -> String {
    let mut h = Sha256::new();
    for id in ids {
        h.update((id.len() as u64).to_be_bytes());
        h.update(id)
    }
    format!("{:x}", h.finalize())
}

pub fn advance_checkpoint_reference_backfill(c: &Connection) -> Result<(), LatticeError> {
    let tx = rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate)
        .map_err(|e| err(e.to_string()))?;
    let (mut cursor,complete,blocked):(i64,bool,Option<i64>)=tx.query_row("SELECT next_checkpoint_id,complete,blocked_checkpoint_id FROM working_memory_checkpoint_ref_backfill WHERE id=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(|e|err(e.to_string()))?;
    if complete {
        tx.commit().map_err(|e| err(e.to_string()))?;
        return Ok(());
    }
    if let Some(id) = blocked {
        let current:Option<usize>=tx.query_row("SELECT octet_length(state_json) FROM working_memory_checkpoints WHERE checkpoint_id=?1",[id],|r|r.get(0)).optional().map_err(|e|err(e.to_string()))?;
        match current {
            None => {
                tx.execute("UPDATE working_memory_checkpoint_ref_backfill SET blocked_checkpoint_id=NULL,blocked_payload_bytes=NULL,last_error=NULL WHERE id=1",[]).map_err(|e|err(e.to_string()))?;
            }
            Some(size) if size <= MAX_LEGACY_CHECKPOINT_PAYLOAD_BYTES && tx.query_row("SELECT json_valid(state_json) FROM working_memory_checkpoints WHERE checkpoint_id=?1",[id],|r|r.get::<_,bool>(0)).unwrap_or(false) => {
                tx.execute("UPDATE working_memory_checkpoint_ref_backfill SET blocked_checkpoint_id=NULL,blocked_payload_bytes=NULL,last_error=NULL WHERE id=1",[]).map_err(|e|err(e.to_string()))?;
            }
            Some(_) => {
                tx.commit().map_err(|e| err(e.to_string()))?;
                return Err(err(format!(
                    "memory purge blocked by unindexable legacy working-memory checkpoint {id}"
                )));
            }
        }
    }
    let query="SELECT checkpoint_id,octet_length(state_json) FROM working_memory_checkpoints WHERE checkpoint_id>?1 ORDER BY checkpoint_id LIMIT ?2";
    let mut stmt = tx.prepare(&query).map_err(|e| err(e.to_string()))?;
    let mut rows = stmt
        .query(params![
            cursor,
            CHECKPOINT_REFERENCE_BACKFILL_ROWS_PER_PASS as i64
        ])
        .map_err(|e| err(e.to_string()))?;
    let mut count = 0usize;
    let mut bytes = 0usize;
    while let Some(row) = rows.next().map_err(|e| err(e.to_string()))? {
        let id: i64 = row.get(0).map_err(|e| err(e.to_string()))?;
        let size: usize = row.get(1).map_err(|e| err(e.to_string()))?;
        if size > MAX_LEGACY_CHECKPOINT_PAYLOAD_BYTES {
            drop(rows);
            drop(stmt);
            let msg=format!("legacy working-memory checkpoint {id} is {size} bytes; purge is blocked until remediated");
            tx.execute("UPDATE working_memory_checkpoint_ref_backfill SET blocked_checkpoint_id=?1,blocked_payload_bytes=?2,last_error=?3 WHERE id=1",params![id,size as i64,msg]).map_err(|e|err(e.to_string()))?;
            tx.commit().map_err(|e| err(e.to_string()))?;
            return Err(err(msg));
        }
        if count > 0 && bytes.saturating_add(size) > CHECKPOINT_REFERENCE_BACKFILL_BYTES_PER_PASS {
            break;
        }
        let json:Vec<u8>=tx.query_row("SELECT CAST(state_json AS BLOB) FROM working_memory_checkpoints WHERE checkpoint_id=?1",[id],|r|r.get(0)).map_err(|e|err(e.to_string()))?;
        let state: WorkingMemoryState = match serde_json::from_slice(&json) {
            Ok(v) => v,
            Err(e) => {
                drop(rows);
                drop(stmt);
                let msg = format!(
                    "legacy working-memory checkpoint {id} is malformed; purge is blocked: {e}"
                );
                tx.execute("UPDATE working_memory_checkpoint_ref_backfill SET blocked_checkpoint_id=?1,blocked_payload_bytes=?2,last_error=?3 WHERE id=1",params![id,size as i64,msg]).map_err(|e|err(e.to_string()))?;
                tx.commit().map_err(|e| err(e.to_string()))?;
                return Err(err(msg));
            }
        };
        let referenced = crate::working_memory::state::referenced_memory_ids(&state);
        if referenced.len() > MAX_CHECKPOINT_MEMORY_REFERENCES {
            drop(rows);
            drop(stmt);
            let msg=format!("legacy working-memory checkpoint {id} has {} memory references; purge is blocked until reduced below {MAX_CHECKPOINT_MEMORY_REFERENCES}",referenced.len());
            tx.execute("UPDATE working_memory_checkpoint_ref_backfill SET blocked_checkpoint_id=?1,blocked_payload_bytes=?2,last_error=?3 WHERE id=1",params![id,size as i64,msg]).map_err(|e|err(e.to_string()))?;
            tx.commit().map_err(|e| err(e.to_string()))?;
            return Err(err(msg));
        }
        tx.execute(
            "DELETE FROM working_memory_checkpoint_memory_refs WHERE checkpoint_id=?1",
            [id],
        )
        .map_err(|e| err(e.to_string()))?;
        for memory in referenced {
            tx.execute(
                "INSERT OR IGNORE INTO working_memory_checkpoint_memory_refs VALUES(?1,?2)",
                params![id, memory],
            )
            .map_err(|e| err(e.to_string()))?;
        }
        cursor = id;
        count += 1;
        bytes += size;
    }
    drop(rows);
    drop(stmt);
    let more: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM working_memory_checkpoints WHERE checkpoint_id>?1)",
            [cursor],
            |r| r.get(0),
        )
        .map_err(|e| err(e.to_string()))?;
    tx.execute("UPDATE working_memory_checkpoint_ref_backfill SET next_checkpoint_id=?1,complete=?2,last_error=NULL WHERE id=1",params![cursor,!more]).map_err(|e|err(e.to_string()))?;
    tx.commit().map_err(|e| err(e.to_string()))?;
    if more {
        Err(err(format!("memory purge blocked while working-memory checkpoint reference backfill advances ({count} rows, {bytes} bytes this pass)")))
    } else {
        Ok(())
    }
}
fn binding_ok(b: &DeliveryBinding<'_>, ids: &[String]) -> Result<(), LatticeError> {
    if b.delivery_id.is_empty()
        || b.delivery_id.len() > 256
        || b.repository_id.is_empty()
        || b.session_id.is_empty()
        || b.payload_hash.is_empty()
        || ids.is_empty()
        || ids.len() > 256
    {
        Err(err("invalid bounded delivery binding"))
    } else {
        Ok(())
    }
}

fn advance_receipt_count_backfill(c: &Connection) -> Result<bool, LatticeError> {
    (|| -> rusqlite::Result<bool> {
    let tx = rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
    let (cursor, complete): (String, bool) = tx.query_row(
            "SELECT cursor,complete FROM memory_receipt_count_state WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
    if complete {
        tx.commit()?;
        return Ok(true);
    }
    let ids=tx.prepare("SELECT memory_id FROM memory_deletion_receipts WHERE memory_id>?1 ORDER BY memory_id LIMIT 64")?.query_map([&cursor],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let next = ids.last().cloned().unwrap_or(cursor);
    let more: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM memory_deletion_receipts WHERE memory_id>?1)",
        [&next],
        |r| r.get(0),
    )?;
    tx.execute(
        "UPDATE memory_receipt_count_state SET cursor=?1,counted=counted+?2,complete=?3 WHERE id=1",
        params![next, ids.len(), !more],
    )?;
    tx.commit()?;
    Ok(!more)
    })().map_err(|e|err(format!("deletion-receipt count migration failed: {e}")))
}
pub fn attempt_delivery(
    c: &Connection,
    b: &DeliveryBinding<'_>,
    ids: &[String],
    now: u64,
) -> Result<(), LatticeError> {
    binding_ok(b, ids)?;
    let set: BTreeSet<_> = ids.iter().cloned().collect();
    let hash = set_hash(&set);
    sql((|| {
        let tx = rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR IGNORE INTO memory_deliveries VALUES(?1,?2,?3,?4,?5,?6,NULL)",
            params![
                b.delivery_id,
                b.repository_id,
                b.session_id,
                b.payload_hash,
                hash,
                now
            ],
        )?;
        let got:(String,String,String,String)=tx.query_row("SELECT repository_id,session_id,payload_hash,memory_set_hash FROM memory_deliveries WHERE delivery_id=?1",[b.delivery_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        if got
            != (
                b.repository_id.into(),
                b.session_id.into(),
                b.payload_hash.into(),
                hash,
            )
        {
            return Err(rusqlite::Error::InvalidQuery);
        }
        for id in set {
            tx.execute(
                "INSERT OR IGNORE INTO memory_delivery_items VALUES(?1,?2)",
                params![b.delivery_id, id],
            )?;
        }
        tx.commit()
    })())
}

pub(crate) fn attempt_delivery_in_transaction(
    tx: &rusqlite::Transaction<'_>,
    b: &DeliveryBinding<'_>,
    ids: &[String],
    now: u64,
) -> rusqlite::Result<()> {
    let set: BTreeSet<_> = ids.iter().cloned().collect();
    let hash = set_hash(&set);
    tx.execute(
        "INSERT OR IGNORE INTO memory_deliveries VALUES(?1,?2,?3,?4,?5,?6,NULL)",
        params![
            b.delivery_id,
            b.repository_id,
            b.session_id,
            b.payload_hash,
            hash,
            now
        ],
    )?;
    let got:(String,String,String,String)=tx.query_row("SELECT repository_id,session_id,payload_hash,memory_set_hash FROM memory_deliveries WHERE delivery_id=?1",[b.delivery_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    if got
        != (
            b.repository_id.into(),
            b.session_id.into(),
            b.payload_hash.into(),
            hash,
        )
    {
        return Err(rusqlite::Error::InvalidQuery);
    }
    for id in set {
        tx.execute(
            "INSERT OR IGNORE INTO memory_delivery_items VALUES(?1,?2)",
            params![b.delivery_id, id],
        )?;
    }
    Ok(())
}
pub fn acknowledge_delivery(
    c: &Connection,
    b: &DeliveryBinding<'_>,
    now: u64,
) -> Result<usize, LatticeError> {
    sql((|| {
        let tx = rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
        let row=tx.query_row("SELECT attempted_at,acknowledged_at FROM memory_deliveries WHERE delivery_id=?1 AND repository_id=?2 AND session_id=?3 AND payload_hash=?4",params![b.delivery_id,b.repository_id,b.session_id,b.payload_hash],|r|Ok((r.get::<_,u64>(0)?,r.get::<_,Option<u64>>(1)?))).optional()?;
        let Some((at, None)) = row else {
            tx.commit()?;
            return Ok(0);
        };
        if at > now || at < now.saturating_sub(MAX_REPLAY_AGE_SECS) {
            tx.commit()?;
            return Ok(0);
        }
        let ids = tx
            .prepare("SELECT memory_id FROM memory_delivery_items WHERE delivery_id=?1")?
            .query_map([b.delivery_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if ids.is_empty() {
            tx.commit()?;
            return Ok(0);
        }
        let mut n = 0;
        for id in ids {
            n+=tx.execute("UPDATE memories SET last_recalled_at=MAX(COALESCE(last_recalled_at,0),?2),retention_stale=0 WHERE id=?1 AND purge_pending=0",params![id,now])?
        }
        tx.execute(
            "UPDATE memory_deliveries SET acknowledged_at=?2 WHERE delivery_id=?1",
            params![b.delivery_id, now],
        )?;
        tx.commit()?;
        Ok(n)
    })())
}

/// True only for an acknowledgement that has already committed with this
/// exact binding.  Transports use this after a zero-row acknowledgement to
/// accept an idempotent retry while rejecting an unknown, mismatched, or
/// expired receipt.
pub fn acknowledgement_was_recorded(
    c: &Connection,
    b: &DeliveryBinding<'_>,
    now: u64,
) -> Result<bool, LatticeError> {
    sql(c.query_row(
            "SELECT acknowledged_at IS NOT NULL FROM memory_deliveries WHERE delivery_id=?1 AND repository_id=?2 AND session_id=?3 AND payload_hash=?4 AND attempted_at<=?5 AND attempted_at>=?6",
            params![b.delivery_id, b.repository_id, b.session_id, b.payload_hash, now, now.saturating_sub(MAX_REPLAY_AGE_SECS)],
            |row| row.get::<_, bool>(0),
        ).optional()).map(|value|value.unwrap_or(false))
}
pub fn sweep(c: &Connection, now: u64, p: &RetentionPolicy) -> Result<SweepReport, LatticeError> {
    p.validate()?;
    if !advance_receipt_count_backfill(c)? {
        return Err(err(
            "memory purge blocked while deletion-receipt count migration advances",
        ));
    }
    if let Err(error) = advance_checkpoint_reference_backfill(c) {
        let _ = c.execute(
            "UPDATE memory_retention_control SET last_attempt=?1,last_error=?2 WHERE id=1",
            params![now, error.to_string()],
        );
        return Err(error);
    }
    let references = crate::consolidation::proposal_references::advance_reference_backfill(c)?;
    if let Some(id) = references.blocked_proposal_id {
        let error = err(format!(
            "memory purge blocked by unindexable legacy consolidation proposal '{id}'"
        ));
        let _ = c.execute(
            "UPDATE memory_retention_control SET last_attempt=?1,last_error=?2 WHERE id=1",
            params![now, error.to_string()],
        );
        return Err(error);
    }
    if !references.complete {
        let error = err(format!("memory purge blocked while consolidation proposal reference backfill advances ({} rows, {} bytes this pass)",references.rows_processed,references.payload_bytes_processed));
        let _ = c.execute(
            "UPDATE memory_retention_control SET last_attempt=?1,last_error=?2 WHERE id=1",
            params![now, error.to_string()],
        );
        return Err(error);
    }
    let result = sql((|| {
        let tx = rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
        if tx.execute("UPDATE memory_retention_control SET next_sweep=?1,last_attempt=?2,last_error=NULL WHERE id=1 AND next_sweep<=?2",params![now.saturating_add(p.sweep_interval_secs),now])?==0{return Ok(SweepReport{skipped:true,..Default::default()})}
        let mut o = SweepReport::default();
        let attribution = super::attribution::prune_in_transaction(
            &tx,
            now,
            super::attribution::AttributionPrunePolicy {
                max_resolved_age_secs: p.receipt_retention_secs,
                max_pending_age_secs: p.receipt_retention_secs,
                max_metric_pending_age_secs: p.receipt_retention_secs.saturating_mul(2),
                max_resolved_retrievals: p
                    .max_receipts
                    .min(super::attribution::MAX_ATTRIBUTION_RETAINED),
                batch_limit: p
                    .batch_size
                    .min(super::attribution::MAX_ATTRIBUTION_PRUNE_BATCH),
            },
        )
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        o.attribution_retrievals_pruned = attribution.retrievals_pruned;
        o.attribution_expired_receipts_pruned = attribution.expired_receipts_pruned;
        o.attribution_metric_dead_letters = attribution.metric_dead_letters;
        let stale_cursor: String = tx.query_row(
            "SELECT stale_scan_cursor FROM memory_retention_control WHERE id=1",
            [],
            |r| r.get(0),
        )?;
        let stale_page = tx
            .prepare("SELECT id FROM memories WHERE id>?1 ORDER BY id LIMIT ?2")?
            .query_map(params![stale_cursor, p.batch_size], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in &stale_page {
            o.stale+=tx.execute("UPDATE memories SET retention_stale=1 WHERE id=?1 AND retention_stale=0 AND retention_grace_until<=?2 AND COALESCE(last_recalled_at,created_at)<=?3",params![id,now,now.saturating_sub(p.stale_after_secs)])?;
        }
        let stale_more = stale_page.last().is_some_and(|last| {
            tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM memories WHERE id>?1)",
                [last],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false)
        });
        tx.execute(
            "UPDATE memory_retention_control SET stale_scan_cursor=?1 WHERE id=1",
            [if stale_more {
                stale_page.last().map(String::as_str).unwrap_or("")
            } else {
                ""
            }],
        )?;
        let purge_cursor: String = tx.query_row(
            "SELECT purge_scan_cursor FROM memory_retention_control WHERE id=1",
            [],
            |r| r.get(0),
        )?;
        let mut ids=tx.prepare("SELECT id,length(CAST(content AS BLOB))FROM memories WHERE purge_pending=1 ORDER BY id LIMIT ?1")?.query_map([p.batch_size],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        if ids.len() < p.batch_size {
            let remaining = p.batch_size - ids.len();
            ids.extend(tx.prepare("SELECT id,length(CAST(content AS BLOB))FROM memories WHERE purge_pending=0 AND id>?1 ORDER BY id LIMIT ?2")?.query_map(params![purge_cursor,remaining],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        let purge_more = ids.last().is_some_and(|(last, _)| {
            tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM memories WHERE id>?1)",
                [last],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false)
        });
        let purge_next = ids.last().map(|v| v.0.clone()).unwrap_or_default();
        for (id, b) in ids {
            let eligible:bool=tx.query_row("SELECT purge_pending=1 OR (retention_stale=1 AND retention_grace_until<=?2 AND COALESCE(last_recalled_at,created_at)<=?3) FROM memories WHERE id=?1",params![id,now,now.saturating_sub(p.purge_after_secs)],|r|r.get(0))?;
            if !eligible {
                continue;
            }
            let pending: bool = tx.query_row(
                "SELECT purge_pending FROM memories WHERE id=?1",
                [&id],
                |r| r.get(0),
            )?;
            if !pending {
                tx.execute(
                    "UPDATE memories SET purge_pending=1,is_invalidated=1 WHERE id=?1",
                    [&id],
                )?;
                tx.execute(
                    "INSERT INTO memory_deletion_receipts(memory_id,deleted_at)VALUES(?1,?2) ON CONFLICT(memory_id) DO UPDATE SET deleted_at=excluded.deleted_at",
                    params![id, now],
                )?;
                tx.execute("UPDATE memory_retention_control SET restore_floor=MAX(restore_floor,?1) WHERE id=1",[now])?;
            }
            tx.execute("DELETE FROM working_memory_checkpoints WHERE checkpoint_id IN(SELECT checkpoint_id FROM working_memory_checkpoint_memory_refs WHERE memory_id=?1 ORDER BY checkpoint_id LIMIT ?2)",params![id,RETENTION_DEPENDENCY_DELETE_BATCH])?;
            tx.execute("DELETE FROM consolidation_proposals WHERE proposal_id IN(SELECT proposal_id FROM consolidation_proposal_memory_refs INDEXED BY idx_consolidation_proposal_memory_refs_memory WHERE memory_id=?1 ORDER BY proposal_id LIMIT ?2)",params![id,RETENTION_DEPENDENCY_DELETE_BATCH])?;
            for (table, predicate) in [
                ("memory_evidence", "memory_id=?1"),
                ("memory_links", "source_memory_id=?1 OR target_memory_id=?1"),
                ("memory_accesses", "memory_id=?1"),
                ("memory_attribution_accesses", "memory_id=?1"),
                ("memory_scores", "memory_id=?1"),
                ("memory_delivery_items", "memory_id=?1"),
                ("trusted_check_observations", "memory_id=?1"),
                ("memory_retrieval_paths", "memory_id=?1"),
                ("memory_retrieval_symbols", "memory_id=?1"),
                ("memory_retrieval_docs", "memory_id=?1"),
                ("memory_retrieval_failures", "memory_id=?1"),
                ("verification_jobs", "target_memory_id=?1"),
                ("session_digest_capture_commits", "memory_id=?1"),
                ("memory_scope_filter_events", "memory_id=?1"),
            ] {
                tx.execute(&format!("DELETE FROM {table} WHERE rowid IN(SELECT rowid FROM {table} WHERE {predicate} ORDER BY rowid LIMIT ?2)"),params![id,RETENTION_DEPENDENCY_DELETE_BATCH])?;
            }
            let dependencies_remain:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM working_memory_checkpoint_memory_refs WHERE memory_id=?1) OR EXISTS(SELECT 1 FROM consolidation_proposal_memory_refs INDEXED BY idx_consolidation_proposal_memory_refs_memory WHERE memory_id=?1) OR EXISTS(SELECT 1 FROM memory_evidence WHERE memory_id=?1) OR EXISTS(SELECT 1 FROM memory_links WHERE source_memory_id=?1 OR target_memory_id=?1) OR EXISTS(SELECT 1 FROM memory_accesses WHERE memory_id=?1) OR EXISTS(SELECT 1 FROM memory_attribution_accesses WHERE memory_id=?1) OR EXISTS(SELECT 1 FROM memory_scores WHERE memory_id=?1) OR EXISTS(SELECT 1 FROM memory_delivery_items WHERE memory_id=?1) OR EXISTS(SELECT 1 FROM trusted_check_observations WHERE memory_id=?1) OR EXISTS(SELECT 1 FROM memory_retrieval_paths WHERE memory_id=?1) OR EXISTS(SELECT 1 FROM memory_retrieval_symbols WHERE memory_id=?1) OR EXISTS(SELECT 1 FROM memory_retrieval_docs WHERE memory_id=?1) OR EXISTS(SELECT 1 FROM memory_retrieval_failures WHERE memory_id=?1) OR EXISTS(SELECT 1 FROM verification_jobs WHERE target_memory_id=?1) OR EXISTS(SELECT 1 FROM session_digest_capture_commits WHERE memory_id=?1) OR EXISTS(SELECT 1 FROM memory_scope_filter_events WHERE memory_id=?1)",[&id],|r|r.get(0))?;
            if dependencies_remain {
                continue;
            }
            tx.execute("DELETE FROM memories_fts WHERE memory_id=?1", [&id])?;
            tx.execute("DELETE FROM memories WHERE id=?1", [&id])?;
            o.purged += 1;
            o.logical_payload_bytes_purged += b
        }
        tx.execute(
            "UPDATE memory_retention_control SET purge_scan_cursor=?1 WHERE id=1",
            [if purge_more { purge_next.as_str() } else { "" }],
        )?;
        tx.execute("DELETE FROM memory_deliveries WHERE delivery_id IN(SELECT delivery_id FROM memory_deliveries WHERE attempted_at<?1 ORDER BY attempted_at,delivery_id LIMIT ?2)",params![now.saturating_sub(MAX_REPLAY_AGE_SECS),p.batch_size])?;
        o.receipts_expired=tx.execute("DELETE FROM memory_deletion_receipts WHERE memory_id IN(SELECT r.memory_id FROM memory_deletion_receipts r LEFT JOIN memories m ON m.id=r.memory_id AND m.purge_pending=1 WHERE r.deleted_at<?1 AND m.id IS NULL ORDER BY r.deleted_at,r.memory_id LIMIT ?2)",params![now.saturating_sub(p.receipt_retention_secs),p.batch_size])?;
        let receipt_count: u64 = tx.query_row(
            "SELECT counted FROM memory_receipt_count_state WHERE id=1 AND complete=1",
            [],
            |r| r.get(0),
        )?;
        if receipt_count > p.max_receipts as u64 {
            o.receipts_expired+=tx.execute("DELETE FROM memory_deletion_receipts WHERE memory_id IN(SELECT r.memory_id FROM memory_deletion_receipts r LEFT JOIN memories m ON m.id=r.memory_id AND m.purge_pending=1 WHERE m.id IS NULL ORDER BY r.deleted_at,r.memory_id LIMIT ?1)",[usize::min(p.batch_size,(receipt_count-p.max_receipts as u64) as usize)])?;
        }
        let purge_pending: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM memories WHERE purge_pending=1)",
            [],
            |r| r.get(0),
        )?;
        if stale_more || purge_more || purge_pending {
            tx.execute(
                "UPDATE memory_retention_control SET next_sweep=?1 WHERE id=1",
                [now],
            )?;
        }
        tx.execute("UPDATE memory_retention_control SET last_success=?1,logical_payload_bytes_purged=logical_payload_bytes_purged+?2,restore_floor=CASE WHEN ?3>0 THEN MAX(restore_floor,?1)ELSE restore_floor END WHERE id=1",params![now,o.logical_payload_bytes_purged,o.purged])?;
        tx.commit()?;
        Ok(o)
    })());
    if let Err(error) = &result {
        let _ = c.execute(
            "UPDATE memory_retention_control SET last_attempt=?1,last_error=?2 WHERE id=1",
            params![now, error.to_string()],
        );
    }
    result
}

pub fn maintenance_needs_continuation(c: &Connection) -> Result<bool, LatticeError> {
    sql(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM consolidation_proposal_ref_backfill WHERE complete=0 AND blocked_proposal_id IS NULL) OR EXISTS(SELECT 1 FROM working_memory_checkpoint_ref_backfill WHERE complete=0 AND blocked_checkpoint_id IS NULL) OR EXISTS(SELECT 1 FROM memory_receipt_count_state WHERE complete=0) OR EXISTS(SELECT 1 FROM memories WHERE purge_pending=1) OR EXISTS(SELECT 1 FROM memory_retention_control WHERE stale_scan_cursor<>'' OR purge_scan_cursor<>'')",
        [],
        |r| r.get(0),
    ))
}
pub fn health(
    c: &Connection,
    now: u64,
    p: &RetentionPolicy,
) -> Result<RetentionHealth, LatticeError> {
    p.validate()?;
    sql((|| {
        let(a,s,d,next)=c.query_row("SELECT COUNT(*)FILTER(WHERE retention_stale=0),COUNT(*)FILTER(WHERE retention_stale=1),COUNT(*)FILTER(WHERE retention_grace_until<=?1 AND COALESCE(last_recalled_at,created_at)<=?2),MIN(MAX(retention_grace_until,COALESCE(last_recalled_at,created_at)+CASE WHEN retention_stale=0 THEN ?3 ELSE ?4 END))FROM memories",params![now,now.saturating_sub(p.purge_after_secs),p.stale_after_secs,p.purge_after_secs],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        let(at,ok,e,b,physical)=c.query_row("SELECT last_attempt,last_success,last_error,logical_payload_bytes_purged,physical_bytes_reclaimed FROM memory_retention_control WHERE id=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        let (receipts, receipt_count_complete): (u64, bool) = c.query_row(
            "SELECT counted,complete FROM memory_receipt_count_state WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(RetentionHealth {
            active: a,
            retention_stale: s,
            purge_due: d,
            receipts,
            receipts_complete: receipt_count_complete,
            last_attempted_sweep: at,
            last_successful_sweep: ok,
            last_error: e,
            logical_payload_bytes_purged: b,
            physical_bytes_reclaimed: physical,
            reclamation_pending: b > physical || !receipt_count_complete,
            next_deadline: next,
        })
    })())
}
pub fn validate_restore_time(c: &Connection, t: u64) -> Result<(), LatticeError> {
    let f: u64 = sql(c.query_row(
        "SELECT restore_floor FROM memory_retention_control WHERE id=1",
        [],
        |r| r.get(0),
    ))?;
    if t < f {
        Err(err(format!(
            "snapshot predates memory purge restore floor {f}"
        )))
    } else {
        Ok(())
    }
}
fn has_column(c: &Connection, t: &str, n: &str) -> rusqlite::Result<bool> {
    c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1)WHERE name=?2)",
        params![t, n],
        |r| r.get(0),
    )
}
fn err(s: impl Into<String>) -> LatticeError {
    LatticeError::Storage(s.into())
}
fn sql<T>(r: rusqlite::Result<T>) -> Result<T, LatticeError> {
    r.map_err(|e| err(format!("memory retention transaction failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;
    fn fixture() -> MemoryStore {
        let s = MemoryStore::open_in_memory().unwrap();
        s.with_connection(|c| { c.execute("INSERT INTO memories(id,content,memory_type,created_at,last_accessed,retention_grace_until)VALUES('m','payload','fact',100,100,0)",[]).unwrap(); Ok(()) }).unwrap();
        s
    }
    fn policy() -> RetentionPolicy {
        RetentionPolicy {
            stale_after_secs: 90,
            purge_after_secs: 180,
            sweep_interval_secs: 1,
            receipt_retention_secs: MAX_REPLAY_AGE_SECS,
            max_receipts: 2,
            batch_size: 10,
        }
    }
    fn binding<'a>(id: &'a str) -> DeliveryBinding<'a> {
        DeliveryBinding {
            delivery_id: id,
            repository_id: "repo",
            session_id: "session",
            payload_hash: "hash",
        }
    }
    #[test]
    fn age_boundaries_are_inclusive_and_attempt_is_not_recall() {
        let s = fixture();
        s.with_connection(|c| {
            attempt_delivery(c, &binding("d"), &["m".into()], 190)?;
            assert_eq!(sweep(c, 189, &policy())?.stale, 0);
            assert_eq!(sweep(c, 190, &policy())?.stale, 1);
            assert_eq!(sweep(c, 279, &policy())?.purged, 0);
            assert_eq!(sweep(c, 280, &policy())?.purged, 1);
            Ok(())
        })
        .unwrap()
    }
    #[test]
    fn acknowledgement_is_exact_bound_and_preserves_trust() {
        let s = fixture();
        s.with_connection(|c| {
            c.execute(
                "UPDATE memories SET is_stale=1,verification_status='stale' WHERE id='m'",
                [],
            )
            .unwrap();
            attempt_delivery(c, &binding("d"), &["m".into()], 190)?;
            let wrong = DeliveryBinding {
                session_id: "other",
                ..binding("d")
            };
            assert_eq!(acknowledge_delivery(c, &wrong, 191)?, 0);
            assert_eq!(acknowledge_delivery(c, &binding("d"), 191)?, 1);
            let r: (i64, String) = c
                .query_row(
                    "SELECT is_stale,verification_status FROM memories WHERE id='m'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(r, (1, "stale".into()));
            Ok(())
        })
        .unwrap()
    }
    #[test]
    fn rejected_and_replayed_receipts_do_not_renew_twice() {
        let s = fixture();
        s.with_connection(|c| {
            attempt_delivery(c, &binding("d"), &["m".into()], 100)?;
            let wrong_hash = DeliveryBinding {
                payload_hash: "other",
                ..binding("d")
            };
            assert_eq!(acknowledge_delivery(c, &wrong_hash, 101)?, 0);
            assert!(!acknowledgement_was_recorded(c, &wrong_hash, 101)?);
            assert_eq!(acknowledge_delivery(c, &binding("d"), 101)?, 1);
            assert!(acknowledgement_was_recorded(c, &binding("d"), 101)?);
            assert_eq!(acknowledge_delivery(c, &binding("d"), 102)?, 0);
            let expired = DeliveryBinding {
                delivery_id: "expired",
                ..binding("d")
            };
            attempt_delivery(c, &expired, &["m".into()], 100)?;
            assert_eq!(
                acknowledge_delivery(c, &expired, 100 + MAX_REPLAY_AGE_SECS + 1)?,
                0
            );
            assert!(!acknowledgement_was_recorded(
                c,
                &expired,
                100 + MAX_REPLAY_AGE_SECS + 1
            )?);
            let recalled: u64 = c
                .query_row(
                    "SELECT last_recalled_at FROM memories WHERE id='m'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(recalled, 101);
            Ok(())
        })
        .unwrap();
    }
    #[test]
    fn receipt_expiry_keeps_restore_floor() {
        let s = fixture();
        let mut p = policy();
        p.receipt_retention_secs = MAX_REPLAY_AGE_SECS;
        s.with_connection(|c| {
            sweep(c, 280, &p)?;
            assert!(validate_restore_time(c, 279).is_err());
            sweep(c, 280 + MAX_REPLAY_AGE_SECS + 1, &p)?;
            assert_eq!(health(c, 999, &p)?.receipts, 0);
            assert!(validate_restore_time(c, 279).is_err());
            Ok(())
        })
        .unwrap()
    }
    #[test]
    fn purge_removes_only_dependent_checkpoint_and_owned_evidence() {
        let s = fixture();
        s.with_connection(|c| {
            c.execute("INSERT INTO memories(id,content,memory_type,created_at,last_accessed,retention_grace_until)VALUES('keep','other','fact',200,200,0)",[]).unwrap();
            c.execute("INSERT INTO memory_evidence(evidence_id,memory_id,kind,reference)VALUES('e1','m','file','shared'),('e2','keep','file','shared')",[]).unwrap();
            c.execute("INSERT INTO consolidation_jobs(job_id,workspace_id,kind,mode,status,enqueued_at)VALUES('j','w','refresh','manual_review','proposed',1)",[]).unwrap();
            c.execute("INSERT INTO consolidation_proposals(proposal_id,job_id,proposal_kind,prior_state,proposed_state,evidence,decision)VALUES('drop','j','refresh','{\"memory_id\":\"m\"}','{}','[]','pending'),('retain','j','refresh','{\"memory_id\":\"keep\"}','{}','[]','pending')",[]).unwrap();
            for (id, memory) in [(1,"m"),(2,"keep")] { c.execute("INSERT INTO working_memory_checkpoints(checkpoint_id,workspace_id,session_id,task_id,checkpoint_name,created_at,state_version,state_json,state_hash)VALUES(?1,'w','s','t','c',1,1,'{}','h')",[id]).unwrap(); c.execute("INSERT INTO working_memory_checkpoint_memory_refs VALUES(?1,?2)",params![id,memory]).unwrap(); }
            sweep(c,280,&policy())?;
            assert_eq!(c.query_row::<i64,_,_>("SELECT COUNT(*) FROM working_memory_checkpoints WHERE checkpoint_id=1",[],|r|r.get(0)).unwrap(),0);
            assert_eq!(c.query_row::<i64,_,_>("SELECT COUNT(*) FROM working_memory_checkpoints WHERE checkpoint_id=2",[],|r|r.get(0)).unwrap(),1);
            assert_eq!(c.query_row::<i64,_,_>("SELECT COUNT(*) FROM memory_evidence WHERE evidence_id='e2'",[],|r|r.get(0)).unwrap(),1);
            assert_eq!(c.query_row::<i64,_,_>("SELECT COUNT(*) FROM consolidation_proposals WHERE proposal_id='drop'",[],|r|r.get(0)).unwrap(),0);
            assert_eq!(c.query_row::<i64,_,_>("SELECT COUNT(*) FROM consolidation_proposals WHERE proposal_id='retain'",[],|r|r.get(0)).unwrap(),1);
            Ok(())
        }).unwrap()
    }
    #[test]
    fn rollback_keeps_memory_and_retry_survives_reopen_contract() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.db");
        let s = MemoryStore::open(&path).unwrap();
        s.with_connection(|c| { c.execute("INSERT INTO memories(id,content,memory_type,created_at,last_accessed,retention_grace_until)VALUES('m','payload','fact',100,100,0)",[]).unwrap(); c.execute_batch("CREATE TRIGGER fail_retention BEFORE DELETE ON memories BEGIN SELECT RAISE(ABORT,'injected');END;").unwrap(); assert!(sweep(c,280,&policy()).is_err()); assert_eq!(c.query_row::<i64,_,_>("SELECT COUNT(*)FROM memories WHERE id='m'",[],|r|r.get(0)).unwrap(),1); assert_eq!(c.query_row::<i64,_,_>("SELECT COUNT(*)FROM memory_deletion_receipts",[],|r|r.get(0)).unwrap(),0); c.execute_batch("DROP TRIGGER fail_retention").unwrap(); assert_eq!(sweep(c,280,&policy())?.purged,1); Ok(()) }).unwrap();
        drop(s);
        let reopened = MemoryStore::open(&path).unwrap();
        reopened
            .with_connection(|c| {
                assert_eq!(
                    c.query_row::<i64, _, _>(
                        "SELECT COUNT(*)FROM memory_deletion_receipts WHERE memory_id='m'",
                        [],
                        |r| r.get(0)
                    )
                    .unwrap(),
                    1
                );
                assert!(c
                    .execute(
                        "INSERT INTO memories(id,content,memory_type)VALUES('m','replay','fact')",
                        []
                    )
                    .is_err());
                Ok(())
            })
            .unwrap();
    }
    #[test]
    fn twenty_store_callers_admit_one_sweep() {
        use std::sync::{Arc, Barrier};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.db");
        let seed = MemoryStore::open(&path).unwrap();
        seed.with_connection(|c|{c.execute("INSERT INTO memories(id,content,memory_type,created_at,last_accessed,retention_grace_until)VALUES('m','payload','fact',100,100,0)",[]).unwrap();Ok(())}).unwrap();
        drop(seed);
        let stores = (0..20)
            .map(|_| MemoryStore::open(&path).unwrap())
            .collect::<Vec<_>>();
        let barrier = Arc::new(Barrier::new(20));
        let handles = stores
            .into_iter()
            .map(|store| {
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    store.with_connection(|c| sweep(c, 190, &policy())).unwrap()
                })
            })
            .collect::<Vec<_>>();
        let reports = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(reports.iter().filter(|r| !r.skipped).count(), 1);
        assert_eq!(reports.iter().map(|r| r.stale).sum::<usize>(), 1);
    }
    #[test]
    fn recall_then_purge_is_serial_and_deterministic() {
        let s = fixture();
        s.with_connection(|c| {
            attempt_delivery(c, &binding("race"), &["m".into()], 279)?;
            assert_eq!(acknowledge_delivery(c, &binding("race"), 279)?, 1);
            assert_eq!(sweep(c, 280, &policy())?.purged, 0);
            assert_eq!(sweep(c, 459, &policy())?.purged, 1);
            Ok(())
        })
        .unwrap()
    }
    #[test]
    fn physical_reclamation_is_bounded_and_measured() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.db");
        let s = MemoryStore::open(&path).unwrap();
        s.with_connection(|c| { assert!(reclaim_free_pages(c,0).is_err()); c.execute_batch("CREATE TABLE reclaim_fixture(body BLOB); INSERT INTO reclaim_fixture VALUES(zeroblob(1048576)); DELETE FROM reclaim_fixture;").unwrap(); let page_size:u64=c.query_row("PRAGMA page_size",[],|r|r.get(0)).unwrap(); let report=reclaim_free_pages(c,16)?; assert!(report.pages_reclaimed<=16); assert_eq!(report.bytes_reclaimed,report.pages_reclaimed*page_size); let h=health(c,1,&policy())?; assert_eq!(h.physical_bytes_reclaimed,report.bytes_reclaimed); Ok(()) }).unwrap()
    }
}
