//! Fences disposable-cache publication against repository accounting pages.
use super::{managed_sqlite::ManagedSqlite, SecureDir};
use crate::LatticeError;
use rusqlite::{params, OpenFlags};
use std::{fs::File, path::Path};

pub struct CachePublicationAuthority {
    repository: SecureDir,
    cache: SecureDir,
    checkout_id: String,
}

impl CachePublicationAuthority {
    pub fn for_path(path: &Path) -> Result<Option<(Self, String)>, LatticeError> {
        Self::resolve(path).map_err(storage)
    }

    fn resolve(path: &Path) -> anyhow::Result<Option<(Self, String)>> {
        let leaf = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("cache payload has no UTF-8 leaf name"))?;
        let Some(cache_path) = path
            .parent()
            .filter(|p| p.file_name().is_some_and(|n| n == "cache"))
        else {
            return Ok(None);
        };
        let Some(checkout_path) = cache_path.parent() else {
            return Ok(None);
        };
        let Some(checkouts) = checkout_path
            .parent()
            .filter(|p| p.file_name().is_some_and(|n| n == "checkouts"))
        else {
            return Ok(None);
        };
        let Some(root) = checkouts.parent() else {
            return Ok(None);
        };
        let checkout_id = checkout_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid checkout namespace"))?
            .to_owned();
        let repository = SecureDir::open(root)?;
        let cache = repository.open_dir(Path::new("checkouts").join(&checkout_id).join("cache"))?;
        let authority = Self {
            repository,
            cache,
            checkout_id,
        };
        authority.validate_owner()?;
        Ok(Some((authority, leaf.to_owned())))
    }

    pub fn open_sqlite(&self, leaf: &str, flags: OpenFlags) -> rusqlite::Result<ManagedSqlite> {
        ManagedSqlite::open(&self.cache, leaf, flags)
    }
    pub fn cache_dir(&self) -> std::io::Result<SecureDir> {
        self.cache.try_clone()
    }
    pub fn begin(&self) -> Result<CachePublicationGuard, LatticeError> {
        self.acquire().map_err(storage)
    }

    fn validate_owner(&self) -> anyhow::Result<()> {
        let registry = self
            .repository
            .metadata("storage-registry.db")?
            .ok_or_else(|| anyhow::anyhow!("checkout cache has no repository registry"))?;
        if !registry.is_file {
            anyhow::bail!("cache registry is not a regular file")
        }
        let connection = ManagedSqlite::open(
            &self.repository,
            "storage-registry.db",
            OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        let owned: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM repository_home h,checkout_registry c WHERE h.id=1 AND h.canonical_home=?1 AND c.checkout_id=?2)", params![self.repository.path().to_string_lossy(), self.checkout_id], |row| row.get(0))?;
        if !owned {
            anyhow::bail!("cache publication is not owned by the repository registry")
        }
        Ok(())
    }

    fn acquire(&self) -> anyhow::Result<CachePublicationGuard> {
        self.validate_owner()?;
        let lease = self
            .repository
            .open_dir("leases")?
            .open_or_create_file("inventory.lock")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            match lease.try_lock_shared() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    anyhow::bail!("repository accounting is busy; retry cache publication")
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        let connection = ManagedSqlite::open(
            &self.repository,
            "storage-registry.db",
            OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        if connection.execute(
            "UPDATE storage_inventory_control SET generation=generation+1,invalidated=1 WHERE id=1",
            [],
        )? != 1
        {
            anyhow::bail!("cache inventory authority is unavailable")
        }
        Ok(CachePublicationGuard { _lease: lease })
    }
}

