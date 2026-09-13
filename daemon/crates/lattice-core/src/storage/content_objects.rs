//! Immutable repository-shared symbol bodies and their reachability metadata.
use super::managed_fs::SecureDir;
use super::managed_sqlite::ManagedSqlite;
use crate::error::LatticeError;
use rusqlite::{params, Connection, TransactionBehavior};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct ContentObjectStore {
    managed: Arc<SecureDir>,
    refs: Arc<Mutex<ManagedSqlite>>,
}
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ObjectGcReport {
    pub examined: usize,
    pub removed: usize,
    pub released_bytes: u64,
    pub remaining_candidates: bool,
}
pub(crate) struct ObjectPublication {
    store: ContentObjectStore,
    owner_id: String,
    _lock: File,
}

const REFS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS objects(key TEXT PRIMARY KEY CHECK(length(key)=64), bytes INTEGER NOT NULL CHECK(bytes>=0), state TEXT NOT NULL CHECK(state IN('active','staging')), staged_name TEXT);
CREATE TABLE IF NOT EXISTS owners(owner_id TEXT PRIMARY KEY, committed_epoch INTEGER NOT NULL DEFAULT 0 CHECK(committed_epoch>=0));
CREATE TABLE IF NOT EXISTS committed_refs(owner_id TEXT NOT NULL REFERENCES owners(owner_id) ON DELETE CASCADE,key TEXT NOT NULL CHECK(length(key)=64),PRIMARY KEY(owner_id,key));
CREATE INDEX IF NOT EXISTS committed_refs_key ON committed_refs(key);
CREATE TABLE IF NOT EXISTS publications(publication_id TEXT PRIMARY KEY,owner_id TEXT NOT NULL UNIQUE REFERENCES owners(owner_id) ON DELETE CASCADE,created_at INTEGER NOT NULL DEFAULT(unixepoch()));
CREATE TABLE IF NOT EXISTS publication_pins(publication_id TEXT NOT NULL REFERENCES publications(publication_id) ON DELETE CASCADE,key TEXT NOT NULL,PRIMARY KEY(publication_id,key));
CREATE INDEX IF NOT EXISTS publication_pins_key ON publication_pins(key);
CREATE TABLE IF NOT EXISTS object_gc_journal(key TEXT PRIMARY KEY,name TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN('planned','deleted')),bytes INTEGER NOT NULL);
"#;

impl ContentObjectStore {
    pub fn open(root: &Path) -> Result<Self, LatticeError> {
        checked_create_dir(root)?;
        let root = root.canonicalize().map_err(io_err)?;
        let managed = Arc::new(SecureDir::open(&root).map_err(io_err)?);
        Self::open_in(managed)
    }

