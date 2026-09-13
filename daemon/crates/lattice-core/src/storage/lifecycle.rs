//! Repository-owned registry, checkout leases, and journaled derived-cache GC.
//! Knowledge, events, snapshots, and unknown artifacts are never GC candidates.
use super::managed_fs::SecureDir;
use super::managed_sqlite::ManagedSqlite;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;

const DERIVED_FILES: &[&str] = &[
    "graph.db",
    "graph.db-wal",
    "graph.db-shm",
    "vectors.db",
    "vectors.db-wal",
    "vectors.db-shm",
    "vectors.usearch",
    "vectors.usearch.meta.json",
    "context_handles.json",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachePolicy {
    pub high_bytes: u64,
    pub low_bytes: u64,
    pub idle_grace_secs: u64,
    pub batch_files: usize,
}
impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            high_bytes: 2 * 1024 * 1024 * 1024,
            low_bytes: 1536 * 1024 * 1024,
            idle_grace_secs: 86400,
            batch_files: 64,
        }
    }
}
impl CachePolicy {
    pub fn validate(&self) -> Result<()> {
        if self.low_bytes >= self.high_bytes || self.batch_files == 0 || self.batch_files > 4096 {
            bail!("cache policy requires low < high and 1..4096 files per batch");
        }
        Ok(())
    }
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageInventory {
    /// True only for a fully published traversal of one stable registry generation.
    pub accounting_complete: bool,
    /// True while totals are absent, stale, invalidated, or being rebuilt.
    pub pressure_unknown: bool,
    pub observed_at: u64,
    pub shared_cache_logical_bytes: u64,
    pub shared_cache_allocated_bytes: u64,
    pub shared_cache_wal_bytes: u64,
    pub shared_accounting_complete: bool,
    pub derived_logical_bytes: u64,
    pub derived_allocated_bytes: u64,
    pub wal_bytes: u64,
    pub telemetry_logical_bytes: u64,
    pub telemetry_allocated_bytes: u64,
    pub telemetry_wal_bytes: u64,
    pub active_checkouts: usize,
    pub retained_checkouts: usize,
    pub reclaimable_bytes: u64,
    pub unknown_artifacts: Vec<String>,
    pub unknown_artifacts_truncated: bool,
    /// Derived files written by releases predating the cache-bundle layout.
    /// They are reported but never moved or deleted while the daemon is live.
    pub historical_derived_artifacts: Vec<String>,
    pub historical_derived_artifacts_truncated: bool,
    pub historical_derived_allocated_bytes: u64,
}

pub struct StorageRegistry {
    root: PathBuf,
    managed: std::sync::Arc<SecureDir>,
    conn: ManagedSqlite,
}
pub struct CheckoutLease {
    _lock: File,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcCandidate {
    pub checkout_id: String,
    pub bytes: u64,
}
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct GcReport {
    pub moved_files: usize,
    pub deleted_files: usize,
    pub released_bytes: u64,
    pub skipped_active: usize,
}

impl StorageRegistry {
    pub fn repository_id(&self) -> Result<String> {
        Ok(self.conn.query_row(
            "SELECT repository_id FROM repository_home WHERE id=1",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn repository_home(&self) -> &Path {
        &self.root
    }

    /// Retain this registry's pinned authority for operator access.
    pub fn operator(&self) -> Result<super::StorageOperator> {
        super::StorageOperator::from_pinned_home(self.managed.clone(), &self.repository_id()?)
    }

    /// Open an initialized registry without creating directories, files, or
    /// schema. Intended for operator inventory and dry-run planning.
    pub fn inspect(root: &Path, repository_id: &str) -> Result<Self> {
        let metadata =
            fs::symlink_metadata(root).context("repository storage home is unavailable")?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("repository storage home must be an existing non-symlink directory");
        }
        let root = root.canonicalize()?;
        let db = root.join("storage-registry.db");
        reject_symlink(&db)?;
        let managed = std::sync::Arc::new(SecureDir::open(&root)?);
        Self::inspect_in(managed, repository_id, false)
    }

    pub(crate) fn inspect_in(
        managed: std::sync::Arc<SecureDir>,
        repository_id: &str,
        writable: bool,
    ) -> Result<Self> {
        let root = managed.path().to_path_buf();
        let conn = ManagedSqlite::open(
            &managed,
            "storage-registry.db",
            (if writable {
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            } else {
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            }) | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        if writable {
            initialize_inventory_schema(&conn)?;
        }
        let (owner, home): (String, String) = conn.query_row(
            "SELECT repository_id,canonical_home FROM repository_home WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if owner != repository_id || home != root.to_string_lossy() {
            bail!("storage home ownership mismatch; explicit registry relocation required");
        }
        Ok(Self {
            root,
            managed,
            conn,
        })
    }

    pub fn open(root: &Path, repository_id: &str) -> Result<Self> {
        reject_symlink(root)?;
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        let managed = std::sync::Arc::new(SecureDir::open(&root)?);
        for child in ["leases", "trash", "checkouts"] {
            managed.create_dir(child)?;
        }
        let db = root.join("storage-registry.db");
        reject_symlink(&db)?;
        let conn = ManagedSqlite::open(
            &managed,
            "storage-registry.db",
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS repository_home(id INTEGER PRIMARY KEY CHECK(id=1), repository_id TEXT NOT NULL, canonical_home TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS checkout_registry(checkout_id TEXT PRIMARY KEY, root TEXT NOT NULL, last_seen INTEGER NOT NULL, generation INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS cache_gc_journal(id INTEGER PRIMARY KEY, checkout_id TEXT NOT NULL, file TEXT NOT NULL, trash_name TEXT NOT NULL UNIQUE, state TEXT NOT NULL CHECK(state IN ('planned','moved','deleted')), bytes INTEGER NOT NULL);")?;
        initialize_gc_identity_schema(&conn)?;
        initialize_inventory_schema(&conn)?;
        conn.execute(
            "INSERT OR IGNORE INTO repository_home VALUES (1,?1,?2)",
            params![repository_id, root.to_string_lossy()],
        )?;
        let (owner, home): (String, String) = conn.query_row(
            "SELECT repository_id,canonical_home FROM repository_home WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if owner != repository_id || home != root.to_string_lossy() {
            bail!("storage home ownership mismatch; explicit registry relocation required");
        }
        Ok(Self {
            root,
            managed,
            conn,
        })
    }

    pub fn register_and_lease(
        &mut self,
        checkout_id: &str,
        checkout_root: &Path,
        now: u64,
    ) -> Result<CheckoutLease> {
        validate_id(checkout_id)?;
        let _maintenance = descriptor_lock(&self.managed, "maintenance.lock", false)?
            .context("repository maintenance is in progress; retry registration")?;
        let canonical = checkout_root
            .canonicalize()
            .context("checkout root is unavailable; registration unchanged")?;
        let lock = self
            .checkout_lock(checkout_id, false)?
            .context("checkout cache maintenance in progress; retry registration")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT root FROM checkout_registry WHERE checkout_id=?1",
                [checkout_id],
                |r| r.get(0),
            )
            .optional()?;
        if previous
            .as_deref()
            .is_some_and(|root| root != canonical.to_string_lossy())
        {
            bail!("checkout identity already belongs to another canonical root");
        }
        tx.execute("INSERT INTO checkout_registry(checkout_id,root,last_seen) VALUES (?1,?2,?3) ON CONFLICT(checkout_id) DO UPDATE SET last_seen=excluded.last_seen", params![checkout_id, canonical.to_string_lossy(), now])?;
        tx.commit()?;
        let checkouts = self.managed.open_dir("checkouts")?;
        let checkout = checkouts.create_dir(checkout_id)?;
        checkout.create_dir("cache")?;
        Ok(CheckoutLease { _lock: lock })
    }

    pub fn heartbeat(&self, checkout_id: &str, now: u64, generation: u64) -> Result<()> {
        let changed = self.conn.execute("UPDATE checkout_registry SET last_seen=MAX(last_seen,?2), generation=MAX(generation,?3) WHERE checkout_id=?1", params![checkout_id, now, generation])?;
        if changed != 1 {
            bail!("checkout heartbeat has no registered owner");
        }
        Ok(())
    }

    /// Reconcile graph manifests after checkout eviction and reclaim a bounded
    /// number of repository-shared symbol bodies no committed graph references.
    pub fn collect_parsed_objects(&self, limit: usize) -> Result<usize> {
        if self.managed.metadata("parsed-cache.db")?.is_none() {
            return Ok(0);
        }
        let cache = super::ParsedFileCache::open_in(&self.managed, "parsed-cache.db")?;
        cache.retire_commit_manifests(limit, limit)?;
        Ok(cache.gc_unreferenced(limit)?)
    }

    pub fn collect_content_objects(&self, limit: usize) -> Result<super::ObjectGcReport> {
        let objects = super::ContentObjectStore::open_in(std::sync::Arc::new(
            self.managed.create_dir("symbol-bodies")?,
        ))?;
        Ok(objects.collect_garbage(limit)?)
    }

    pub fn collect_embedding_objects(
        &self,
        max_bytes: u64,
        limit: usize,
    ) -> Result<crate::embeddings::EmbeddingGcReport> {
        Ok(
            crate::embeddings::EmbeddingObjectCache::open_in(std::sync::Arc::new(
                self.managed.create_dir("embedding-objects")?,
            ))?
            .collect_garbage(max_bytes, limit)?,
        )
    }

    pub fn inventory(&self, now: u64, policy: &CachePolicy) -> Result<StorageInventory> {
        policy.validate()?;
        let has_accounting: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='storage_inventory_control')", [], |row| row.get(0))?;
        if !has_accounting {
            return Ok(StorageInventory {
                pressure_unknown: true,
                ..Default::default()
            });
        }
        let row: Option<(String, u64, i64)> = self.conn.query_row(
            "SELECT published_json,published_at,invalidated FROM storage_inventory_control WHERE id=1",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        let Some((json, observed_at, invalidated)) = row else {
            return Ok(StorageInventory {
                pressure_unknown: true,
                ..Default::default()
            });
        };
        let mut inventory: StorageInventory = serde_json::from_str(&json)?;
        inventory.observed_at = observed_at;
        inventory.accounting_complete = invalidated == 0;
        inventory.pressure_unknown |= invalidated != 0 || now.saturating_sub(observed_at) > 60;
        self.add_shared_accounting(&mut inventory)?;
        Ok(inventory)
    }

    /// Advance at most `page_limit` checkout records and publish totals only
    /// after one stable registry generation has been traversed.
    pub fn advance_inventory(
        &self,
        now: u64,
        policy: &CachePolicy,
        page_limit: usize,
    ) -> Result<StorageInventory> {
        policy.validate()?;
        if page_limit == 0 || page_limit > 4096 {
            bail!("inventory page limit must be in 1..=4096");
        }
        initialize_inventory_schema(&self.conn)?;
        let inventory_locks = self.managed.open_dir("leases")?;
        let _inventory_lock = descriptor_lock(&inventory_locks, "inventory.lock", true)?
            .context("repository inventory is already advancing")?;
        let (generation, cycle_generation, cursor, working): (i64,i64,String,String) = self.conn.query_row(
            "SELECT generation,cycle_generation,cursor,working_json FROM storage_inventory_control WHERE id=1", [],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        let mut aggregate: StorageInventory = if generation == cycle_generation {
            serde_json::from_str(&working)?
        } else {
            StorageInventory::default()
        };
        let after = if generation == cycle_generation {
            cursor
        } else {
            self.conn.execute("UPDATE storage_inventory_control SET cycle_generation=?1,cursor='',working_json=?2,invalidated=1 WHERE id=1 AND generation=?1", params![generation,serde_json::to_string(&StorageInventory::default())?])?;
            String::new()
        };
        let (page, last, more) = self.scan_inventory_page(now, policy, &after, page_limit)?;
        merge_inventory(&mut aggregate, page);
        let current_generation: i64 = self.conn.query_row(
            "SELECT generation FROM storage_inventory_control WHERE id=1",
            [],
            |r| r.get(0),
        )?;
        if current_generation != generation {
            let changed=self.conn.execute("UPDATE storage_inventory_control SET cycle_generation=?1,cursor='',working_json=?2,invalidated=1 WHERE id=1 AND generation=?1", params![current_generation, serde_json::to_string(&StorageInventory::default())?])?;
            if changed != 1 {
                return self.inventory(now, policy);
            }
            return self.inventory(now, policy);
        }
        if more {
            let changed=self.conn.execute("UPDATE storage_inventory_control SET cycle_generation=?1,cursor=?2,working_json=?3,invalidated=1 WHERE id=1 AND generation=?1", params![generation,last,serde_json::to_string(&aggregate)?])?;
            if changed != 1 {
                return self.inventory(now, policy);
            }
            return self.inventory(now, policy);
        }
        aggregate.accounting_complete = true;
        aggregate.pressure_unknown =
            aggregate.unknown_artifacts_truncated || !aggregate.unknown_artifacts.is_empty();
        aggregate.observed_at = now;
        // Shared object counters are read fresh by status, independent of a
        // checkout scan's duration. Persist only the checkout aggregate.
        let encoded = serde_json::to_string(&aggregate)?;
        let changed=self.conn.execute("UPDATE storage_inventory_control SET cycle_generation=generation,cursor='',working_json=?1,published_json=?2,published_at=?3,invalidated=0 WHERE id=1 AND generation=?4", params![serde_json::to_string(&StorageInventory::default())?,encoded,now,generation])?;
        if changed != 1 {
            return self.inventory(now, policy);
        }
        self.add_shared_accounting(&mut aggregate)?;
        Ok(aggregate)
    }

    pub fn advance_shared_accounting(&self, limit: usize) -> Result<()> {
        if self.managed.metadata("symbol-bodies")?.is_some() {
            let store = super::ContentObjectStore::open_in(std::sync::Arc::new(
                self.managed.open_dir("symbol-bodies")?,
            ))?;
            if let Err(error) = store.advance_accounting(limit) {
                tracing::warn!(%error, "Shared body accounting incomplete; bounded GC/recovery may continue");
            }
        }
        if self.managed.metadata("embedding-objects")?.is_some() {
            let store = crate::embeddings::EmbeddingObjectCache::open_in(std::sync::Arc::new(
                self.managed.open_dir("embedding-objects")?,
            ))?;
            if let Err(error) = store.advance_accounting(limit) {
                tracing::warn!(%error, "Embedding accounting incomplete; bounded GC/recovery may continue");
            }
        }
        Ok(())
    }

    fn add_shared_accounting(&self, inventory: &mut StorageInventory) -> Result<()> {
        inventory.shared_cache_logical_bytes = 0;
        inventory.shared_cache_allocated_bytes = 0;
        inventory.shared_cache_wal_bytes = 0;
        inventory.shared_accounting_complete = true;
        for name in ["parsed-cache.db", "history-object-cache.db"] {
            add_database_accounting(&self.managed, name, inventory)?;
        }
        for (directory, database) in [
            ("symbol-bodies", "refs.db"),
            ("embedding-objects", "index.db"),
        ] {
            if self.managed.metadata(directory)?.is_none() {
                continue;
            }
            let directory = self.managed.open_dir(directory)?;
            add_database_accounting(&directory, database, inventory)?;
            if directory.metadata(database)?.is_none() {
                inventory.shared_accounting_complete = false;
                continue;
            }
            let connection = ManagedSqlite::open(
                &directory,
                database,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            let initialized: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='object_accounting_totals')", [], |row| row.get(0))?;
            if !initialized {
                inventory.shared_accounting_complete = false;
                continue;
            }
            let accounting = super::object_accounting::read(&connection)?;
            inventory.shared_cache_logical_bytes = inventory
                .shared_cache_logical_bytes
                .saturating_add(accounting.logical_bytes);
            inventory.shared_cache_allocated_bytes = inventory
                .shared_cache_allocated_bytes
                .saturating_add(accounting.allocated_bytes);
            inventory.shared_accounting_complete &= accounting.complete;
        }
        inventory.pressure_unknown |= !inventory.shared_accounting_complete;
        Ok(())
    }

    pub fn invalidate_inventory(&self) -> Result<()> {
        self.conn.execute(
            "UPDATE storage_inventory_control SET generation=generation+1,invalidated=1 WHERE id=1",
            [],
        )?;
        Ok(())
    }

    fn scan_inventory_page(
        &self,
        now: u64,
        policy: &CachePolicy,
        after: &str,
        limit: usize,
    ) -> Result<(StorageInventory, String, bool)> {
        policy.validate()?;
        let mut inventory = StorageInventory::default();
        let mut statement = self
            .conn
            .prepare("SELECT checkout_id,last_seen FROM checkout_registry WHERE checkout_id>?1 ORDER BY checkout_id LIMIT ?2")?;
        let rows = statement
            .query_map(params![after, limit.saturating_add(1) as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let more = rows.len() > limit;
        let mut last = after.to_owned();
        for (id, seen) in rows.into_iter().take(limit) {
            last = id.clone();
            let allocated_before = inventory.derived_allocated_bytes;
            let idle_lock = self.checkout_lock(&id, true)?;
            let active = idle_lock.is_none();
            if active {
                inventory.active_checkouts += 1;
            } else {
                inventory.retained_checkouts += 1;
            }
            let checkout = match self.managed.open_dir(Path::new("checkouts").join(&id)) {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            for name in ["events.db", "events.db-wal", "events.db-shm"] {
                if let Some(meta) = checkout.metadata(name)? {
                    if !meta.is_file {
                        bail!("telemetry artifact is not a regular file: {name}");
                    }
                    inventory.telemetry_logical_bytes =
                        inventory.telemetry_logical_bytes.saturating_add(meta.len);
                    inventory.telemetry_allocated_bytes = inventory
                        .telemetry_allocated_bytes
                        .saturating_add(meta.allocated);
                    if name.ends_with("-wal") {
                        inventory.telemetry_wal_bytes =
                            inventory.telemetry_wal_bytes.saturating_add(meta.len);
                    }
                }
            }
            for name in DERIVED_FILES {
                if let Some(meta) = checkout.metadata(name)? {
                    if meta.is_file {
                        inventory.historical_derived_allocated_bytes = inventory
                            .historical_derived_allocated_bytes
                            .saturating_add(meta.allocated);
                        if inventory.historical_derived_artifacts.len() < 256 {
                            inventory
                                .historical_derived_artifacts
                                .push(format!("checkouts/{id}/{name}"));
                        } else {
                            inventory.historical_derived_artifacts_truncated = true;
                        }
                    }
                }
            }
            let directory = match checkout.open_dir("cache") {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            for name in DERIVED_FILES {
                if let Some(entry) = directory.metadata(name)? {
                    if !entry.is_file {
                        bail!("derived cache allowlist entry is not a regular file: {name}");
                    }
                    inventory.derived_logical_bytes =
                        inventory.derived_logical_bytes.saturating_add(entry.len);
                    inventory.derived_allocated_bytes = inventory
                        .derived_allocated_bytes
                        .saturating_add(entry.allocated);
                    if name.ends_with("-wal") {
                        inventory.wal_bytes = inventory.wal_bytes.saturating_add(entry.len);
                    }
                    if !active && now.saturating_sub(seen) >= policy.idle_grace_secs {
                        inventory.reclaimable_bytes =
                            inventory.reclaimable_bytes.saturating_add(entry.allocated);
                    }
                }
            }
            let directory_page = directory.read_dir_page(None, DERIVED_FILES.len() + 1)?;
            if directory_page.next_cookie.is_some() {
                inventory.unknown_artifacts_truncated = true;
            }
            for entry in directory_page.entries {
                let name = entry.name;
                if !entry.is_file || !DERIVED_FILES.contains(&name.as_str()) {
                    if inventory.unknown_artifacts.len() < 256 {
                        inventory
                            .unknown_artifacts
                            .push(format!("checkouts/{id}/cache/{name}"));
                    } else {
                        inventory.unknown_artifacts_truncated = true;
                    }
                    continue;
                }
            }
            let observed_bytes = inventory
                .derived_allocated_bytes
                .saturating_sub(allocated_before);
            self.conn.execute("INSERT INTO storage_checkout_accounting(checkout_id,scan_generation,allocated_bytes,reclaimable,last_seen) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(checkout_id) DO UPDATE SET scan_generation=excluded.scan_generation,allocated_bytes=excluded.allocated_bytes,reclaimable=excluded.reclaimable,last_seen=excluded.last_seen", params![id,self.conn.query_row::<i64,_,_>("SELECT cycle_generation FROM storage_inventory_control WHERE id=1",[],|r|r.get(0))?,observed_bytes,(!active && now.saturating_sub(seen)>=policy.idle_grace_secs) as i64,seen])?;
        }
        Ok((inventory, last, more))
    }

    pub fn plan_gc(&self, now: u64, policy: &CachePolicy) -> Result<Vec<GcCandidate>> {
        policy.validate()?;
        let inventory = self.inventory(now, policy)?;
        if !inventory.accounting_complete || inventory.pressure_unknown {
            bail!("cache pressure is unknown until a fresh bounded inventory cycle completes");
        }
        let total = inventory
            .derived_allocated_bytes
            .saturating_add(inventory.shared_cache_allocated_bytes);
        if total <= policy.high_bytes {
            return Ok(Vec::new());
        }
        let mut remaining = total;
        let mut candidates = Vec::new();
        let mut statement = self.conn.prepare("SELECT checkout_id FROM storage_checkout_accounting WHERE scan_generation=(SELECT generation FROM storage_inventory_control WHERE id=1) AND reclaimable=1 ORDER BY last_seen,checkout_id LIMIT ?1")?;
        for id in statement.query_map([policy.batch_files as i64], |r| r.get::<_, String>(0))? {
            let id = id?;
            let Some(_lock) = self.checkout_lock(&id, true)? else {
                continue;
            };
            let cache = match self
                .managed
                .open_dir(Path::new("checkouts").join(&id).join("cache"))
            {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            let bytes = cache_bundle_bytes_secure(&cache)?;
            candidates.push(GcCandidate {
                checkout_id: id.clone(),
                bytes,
            });
            remaining = remaining.saturating_sub(bytes);
            if candidates.len() == policy.batch_files || remaining <= policy.low_bytes {
                return Ok(candidates);
            }
        }
        Ok(candidates)
    }

    pub fn execute_gc(
        &mut self,
        candidates: &[GcCandidate],
        now: u64,
        policy: &CachePolicy,
    ) -> Result<GcReport> {
        policy.validate()?;
        if candidates.len() > policy.batch_files {
            bail!("GC plan exceeds configured batch");
        }
        let _maintenance = descriptor_lock(&self.managed, "maintenance.lock", true)?
            .context("repository maintenance is already running")?;
        let mut report = self.resume_trash(policy.batch_files)?;
        for candidate in candidates {
            validate_id(&candidate.checkout_id)?;
            let Some(_checkout_lock) = self.checkout_lock(&candidate.checkout_id, true)? else {
                report.skipped_active += 1;
                continue;
            };
            let seen: Option<u64> = self
                .conn
                .query_row(
                    "SELECT last_seen FROM checkout_registry WHERE checkout_id=?1",
                    [&candidate.checkout_id],
                    |r| r.get(0),
                )
                .optional()?;
            if !seen.is_some_and(|seen| now.saturating_sub(seen) >= policy.idle_grace_secs) {
                continue;
            }
            let directory = self
                .managed
                .open_dir(Path::new("checkouts").join(&candidate.checkout_id))?;
            let Some(source_meta) = directory.metadata("cache")? else {
                continue;
            };
            if !source_meta.is_dir {
                bail!("checkout cache is not a managed directory");
            }
            let source = directory.open_dir("cache")?;
            let bytes = cache_bundle_bytes_secure(&source)?;
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let sequence: i64 = tx.query_row(
                "SELECT COALESCE(MAX(id),0)+1 FROM cache_gc_journal",
                [],
                |r| r.get(0),
            )?;
            let trash_name = format!("{sequence}-{}-cache", candidate.checkout_id);
            tx.execute("INSERT INTO cache_gc_journal(id,checkout_id,file,trash_name,state,bytes,source_dev,source_ino) VALUES (?1,?2,?3,?4,'planned',?5,?6,?7)", params![sequence,candidate.checkout_id,"cache",trash_name,bytes,source_meta.identity.dev,source_meta.identity.ino])?;
            tx.commit()?;
            let trash = self.managed.open_dir("trash")?;
            directory.rename_to("cache", &trash, &trash_name, source_meta.identity)?;
            self.invalidate_inventory()?;
            self.conn.execute(
                "UPDATE cache_gc_journal SET state='moved' WHERE id=?1",
                [sequence],
            )?;
            report.moved_files += 1;
        }
        let reclaimed = self.resume_trash(policy.batch_files)?;
        report.deleted_files += reclaimed.deleted_files;
        report.released_bytes += reclaimed.released_bytes;
        Ok(report)
    }

    fn resume_trash(&self, limit: usize) -> Result<GcReport> {
        let mut report = GcReport::default();
        let mut statement = self.conn.prepare("SELECT id,checkout_id,file,trash_name,bytes,source_dev,source_ino FROM cache_gc_journal WHERE state!='deleted' ORDER BY id LIMIT ?1")?;
        let rows = statement
            .query_map([limit], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, u64>(4)?,
                    r.get::<_, Option<u64>>(5)?,
                    r.get::<_, Option<u64>>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, checkout, name, trash, bytes, source_dev, source_ino) in rows {
            validate_id(&checkout)?;
            if name != "cache" || trash != format!("{id}-{checkout}-{name}") {
                bail!("invalid GC journal ownership; manual recovery required");
            }
            let trash_dir = self.managed.open_dir("trash")?;
            if let Some(bundle_meta) = trash_dir.metadata(&trash)? {
                if !bundle_meta.is_dir {
                    bail!("GC trash entry is not a managed directory");
                }
                if source_dev != Some(bundle_meta.identity.dev)
                    || source_ino != Some(bundle_meta.identity.ino)
                {
                    bail!("GC trash identity is not proven by its journal; preserving payloads and references for operator review");
                }
                let bundle = trash_dir.open_dir(&trash)?;
                cache_bundle_bytes_secure(&bundle)?;
                // A new cache may have been published since the old bundle
                // moved. Retire checkout-wide references only while holding
                // its exclusive lease and proving no current cache exists.
                let checkout_lock = self.checkout_lock(&checkout, true)?;
                let current_exists = match self
                    .managed
                    .open_dir(Path::new("checkouts").join(&checkout))
                {
                    Ok(current) => current.metadata("cache")?.is_some(),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => return Err(error.into()),
                };
                if checkout_lock.is_some() && !current_exists {
                    if self.managed.metadata("parsed-cache.db")?.is_some() {
                        super::ParsedFileCache::open_in(&self.managed, "parsed-cache.db")?
                            .release_commit_manifest(&checkout)?;
                    }
                    // The cache rename is the ownership boundary for embedding
                    // membership. Replay this after a crash only when trash proves
                    // that the rename completed; a merely planned row retains it.
                    if self.managed.metadata("embedding-objects")?.is_some() {
                        let objects =
                            std::sync::Arc::new(self.managed.open_dir("embedding-objects")?);
                        if objects.metadata("index.db")?.is_some() {
                            crate::embeddings::EmbeddingObjectCache::open_in(objects)?
                                .remove_checkout_membership(&checkout)?;
                        }
                    }
                    if self.managed.metadata("symbol-bodies")?.is_some() {
                        let objects = std::sync::Arc::new(self.managed.open_dir("symbol-bodies")?);
                        if objects.metadata("refs.db")?.is_some() {
                            super::ContentObjectStore::open_in(objects)?
                                .retire_checkout(&checkout)?;
                        }
                    }
                }
                let bundle = trash_dir.open_dir(&trash)?;
                cache_bundle_bytes_secure(&bundle)?;
                for entry in bundle.read_dir_page(None, DERIVED_FILES.len() + 1)?.entries {
                    if !entry.is_file {
                        bail!("GC trash changed after validation; automatic reclamation refused");
                    }
                    bundle.remove_file(&entry.name, entry.identity)?;
                }
                trash_dir.remove_dir(&trash, bundle_meta.identity)?;
                report.deleted_files += 1;
                report.released_bytes += bytes;
            }
            // If the crash happened before rename, the source stays registered
            // and is eligible for a new plan. Never delete a source on replay.
            self.conn.execute(
                "UPDATE cache_gc_journal SET state='deleted' WHERE id=?1",
                [id],
            )?;
        }
        self.conn.execute("DELETE FROM cache_gc_journal WHERE state='deleted' AND id < (SELECT COALESCE(MAX(id),0)-4096 FROM cache_gc_journal)", [])?;
        Ok(report)
    }

    fn checkout_lock(&self, id: &str, exclusive: bool) -> Result<Option<File>> {
        validate_id(id)?;
        let leases = self.managed.open_dir("leases")?;
        descriptor_lock(&leases, &format!("{id}.lock"), exclusive)
    }
}

fn initialize_gc_identity_schema(connection: &Connection) -> Result<()> {
    let transaction =
        rusqlite::Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
    let columns = {
        let mut query = transaction.prepare("PRAGMA table_info(cache_gc_journal)")?;
        let columns = query
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        columns
    };
    for column in ["source_dev", "source_ino"] {
        if !columns.iter().any(|existing| existing == column) {
            transaction.execute_batch(&format!(
                "ALTER TABLE cache_gc_journal ADD COLUMN {column} INTEGER"
            ))?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn initialize_inventory_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS storage_inventory_control(id INTEGER PRIMARY KEY CHECK(id=1),generation INTEGER NOT NULL DEFAULT 0,cycle_generation INTEGER NOT NULL DEFAULT 0,cursor TEXT NOT NULL DEFAULT '',working_json TEXT NOT NULL,published_json TEXT NOT NULL,published_at INTEGER NOT NULL DEFAULT 0,invalidated INTEGER NOT NULL DEFAULT 1); CREATE TABLE IF NOT EXISTS storage_checkout_accounting(checkout_id TEXT PRIMARY KEY,scan_generation INTEGER NOT NULL,allocated_bytes INTEGER NOT NULL,reclaimable INTEGER NOT NULL,last_seen INTEGER NOT NULL); CREATE INDEX IF NOT EXISTS storage_checkout_reclaimable ON storage_checkout_accounting(scan_generation,reclaimable,last_seen,checkout_id); CREATE TRIGGER IF NOT EXISTS storage_inventory_checkout_insert AFTER INSERT ON checkout_registry BEGIN UPDATE storage_inventory_control SET generation=generation+1,invalidated=1 WHERE id=1; END; CREATE TRIGGER IF NOT EXISTS storage_inventory_checkout_delete AFTER DELETE ON checkout_registry BEGIN UPDATE storage_inventory_control SET generation=generation+1,invalidated=1 WHERE id=1; END; CREATE TRIGGER IF NOT EXISTS storage_inventory_checkout_root_update AFTER UPDATE OF root ON checkout_registry WHEN old.root<>new.root BEGIN UPDATE storage_inventory_control SET generation=generation+1,invalidated=1 WHERE id=1; END;")?;
    let empty = serde_json::to_string(&StorageInventory::default())?;
    connection.execute("INSERT OR IGNORE INTO storage_inventory_control(id,working_json,published_json) VALUES(1,?1,?1)",[empty])?;
    Ok(())
}

fn add_database_accounting(
    directory: &SecureDir,
    database: &str,
    inventory: &mut StorageInventory,
) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        if let Some(entry) = directory.metadata(&format!("{database}{suffix}"))? {
            if !entry.is_file {
                bail!("shared cache database accounting encountered a non-regular file");
            }
            inventory.shared_cache_logical_bytes = inventory
                .shared_cache_logical_bytes
                .saturating_add(entry.len);
            inventory.shared_cache_allocated_bytes = inventory
                .shared_cache_allocated_bytes
                .saturating_add(entry.allocated);
            if suffix == "-wal" {
                inventory.shared_cache_wal_bytes =
                    inventory.shared_cache_wal_bytes.saturating_add(entry.len);
            }
        }
    }
    Ok(())
}

fn merge_inventory(target: &mut StorageInventory, page: StorageInventory) {
    target.derived_logical_bytes = target
        .derived_logical_bytes
        .saturating_add(page.derived_logical_bytes);
    target.derived_allocated_bytes = target
        .derived_allocated_bytes
        .saturating_add(page.derived_allocated_bytes);
    target.wal_bytes = target.wal_bytes.saturating_add(page.wal_bytes);
    target.telemetry_logical_bytes = target
        .telemetry_logical_bytes
        .saturating_add(page.telemetry_logical_bytes);
    target.telemetry_allocated_bytes = target
        .telemetry_allocated_bytes
        .saturating_add(page.telemetry_allocated_bytes);
    target.telemetry_wal_bytes = target
        .telemetry_wal_bytes
        .saturating_add(page.telemetry_wal_bytes);
    target.active_checkouts = target
        .active_checkouts
        .saturating_add(page.active_checkouts);
    target.retained_checkouts = target
        .retained_checkouts
        .saturating_add(page.retained_checkouts);
    target.reclaimable_bytes = target
        .reclaimable_bytes
        .saturating_add(page.reclaimable_bytes);
    target.historical_derived_allocated_bytes = target
        .historical_derived_allocated_bytes
        .saturating_add(page.historical_derived_allocated_bytes);
    for value in page.unknown_artifacts {
        if target.unknown_artifacts.len() < 256 {
            target.unknown_artifacts.push(value)
        } else {
            target.unknown_artifacts_truncated = true;
        }
    }
    target.unknown_artifacts_truncated |= page.unknown_artifacts_truncated;
    for value in page.historical_derived_artifacts {
        if target.historical_derived_artifacts.len() < 256 {
            target.historical_derived_artifacts.push(value)
        } else {
            target.historical_derived_artifacts_truncated = true;
        }
    }
    target.historical_derived_artifacts_truncated |= page.historical_derived_artifacts_truncated;
}

fn validate_id(id: &str) -> Result<()> {
    if !id.starts_with("checkout_")
        || id.len() != 73
        || !id[9..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        bail!("invalid registered checkout identity");
    }
    Ok(())
}
fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!("managed storage symlink refused: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn validate_managed_path(root: &Path, path: &Path) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .context("managed storage path escaped repository home")?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                bail!("managed storage symlink refused: {}", current.display())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
fn descriptor_lock(directory: &SecureDir, name: &str, exclusive: bool) -> Result<Option<File>> {
    let file = directory.open_or_create_file(name)?;
    let result = if exclusive {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    match result {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

fn cache_bundle_bytes_secure(directory: &SecureDir) -> Result<u64> {
    let entries = directory
        .read_dir_page(None, DERIVED_FILES.len() + 1)?
        .entries;
    let mut bytes = 0u64;
    for entry in entries {
        if !entry.is_file || !DERIVED_FILES.contains(&entry.name.as_str()) {
            bail!("unknown or non-file artifact in derived cache bundle; automatic reclamation refused: {}", entry.name);
        }
        bytes = bytes.saturating_add(entry.allocated);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id(i: usize) -> String {
        format!("checkout_{i:064x}")
    }
    fn write_cache(root: &Path, id: &str) {
        fs::write(
            root.join("checkouts").join(id).join("cache/graph.db"),
            vec![42; 8192],
        )
        .unwrap();
        fs::write(
            root.join("checkouts").join(id).join("cache/graph.db-wal"),
            vec![7; 4096],
        )
        .unwrap();
    }
    #[test]
    fn hundred_checkout_churn_reclaims_only_unleased_bundles() {
        let root = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(root.path(), "repository").unwrap();
        fs::write(root.path().join("memories.db"), "durable knowledge").unwrap();
        let mut active = Vec::new();
        for i in 0..100 {
            let lease = registry
                .register_and_lease(&id(i), checkout.path(), 100)
                .unwrap();
            write_cache(root.path(), &id(i));
            fs::write(
                root.path().join("checkouts").join(id(i)).join("events.db"),
                "evidence",
            )
            .unwrap();
            if i < 2 {
                active.push(lease);
            }
        }
        let policy = CachePolicy {
            high_bytes: 1,
            low_bytes: 0,
            idle_grace_secs: 10,
            batch_files: 16,
        };
        loop {
            registry.advance_inventory(111, &policy, 4096).unwrap();
            let plan = registry.plan_gc(111, &policy).unwrap();
            if plan.is_empty() {
                break;
            }
            registry.execute_gc(&plan, 111, &policy).unwrap();
        }
        assert_eq!(
            fs::read_to_string(root.path().join("memories.db")).unwrap(),
            "durable knowledge"
        );
        for i in 0..100 {
            let path = root.path().join("checkouts").join(id(i));
            assert_eq!(path.join("cache").exists(), i < 2);
            assert_eq!(
                fs::read_to_string(path.join("events.db")).unwrap(),
                "evidence"
            );
        }
        assert_eq!(
            registry.inventory(111, &policy).unwrap().active_checkouts,
            2
        );
        drop(active);
    }

    #[test]
    fn cache_retirement_releases_commit_claims_then_reclaims_parse_payloads() {
        use crate::storage::commit_manifest::{
            CommitManifestEntry, CommitManifestIdentity, GitObjectFormat, ManifestLimits,
        };
        use crate::storage::{content_sha256, ParsedCacheLookup, ParsedFileCache};
        use crate::symbols::Language;
        let root = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let owner = id(71);
        let mut registry = StorageRegistry::open(root.path(), "repository").unwrap();
        let lease = registry
            .register_and_lease(&owner, checkout.path(), 1)
            .unwrap();
        write_cache(root.path(), &owner);
        let cache = ParsedFileCache::open(&root.path().join("parsed-cache.db")).unwrap();
        let source = "pub fn shared() {}\n";
        let hash = content_sha256(source.as_bytes());
        cache
            .put(
                &hash,
                &crate::parser::parse_file("src/lib.rs", source).unwrap(),
            )
            .unwrap();
        let identity = CommitManifestIdentity {
            repository_id: "repository".into(),
            object_format: GitObjectFormat::Sha1,
            commit_oid: "a".repeat(40),
            parser_version: 1,
            schema_version: 1,
            config_identity: "default-v1".into(),
        };
        cache
            .publish_and_bind_commit_manifest(
                &identity,
                &[CommitManifestEntry {
                    path: "src/lib.rs".into(),
                    mode: 0o100644,
                    blob_oid: "b".repeat(40),
                    content_hash: hash.clone(),
                    parse_key: ParsedFileCache::parse_key(&hash, Language::Rust),
                }],
                ManifestLimits::default(),
                &owner,
            )
            .unwrap();
        assert_eq!(registry.collect_parsed_objects(16).unwrap(), 0);
        drop(lease);
        let policy = CachePolicy {
            high_bytes: 1,
            low_bytes: 0,
            idle_grace_secs: 0,
            batch_files: 16,
        };
        registry.advance_inventory(2, &policy, 4096).unwrap();
        let plan = registry.plan_gc(2, &policy).unwrap();
        assert!(!plan.is_empty());
        // A dry-run does not release the generation's consumer claim.
        assert_eq!(registry.collect_parsed_objects(16).unwrap(), 0);
        assert!(cache.find_commit_manifest(&identity).unwrap().is_some());
        registry.execute_gc(&plan, 2, &policy).unwrap();
        assert_eq!(registry.collect_parsed_objects(16).unwrap(), 1);
        assert!(cache.find_commit_manifest(&identity).unwrap().is_none());
        assert_eq!(
            cache.get(&hash, Language::Rust, "src/lib.rs").unwrap().0,
            ParsedCacheLookup::Miss
        );
    }

    #[test]
    fn moved_checkout_cache_retires_body_references_before_object_gc() {
        use crate::graph::CodeGraph;
        use crate::storage::{ContentObjectStore, GraphStore};
        use crate::symbols::{Language, SymbolId, SymbolKind};
        let root = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let owner = id(7);
        let mut registry = StorageRegistry::open(root.path(), "repository").unwrap();
        let lease = registry
            .register_and_lease(&owner, checkout.path(), 1)
            .unwrap();
        let graph_path = root
            .path()
            .join("checkouts")
            .join(&owner)
            .join("cache/graph.db");
        let objects_path = root.path().join("symbol-bodies");
        let store =
            GraphStore::open_recovering_with_objects(&graph_path, &objects_path, &owner).unwrap();
        let mut graph = CodeGraph::new();
        graph.add_node(
            SymbolId {
                file: "src/lib.rs".into(),
                name: "owned".into(),
                byte_offset: 0,
            },
            SymbolKind::Function,
            "owned".into(),
            "fn owned()",
            "exclusive body",
            "src/lib.rs".into(),
            1,
            1,
            true,
            Language::Rust,
        );
        store.save_graph(&graph).unwrap();
        drop(store);
        drop(lease);
        let policy = CachePolicy {
            high_bytes: 1,
            low_bytes: 0,
            idle_grace_secs: 1,
            batch_files: 16,
        };
        registry.advance_inventory(3, &policy, 4096).unwrap();
        let plan = registry.plan_gc(3, &policy).unwrap();
        registry.execute_gc(&plan, 3, &policy).unwrap();
        let objects = ContentObjectStore::open(&objects_path).unwrap();
        assert_eq!(objects.collect_garbage(16).unwrap().removed, 1);
    }
    #[test]
    fn lease_revalidation_and_unknown_artifacts_prevent_deletion() {
        let root = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(root.path(), "repository").unwrap();
        drop(
            registry
                .register_and_lease(&id(1), checkout.path(), 1)
                .unwrap(),
        );
        write_cache(root.path(), &id(1));
        let policy = CachePolicy {
            high_bytes: 1,
            low_bytes: 0,
            idle_grace_secs: 1,
            batch_files: 16,
        };
        registry.advance_inventory(10, &policy, 4096).unwrap();
        let plan = registry.plan_gc(10, &policy).unwrap();
        let lease = registry
            .register_and_lease(&id(1), checkout.path(), 10)
            .unwrap();
        let reader = Connection::open(
            root.path()
                .join("checkouts")
                .join(id(1))
                .join("cache/graph.db"),
        )
        .unwrap();
        assert_eq!(
            registry
                .execute_gc(&plan, 20, &policy)
                .unwrap()
                .skipped_active,
            1
        );
        drop(reader);
        drop(lease);
        fs::write(
            root.path()
                .join("checkouts")
                .join(id(1))
                .join("cache/memories.db"),
            "knowledge",
        )
        .unwrap();
        assert!(registry.plan_gc(20, &policy).is_err());
    }

    #[test]
    fn historical_flat_cache_is_reported_and_never_reclaimed() {
        let root = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(root.path(), "repository").unwrap();
        drop(
            registry
                .register_and_lease(&id(1), checkout.path(), 1)
                .unwrap(),
        );
        let historical = root.path().join("checkouts").join(id(1)).join("graph.db");
        fs::write(&historical, vec![1; 4096]).unwrap();
        let policy = CachePolicy {
            high_bytes: 1,
            low_bytes: 0,
            idle_grace_secs: 1,
            batch_files: 16,
        };
        registry.advance_inventory(10, &policy, 4096).unwrap();
        let inventory = registry.inventory(10, &policy).unwrap();
        assert_eq!(
            inventory.historical_derived_artifacts,
            vec![format!("checkouts/{}/graph.db", id(1))]
        );
        assert!(inventory.historical_derived_allocated_bytes > 0);
        assert!(registry.plan_gc(10, &policy).unwrap().is_empty());
        assert!(historical.exists());
    }
    #[test]
    fn planned_journal_does_not_authorize_a_forged_trash_directory() {
        let root = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(root.path(), "repository").unwrap();
        drop(
            registry
                .register_and_lease(&id(1), checkout.path(), 1)
                .unwrap(),
        );
        write_cache(root.path(), &id(1));
        let source = registry
            .managed
            .open_dir(Path::new("checkouts").join(id(1)))
            .unwrap()
            .metadata("cache")
            .unwrap()
            .unwrap();
        let trash = format!("1-{}-cache", id(1));
        registry.conn.execute("INSERT INTO cache_gc_journal(id,checkout_id,file,trash_name,state,bytes,source_dev,source_ino) VALUES(1,?1,'cache',?2,'planned',1,?3,?4)", params![id(1),trash,source.identity.dev,source.identity.ino]).unwrap();
        let forged = registry
            .managed
            .open_dir("trash")
            .unwrap()
            .create_dir(&trash)
            .unwrap();
        use std::io::Write;
        forged
            .open_new_file("graph.db")
            .unwrap()
            .write_all(b"unproven")
            .unwrap();
        assert!(registry
            .execute_gc(&[], 2, &CachePolicy::default())
            .unwrap_err()
            .to_string()
            .contains("identity is not proven"));
        assert_eq!(
            fs::read(forged.path().join("graph.db")).unwrap(),
            b"unproven"
        );
        assert!(root
            .path()
            .join("checkouts")
            .join(id(1))
            .join("cache/graph.db")
            .exists());
    }

    #[test]
    fn interrupted_rename_recovery_never_deletes_new_source_generation() {
        let root = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(root.path(), "repository").unwrap();
        drop(
            registry
                .register_and_lease(&id(1), checkout.path(), 1)
                .unwrap(),
        );
        write_cache(root.path(), &id(1));
        let trash = format!("1-{}-cache", id(1));
        let original = registry
            .managed
            .open_dir(Path::new("checkouts").join(id(1)))
            .unwrap()
            .metadata("cache")
            .unwrap()
            .unwrap();
        registry
            .conn
            .execute(
                "INSERT INTO cache_gc_journal(id,checkout_id,file,trash_name,state,bytes,source_dev,source_ino) VALUES(1,?1,'cache',?2,'planned',12288,?3,?4)",
                params![id(1), trash, original.identity.dev, original.identity.ino],
            )
            .unwrap();
        fs::rename(
            root.path().join("checkouts").join(id(1)).join("cache"),
            root.path().join("trash").join(&trash),
        )
        .unwrap();
        drop(registry);
        let mut registry = StorageRegistry::open(root.path(), "repository").unwrap();
        let lease = registry
            .register_and_lease(&id(1), checkout.path(), 100)
            .unwrap();
        write_cache(root.path(), &id(1));
        let cache =
            super::super::ParsedFileCache::open(&root.path().join("parsed-cache.db")).unwrap();
        let source = "pub fn replacement_generation() {}";
        let hash = super::super::content_sha256(source.as_bytes());
        cache
            .put(
                &hash,
                &crate::parser::parse_file("src/lib.rs", source).unwrap(),
            )
            .unwrap();
        let identity = super::super::commit_manifest::CommitManifestIdentity {
            repository_id: "repository".into(),
            object_format: super::super::commit_manifest::GitObjectFormat::Sha1,
            commit_oid: "c".repeat(40),
            parser_version: 1,
            schema_version: 1,
            config_identity: "default-v1".into(),
        };
        cache
            .publish_and_bind_commit_manifest(
                &identity,
                &[super::super::commit_manifest::CommitManifestEntry {
                    path: "src/lib.rs".into(),
                    mode: 0o100644,
                    blob_oid: "d".repeat(40),
                    content_hash: hash.clone(),
                    parse_key: super::super::ParsedFileCache::parse_key(
                        &hash,
                        crate::symbols::Language::Rust,
                    ),
                }],
                super::super::commit_manifest::ManifestLimits::default(),
                &id(1),
            )
            .unwrap();
        registry
            .execute_gc(&[], 100, &CachePolicy::default())
            .unwrap();
        assert!(!root.path().join("trash").join(trash).exists());
        assert_eq!(registry.collect_parsed_objects(16).unwrap(), 0);
        assert!(cache.find_commit_manifest(&identity).unwrap().is_some());
        assert_eq!(
            cache
                .get(&hash, crate::symbols::Language::Rust, "src/lib.rs")
                .unwrap()
                .0,
            super::super::ParsedCacheLookup::Hit
        );
        assert!(root
            .path()
            .join("checkouts")
            .join(id(1))
            .join("cache/graph.db")
            .exists());
        drop(lease);
    }

    #[test]
    fn legacy_trash_without_identity_proof_is_preserved_for_manual_recovery() {
        let root = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(root.path(), "repository").unwrap();
        drop(
            registry
                .register_and_lease(&id(1), checkout.path(), 1)
                .unwrap(),
        );
        write_cache(root.path(), &id(1));
        let trash = format!("1-{}-cache", id(1));
        fs::rename(
            root.path().join("checkouts").join(id(1)).join("cache"),
            root.path().join("trash").join(&trash),
        )
        .unwrap();
        registry.conn.execute(
            "INSERT INTO cache_gc_journal(id,checkout_id,file,trash_name,state,bytes,source_dev,source_ino) VALUES(1,?1,'cache',?2,'moved',12288,NULL,NULL)",
            params![id(1), trash],
        ).unwrap();
        let error = registry
            .execute_gc(&[], 2, &CachePolicy::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("identity is not proven"), "{error}");
        assert!(root
            .path()
            .join("trash")
            .join(trash)
            .join("graph.db")
            .is_file());
    }

    #[test]
    fn persisted_inventory_progresses_beyond_four_thousand_checkouts() {
        let root = tempfile::tempdir().unwrap();
        let registry = StorageRegistry::open(root.path(), "repository").unwrap();
        {
            let transaction = registry.conn.unchecked_transaction().unwrap();
            for ordinal in 0..4_100 {
                transaction
                    .execute(
                        "INSERT INTO checkout_registry(checkout_id,root,last_seen) VALUES(?1,?2,1)",
                        params![id(ordinal), format!("/missing/{ordinal}")],
                    )
                    .unwrap();
            }
            transaction.commit().unwrap();
        }
        let policy = CachePolicy::default();
        for _ in 0..4 {
            let partial = registry.advance_inventory(10, &policy, 1_024).unwrap();
            assert!(partial.pressure_unknown);
            assert!(!partial.accounting_complete);
        }
        let complete = registry.advance_inventory(10, &policy, 1_024).unwrap();
        assert!(complete.accounting_complete);
        assert!(!complete.pressure_unknown);
        assert_eq!(complete.retained_checkouts, 4_100);
        drop(registry);
        let reopened = StorageRegistry::open(root.path(), "repository").unwrap();
        assert_eq!(
            reopened.inventory(10, &policy).unwrap().retained_checkouts,
            4_100
        );
    }

    #[test]
    fn inventory_membership_mutation_restarts_before_publication() {
        let root = tempfile::tempdir().unwrap();
        let registry = StorageRegistry::open(root.path(), "repository").unwrap();
        registry.conn.execute("INSERT INTO checkout_registry(checkout_id,root,last_seen) VALUES(?1,'/missing/a',1)",[id(2)]).unwrap();
        registry.conn.execute("INSERT INTO checkout_registry(checkout_id,root,last_seen) VALUES(?1,'/missing/b',1)",[id(3)]).unwrap();
        assert!(
            registry
                .advance_inventory(10, &CachePolicy::default(), 1)
                .unwrap()
                .pressure_unknown
        );
        registry.conn.execute("INSERT INTO checkout_registry(checkout_id,root,last_seen) VALUES(?1,'/missing/prior',1)",[id(1)]).unwrap();
        let mut result = StorageInventory::default();
        for _ in 0..3 {
            result = registry
                .advance_inventory(10, &CachePolicy::default(), 1)
                .unwrap();
        }
        assert!(result.accounting_complete);
        assert_eq!(result.retained_checkouts, 3);
    }

    #[test]
    fn indexed_plan_reaches_cache_after_four_thousand_absent_rows() {
        let root = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(root.path(), "repository").unwrap();
        for n in 0..4_100 {
            registry
                .conn
                .execute(
                    "INSERT INTO checkout_registry(checkout_id,root,last_seen) VALUES(?1,?2,1)",
                    params![id(n), format!("/missing/{n}")],
                )
                .unwrap();
        }
        let eligible = id(5_000);
        drop(
            registry
                .register_and_lease(&eligible, checkout.path(), 1)
                .unwrap(),
        );
        write_cache(root.path(), &eligible);
        let policy = CachePolicy {
            high_bytes: 1,
            low_bytes: 0,
            idle_grace_secs: 1,
            batch_files: 8,
        };
        for _ in 0..5 {
            registry.advance_inventory(10, &policy, 1_024).unwrap();
        }
        let plan = registry.plan_gc(10, &policy).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].checkout_id, eligible);
        assert!(plan[0].bytes > 0);
    }

    #[test]
    fn lexical_unknown_cannot_hide_allowlisted_bytes() {
        let root = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(root.path(), "repository").unwrap();
        drop(
            registry
                .register_and_lease(&id(1), checkout.path(), 1)
                .unwrap(),
        );
        write_cache(root.path(), &id(1));
        let cache = root.path().join("checkouts").join(id(1)).join("cache");
        for n in 0..16 {
            fs::write(cache.join(format!("aaa-{n:02}")), b"unknown").unwrap();
        }
        let inventory = registry
            .advance_inventory(10, &CachePolicy::default(), 16)
            .unwrap();
        assert!(inventory.derived_allocated_bytes > 0);
        assert!(inventory.pressure_unknown);
        assert!(inventory.unknown_artifacts_truncated);
        assert!(
            registry
                .inventory(10, &CachePolicy::default())
                .unwrap()
                .pressure_unknown
        );
        assert!(registry.plan_gc(10, &CachePolicy::default()).is_err());
    }

    #[test]
    fn usearch_metadata_is_accounted_and_retired_with_its_index() {
        let root = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(root.path(), "repository").unwrap();
        drop(
            registry
                .register_and_lease(&id(1), checkout.path(), 1)
                .unwrap(),
        );
        let cache = root.path().join("checkouts").join(id(1)).join("cache");
        fs::write(cache.join("vectors.usearch"), vec![42; 8192]).unwrap();
        fs::write(
            cache.join("vectors.usearch.meta.json"),
            br#"{"generation":1,"dimension":384}"#,
        )
        .unwrap();
        let policy = CachePolicy {
            high_bytes: 1,
            low_bytes: 0,
            idle_grace_secs: 0,
            batch_files: 4,
        };
        let inventory = registry.advance_inventory(10, &policy, 16).unwrap();
        assert!(!inventory.pressure_unknown);
        assert!(inventory.derived_logical_bytes > 8192);
        let plan = registry.plan_gc(10, &policy).unwrap();
        assert_eq!(plan.len(), 1);
        registry.execute_gc(&plan, 10, &policy).unwrap();
        assert!(!cache.exists());
    }

    #[test]
    fn historical_flat_file_is_counted_without_cache_directory() {
        let root = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(root.path(), "repository").unwrap();
        drop(
            registry
                .register_and_lease(&id(1), checkout.path(), 1)
                .unwrap(),
        );
        fs::remove_dir(root.path().join("checkouts").join(id(1)).join("cache")).unwrap();
        fs::write(
            root.path().join("checkouts").join(id(1)).join("graph.db"),
            vec![1; 4096],
        )
        .unwrap();
        let inventory = registry
            .advance_inventory(10, &CachePolicy::default(), 16)
            .unwrap();
        assert!(inventory.historical_derived_allocated_bytes > 0);
    }

    #[test]
    fn maintenance_fence_blocks_checkout_registration() {
        let root = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(root.path(), "repository").unwrap();
        let _maintenance = descriptor_lock(&registry.managed, "maintenance.lock", true)
            .unwrap()
            .unwrap();
        let result = registry.register_and_lease(&id(1), checkout.path(), 1);
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("maintenance"));
    }
    #[cfg(unix)]
    #[test]
    fn symlink_and_foreign_home_are_refused() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let registry = StorageRegistry::open(root.path(), "repository").unwrap();
        drop(registry);
        assert!(StorageRegistry::open(root.path(), "foreign").is_err());
        let home = outside.path().join("unsafe");
        fs::create_dir(&home).unwrap();
        symlink(root.path(), home.join("trash")).unwrap();
        assert!(StorageRegistry::open(&home, "repository").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn checkout_directory_symlink_cannot_delete_external_cache() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(root.path(), "repository").unwrap();
        drop(
            registry
                .register_and_lease(&id(1), checkout.path(), 1)
                .unwrap(),
        );
        let managed = root.path().join("checkouts").join(id(1));
        fs::remove_dir_all(&managed).unwrap();
        fs::create_dir(outside.path().join("cache")).unwrap();
        fs::write(outside.path().join("cache/graph.db"), vec![1; 8192]).unwrap();
        symlink(outside.path(), &managed).unwrap();
        let policy = CachePolicy {
            high_bytes: 1,
            low_bytes: 0,
            idle_grace_secs: 1,
            batch_files: 16,
        };
        assert!(registry.plan_gc(10, &policy).is_err());
        assert!(outside.path().join("cache/graph.db").exists());
    }
}
