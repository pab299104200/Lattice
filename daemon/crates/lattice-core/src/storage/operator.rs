//! Explicit, bounded operator surface for repository-owned storage.
//!
//! Inspection and planning are read-only. Mutation is opt-in and delegates
//! cache removal to [`StorageRegistry`], which revalidates ownership, checkout
//! leases, idle age, and the positive derived-file allowlist under its
//! repository maintenance lock.

use super::managed_sqlite::ManagedSqlite;
use super::{CachePolicy, GcCandidate, GcReport, StorageInventory, StorageRegistry};
use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_PLAN_CANDIDATES: usize = 4096;

#[cfg(test)]
thread_local! { static OPERATOR_FAILURE_POINT: std::cell::RefCell<Option<&'static str>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
thread_local! { static OPERATOR_ACTION: std::cell::RefCell<Option<(&'static str,Box<dyn FnOnce()>)>> = const { std::cell::RefCell::new(None) }; }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StorageStatus {
    pub repository_id: String,
    pub canonical_home: PathBuf,
    pub registry_bytes: u64,
    pub registry_wal_bytes: u64,
    pub registry_free_bytes: u64,
    pub inventory: StorageInventory,
    pub classes: StorageClasses,
    pub complete: bool,
    pub limits: AccountingLimits,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ByteAccounting {
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub wal_bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StorageClasses {
    pub durable_knowledge: ByteAccounting,
    pub disposable_cache: ByteAccounting,
    pub telemetry: ByteAccounting,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct AccountingLimits {
    pub max_checkouts: usize,
    pub max_artifacts_per_checkout: usize,
}

impl Default for AccountingLimits {
    fn default() -> Self {
        Self {
            max_checkouts: 4096,
            max_artifacts_per_checkout: 256,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheMaintenancePlan {
    pub schema_version: u32,
    pub repository_id: String,
    pub canonical_home: PathBuf,
    pub planned_at: u64,
    pub policy: CachePolicy,
    pub inventory: StorageInventory,
    pub candidates: Vec<GcCandidate>,
    pub fingerprint: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheMaintenanceOutcome {
    pub applied: bool,
    pub plan_fingerprint: String,
    pub report: GcReport,
    pub inventory_after: StorageInventory,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KnowledgeBackupManifest {
    pub schema_version: u32,
    pub repository_id: String,
    pub canonical_home: PathBuf,
    pub created_at: u64,
    pub database_file: String,
    pub database_sha256: String,
    pub memory_count: u64,
    pub restore_floor: u64,
}

#[derive(Clone, Debug)]
pub struct KnowledgeBackupRequest {
    pub destination: PathBuf,
    pub created_at: u64,
}

#[derive(Clone, Debug)]
pub struct KnowledgeRestoreRequest {
    pub backup_directory: PathBuf,
    /// Replacement is never implicit. A false value rejects an existing
    /// authority; true performs an atomic, rollback-capable replacement.
    pub replace_existing: bool,
    /// Required because readers from releases predating the owner/maintenance
    /// fences cannot be detected or safely drained.
    pub operator_confirms_all_lattice_processes_stopped: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KnowledgeRestoreOutcome {
    pub restored: bool,
    pub database_sha256: String,
    pub memory_count: u64,
    pub restore_floor: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoricalRetirementEntry {
    pub checkout_id: String,
    pub file: String,
    pub device: u64,
    pub inode: u64,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoricalRetirementPlan {
    pub schema_version: u32,
    pub repository_id: String,
    pub canonical_home: PathBuf,
    pub planned_at: u64,
    pub backup_directory: PathBuf,
    pub backup_manifest_sha256: String,
    pub backup_manifest_device: u64,
    pub backup_manifest_inode: u64,
    pub entries: Vec<HistoricalRetirementEntry>,
    pub fingerprint: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoricalRetirementOutcome {
    pub plan_fingerprint: String,
    pub deleted_files: usize,
    pub released_bytes: u64,
    pub resumed_files: usize,
}

pub struct StorageOperator {
    home: PathBuf,
    repository_id: String,
    managed: std::sync::Arc<super::SecureDir>,
}

impl StorageOperator {
    pub(crate) fn from_pinned_home(
        managed: std::sync::Arc<super::SecureDir>,
        repository_id: &str,
    ) -> Result<Self> {
        let home = managed.path().to_path_buf();
        let conn = ManagedSqlite::open(
            &managed,
            "storage-registry.db",
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let (owner, recorded_home): (String, String) = conn
            .query_row(
                "SELECT repository_id, canonical_home FROM repository_home WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .context("storage registry has no repository ownership record")?;
        if owner != repository_id || Path::new(&recorded_home) != home {
            bail!("storage home ownership mismatch");
        }
        Ok(Self {
            home,
            repository_id: repository_id.to_owned(),
            managed,
        })
    }

    /// Opens and verifies an existing repository home. This constructor never
    /// creates or migrates storage and is consequently safe for status/plan.
    pub fn open_existing(home: &Path, repository_id: &str) -> Result<Self> {
        if repository_id.trim().is_empty() {
            bail!("repository identity is required");
        }
        let metadata =
            fs::symlink_metadata(home).context("repository storage home is unavailable")?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("repository storage home must be an existing non-symlink directory");
        }
        let home = home.canonicalize()?;
        let managed = std::sync::Arc::new(super::SecureDir::open(&home)?);
        let db_meta = fs::symlink_metadata(home.join("storage-registry.db"))
            .context("storage registry is unavailable")?;
        if db_meta.file_type().is_symlink() || !db_meta.is_file() {
            bail!("storage registry must be an existing non-symlink file");
        }
        Self::from_pinned_home(managed, repository_id)
    }

    /// Stable identity of the pinned repository-home directory.
    pub fn home_identity(&self) -> Result<super::ManagedIdentity> {
        let fingerprint = self.managed.directory_fingerprint()?;
        Ok(super::ManagedIdentity {
            dev: fingerprint.device,
            ino: fingerprint.inode,
        })
    }

    /// Open the writable lifecycle registry through this operator's already
    /// verified directory handle. No path lookup occurs between proof and use.
    pub fn open_registry(&self) -> Result<StorageRegistry> {
        StorageRegistry::inspect_in(self.managed.clone(), &self.repository_id, true)
    }

    pub fn status(
        &self,
        now: u64,
        policy: &CachePolicy,
        limits: AccountingLimits,
    ) -> Result<StorageStatus> {
        policy.validate()?;
        let conn = self.readonly_registry()?;
        let page_size: u64 = conn.pragma_query_value(None, "page_size", |r| r.get(0))?;
        let page_count: u64 = conn.pragma_query_value(None, "page_count", |r| r.get(0))?;
        let free_pages: u64 = conn.pragma_query_value(None, "freelist_count", |r| r.get(0))?;
        let classes = self.account_classes(&conn)?;
        drop(conn);
        // inventory is bounded by the registry checkout count and its built-in
        // 256-item diagnostic cap. It does not traverse repository contents.
        let registry =
            StorageRegistry::inspect_in(self.managed.clone(), &self.repository_id, false)?;
        let inventory = registry.inventory(now, policy)?;
        let registry_wal_bytes = self
            .managed
            .metadata("storage-registry.db-wal")?
            .map_or(0, |entry| entry.len);
        let complete = inventory.accounting_complete
            && !inventory.pressure_unknown
            && !inventory.unknown_artifacts_truncated
            && !inventory.historical_derived_artifacts_truncated;
        Ok(StorageStatus {
            repository_id: self.repository_id.clone(),
            canonical_home: self.home.clone(),
            registry_bytes: page_count.saturating_mul(page_size),
            registry_wal_bytes,
            registry_free_bytes: free_pages.saturating_mul(page_size),
            inventory,
            classes,
            complete,
            limits,
        })
    }

    pub fn plan_cache_maintenance(
        &self,
        now: u64,
        policy: CachePolicy,
    ) -> Result<CacheMaintenancePlan> {
        policy.validate()?;
        let registry =
            StorageRegistry::inspect_in(self.managed.clone(), &self.repository_id, true)?;
        registry.advance_shared_accounting(256)?;
        let inventory = registry.advance_inventory(now, &policy, 256)?;
        let candidates = registry.plan_gc(now, &policy)?;
        if candidates.len() > MAX_PLAN_CANDIDATES {
            bail!("cache plan exceeds operator resource cap");
        }
        let mut plan = CacheMaintenancePlan {
            schema_version: 1,
            repository_id: self.repository_id.clone(),
            canonical_home: self.home.clone(),
            planned_at: now,
            policy,
            inventory,
            candidates,
            fingerprint: String::new(),
        };
        plan.fingerprint = fingerprint_plan(&plan)?;
        Ok(plan)
    }

    pub fn apply_cache_maintenance(
        &self,
        plan: &CacheMaintenancePlan,
        now: u64,
    ) -> Result<CacheMaintenanceOutcome> {
        if plan.schema_version != 1 {
            bail!("unsupported cache maintenance plan version");
        }
        if plan.repository_id != self.repository_id || plan.canonical_home != self.home {
            bail!("cache maintenance plan belongs to another repository home");
        }
        if plan.candidates.len() > MAX_PLAN_CANDIDATES
            || plan.candidates.len() > plan.policy.batch_files
        {
            bail!("cache maintenance plan exceeds resource cap");
        }
        if fingerprint_plan(plan)? != plan.fingerprint {
            bail!("cache maintenance plan fingerprint is invalid");
        }
        let mut registry =
            StorageRegistry::inspect_in(self.managed.clone(), &self.repository_id, true)?;
        registry.advance_inventory(now, &plan.policy, 256)?;
        let current = registry.plan_gc(now, &plan.policy)?;
        if !same_candidates(&current, &plan.candidates) {
            bail!("cache maintenance plan is stale; rerun the dry-run plan");
        }
        let report = registry.execute_gc(&plan.candidates, now, &plan.policy)?;
        let inventory_after = registry.advance_inventory(now, &plan.policy, 256)?;
        Ok(CacheMaintenanceOutcome {
            applied: true,
            plan_fingerprint: plan.fingerprint.clone(),
            report,
            inventory_after,
        })
    }

    /// Inventory the exact old flat-layout cache files eligible for retirement.
    /// A verified knowledge backup is required before a plan can be issued.
    pub fn plan_historical_retirement(
        &self,
        backup_directory: PathBuf,
        planned_at: u64,
    ) -> Result<HistoricalRetirementPlan> {
        self.require_offline()?;
        let backup_directory = backup_directory
            .canonicalize()
            .context("knowledge backup directory is unavailable")?;
        let backup = super::SecureDir::open(&backup_directory)?;
        let manifest_meta = backup
            .metadata("manifest.json")?
            .context("knowledge backup manifest is unavailable")?;
        if !manifest_meta.is_file {
            bail!("knowledge backup manifest is not a regular file");
        }
        let manifest_bytes = read_secure_file(&backup, "manifest.json")?;
        let (backup_manifest_device, backup_manifest_inode) =
            (manifest_meta.identity.dev, manifest_meta.identity.ino);
        let manifest: KnowledgeBackupManifest = serde_json::from_slice(&manifest_bytes)?;
        if manifest.schema_version != 1 || manifest.repository_id != self.repository_id {
            bail!("knowledge backup belongs to another repository or schema");
        }
        if manifest.database_file != "memories.db" {
            bail!("knowledge backup manifest database name is invalid");
        }
        if !backup
            .metadata(&manifest.database_file)?
            .is_some_and(|v| v.is_file)
            || hash_secure_file(&backup, &manifest.database_file)? != manifest.database_sha256
        {
            bail!("knowledge backup checksum mismatch");
        }
        let registry =
            StorageRegistry::inspect_in(self.managed.clone(), &self.repository_id, true)?;
        let inventory = registry.advance_inventory(planned_at, &CachePolicy::default(), 256)?;
        if !inventory.accounting_complete
            || inventory.pressure_unknown
            || inventory.historical_derived_artifacts_truncated
        {
            bail!("historical retirement inventory exceeds the bounded plan limit");
        }
        let managed = &*self.managed;
        let checkouts = managed.open_dir("checkouts")?;
        let mut entries = Vec::new();
        for relative in inventory.historical_derived_artifacts {
            let mut parts = relative.split('/');
            if parts.next() != Some("checkouts") {
                bail!("invalid historical derived inventory path");
            }
            let checkout_id = parts.next().context("historical path has no checkout")?;
            let file = parts.next().context("historical path has no file")?;
            if parts.next().is_some() {
                bail!("invalid historical derived inventory depth");
            }
            let checkout = checkouts.open_dir(checkout_id)?;
            let metadata = checkout
                .metadata(file)?
                .context("historical file disappeared")?;
            if !metadata.is_file {
                bail!("historical derived candidate is not a regular file");
            }
            let mut opened = checkout.open_file(file, false)?;
            entries.push(HistoricalRetirementEntry {
                checkout_id: checkout_id.into(),
                file: file.into(),
                device: metadata.identity.dev,
                inode: metadata.identity.ino,
                bytes: metadata.len,
                sha256: hash_reader(&mut opened)?,
            });
        }
        let mut plan = HistoricalRetirementPlan {
            schema_version: 1,
            repository_id: self.repository_id.clone(),
            canonical_home: self.home.clone(),
            planned_at,
            backup_directory,
            backup_manifest_sha256: format!("sha256:{:x}", Sha256::digest(&manifest_bytes)),
            backup_manifest_device,
            backup_manifest_inode,
            entries,
            fingerprint: String::new(),
        };
        plan.fingerprint = fingerprint_retirement(&plan)?;
        Ok(plan)
    }

    /// Retire only entries whose descriptor-relative identity and digest still
    /// match the reviewed plan. Unknown artifacts are never candidates.
    pub fn apply_historical_retirement(
        &self,
        plan: &HistoricalRetirementPlan,
        operator_confirms_all_lattice_processes_stopped: bool,
    ) -> Result<HistoricalRetirementOutcome> {
        if !operator_confirms_all_lattice_processes_stopped {
            bail!("historical retirement requires explicit confirmation that every Lattice process using the repository is stopped; pre-contract binaries cannot be fenced");
        }
        self.require_offline()?;
        if plan.schema_version != 1
            || plan.repository_id != self.repository_id
            || plan.canonical_home != self.home
            || fingerprint_retirement(plan)? != plan.fingerprint
        {
            bail!("historical retirement plan is invalid or belongs to another repository home");
        }
        let backup_path = plan
            .backup_directory
            .canonicalize()
            .context("knowledge backup directory is unavailable")?;
        let backup = super::SecureDir::open(&backup_path)?;
        let manifest_meta = backup
            .metadata("manifest.json")?
            .context("knowledge backup manifest is unavailable")?;
        if (manifest_meta.identity.dev, manifest_meta.identity.ino)
            != (plan.backup_manifest_device, plan.backup_manifest_inode)
        {
            bail!("knowledge backup manifest identity changed after retirement planning");
        }
        if hash_secure_file(&backup, "manifest.json")? != plan.backup_manifest_sha256 {
            bail!("knowledge backup manifest changed after retirement planning");
        }
        let manifest: KnowledgeBackupManifest =
            serde_json::from_slice(&read_secure_file(&backup, "manifest.json")?)?;
        if manifest.database_file != "memories.db"
            || !backup
                .metadata(&manifest.database_file)?
                .is_some_and(|v| v.is_file)
            || hash_secure_file(&backup, &manifest.database_file)? != manifest.database_sha256
        {
            bail!("knowledge backup checksum mismatch");
        }
        let _owner = crate::memory::RepositoryMemoryOwner::acquire_in(
            &self.managed,
            Duration::from_secs(1),
        )?;
        let _maintenance = acquire_exclusive(&self.managed)?;
        self.require_offline()?;
        let managed = &*self.managed;
        let checkouts = managed.open_dir("checkouts")?;
        let trash = managed.create_dir("historical-retirement-trash")?;
        let conn = ManagedSqlite::open(
            &self.managed,
            "storage-registry.db",
            OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS historical_retirement_journal_v2(plan_fingerprint TEXT NOT NULL,checkout_id TEXT NOT NULL,file TEXT NOT NULL,trash_name TEXT NOT NULL UNIQUE,device INTEGER NOT NULL,inode INTEGER NOT NULL,bytes INTEGER NOT NULL,sha256 TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN('planned','moved','deleted')),PRIMARY KEY(plan_fingerprint,checkout_id,file));")?;
        let mut deleted = 0;
        let mut released: u64 = 0;
        let mut resumed = 0;
        for entry in &plan.entries {
            let trash_name = format!(
                "{}-{}-{}",
                &plan.fingerprint[7..23],
                entry.checkout_id,
                entry.file
            );
            let state:Option<String>=conn.query_row("SELECT state FROM historical_retirement_journal_v2 WHERE plan_fingerprint=?1 AND checkout_id=?2 AND file=?3",rusqlite::params![plan.fingerprint,entry.checkout_id,entry.file],|r|r.get(0)).optional()?;
            if state.as_deref() == Some("deleted") {
                resumed += 1;
                continue;
            }
            let checkout = checkouts.open_dir(&entry.checkout_id)?;
            conn.execute("INSERT OR IGNORE INTO historical_retirement_journal_v2 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'planned')",rusqlite::params![plan.fingerprint,entry.checkout_id,entry.file,trash_name,entry.device,entry.inode,entry.bytes,entry.sha256])?;
            let expected = super::ManagedIdentity {
                dev: entry.device,
                ino: entry.inode,
            };
            if state.as_deref() != Some("moved") {
                if let Some(current) = checkout.metadata(&entry.file)? {
                    if !current.is_file
                        || current.identity != expected
                        || current.len != entry.bytes
                    {
                        bail!(
                            "historical file identity changed after planning; retirement refused"
                        );
                    }
                    let mut file = checkout.open_file(&entry.file, false)?;
                    if hash_reader(&mut file)? != entry.sha256 {
                        bail!("historical file content changed after planning; retirement refused");
                    }
                    checkout.rename_to(&entry.file, &trash, &trash_name, current.identity)?;
                    operator_test_point("retirement_file_moved")?;
                } else if trash.metadata(&trash_name)?.map(|v| v.identity) != Some(expected) {
                    bail!("planned historical file disappeared outside its retirement journal");
                }
                conn.execute("UPDATE historical_retirement_journal_v2 SET state='moved' WHERE plan_fingerprint=?1 AND checkout_id=?2 AND file=?3",rusqlite::params![plan.fingerprint,entry.checkout_id,entry.file])?;
            }
            if let Some(moved) = trash.metadata(&trash_name)? {
                if !moved.is_file || moved.identity != expected {
                    bail!("historical retirement trash identity mismatch");
                }
                trash.remove_file(&trash_name, moved.identity)?;
                operator_test_point("retirement_file_deleted")?;
            }
            conn.execute("UPDATE historical_retirement_journal_v2 SET state='deleted' WHERE plan_fingerprint=?1 AND checkout_id=?2 AND file=?3",rusqlite::params![plan.fingerprint,entry.checkout_id,entry.file])?;
            deleted += 1;
            released = released.saturating_add(entry.bytes);
        }
        Ok(HistoricalRetirementOutcome {
            plan_fingerprint: plan.fingerprint.clone(),
            deleted_files: deleted,
            released_bytes: released,
            resumed_files: resumed,
        })
    }

    /// Create a coherent SQLite knowledge backup while every checkout and the
    /// repository memory owner are offline. The destination must not exist.
    pub fn backup_knowledge(
        &self,
        request: KnowledgeBackupRequest,
    ) -> Result<KnowledgeBackupManifest> {
        self.require_offline()?;
        if !self
            .managed
            .metadata("memories.db")?
            .is_some_and(|entry| entry.is_file)
        {
            bail!("memory authority must be an existing regular file");
        }
        let _owner = crate::memory::RepositoryMemoryOwner::acquire_in(
            &self.managed,
            Duration::from_secs(1),
        )?;
        let _maintenance = acquire_exclusive(&self.managed)?;
        self.require_offline()?;
        let home = &*self.managed;
        let (destination_parent_path, destination_name) = parent_and_leaf(&request.destination)?;
        let destination_parent = super::SecureDir::open(&destination_parent_path.canonicalize()?)?;
        if destination_parent.metadata(&destination_name)?.is_some() {
            bail!("backup destination exists; refusing overwrite");
        }
        let destination = destination_parent.create_dir(&destination_name)?;
        let destination_identity = destination.identity()?;
        operator_test_point("backup_destination_created")?;
        let temporary_name = format!("knowledge-backup-{}.tmp", std::process::id());
        if home.metadata(&temporary_name)?.is_some() {
            bail!("backup staging file exists; recovery required");
        }
        let result = (|| {
            let conn = ManagedSqlite::open(home, "memories.db", OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let temporary_path = conn.sibling_path(&temporary_name)?;
            conn.busy_timeout(Duration::from_millis(250))?;
            conn.execute(
                "VACUUM INTO ?1",
                [temporary_path.to_string_lossy().as_ref()],
            )?;
            let temporary_meta = home
                .metadata(&temporary_name)?
                .context("backup staging file was not created in the pinned repository home")?;
            let mut source = home.open_file(&temporary_name, false)?;
            let mut database = destination.open_new_file("memories.db")?;
            std::io::copy(&mut source, &mut database)?;
            database.sync_all()?;
            destination.sync()?;
            let backup = ManagedSqlite::open(
                &destination,
                "memories.db",
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            let integrity: String = backup.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
            if integrity != "ok" {
                bail!("knowledge backup failed integrity check: {integrity}");
            }
            let memory_count = table_count(&backup, "memories")?;
            let restore_floor = restore_floor(&backup)?;
            drop(backup);
            let manifest = KnowledgeBackupManifest {
                schema_version: 1,
                repository_id: self.repository_id.clone(),
                canonical_home: self.home.clone(),
                created_at: request.created_at,
                database_file: "memories.db".into(),
                database_sha256: hash_secure_file(&destination, "memories.db")?,
                memory_count,
                restore_floor,
            };
            let mut manifest_file = destination.open_new_file("manifest.json")?;
            manifest_file.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
            manifest_file.sync_all()?;
            destination.sync()?;
            home.remove_file(&temporary_name, temporary_meta.identity)?;
            if destination_parent
                .metadata(&destination_name)?
                .map(|v| v.identity)
                != Some(destination_identity)
            {
                bail!("backup destination identity changed during publication");
            }
            Ok(manifest)
        })();
        if result.is_err() {
            if let Ok(Some(meta)) = home.metadata(&temporary_name) {
                let _ = home.remove_file(&temporary_name, meta.identity);
            }
            for name in ["manifest.json", "memories.db"] {
                if let Ok(Some(meta)) = destination.metadata(name) {
                    let _ = destination.remove_file(name, meta.identity);
                }
            }
            if let Ok(Some(meta)) = destination_parent.metadata(&destination_name) {
                if meta.identity == destination_identity {
                    let _ = destination_parent.remove_dir(&destination_name, meta.identity);
                }
            }
        }
        result
    }

    /// Restore a verified backup as one closed-store replacement. The current
    /// authority remains at a rollback path until the replacement is durable.
    pub fn restore_knowledge(
        &self,
        request: KnowledgeRestoreRequest,
    ) -> Result<KnowledgeRestoreOutcome> {
        if !request.operator_confirms_all_lattice_processes_stopped {
            bail!("knowledge restore requires explicit confirmation that every Lattice process using the repository is stopped; pre-contract readers cannot be fenced");
        }
        self.require_offline()?;
        let backup_path = request
            .backup_directory
            .canonicalize()
            .context("backup directory is unavailable")?;
        let backup_dir = super::SecureDir::open(&backup_path)?;
        let manifest_bytes = read_secure_file(&backup_dir, "manifest.json")?;
        let manifest_sha256 = format!("sha256:{:x}", Sha256::digest(&manifest_bytes));
        let manifest: KnowledgeBackupManifest = serde_json::from_slice(&manifest_bytes)?;
        if manifest.schema_version != 1 || manifest.repository_id != self.repository_id {
            bail!("knowledge backup belongs to another repository or schema");
        }
        if manifest.database_file != "memories.db" {
            bail!("backup manifest database name is invalid");
        }
        let source_meta = backup_dir
            .metadata(&manifest.database_file)?
            .context("backup database is unavailable")?;
        let mut source_file = backup_dir.open_file(&manifest.database_file, false)?;
        if !source_meta.is_file || hash_reader(&mut source_file)? != manifest.database_sha256 {
            bail!("knowledge backup checksum mismatch");
        }
        source_file.rewind()?;
        let source_conn = ManagedSqlite::open(
            &backup_dir,
            &manifest.database_file,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let integrity: String =
            source_conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if integrity != "ok" {
            bail!("knowledge backup failed integrity check: {integrity}");
        }
        if table_count(&source_conn, "memories")? != manifest.memory_count {
            bail!("knowledge backup count does not match manifest");
        }
        operator_test_point("restore_source_verified")?;

        if !request.replace_existing {
            bail!("knowledge authority exists; pass explicit replace_existing after reviewing the backup");
        }
        let _owner = crate::memory::RepositoryMemoryOwner::acquire_in(
            &self.managed,
            Duration::from_secs(1),
        )?;
        let _maintenance = acquire_exclusive(&self.managed)?;
        self.require_offline()?;
        let home = &*self.managed;
        let registry = ManagedSqlite::open(
            &self.managed,
            "storage-registry.db",
            OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        ensure_knowledge_restore_schema(&registry)?;
        if recover_knowledge_restore(&home, &registry, &manifest_sha256)? {
            let restored = ManagedSqlite::open(
                home,
                "memories.db",
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            let floor = restore_floor(&restored)?;
            return Ok(KnowledgeRestoreOutcome {
                restored: true,
                database_sha256: manifest.database_sha256,
                memory_count: table_count(&restored, "memories")?,
                restore_floor: floor,
            });
        }
        let target_meta = home.metadata("memories.db")?.context(
            "restore requires the current knowledge authority to enforce its purge floor",
        )?;
        if !target_meta.is_file {
            bail!("memory authority is not a regular file");
        }
        let current = ManagedSqlite::open(
            home,
            "memories.db",
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        current.busy_timeout(Duration::from_millis(250))?;
        current.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        current
            .execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE; COMMIT;")
            .context("knowledge restore could not fence a live SQLite reader")?;
        crate::memory::retention::validate_restore_time(&current, manifest.created_at)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        // A retained receipt is direct proof that this memory must not return.
        let mut receipts = current.prepare("SELECT memory_id,deleted_at FROM memory_deletion_receipts ORDER BY memory_id LIMIT 100001")?;
        let ids = receipts
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if ids.len() > 100_000 {
            bail!("current deletion receipt set exceeds restore verification cap");
        }
        for (id, _) in &ids {
            let exists: bool = source_conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM memories WHERE id=?1)",
                [&id],
                |r| r.get(0),
            )?;
            if exists {
                bail!("backup would restore purged knowledge `{id}`; restore refused");
            }
        }
        let current_floor = restore_floor(&current)?;
        drop(receipts);
        drop(current);
        drop(source_conn);
        for sidecar in ["memories.db-wal", "memories.db-shm"] {
            if let Some(meta) = home.metadata(sidecar)? {
                if !meta.is_file {
                    bail!("memory sidecar is not a regular file");
                }
                home.remove_file(sidecar, meta.identity)?;
            }
        }
        const STAGING: &str = "memories.restore.staging";
        const ROLLBACK: &str = "memories.restore.rollback";
        if home.metadata(STAGING)?.is_some() || home.metadata(ROLLBACK)?.is_some() {
            bail!("restore recovery left unexpected staging artifacts");
        }
        registry.execute(
            "INSERT OR REPLACE INTO knowledge_restore_journal(id,state,staging_name,rollback_name,staging_sha256,source_manifest_sha256) VALUES(1,'staging',?1,?2,'',?3)",
            rusqlite::params![STAGING, ROLLBACK, manifest_sha256],
        )?;
        if source_file.metadata()?.len() != source_meta.len {
            bail!("backup database changed before copy");
        }
        let mut staged_file = home.open_new_file(STAGING)?;
        std::io::copy(&mut source_file, &mut staged_file)?;
        staged_file.sync_all()?;
        home.sync()?;
        operator_test_point("restore_staging_created")?;
        {
            let mut staged = ManagedSqlite::open(home, STAGING, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
            let tx = staged.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute("UPDATE memory_retention_control SET restore_floor=MAX(restore_floor,?1) WHERE id=1", [current_floor])?;
            for (id, deleted_at) in &ids {
                tx.execute("INSERT INTO memory_deletion_receipts(memory_id,deleted_at) VALUES(?1,?2) ON CONFLICT(memory_id) DO UPDATE SET deleted_at=MAX(deleted_at,excluded.deleted_at)", rusqlite::params![id, deleted_at])?;
            }
            tx.commit()?;
            staged.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        }
        let staging_meta = home
            .metadata(STAGING)?
            .context("restore staging disappeared")?;
        let staging_hash = hash_secure_file(&home, STAGING)?;
        registry.execute(
            "UPDATE knowledge_restore_journal SET state='prepared',staging_sha256=?1 WHERE id=1",
            [&staging_hash],
        )?;
        operator_test_point("restore_prepared")?;
        home.rename_to("memories.db", &home, ROLLBACK, target_meta.identity)?;
        operator_test_point("restore_old_renamed")?;
        registry.execute(
            "UPDATE knowledge_restore_journal SET state='old_moved' WHERE id=1",
            [],
        )?;
        home.rename_to(STAGING, &home, "memories.db", staging_meta.identity)?;
        operator_test_point("restore_new_renamed")?;
        registry.execute(
            "UPDATE knowledge_restore_journal SET state='committed' WHERE id=1",
            [],
        )?;
        operator_test_point("restore_committed")?;
        validate_restored_database(home, &staging_hash)?;
        let rollback_meta = home
            .metadata(ROLLBACK)?
            .context("restore rollback authority disappeared")?;
        home.remove_file(ROLLBACK, rollback_meta.identity)?;
        registry.execute("DELETE FROM knowledge_restore_journal WHERE id=1", [])?;
        Ok(KnowledgeRestoreOutcome {
            restored: true,
            database_sha256: manifest.database_sha256,
            memory_count: manifest.memory_count,
            restore_floor: current_floor.max(manifest.restore_floor),
        })
    }

    fn require_offline(&self) -> Result<()> {
        // Cached activity counts are diagnostic only. Stream current leases;
        // callers repeat this while holding maintenance, which fences new
        // registration, before touching an offline authority.
        let connection = self.readonly_registry()?;
        let leases = self.managed.open_dir("leases")?;
        let mut statement =
            connection.prepare("SELECT checkout_id FROM checkout_registry ORDER BY checkout_id")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        for id in rows {
            let id = id?;
            if id.len() != 73
                || !id.starts_with("checkout_")
                || !id[9..].bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                bail!("invalid checkout lease authority in storage registry");
            }
            let name = format!("{id}.lock");
            if leases.metadata(&name)?.is_none() {
                continue;
            }
            let lease = leases.open_file(&name, false)?;
            match lease.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    bail!("offline storage operation rejected: active checkout lease {id}")
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn readonly_registry(&self) -> Result<ManagedSqlite> {
        Ok(ManagedSqlite::open(
            &self.managed,
            "storage-registry.db",
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?)
    }

    fn account_classes(&self, _conn: &Connection) -> Result<StorageClasses> {
        let mut classes = StorageClasses::default();
        for name in ["memories.db", "memories.db-wal", "memories.db-shm"] {
            add_file(
                &mut classes.durable_knowledge,
                &self.managed,
                name,
                name.ends_with("-wal"),
            )?;
        }
        // The cache class comes from the positive lifecycle allowlist.
        let inventory =
            StorageRegistry::inspect_in(self.managed.clone(), &self.repository_id, false)?
                .inventory(0, &CachePolicy::default())?;
        classes.telemetry = ByteAccounting {
            logical_bytes: inventory.telemetry_logical_bytes,
            allocated_bytes: inventory.telemetry_allocated_bytes,
            wal_bytes: inventory.telemetry_wal_bytes,
        };
        classes.disposable_cache = ByteAccounting {
            logical_bytes: inventory
                .derived_logical_bytes
                .saturating_add(inventory.shared_cache_logical_bytes),
            allocated_bytes: inventory
                .derived_allocated_bytes
                .saturating_add(inventory.shared_cache_allocated_bytes),
            wal_bytes: inventory
                .wal_bytes
                .saturating_add(inventory.shared_cache_wal_bytes),
        };
        Ok(classes)
    }
}

fn same_candidates(left: &[GcCandidate], right: &[GcCandidate]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(a, b)| a.checkout_id == b.checkout_id && a.bytes == b.bytes)
}

fn fingerprint_plan(plan: &CacheMaintenancePlan) -> Result<String> {
    let mut unsigned = plan.clone();
    unsigned.fingerprint.clear();
    let encoded = serde_json::to_vec(&unsigned)?;
    Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
}

fn fingerprint_retirement(plan: &HistoricalRetirementPlan) -> Result<String> {
    let mut unsigned = plan.clone();
    unsigned.fingerprint.clear();
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&unsigned)?)
    ))
}

fn hash_reader(reader: &mut File) -> Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn read_secure_file(directory: &super::SecureDir, name: &str) -> Result<Vec<u8>> {
    let mut file = directory.open_file(name, false)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn hash_secure_file(directory: &super::SecureDir, name: &str) -> Result<String> {
    let mut file = directory.open_file(name, false)?;
    hash_reader(&mut file)
}

fn parent_and_leaf(path: &Path) -> Result<(PathBuf, String)> {
    let parent = path
        .parent()
        .context("storage destination has no parent")?
        .to_path_buf();
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .context("storage destination name is not valid UTF-8")?
        .to_owned();
    if name.is_empty() || name == "." || name == ".." {
        bail!("storage destination name is invalid");
    }
    Ok((parent, name))
}

fn validate_restored_database(home: &super::SecureDir, expected_hash: &str) -> Result<()> {
    let conn = ManagedSqlite::open(
        home,
        "memories.db",
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        bail!("restored knowledge authority failed integrity check: {integrity}");
    }
    drop(conn);
    if hash_secure_file(home, "memories.db")? != expected_hash {
        bail!("restored knowledge authority digest mismatch");
    }
    Ok(())
}

fn ensure_knowledge_restore_schema(registry: &Connection) -> Result<()> {
    registry.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS knowledge_restore_journal(id INTEGER PRIMARY KEY CHECK(id=1),state TEXT NOT NULL CHECK(state IN('staging','prepared','old_moved','committed')),staging_name TEXT NOT NULL,rollback_name TEXT NOT NULL,staging_sha256 TEXT NOT NULL,source_manifest_sha256 TEXT);")?;
    let columns = registry
        .prepare("PRAGMA table_info(knowledge_restore_journal)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns
        .iter()
        .any(|column| column == "source_manifest_sha256")
    {
        registry.execute(
            "ALTER TABLE knowledge_restore_journal ADD COLUMN source_manifest_sha256 TEXT",
            [],
        )?;
    }
    Ok(())
}

fn recover_knowledge_restore(
    home: &super::SecureDir,
    registry: &Connection,
    requested_manifest_sha256: &str,
) -> Result<bool> {
    let row:Option<(String,String,String,String,Option<String>)>=registry.query_row("SELECT state,staging_name,rollback_name,staging_sha256,source_manifest_sha256 FROM knowledge_restore_journal WHERE id=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    let Some((mut state, staging, rollback, expected_hash, source_manifest_sha256)) = row else {
        return Ok(false);
    };
    let source_manifest_sha256 = source_manifest_sha256.context(
        "interrupted knowledge restore predates source-manifest binding; automatic recovery refused",
    )?;
    if source_manifest_sha256 != requested_manifest_sha256 {
        bail!("interrupted knowledge restore belongs to a different backup manifest; retry with the original backup");
    }
    if staging != "memories.restore.staging" || rollback != "memories.restore.rollback" {
        bail!("invalid knowledge restore journal ownership");
    }
    let mut target = home.metadata("memories.db")?;
    let mut staged = home.metadata(&staging)?;
    let mut old = home.metadata(&rollback)?;
    if state == "staging" {
        if old.is_some() || target.is_none() {
            bail!("knowledge restore staging journal/filesystem state is inconsistent");
        }
        if let Some(candidate) = staged {
            home.remove_file(&staging, candidate.identity)?;
        }
        registry.execute("DELETE FROM knowledge_restore_journal WHERE id=1", [])?;
        return Ok(false);
    }
    if state == "prepared" {
        match (target.as_ref(), staged.as_ref(), old.as_ref()) {
            (Some(current), Some(_), None) => {
                home.rename_to("memories.db", home, &rollback, current.identity)?;
                state = "old_moved".into();
                registry.execute(
                    "UPDATE knowledge_restore_journal SET state='old_moved' WHERE id=1",
                    [],
                )?;
            }
            (None, Some(_), Some(_)) => {
                state = "old_moved".into();
                registry.execute(
                    "UPDATE knowledge_restore_journal SET state='old_moved' WHERE id=1",
                    [],
                )?;
            }
            (Some(_), None, Some(_)) => {
                state = "committed".into();
                registry.execute(
                    "UPDATE knowledge_restore_journal SET state='committed' WHERE id=1",
                    [],
                )?;
            }
            _ => bail!("knowledge restore journal/filesystem state is inconsistent"),
        }
        target = home.metadata("memories.db")?;
        staged = home.metadata(&staging)?;
        old = home.metadata(&rollback)?;
    }
    if state == "old_moved" {
        if target.is_none() {
            let candidate = staged
                .as_ref()
                .context("knowledge restore staging disappeared after old authority moved")?;
            home.rename_to(&staging, home, "memories.db", candidate.identity)?;
        } else if staged.is_some() {
            bail!("knowledge restore has both target and staging after old authority moved");
        }
        registry.execute(
            "UPDATE knowledge_restore_journal SET state='committed' WHERE id=1",
            [],
        )?;
        state = "committed".into();
        target = home.metadata("memories.db")?;
        old = home.metadata(&rollback)?;
    }
    if state == "committed" {
        if let Err(error) = validate_restored_database(home, &expected_hash) {
            if let Some(current) = target {
                home.remove_file("memories.db", current.identity)?;
            }
            let rollback_entry = old.context("invalid restored authority has no rollback")?;
            home.rename_to(&rollback, home, "memories.db", rollback_entry.identity)?;
            registry.execute("DELETE FROM knowledge_restore_journal WHERE id=1", [])?;
            bail!("interrupted knowledge restore was rolled back: {error}");
        }
        if let Some(rollback_entry) = old {
            home.remove_file(&rollback, rollback_entry.identity)?;
        }
        registry.execute("DELETE FROM knowledge_restore_journal WHERE id=1", [])?;
    }
    Ok(true)
}

#[cfg(test)]
fn operator_test_point(point: &'static str) -> Result<()> {
    if let Some(action) = OPERATOR_ACTION.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|v| v.0 == point) {
            slot.take().map(|v| v.1)
        } else {
            None
        }
    }) {
        action();
    }
    let fail = OPERATOR_FAILURE_POINT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if *slot == Some(point) {
            slot.take();
            true
        } else {
            false
        }
    });
    if fail {
        bail!("injected operator failure at {point}");
    }
    Ok(())
}
#[cfg(not(test))]
fn operator_test_point(_: &'static str) -> Result<()> {
    Ok(())
}

fn add_file(
    account: &mut ByteAccounting,
    directory: &super::SecureDir,
    name: &str,
    wal: bool,
) -> Result<()> {
    if let Some(meta) = directory.metadata(name)? {
        if !meta.is_file {
            bail!("managed storage artifact is not a regular file: {name}");
        }
        account.logical_bytes = account.logical_bytes.saturating_add(meta.len);
        account.allocated_bytes = account.allocated_bytes.saturating_add(meta.allocated);
        if wal {
            account.wal_bytes = account.wal_bytes.saturating_add(meta.len);
        }
    }
    Ok(())
}

fn table_count(conn: &Connection, table: &str) -> Result<u64> {
    if table != "memories" {
        bail!("unsupported backup table");
    }
    Ok(conn.query_row("SELECT count(*) FROM memories", [], |r| r.get(0))?)
}

fn restore_floor(conn: &Connection) -> Result<u64> {
    Ok(conn.query_row(
        "SELECT restore_floor FROM memory_retention_control WHERE id=1",
        [],
        |r| r.get(0),
    )?)
}

fn acquire_exclusive(home: &super::SecureDir) -> Result<File> {
    let file = home.open_or_create_file("maintenance.lock")?;
    file.try_lock()
        .map_err(|e| anyhow::anyhow!("repository maintenance is already running: {e}"))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(i: usize) -> String {
        format!("checkout_{i:064x}")
    }

    #[test]
    fn status_is_read_only_plan_advances_accounting_and_apply_revalidates_lease() {
        let home = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(home.path(), "repo").unwrap();
        let lease = registry
            .register_and_lease(&id(1), checkout.path(), 1)
            .unwrap();
        fs::write(
            home.path()
                .join("checkouts")
                .join(id(1))
                .join("cache/graph.db"),
            vec![1; 8192],
        )
        .unwrap();
        drop(lease);
        let operator = StorageOperator::open_existing(home.path(), "repo").unwrap();
        let policy = CachePolicy {
            high_bytes: 1,
            low_bytes: 0,
            idle_grace_secs: 1,
            batch_files: 4,
        };
        let before = fs::read(
            home.path()
                .join("checkouts")
                .join(id(1))
                .join("cache/graph.db"),
        )
        .unwrap();
        let status = operator
            .status(10, &policy, AccountingLimits::default())
            .unwrap();
        assert!(!status.complete);
        assert!(status.inventory.pressure_unknown);
        let plan = operator.plan_cache_maintenance(10, policy.clone()).unwrap();
        let status = operator
            .status(10, &policy, AccountingLimits::default())
            .unwrap();
        assert!(status.complete);
        assert_eq!(status.retained_checkouts(), 1);
        assert_eq!(
            before,
            fs::read(
                home.path()
                    .join("checkouts")
                    .join(id(1))
                    .join("cache/graph.db")
            )
            .unwrap()
        );
        let live = registry
            .register_and_lease(&id(1), checkout.path(), 11)
            .unwrap();
        assert!(operator.apply_cache_maintenance(&plan, 11).is_err());
        assert!(home
            .path()
            .join("checkouts")
            .join(id(1))
            .join("cache/graph.db")
            .exists());
        drop(live);
    }

    #[test]
    fn apply_rejects_tampered_and_foreign_plans() {
        let home = tempfile::tempdir().unwrap();
        StorageRegistry::open(home.path(), "repo").unwrap();
        let operator = StorageOperator::open_existing(home.path(), "repo").unwrap();
        let mut plan = operator
            .plan_cache_maintenance(1, CachePolicy::default())
            .unwrap();
        plan.repository_id = "other".into();
        assert!(operator.apply_cache_maintenance(&plan, 1).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn existing_home_and_registry_symlinks_are_refused() {
        use std::os::unix::fs::symlink;
        let real = tempfile::tempdir().unwrap();
        StorageRegistry::open(real.path(), "repo").unwrap();
        let parent = tempfile::tempdir().unwrap();
        symlink(real.path(), parent.path().join("linked")).unwrap();
        assert!(StorageOperator::open_existing(&parent.path().join("linked"), "repo").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn verified_operator_keeps_original_home_pinned_across_path_replacement() {
        let parent = tempfile::tempdir().unwrap();
        let home = parent.path().join("home");
        fs::create_dir(&home).unwrap();
        StorageRegistry::open(&home, "repo").unwrap();
        let operator = StorageOperator::open_existing(&home, "repo").unwrap();
        let displaced = parent.path().join("displaced");
        fs::rename(&home, &displaced).unwrap();
        fs::create_dir(&home).unwrap();
        fs::write(home.join("marker"), b"replacement").unwrap();

        operator.open_registry().unwrap();

        assert_eq!(fs::read(home.join("marker")).unwrap(), b"replacement");
        assert!(!home.join("storage-registry.db").exists());
        assert!(displaced.join("storage-registry.db").exists());
    }

    #[test]
    fn backup_is_coherent_no_overwrite_and_restore_honors_floor() {
        let home = tempfile::tempdir().unwrap();
        StorageRegistry::open(home.path(), "repo").unwrap();
        let memory = home.path().join("memories.db");
        drop(crate::memory::MemoryStore::open(&memory).unwrap());
        let operator = StorageOperator::open_existing(home.path(), "repo").unwrap();
        let backup = home.path().join("backup");
        let manifest = operator
            .backup_knowledge(KnowledgeBackupRequest {
                destination: backup.clone(),
                created_at: 100,
            })
            .unwrap();
        assert_eq!(manifest.memory_count, 0);
        assert!(operator
            .backup_knowledge(KnowledgeBackupRequest {
                destination: backup.clone(),
                created_at: 100,
            })
            .is_err());
        let current = Connection::open(&memory).unwrap();
        current
            .execute(
                "UPDATE memory_retention_control SET restore_floor=101 WHERE id=1",
                [],
            )
            .unwrap();
        drop(current);
        assert!(operator
            .restore_knowledge(KnowledgeRestoreRequest {
                backup_directory: backup,
                replace_existing: true,
                operator_confirms_all_lattice_processes_stopped: true,
            })
            .unwrap_err()
            .to_string()
            .contains("restore floor"));
    }

    #[test]
    fn backup_and_restore_keep_the_open_repository_authority_after_directory_replacement() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let moved = root.path().join("moved");
        StorageRegistry::open(&home, "repo").unwrap();
        drop(crate::memory::MemoryStore::open(&home.join("memories.db")).unwrap());
        let operator = StorageOperator::open_existing(&home, "repo").unwrap();
        fs::rename(&home, &moved).unwrap();
        fs::create_dir(&home).unwrap();
        fs::write(home.join("memories.db"), b"foreign replacement").unwrap();
        fs::write(home.join("maintenance.lock"), b"foreign lock").unwrap();
        let backup = root.path().join("backup");
        operator
            .backup_knowledge(KnowledgeBackupRequest {
                destination: backup.clone(),
                created_at: 1,
            })
            .unwrap();
        operator
            .restore_knowledge(KnowledgeRestoreRequest {
                backup_directory: backup,
                replace_existing: true,
                operator_confirms_all_lattice_processes_stopped: true,
            })
            .unwrap();
        assert_eq!(
            fs::read(home.join("memories.db")).unwrap(),
            b"foreign replacement"
        );
        assert_eq!(
            fs::read(home.join("maintenance.lock")).unwrap(),
            b"foreign lock"
        );
        assert!(!home.join("storage-registry.db").exists());
        let restored = Connection::open(moved.join("memories.db")).unwrap();
        assert_eq!(
            restored
                .query_row::<String, _, _>("PRAGMA integrity_check", [], |row| row.get(0))
                .unwrap(),
            "ok"
        );
    }

    #[test]
    fn backup_rejects_active_checkout_lease() {
        let home = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(home.path(), "repo").unwrap();
        drop(crate::memory::MemoryStore::open(&home.path().join("memories.db")).unwrap());
        let _lease = registry
            .register_and_lease(&id(2), checkout.path(), 1)
            .unwrap();
        let operator = StorageOperator::open_existing(home.path(), "repo").unwrap();
        assert!(operator
            .backup_knowledge(KnowledgeBackupRequest {
                destination: home.path().join("backup"),
                created_at: 2,
            })
            .unwrap_err()
            .to_string()
            .contains("active checkout lease"));
    }

    #[cfg(unix)]
    #[test]
    fn backup_destination_swap_cannot_write_or_cleanup_outside() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        StorageRegistry::open(home.path(), "repo").unwrap();
        drop(crate::memory::MemoryStore::open(&home.path().join("memories.db")).unwrap());
        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("protected"), b"keep").unwrap();
        let destination = parent.path().join("backup");
        let moved = parent.path().join("moved");
        let outside_path = outside.path().to_path_buf();
        let destination_for_hook = destination.clone();
        OPERATOR_ACTION.with(|slot| {
            *slot.borrow_mut() = Some((
                "backup_destination_created",
                Box::new(move || {
                    fs::rename(&destination_for_hook, &moved).unwrap();
                    symlink(&outside_path, &destination_for_hook).unwrap();
                }),
            ))
        });
        let operator = StorageOperator::open_existing(home.path(), "repo").unwrap();
        assert!(operator
            .backup_knowledge(KnowledgeBackupRequest {
                destination,
                created_at: 1
            })
            .is_err());
        assert_eq!(fs::read(outside.path().join("protected")).unwrap(), b"keep");
        assert!(!outside.path().join("memories.db").exists());
    }

    #[test]
    fn backup_failure_preserves_an_empty_destination_replacement() {
        let home = tempfile::tempdir().unwrap();
        StorageRegistry::open(home.path(), "repo").unwrap();
        drop(crate::memory::MemoryStore::open(&home.path().join("memories.db")).unwrap());
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("backup");
        let moved = parent.path().join("moved");
        let destination_for_hook = destination.clone();
        OPERATOR_ACTION.with(|slot| {
            *slot.borrow_mut() = Some((
                "backup_destination_created",
                Box::new(move || {
                    fs::rename(&destination_for_hook, &moved).unwrap();
                    fs::create_dir(&destination_for_hook).unwrap();
                }),
            ))
        });
        let operator = StorageOperator::open_existing(home.path(), "repo").unwrap();
        assert!(operator
            .backup_knowledge(KnowledgeBackupRequest {
                destination: destination.clone(),
                created_at: 1,
            })
            .is_err());
        assert!(destination.is_dir());
    }

    #[test]
    fn interrupted_restore_rejects_a_different_backup_manifest() {
        let home = tempfile::tempdir().unwrap();
        StorageRegistry::open(home.path(), "repo").unwrap();
        drop(crate::memory::MemoryStore::open(&home.path().join("memories.db")).unwrap());
        let operator = StorageOperator::open_existing(home.path(), "repo").unwrap();
        let backup_a = home.path().join("backup-a");
        let backup_b = home.path().join("backup-b");
        operator
            .backup_knowledge(KnowledgeBackupRequest {
                destination: backup_a.clone(),
                created_at: 1,
            })
            .unwrap();
        operator
            .backup_knowledge(KnowledgeBackupRequest {
                destination: backup_b.clone(),
                created_at: 2,
            })
            .unwrap();
        let request_a = KnowledgeRestoreRequest {
            backup_directory: backup_a,
            replace_existing: true,
            operator_confirms_all_lattice_processes_stopped: true,
        };
        OPERATOR_FAILURE_POINT.with(|slot| *slot.borrow_mut() = Some("restore_prepared"));
        assert!(operator
            .restore_knowledge(request_a.clone())
            .unwrap_err()
            .to_string()
            .contains("injected"));
        let mismatch = operator
            .restore_knowledge(KnowledgeRestoreRequest {
                backup_directory: backup_b,
                replace_existing: true,
                operator_confirms_all_lattice_processes_stopped: true,
            })
            .unwrap_err()
            .to_string();
        assert!(mismatch.contains("different backup manifest"), "{mismatch}");
        operator.restore_knowledge(request_a).unwrap();
    }

    #[test]
    fn legacy_unbound_restore_journal_is_migrated_and_fails_closed() {
        let home = tempfile::tempdir().unwrap();
        StorageRegistry::open(home.path(), "repo").unwrap();
        drop(crate::memory::MemoryStore::open(&home.path().join("memories.db")).unwrap());
        let operator = StorageOperator::open_existing(home.path(), "repo").unwrap();
        let backup = home.path().join("backup");
        operator
            .backup_knowledge(KnowledgeBackupRequest {
                destination: backup.clone(),
                created_at: 1,
            })
            .unwrap();
        let registry = Connection::open(home.path().join("storage-registry.db")).unwrap();
        registry.execute_batch("CREATE TABLE knowledge_restore_journal(id INTEGER PRIMARY KEY CHECK(id=1),state TEXT NOT NULL CHECK(state IN('staging','prepared','old_moved','committed')),staging_name TEXT NOT NULL,rollback_name TEXT NOT NULL,staging_sha256 TEXT NOT NULL); INSERT INTO knowledge_restore_journal VALUES(1,'staging','memories.restore.staging','memories.restore.rollback','');").unwrap();
        drop(registry);
        let error = operator
            .restore_knowledge(KnowledgeRestoreRequest {
                backup_directory: backup,
                replace_existing: true,
                operator_confirms_all_lattice_processes_stopped: true,
            })
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("predates source-manifest binding"),
            "{error}"
        );
        assert!(home.path().join("memories.db").is_file());
        let registry = Connection::open(home.path().join("storage-registry.db")).unwrap();
        let has_binding_column: bool = registry
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('knowledge_restore_journal') WHERE name='source_manifest_sha256')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(has_binding_column);
    }

    #[test]
    fn restore_recovers_every_durable_rename_boundary() {
        for point in [
            "restore_staging_created",
            "restore_prepared",
            "restore_old_renamed",
            "restore_new_renamed",
            "restore_committed",
        ] {
            let home = tempfile::tempdir().unwrap();
            StorageRegistry::open(home.path(), "repo").unwrap();
            drop(crate::memory::MemoryStore::open(&home.path().join("memories.db")).unwrap());
            let operator = StorageOperator::open_existing(home.path(), "repo").unwrap();
            let backup = home.path().join("backup");
            operator
                .backup_knowledge(KnowledgeBackupRequest {
                    destination: backup.clone(),
                    created_at: 1,
                })
                .unwrap();
            let request = KnowledgeRestoreRequest {
                backup_directory: backup,
                replace_existing: true,
                operator_confirms_all_lattice_processes_stopped: true,
            };
            OPERATOR_FAILURE_POINT.with(|slot| *slot.borrow_mut() = Some(point));
            assert!(operator
                .restore_knowledge(request.clone())
                .unwrap_err()
                .to_string()
                .contains("injected"));
            operator.restore_knowledge(request).unwrap();
            assert!(!home.path().join("memories.restore.staging").exists());
            assert!(!home.path().join("memories.restore.rollback").exists());
            let db = Connection::open(home.path().join("memories.db")).unwrap();
            assert_eq!(
                db.query_row::<String, _, _>("PRAGMA integrity_check", [], |r| r.get(0))
                    .unwrap(),
                "ok"
            );
        }
    }

    #[test]
    fn restore_refuses_a_held_sqlite_reader() {
        let home = tempfile::tempdir().unwrap();
        StorageRegistry::open(home.path(), "repo").unwrap();
        let memory = home.path().join("memories.db");
        drop(crate::memory::MemoryStore::open(&memory).unwrap());
        let operator = StorageOperator::open_existing(home.path(), "repo").unwrap();
        let backup = home.path().join("backup");
        operator
            .backup_knowledge(KnowledgeBackupRequest {
                destination: backup.clone(),
                created_at: 1,
            })
            .unwrap();
        let reader = Connection::open(&memory).unwrap();
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM memories;")
            .unwrap();
        let error = operator
            .restore_knowledge(KnowledgeRestoreRequest {
                backup_directory: backup,
                replace_existing: true,
                operator_confirms_all_lattice_processes_stopped: true,
            })
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("live SQLite reader")
                || error.contains("locked")
                || error.contains("busy"),
            "{error}"
        );
        reader.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn restore_removes_old_wal_generation_before_replacement() {
        let home = tempfile::tempdir().unwrap();
        StorageRegistry::open(home.path(), "repo").unwrap();
        let memory = home.path().join("memories.db");
        drop(crate::memory::MemoryStore::open(&memory).unwrap());
        let operator = StorageOperator::open_existing(home.path(), "repo").unwrap();
        let backup = home.path().join("backup");
        operator
            .backup_knowledge(KnowledgeBackupRequest {
                destination: backup.clone(),
                created_at: 1,
            })
            .unwrap();
        let writer = Connection::open(&memory).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE stale_wal_marker(value TEXT); INSERT INTO stale_wal_marker VALUES('old');").unwrap();
        let saved_db = home.path().join("saved.db");
        let saved_wal = home.path().join("saved.wal");
        fs::copy(&memory, &saved_db).unwrap();
        fs::copy(home.path().join("memories.db-wal"), &saved_wal).unwrap();
        drop(writer);
        fs::copy(saved_db, &memory).unwrap();
        fs::copy(saved_wal, home.path().join("memories.db-wal")).unwrap();
        operator
            .restore_knowledge(KnowledgeRestoreRequest {
                backup_directory: backup,
                replace_existing: true,
                operator_confirms_all_lattice_processes_stopped: true,
            })
            .unwrap();
        let restored = Connection::open(&memory).unwrap();
        let marker: bool = restored
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='stale_wal_marker')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!marker);
        assert!(!home.path().join("memories.db-wal").exists());
    }

    #[cfg(unix)]
    #[test]
    fn restore_copies_the_verified_open_file_after_source_path_swap() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        StorageRegistry::open(home.path(), "repo").unwrap();
        drop(crate::memory::MemoryStore::open(&home.path().join("memories.db")).unwrap());
        let operator = StorageOperator::open_existing(home.path(), "repo").unwrap();
        let backup = home.path().join("backup");
        operator
            .backup_knowledge(KnowledgeBackupRequest {
                destination: backup.clone(),
                created_at: 1,
            })
            .unwrap();
        let original = backup.join("memories.db");
        let parked = backup.join("verified.db");
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), b"not sqlite").unwrap();
        let outside_path = outside.path().to_path_buf();
        let request = KnowledgeRestoreRequest {
            backup_directory: backup.clone(),
            replace_existing: true,
            operator_confirms_all_lattice_processes_stopped: true,
        };
        OPERATOR_ACTION.with(|slot| {
            *slot.borrow_mut() = Some((
                "restore_source_verified",
                Box::new(move || {
                    fs::rename(&original, &parked).unwrap();
                    symlink(&outside_path, &original).unwrap();
                }),
            ))
        });
        operator.restore_knowledge(request).unwrap();
        let db = Connection::open(home.path().join("memories.db")).unwrap();
        assert_eq!(
            db.query_row::<String, _, _>("PRAGMA integrity_check", [], |r| r.get(0))
                .unwrap(),
            "ok"
        );
    }

    #[test]
    fn historical_retirement_requires_backup_and_deletes_only_exact_derived_files() {
        let home = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(home.path(), "repo").unwrap();
        drop(crate::memory::MemoryStore::open(&home.path().join("memories.db")).unwrap());
        let checkout_id = id(9);
        let lease = registry
            .register_and_lease(&checkout_id, checkout.path(), 1)
            .unwrap();
        let flat = home.path().join("checkouts").join(&checkout_id);
        fs::write(flat.join("graph.db"), b"derived").unwrap();
        fs::write(flat.join("operator-notes.txt"), b"preserve").unwrap();
        drop(lease);
        let operator = StorageOperator::open_existing(home.path(), "repo").unwrap();
        assert!(operator
            .plan_historical_retirement(home.path().join("missing"), 2)
            .is_err());
        let backup = home.path().join("knowledge-backup");
        operator
            .backup_knowledge(KnowledgeBackupRequest {
                destination: backup.clone(),
                created_at: 2,
            })
            .unwrap();
        let plan = operator.plan_historical_retirement(backup, 3).unwrap();
        assert_eq!(plan.entries.len(), 1);
        assert!(operator.apply_historical_retirement(&plan, false).is_err());
        let outcome = operator.apply_historical_retirement(&plan, true).unwrap();
        assert_eq!(outcome.deleted_files, 1);
        assert!(!flat.join("graph.db").exists());
        assert_eq!(
            fs::read(flat.join("operator-notes.txt")).unwrap(),
            b"preserve"
        );
        let resumed = operator.apply_historical_retirement(&plan, true).unwrap();
        assert_eq!(resumed.resumed_files, 1);
    }

    #[test]
    fn historical_retirement_replays_move_and_delete_crash_windows() {
        for point in ["retirement_file_moved", "retirement_file_deleted"] {
            let home = tempfile::tempdir().unwrap();
            let checkout = tempfile::tempdir().unwrap();
            let mut registry = StorageRegistry::open(home.path(), "repo").unwrap();
            drop(crate::memory::MemoryStore::open(&home.path().join("memories.db")).unwrap());
            let checkout_id = id(19);
            let lease = registry
                .register_and_lease(&checkout_id, checkout.path(), 1)
                .unwrap();
            let flat = home.path().join("checkouts").join(&checkout_id);
            fs::write(flat.join("graph.db"), b"derived").unwrap();
            drop(lease);
            let operator = StorageOperator::open_existing(home.path(), "repo").unwrap();
            let backup = home.path().join("backup");
            operator
                .backup_knowledge(KnowledgeBackupRequest {
                    destination: backup.clone(),
                    created_at: 1,
                })
                .unwrap();
            let plan = operator.plan_historical_retirement(backup, 2).unwrap();
            OPERATOR_FAILURE_POINT.with(|slot| *slot.borrow_mut() = Some(point));
            assert!(operator.apply_historical_retirement(&plan, true).is_err());
            let result = operator.apply_historical_retirement(&plan, true).unwrap();
            assert_eq!(result.deleted_files, 1);
            assert!(!flat.join("graph.db").exists());
        }
    }

    impl StorageStatus {
        fn retained_checkouts(&self) -> usize {
            self.inventory.retained_checkouts
        }
    }
}
