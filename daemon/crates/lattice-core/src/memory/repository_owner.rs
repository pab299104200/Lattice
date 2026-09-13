use crate::error::LatticeError;
use crate::storage::SecureDir;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

/// Cross-process ownership for repository memory initialization and migration.
/// The lock is advisory and is released by the kernel on process exit.
pub struct RepositoryMemoryOwner {
    file: File,
    path: PathBuf,
    directory: SecureDir,
}

impl RepositoryMemoryOwner {
    pub fn acquire(database_path: &Path, deadline: Duration) -> Result<Self, LatticeError> {
        let parent = database_path.parent().ok_or_else(|| {
            LatticeError::MemoryStorageAccessDenied(format!(
                "database path {} has no parent directory",
                database_path.display()
            ))
        })?;
        let directory = SecureDir::open(parent).map_err(|error| {
            LatticeError::MemoryStorageAccessDenied(format!(
                "cannot pin repository memory directory {}: {error}",
                parent.display()
            ))
        })?;
        Self::acquire_in(&directory, deadline)
    }

    /// Acquire ownership relative to an already-authorized, pinned database
    /// directory. The lock leaf cannot redirect through a symlink, and a
    /// concurrent rename of the directory cannot move authority to a
    /// replacement pathname.
    pub fn acquire_in(directory: &SecureDir, deadline: Duration) -> Result<Self, LatticeError> {
        let path = directory.path().join("memory-owner.lock");
        let file = directory
            .open_or_create_file("memory-owner.lock")
            .map_err(|error| {
                LatticeError::MemoryStorageAccessDenied(format!(
                    "cannot open repository memory owner {}: {error}",
                    path.display()
                ))
            })?;
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => {
                    return Ok(Self {
                        file,
                        path,
                        directory: directory.try_clone().map_err(|error| {
                            LatticeError::MemoryStorageAccessDenied(format!(
                                "cannot retain pinned repository memory directory: {error}"
                            ))
                        })?,
                    })
                }
                Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < deadline => {
                    thread::sleep(Duration::from_millis(25));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(LatticeError::MemoryStorageBusy(format!(
                        "timed out acquiring repository memory owner {} after {} ms",
                        path.display(),
                        deadline.as_millis()
                    )))
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(LatticeError::MemoryStorageAccessDenied(format!(
                        "cannot lock repository memory owner {}: {error}",
                        path.display()
                    )))
                }
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn directory(&self) -> &SecureDir {
        &self.directory
    }

    pub fn open_store(&self, leaf: &str) -> Result<super::MemoryStore, LatticeError> {
        super::MemoryStore::open_in(&self.directory, leaf)
    }
}

impl Drop for RepositoryMemoryOwner {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_contention_is_bounded_and_does_not_modify_database() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("memories.db");
        std::fs::write(&database, b"sentinel").unwrap();
        let first = RepositoryMemoryOwner::acquire(&database, Duration::from_millis(50)).unwrap();
        let error = RepositoryMemoryOwner::acquire(&database, Duration::from_millis(75))
            .err()
            .expect("second owner must time out");
        assert!(matches!(error, LatticeError::MemoryStorageBusy(_)));
        assert_eq!(std::fs::read(&database).unwrap(), b"sentinel");
        drop(first);
        RepositoryMemoryOwner::acquire(&database, Duration::from_millis(50))
            .expect("kernel lock must be released with owner");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_lock_leaf_is_refused_without_touching_its_target() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"sentinel").unwrap();
        symlink(outside.path(), root.path().join("memory-owner.lock")).unwrap();
        let directory = SecureDir::open(root.path()).unwrap();

        let error = RepositoryMemoryOwner::acquire_in(&directory, Duration::from_millis(10))
            .err()
            .expect("symlink lock leaf must be rejected");
        assert!(matches!(error, LatticeError::MemoryStorageAccessDenied(_)));
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"sentinel");
    }

    #[cfg(unix)]
    #[test]
    fn pinned_directory_does_not_follow_a_replacement_path() {
        let parent = tempfile::tempdir().unwrap();
        let original = parent.path().join("memory-home");
        let parked = parent.path().join("parked-home");
        std::fs::create_dir(&original).unwrap();
        let pinned = SecureDir::open(&original).unwrap();
        let first = RepositoryMemoryOwner::acquire_in(&pinned, Duration::from_millis(20)).unwrap();

        std::fs::rename(&original, &parked).unwrap();
        std::fs::create_dir(&original).unwrap();
        let replacement = SecureDir::open(&original).unwrap();
        let replacement_owner =
            RepositoryMemoryOwner::acquire_in(&replacement, Duration::from_millis(20)).unwrap();
        let error = RepositoryMemoryOwner::acquire_in(&pinned, Duration::from_millis(30))
            .err()
            .expect("the original pinned directory remains locked");
        assert!(matches!(error, LatticeError::MemoryStorageBusy(_)));
        assert!(parked.join("memory-owner.lock").is_file());
        assert!(original.join("memory-owner.lock").is_file());

        drop(replacement_owner);
        drop(first);
        RepositoryMemoryOwner::acquire_in(&pinned, Duration::from_millis(20))
            .expect("pinned owner releases independently of replacement path");
    }

    #[cfg(unix)]
    #[test]
    fn owner_opens_store_through_the_same_pinned_directory_after_replacement() {
        let parent = tempfile::tempdir().unwrap();
        let live = parent.path().join("live");
        let parked = parent.path().join("parked");
        std::fs::create_dir(&live).unwrap();
        let directory = SecureDir::open(&live).unwrap();
        let owner =
            RepositoryMemoryOwner::acquire_in(&directory, Duration::from_millis(50)).unwrap();
        std::fs::rename(&live, &parked).unwrap();
        std::fs::create_dir(&live).unwrap();
        let store = owner.open_store("memories.db").unwrap();
        assert!(store.is_persistent_available());
        assert!(parked.join("memories.db").is_file());
        assert_eq!(std::fs::read_dir(&live).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn store_refuses_symlink_database_leaf() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        symlink(outside.path(), root.path().join("memories.db")).unwrap();
        let dir = SecureDir::open(root.path()).unwrap();
        let error = crate::memory::MemoryStore::open_in(&dir, "memories.db")
            .err()
            .expect("symlink database must fail");
        assert!(matches!(
            error,
            LatticeError::MemoryStorageAccessDenied(_) | LatticeError::Storage(_)
        ));
    }
}