pub struct CachePublicationGuard {
    _lease: File,
}
impl CachePublicationGuard {
    pub fn for_path(path: &Path) -> Result<Option<Self>, LatticeError> {
        let Some((authority, _)) = CachePublicationAuthority::for_path(path)? else {
            return Ok(None);
        };
        authority.begin().map(Some)
    }
}
fn storage(error: anyhow::Error) -> LatticeError {
    LatticeError::Storage(format!("Cannot fence cache publication: {error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{
        lifecycle::StorageRegistry, vector_index::VectorIndex, vector_store::VectorStore,
    };
    use rusqlite::Connection;
    use std::{fs, sync::mpsc, thread, time::Duration};

    const CHECKOUT: &str =
        "checkout_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        CachePublicationAuthority,
    ) {
        let root = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(root.path(), "repository").unwrap();
        drop(
            registry
                .register_and_lease(CHECKOUT, checkout.path(), 1)
                .unwrap(),
        );
        drop(registry);
        let payload = root
            .path()
            .join("checkouts")
            .join(CHECKOUT)
            .join("cache/vectors.db");
        let (authority, leaf) = CachePublicationAuthority::for_path(&payload)
            .unwrap()
            .unwrap();
        assert_eq!(leaf, "vectors.db");
        (root, checkout, authority)
    }

    fn control(root: &Path) -> (i64, i64) {
        Connection::open(root.join("storage-registry.db"))
            .unwrap()
            .query_row(
                "SELECT generation, invalidated FROM storage_inventory_control WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    #[test]
    fn failed_publication_remains_invalidated_and_each_batch_counts_once() {
        let (root, _checkout, authority) = fixture();
        let before = control(root.path()).0;
        let guard = authority.begin().unwrap();
        drop(guard); // represents a payload failure after accounting was invalidated
        let after_failure = control(root.path());
        assert_eq!(after_failure, (before + 1, 1));

        let guard = authority.begin().unwrap();
        // Arbitrarily many payload writes share this one publication guard.
        for i in 0..32 {
            fs::write(root.path().join(format!("batch-{i}")), b"payload").unwrap();
        }
        drop(guard);
        assert_eq!(control(root.path()).0, before + 2);
    }

    #[test]
    fn pinned_authority_survives_root_replacement() {
        let (root, _checkout, authority) = fixture();
        let original = root.path().with_extension("original");
        fs::rename(root.path(), &original).unwrap();
        fs::create_dir(root.path()).unwrap();
        fs::write(root.path().join("outside-marker"), b"untouched").unwrap();

        let guard = authority.begin().unwrap();
        drop(guard);
        assert_eq!(control(&original).1, 1);
        assert_eq!(
            fs::read(root.path().join("outside-marker")).unwrap(),
            b"untouched"
        );
    }

    #[test]
    fn accounting_exclusive_lock_blocks_publication_before_invalidation() {
        let (root, _checkout, authority) = fixture();
        let lease = authority
            .repository
            .open_dir("leases")
            .unwrap()
            .open_or_create_file("inventory.lock")
            .unwrap();
        lease.try_lock().unwrap();
        let before = control(root.path()).0;
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || tx.send(authority.begin().is_ok()).unwrap());
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        lease.unlock().unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap());
        worker.join().unwrap();
        assert_eq!(control(root.path()).0, before + 1);
    }

    #[test]
    fn vector_batch_invalidates_once_and_noop_delete_does_not_invalidate() {
        let (root, _checkout, _authority) = fixture();
        let path = root
            .path()
            .join("checkouts")
            .join(CHECKOUT)
            .join("cache/vectors.db");
        let store = VectorStore::open(path.to_str().unwrap()).unwrap();
        store.initialize(3).unwrap();
        let after_initialize = control(root.path()).0;

        store.delete_by_file("missing.rs").unwrap();
        assert_eq!(control(root.path()).0, after_initialize);

        {
            let _batch = store.begin_publication().unwrap();
            for offset in 0..20 {
                store
                    .upsert_vector("batch.rs", "symbol", offset, &[1.0, 0.0, 0.0])
                    .unwrap();
            }
        }
        assert_eq!(control(root.path()).0, after_initialize + 1);
    }
}