    pub(crate) fn open_in(managed: Arc<SecureDir>) -> Result<Self, LatticeError> {
        if managed
            .metadata("refs.db")
            .map_err(io_err)?
            .is_some_and(|entry| !entry.is_file)
        {
            return Err(storage("object reference index must be a regular file"));
        }
        let refs = ManagedSqlite::open(
            &managed,
            "refs.db",
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|e| storage(&format!("open object reference index: {e}")))?;
        refs.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| storage(&e.to_string()))?;
        refs.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|e| storage(&e.to_string()))?;
        refs.execute_batch(REFS_SCHEMA)
            .map_err(|e| storage(&format!("initialize object reference index: {e}")))?;
        super::object_accounting::initialize(&refs).map_err(|error| storage(&error.to_string()))?;
        let store = Self {
            managed,
            refs: Arc::new(Mutex::new(refs)),
        };
        store.managed.sync().map_err(io_err)?;
        Ok(store)
    }

    pub fn put(&self, body: &[u8]) -> Result<String, LatticeError> {
        let _lock = self
            .lock(false)?
            .ok_or_else(|| storage("symbol body maintenance lock unavailable"))?;
        let key = format!("{:x}", Sha256::digest(body));
        let prefix = &key[..2];
        let directory = self.managed.create_dir(prefix).map_err(io_err)?;
        if directory.metadata(&key).map_err(io_err)?.is_some() {
            let stored = read_file(&directory, &key)?;
            self.verify(&key, &stored)?;
            self.refs.lock().map_err(|_| storage("object reference index lock poisoned"))?
                .execute("INSERT OR IGNORE INTO objects(key,bytes,state,staged_name) VALUES(?1,?2,'active',NULL)", params![key,stored.len() as u64])
                .map_err(|e| storage(&format!("index existing symbol body: {e}")))?;
            let metadata = directory
                .metadata(&key)
                .map_err(io_err)?
                .ok_or_else(|| storage("existing body disappeared"))?;
            super::object_accounting::record(
                &**self
                    .refs
                    .lock()
                    .map_err(|_| storage("object index lock poisoned"))?,
                &key,
                metadata.len,
                metadata.allocated,
            )
            .map_err(|error| storage(&error.to_string()))?;
            return Ok(key);
        }
        let temporary = format!(".{key}.{}.tmp", SEQUENCE.fetch_add(1, Ordering::Relaxed));
        self.refs.lock().map_err(|_| storage("object reference index lock poisoned"))?
            .execute("INSERT INTO objects(key,bytes,state,staged_name) VALUES(?1,?2,'staging',?3) ON CONFLICT(key) DO UPDATE SET allocated_bytes=NULL,state='staging',staged_name=excluded.staged_name",params![key,body.len() as u64,temporary])
            .map_err(sql)?;
        let mut file = directory.open_new_file(&temporary).map_err(io_err)?;
        file.write_all(body).map_err(io_err)?;
        file.sync_all().map_err(io_err)?;
        let temporary_meta = directory
            .metadata(&temporary)
            .map_err(io_err)?
            .ok_or_else(|| storage("temporary symbol body disappeared"))?;
        match directory.rename_to(&temporary, &directory, &key, temporary_meta.identity) {
            Ok(()) => {}
            Err(_) if directory.metadata(&key).map_err(io_err)?.is_some() => {
                if let Some(meta) = directory.metadata(&temporary).map_err(io_err)? {
                    let _ = directory.remove_file(&temporary, meta.identity);
                }
                let stored = read_file(&directory, &key)?;
                self.verify(&key, &stored)?;
            }
            Err(error) => return Err(io_err(error)),
        }
        let bytes = directory
            .metadata(&key)
            .map_err(io_err)?
            .ok_or_else(|| storage("published symbol body disappeared"))?
            .len;
        self.refs.lock().map_err(|_| storage("object reference index lock poisoned"))?
            .execute("INSERT INTO objects(key,bytes,state,staged_name) VALUES(?1,?2,'active',NULL) ON CONFLICT(key) DO UPDATE SET bytes=excluded.bytes,state='active',staged_name=NULL", params![key,bytes])
            .map_err(|e| storage(&format!("index symbol body object: {e}")))?;
        let metadata = directory
            .metadata(&key)
            .map_err(io_err)?
            .ok_or_else(|| storage("published body disappeared"))?;
        super::object_accounting::record(
            &**self
                .refs
                .lock()
                .map_err(|_| storage("object index lock poisoned"))?,
            &key,
            metadata.len,
            metadata.allocated,
        )
        .map_err(|error| storage(&error.to_string()))?;
        Ok(key)
    }

    pub fn advance_accounting(
        &self,
        limit: usize,
    ) -> Result<super::object_accounting::ObjectAccounting, LatticeError> {
        let _lease = self
            .lock(true)?
            .ok_or_else(|| storage("symbol body publication is active; retry accounting"))?;
        let connection = self
            .refs
            .lock()
            .map_err(|_| storage("object index lock poisoned"))?;
        for key in super::object_accounting::pending(&connection, limit)
            .map_err(|error| storage(&error.to_string()))?
        {
            validate_key(&key)?;
            let metadata = self
                .managed
                .open_dir(&key[..2])
                .and_then(|directory| directory.metadata(&key));
            let metadata = match metadata {
                Ok(Some(value)) => value,
                Ok(None) => {
                    super::object_accounting::mark_error(
                        &connection,
                        &key,
                        "indexed symbol body is missing",
                    )
                    .map_err(|e| storage(&e.to_string()))?;
                    continue;
                }
                Err(error) => {
                    super::object_accounting::mark_error(
                        &connection,
                        &key,
                        &format!("symbol body metadata: {error}"),
                    )
                    .map_err(|e| storage(&e.to_string()))?;
                    continue;
                }
            };
            if !metadata.is_file {
                super::object_accounting::mark_error(
                    &connection,
                    &key,
                    "indexed body is not a regular file",
                )
                .map_err(|e| storage(&e.to_string()))?;
                continue;
            }
            super::object_accounting::record(&connection, &key, metadata.len, metadata.allocated)
                .map_err(|error| storage(&error.to_string()))?;
        }
        super::object_accounting::read(&connection).map_err(|error| storage(&error.to_string()))
    }

    pub fn get(&self, key: &str) -> Result<Vec<u8>, LatticeError> {
        validate_key(key)?;
        let _lock = self
            .lock(false)?
            .ok_or_else(|| storage("symbol body maintenance lock unavailable"))?;
        let directory = self.managed.open_dir(&key[..2]).map_err(io_err)?;
        let mut file = directory
            .open_file(key, false)
            .map_err(|e| storage(&format!("missing symbol body object {key}: {e}")))?;
        let mut bytes = Vec::new();
        use std::io::Read;
        file.read_to_end(&mut bytes)
            .map_err(|e| storage(&format!("missing symbol body object {key}: {e}")))?;
        self.verify(key, &bytes)?;
        Ok(bytes)
    }

    pub(crate) fn begin_publication<I>(
        &self,
        owner_id: &str,
        objects: I,
    ) -> Result<ObjectPublication, LatticeError>
    where
        I: IntoIterator<Item = String>,
    {
        validate_owner(owner_id)?;
        let lock = self
            .lock(false)?
            .ok_or_else(|| storage("symbol body maintenance lock unavailable"))?;
        let id = format!(
            "{:x}",
            Sha256::digest(
                format!("{owner_id}:{}", SEQUENCE.fetch_add(1, Ordering::Relaxed)).as_bytes()
            )
        );
        let mut refs = self
            .refs
            .lock()
            .map_err(|_| storage("object reference index lock poisoned"))?;
        let tx = refs
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        tx.execute(
            "INSERT OR IGNORE INTO owners(owner_id) VALUES(?1)",
            [owner_id],
        )
        .map_err(sql)?;
        tx.execute(
            "INSERT INTO publications(publication_id,owner_id) VALUES(?1,?2)",
            params![id, owner_id],
        )
        .map_err(sql)?;
        tx.execute(
            "INSERT INTO publication_pins SELECT ?1,key FROM committed_refs WHERE owner_id=?2",
            params![id, owner_id],
        )
        .map_err(sql)?;
        for key in objects {
            validate_key(&key)?;
            tx.execute(
                "INSERT OR IGNORE INTO publication_pins VALUES(?1,?2)",
                params![id, key],
            )
            .map_err(sql)?;
        }
        tx.commit().map_err(sql)?;
        Ok(ObjectPublication {
            store: self.clone(),
            owner_id: owner_id.to_owned(),
            _lock: lock,
        })
    }

    pub fn collect_garbage(&self, limit: usize) -> Result<ObjectGcReport, LatticeError> {
        if limit == 0 {
            return Err(storage("object GC limit must be positive"));
        }
        let _lock = self
            .lock(true)?
            .ok_or_else(|| storage("symbol body publication is active; retry GC"))?;
        let mut report = ObjectGcReport::default();
        let candidates = {
            let refs = self
                .refs
                .lock()
                .map_err(|_| storage("object reference index lock poisoned"))?;
            let mut statement = refs.prepare("SELECT key,bytes,staged_name FROM objects o WHERE NOT EXISTS(SELECT 1 FROM committed_refs r WHERE r.key=o.key) AND NOT EXISTS(SELECT 1 FROM publication_pins p WHERE p.key=o.key) ORDER BY key LIMIT ?1").map_err(sql)?;
            let rows = statement
                .query_map([limit], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, u64>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })
                .map_err(sql)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql)?;
            rows
        };
        report.examined = candidates.len();
        for (key, bytes, staged_name) in candidates {
            let refs = self
                .refs
                .lock()
                .map_err(|_| storage("object reference index lock poisoned"))?;
            refs.execute("INSERT OR IGNORE INTO object_gc_journal(key,name,state,bytes) VALUES(?1,?1,'planned',?2)",params![key,bytes]).map_err(sql)?;
            drop(refs);
            let shard = self.managed.open_dir(&key[..2]).map_err(io_err)?;
            if let Some(staged_name) = staged_name {
                if let Some(entry) = shard.metadata(&staged_name).map_err(io_err)? {
                    shard
                        .remove_file(&staged_name, entry.identity)
                        .map_err(io_err)?;
                }
            }
            if let Some(entry) = shard.metadata(&key).map_err(io_err)? {
                shard.remove_file(&key, entry.identity).map_err(io_err)?;
            }
            let refs = self
                .refs
                .lock()
                .map_err(|_| storage("object reference index lock poisoned"))?;
            refs.execute("DELETE FROM objects WHERE key=?1 AND NOT EXISTS(SELECT 1 FROM committed_refs WHERE key=?1) AND NOT EXISTS(SELECT 1 FROM publication_pins WHERE key=?1)",[&key]).map_err(sql)?;
            refs.execute(
                "UPDATE object_gc_journal SET state='deleted' WHERE key=?1",
                [&key],
            )
            .map_err(sql)?;
            report.removed += 1;
            report.released_bytes = report.released_bytes.saturating_add(bytes);
        }
        let refs = self
            .refs
            .lock()
            .map_err(|_| storage("object reference index lock poisoned"))?;
        report.remaining_candidates = refs.query_row("SELECT EXISTS(SELECT 1 FROM objects o WHERE NOT EXISTS(SELECT 1 FROM committed_refs r WHERE r.key=o.key) AND NOT EXISTS(SELECT 1 FROM publication_pins p WHERE p.key=o.key))",[],|r|r.get(0)).map_err(sql)?;
        refs.execute("DELETE FROM object_gc_journal WHERE state='deleted' AND rowid < (SELECT COALESCE(MAX(rowid),0)-4096 FROM object_gc_journal)",[]).map_err(sql)?;
        Ok(report)
    }

    pub(crate) fn reconcile_owner(
        &self,
        owner_id: &str,
        graph: &Connection,
    ) -> Result<(), LatticeError> {
        validate_owner(owner_id)?;
        let epoch: i64 = graph
            .query_row(
                "SELECT index_epoch FROM graph_metadata WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(sql)?;
        let mut refs = self
            .refs
            .lock()
            .map_err(|_| storage("object reference index lock poisoned"))?;
        let tx = refs
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        tx.execute(
            "INSERT OR IGNORE INTO owners(owner_id) VALUES(?1)",
            [owner_id],
        )
        .map_err(sql)?;
        tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS desired_object_refs(key TEXT PRIMARY KEY); DELETE FROM desired_object_refs").map_err(sql)?;
        {
            let mut read=graph.prepare("SELECT DISTINCT body_hash FROM nodes WHERE body_hash IS NOT NULL ORDER BY body_hash").map_err(sql)?;
            let mut rows = read.query([]).map_err(sql)?;
            while let Some(row) = rows.next().map_err(sql)? {
                let key: String = row.get(0).map_err(sql)?;
                validate_key(&key)?;
                tx.execute("INSERT INTO desired_object_refs VALUES(?1)", [key])
                    .map_err(sql)?;
            }
        }
        tx.execute(
            "INSERT OR IGNORE INTO committed_refs SELECT ?1,key FROM desired_object_refs",
            [owner_id],
        )
        .map_err(sql)?;
        tx.execute("DELETE FROM committed_refs WHERE owner_id=?1 AND key NOT IN(SELECT key FROM desired_object_refs)",[owner_id]).map_err(sql)?;
        tx.execute(
            "UPDATE owners SET committed_epoch=?2 WHERE owner_id=?1",
            params![owner_id, epoch],
        )
        .map_err(sql)?;
        tx.execute("DELETE FROM publications WHERE owner_id=?1", [owner_id])
            .map_err(sql)?;
        tx.commit().map_err(sql)
    }

    pub(crate) fn recover_owner(
        &self,
        owner_id: &str,
        graph: &Connection,
    ) -> Result<(), LatticeError> {
        let _lock = self
            .lock(true)?
            .ok_or_else(|| storage("symbol body publication is active; owner recovery deferred"))?;
        self.reconcile_owner(owner_id, graph)
    }

    pub fn retire_checkout(&self, owner_id: &str) -> Result<(), LatticeError> {
        validate_owner(owner_id)?;
        let _lock = self.lock(true)?.ok_or_else(|| {
            storage("symbol body publication is active; checkout retirement deferred")
        })?;
        self.refs
            .lock()
            .map_err(|_| storage("object reference index lock poisoned"))?
            .execute("DELETE FROM owners WHERE owner_id=?1", [owner_id])
            .map_err(sql)?;
        Ok(())
    }
    fn lock(&self, exclusive: bool) -> Result<Option<File>, LatticeError> {
        let file = self
            .managed
            .open_or_create_file("publication.lock")
            .map_err(io_err)?;
        let result = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        match result {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(io_err(e)),
        }
    }
    fn verify(&self, key: &str, bytes: &[u8]) -> Result<(), LatticeError> {
        let actual = format!("{:x}", Sha256::digest(bytes));
        if actual != key {
            return Err(storage(&format!(
                "corrupt symbol body object {key}: content hash is {actual}"
            )));
        }
        Ok(())
    }
}
impl ObjectPublication {
    pub(crate) fn promote(self, graph: &Connection) -> Result<(), LatticeError> {
        self.store.reconcile_owner(&self.owner_id, graph)
    }
}
fn validate_owner(owner: &str) -> Result<(), LatticeError> {
    if owner.is_empty()
        || owner.len() > 128
        || !owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(storage("invalid symbol body owner identity"));
    }
    Ok(())
}
fn sql(e: rusqlite::Error) -> LatticeError {
    storage(&format!("symbol body reference index failed: {e}"))
}
fn validate_key(key: &str) -> Result<(), LatticeError> {
    if key.len() != 64
        || !key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(storage("invalid symbol body object key"));
    }
    Ok(())
}
fn checked_create_dir(path: &Path) -> Result<(), LatticeError> {
    reject_symlink(path)?;
    if let Some(parent) = path.parent() {
        if parent.exists() {
            reject_symlink(parent)?
        }
    }
    fs::create_dir_all(path).map_err(io_err)?;
    reject_symlink(path)
}
fn reject_symlink(path: &Path) -> Result<(), LatticeError> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(storage(&format!(
            "managed storage symlink refused: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(e)),
    }
}
fn io_err(e: std::io::Error) -> LatticeError {
    if matches!(e.raw_os_error(), Some(code) if code == libc::ELOOP || code == libc::ENOTDIR) {
        return storage(&format!("managed storage symlink refused: {e}"));
    }
    storage(&format!("symbol body object storage failed: {e}"))
}
fn read_file(directory: &SecureDir, name: &str) -> Result<Vec<u8>, LatticeError> {
    let mut file = directory.open_file(name, false).map_err(io_err)?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes).map_err(io_err)?;
    Ok(bytes)
}
fn storage(s: &str) -> LatticeError {
    LatticeError::Storage(s.into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn object_shard_symlink_escape_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = ContentObjectStore::open(root.path()).unwrap();
        let key = store.put(b"protected body").unwrap();
        let shard = root.path().join(&key[..2]);
        fs::remove_file(shard.join(&key)).unwrap();
        fs::remove_dir(&shard).unwrap();
        symlink(outside.path(), &shard).unwrap();

        let error = store.put(b"protected body").unwrap_err().to_string();
        assert!(error.contains("symlink refused"), "{error}");
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[test]
    fn gc_examines_only_its_indexed_candidate_bound_and_preserves_unknown_files() {
        let root = tempfile::tempdir().unwrap();
        let store = ContentObjectStore::open(root.path()).unwrap();
        let keys = (0..10)
            .map(|i| store.put(format!("body-{i}").as_bytes()).unwrap())
            .collect::<Vec<_>>();
        {
            let refs = store.refs.lock().unwrap();
            refs.execute("INSERT INTO owners(owner_id) VALUES('checkout-live')", [])
                .unwrap();
            for key in &keys[..8] {
                refs.execute(
                    "INSERT INTO committed_refs VALUES('checkout-live',?1)",
                    [key],
                )
                .unwrap();
            }
        }
        let unknown_key = "f".repeat(64);
        let unknown_shard = store.managed.create_dir("ff").unwrap();
        let mut unknown = unknown_shard.open_new_file(&unknown_key).unwrap();
        unknown.write_all(b"unindexed historical object").unwrap();
        unknown.sync_all().unwrap();

        let first = store.collect_garbage(1).unwrap();
        assert_eq!(first.examined, 1);
        assert_eq!(first.removed, 1);
        assert!(first.remaining_candidates);
        let second = store.collect_garbage(1).unwrap();
        assert_eq!(second.examined, 1);
        assert_eq!(second.removed, 1);
        assert!(!second.remaining_candidates);
        assert!(unknown_shard.metadata(&unknown_key).unwrap().is_some());
        for key in &keys[..8] {
            assert!(store.get(key).is_ok());
        }
    }
}
