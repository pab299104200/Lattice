//! One bounded lifecycle worker per canonical memory store in this daemon.
use lattice_core::events::{expire_managed_snapshots_dir_page, SnapshotExpiryCursor};
use lattice_core::memory::retention::{self, RetentionPolicy};
use lattice_core::memory::{MemoryStore, RepositoryMemoryOwner};
use lattice_core::storage::managed_fs::ManagedDirFingerprint;
use lattice_core::storage::managed_sqlite::ManagedSqlite;
use lattice_core::storage::{ManagedIdentity, SecureDir};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
static OWNERS: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();
const REGISTRY_VERSION: u16 = 1;
const SNAPSHOT_HORIZON: u64 = 30 * 86_400;
const RECLAIM_PAGES: u32 = 256;
const SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;
const SNAPSHOT_DELETES: usize = 8;
const MAX_REGISTRY_BYTES: u64 = 1024 * 1024;
const MAX_REGISTERED_STORES: usize = 1024;
#[derive(Default, Serialize, Deserialize)]
struct Registry {
    version: u16,
    stores: BTreeSet<PathBuf>,
}
#[derive(Default, Serialize, Deserialize)]
struct CheckoutSnapshotCursor {
    root: Option<SnapshotExpiryCursor>,
    root_complete: bool,
    root_fingerprint: Option<ManagedDirFingerprint>,
    checkout: String,
    directory: Option<SnapshotExpiryCursor>,
    membership_generation: Option<i64>,
}

#[derive(Default)]
struct SnapshotWorkerState {
    active: Option<ActiveSnapshotDirectory>,
}

struct ActiveSnapshotDirectory {
    key: String,
    identity: ManagedIdentity,
    fingerprint: ManagedDirFingerprint,
    directory: SecureDir,
}

impl SnapshotWorkerState {
    fn select(&mut self, key: String, candidate: SecureDir) -> anyhow::Result<(&SecureDir, bool)> {
        let identity = candidate.identity()?;
        let fingerprint = candidate.directory_fingerprint()?;
        let reusable = self.active.as_ref().is_some_and(|active| {
            active.key == key && active.identity == identity && active.fingerprint == fingerprint
        });
        if !reusable {
            self.active = Some(ActiveSnapshotDirectory {
                key,
                identity,
                fingerprint,
                directory: candidate,
            });
        }
        Ok((
            &self.active.as_ref().expect("active directory").directory,
            !reusable,
        ))
    }

    fn clear(&mut self) {
        self.active = None;
    }
}

fn reset_handle_bound_cursor(
    cursor: &mut Option<SnapshotExpiryCursor>,
    handle_changed: bool,
    handle_bound_platform: bool,
) {
    if handle_changed && handle_bound_platform {
        *cursor = None;
    }
}

/// Persist this canonical owner and start workers for every previously known
/// owner. Thus an inactive repository remains eligible after daemon restart.
pub(crate) fn register(path: &Path) -> anyhow::Result<()> {
    let canonical = validate_store(path)?;
    let registry = registry_path()?;
    if let Some(parent) = registry.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            let metadata = parent.symlink_metadata()?;
            if metadata.file_type().is_symlink()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                anyhow::bail!(
                    "memory lifecycle state directory is not private daemon-owned storage"
                )
            }
        }
    }
    let lock_path = registry.with_extension("lock");
    let mut lock_options = std::fs::OpenOptions::new();
    lock_options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        lock_options
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .mode(0o600);
    }
    let lock = lock_options.open(lock_path)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "memory lifecycle registry is busy or unavailable: {error}"
                ))
            }
        }
    }
    let mut entries = read_registry(&registry)?;
    entries.insert(canonical);
    write_registry(&registry, &entries)?;
    lock.unlock()?;
    for entry in entries {
        spawn_once(entry)?
    }
    Ok(())
}
fn spawn_once(path: PathBuf) -> anyhow::Result<()> {
    let policy = RetentionPolicy::from_env()?;
    let mut owners = OWNERS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| anyhow::anyhow!("memory lifecycle registry lock poisoned"))?;
    if !owners.insert(path.clone()) {
        return Ok(());
    }
    drop(owners);
    let snapshot_state = std::sync::Arc::new(Mutex::new(SnapshotWorkerState::default()));
    tokio::spawn(async move {
        let ordinary_delay = Duration::from_secs(policy.sweep_interval_secs);
        let mut delay = Duration::ZERO;
        loop {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let p = path.clone();
            let policy = policy.clone();
            let snapshot_state = snapshot_state.clone();
            let result = tokio::task::spawn_blocking(move || {
                let mut continuation = false;
                let outcome = (|| {
                    let mut snapshot_state = snapshot_state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("snapshot worker state lock poisoned"))?;
                    maintain_with_continuation(
                        &p,
                        &policy,
                        now(),
                        &mut snapshot_state,
                        &mut continuation,
                    )
                })();
                (outcome, continuation)
            })
            .await;
            delay = match result {
                Ok((Ok(()), continuation)) => {
                    tracing::info!(store=%path.display(),continuation,"memory lifecycle maintenance completed");
                    maintenance_delay(ordinary_delay, continuation)
                }
                Ok((Err(error), continuation)) => {
                    if continuation {
                        tracing::info!(store=%path.display(),%error,"memory lifecycle maintenance incomplete; bounded retry scheduled");
                    } else {
                        tracing::error!(store=%path.display(),%error,"memory lifecycle maintenance failed; persisted deadline remains retryable");
                    }
                    maintenance_delay(ordinary_delay, continuation)
                }
                Err(error) => {
                    tracing::error!(store=%path.display(),%error,"memory lifecycle worker failed");
                    ordinary_delay
                }
            };
        }
    });
    Ok(())
}
fn maintenance_delay(ordinary: Duration, continuation: bool) -> Duration {
    if continuation {
        ordinary.min(Duration::from_secs(10))
    } else {
        ordinary
    }
}

#[cfg(test)]
fn maintain(
    path: &Path,
    policy: &RetentionPolicy,
    now: u64,
    snapshot_state: &mut SnapshotWorkerState,
) -> anyhow::Result<()> {
    maintain_with_continuation(path, policy, now, snapshot_state, &mut false)
}
fn maintain_with_continuation(
    path: &Path,
    policy: &RetentionPolicy,
    now: u64,
    snapshot_state: &mut SnapshotWorkerState,
    continuation: &mut bool,
) -> anyhow::Result<()> {
    let owner = RepositoryMemoryOwner::acquire(path, Duration::from_secs(5))?;
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("memory store leaf is not valid UTF-8"))?;
    let store = owner.open_store(leaf)?;
    // Purge is gated on a successful complete snapshot-retirement cycle. A
    // failed or partial cycle leaves authoritative memory unchanged.
    if !expire_owned_snapshots(path, &store, now, snapshot_state, owner.directory())? {
        *continuation = true;
        return Ok(());
    }
    let sweep = store.with_connection(|c| retention::sweep(c, now, policy));
    *continuation = store.with_connection(retention::maintenance_needs_continuation)?;
    let sweep = sweep?;
    if sweep.attribution_metric_dead_letters > 0 {
        tracing::warn!(
            store = %path.display(),
            expired_metric_retrievals = sweep.attribution_metric_dead_letters,
            "memory attribution metrics exceeded their retry horizon; durable dead-letter receipts retained"
        );
    }
    if !sweep.skipped {
        tracing::info!(
            store = %path.display(),
            retention_stale = sweep.stale,
            purged = sweep.purged,
            attribution_retrievals_pruned = sweep.attribution_retrievals_pruned,
            attribution_receipts_pruned = sweep.attribution_expired_receipts_pruned,
            "repository memory retention sweep committed"
        );
    }
    store.with_connection(|c| retention::reclaim_free_pages(c, RECLAIM_PAGES))?;
    Ok(())
}
fn expire_owned_snapshots(
    path: &Path,
    store: &MemoryStore,
    now: u64,
    snapshot_state: &mut SnapshotWorkerState,
    managed_home: &SecureDir,
) -> anyhow::Result<bool> {
    use rusqlite::{OpenFlags, OptionalExtension};
    let sql_error = |e: rusqlite::Error| {
        lattice_core::error::LatticeError::Storage(format!("snapshot maintenance cursor: {e}"))
    };
    let home = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("memory store has no owner directory"))?;
    let encoded: String = store.with_connection(|connection| {
        connection.execute_batch("CREATE TABLE IF NOT EXISTS memory_maintenance_cursor(name TEXT PRIMARY KEY, value TEXT NOT NULL)").map_err(sql_error)?;
        Ok(connection.query_row("SELECT value FROM memory_maintenance_cursor WHERE name='snapshots_v2'", [], |row| row.get(0)).optional().map_err(sql_error)?.unwrap_or_default())
    })?;
    let mut state: CheckoutSnapshotCursor = match serde_json::from_str(&encoded) {
        Ok(state) => state,
        Err(_) if encoded.is_empty() => CheckoutSnapshotCursor::default(),
        Err(error) => {
            tracing::warn!(%error, "invalid snapshot maintenance cursor; restarting the fenced cycle");
            CheckoutSnapshotCursor::default()
        }
    };

    // Finish root inventory once, then retain its fingerprint while checkout
    // pages advance. Completion revalidates it before opening the purge fence.
    if !state.root_complete {
        if managed_home.metadata("snapshots")?.is_some() {
            let candidate = managed_home.open_dir("snapshots")?;
            let (snapshots, handle_changed) =
                snapshot_state.select("root".to_owned(), candidate)?;
            reset_handle_bound_cursor(&mut state.root, handle_changed, cfg!(windows));
            let report = expire_managed_snapshots_dir_page(
                snapshots,
                now,
                SNAPSHOT_HORIZON,
                SNAPSHOT_BYTES,
                SNAPSHOT_DELETES,
                state.root.take(),
            )?;
            if report.rewritten_without_memory > 0 {
                state.root = None;
            } else {
                state.root = report.next_cookie;
            }
            if report.rewritten_without_memory > 0 || state.root.is_some() {
                persist_snapshot_cursor(store, &state)?;
                return Ok(false);
            }
            state.root_fingerprint = Some(snapshots.directory_fingerprint()?);
            snapshot_state.clear();
        } else {
            snapshot_state.clear();
        }
        state.root_complete = true;
    }

    if managed_home.metadata("storage-registry.db")?.is_none() {
        if !root_snapshot_generation_matches(&managed_home, state.root_fingerprint)? {
            persist_snapshot_cursor(store, &CheckoutSnapshotCursor::default())?;
            return Ok(false);
        }
        persist_snapshot_cursor(store, &CheckoutSnapshotCursor::default())?;
        return Ok(true);
    }
    let registry = ManagedSqlite::open(
        &managed_home,
        "storage-registry.db",
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    registry.busy_timeout(Duration::from_millis(250))?;
    let recorded_home: String = registry.query_row(
        "SELECT canonical_home FROM repository_home WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    if Path::new(&recorded_home) != home.canonicalize()? {
        anyhow::bail!("snapshot registry home ownership mismatch");
    }
    install_snapshot_membership_generation(&registry)?;
    let generation: i64 = registry.query_row(
        "SELECT value FROM memory_snapshot_membership_generation WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    let cycle_generation = *state.membership_generation.get_or_insert(generation);

    let is_directory_continuation = state.directory.is_some();
    let query = if is_directory_continuation {
        "SELECT checkout_id FROM checkout_registry WHERE checkout_id>=?1 ORDER BY checkout_id LIMIT 8"
    } else {
        "SELECT checkout_id FROM checkout_registry WHERE checkout_id>?1 ORDER BY checkout_id LIMIT 8"
    };
    let ids = {
        let mut statement = registry.prepare(query)?;
        let rows = statement
            .query_map([&state.checkout], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let id_count = ids.len();
    for id in ids {
        if id.is_empty()
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            anyhow::bail!("invalid registered checkout identity");
        }
        let checkouts = match managed_home.open_dir("checkouts") {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                state.checkout = id;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let checkout = match checkouts.open_dir(&id) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                state.checkout = id;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if checkout.metadata("snapshots")?.is_some() {
            let candidate = checkout.open_dir("snapshots")?;
            let (snapshots, handle_changed) =
                snapshot_state.select(format!("checkout:{id}"), candidate)?;
            reset_handle_bound_cursor(&mut state.directory, handle_changed, cfg!(windows));
            let report = expire_managed_snapshots_dir_page(
                snapshots,
                now,
                SNAPSHOT_HORIZON,
                SNAPSHOT_BYTES,
                SNAPSHOT_DELETES,
                (id == state.checkout)
                    .then(|| state.directory.take())
                    .flatten(),
            )?;
            if report.rewritten_without_memory > 0 {
                state.checkout = id;
                state.directory = None;
                persist_snapshot_cursor(store, &state)?;
                return Ok(false);
            }
            if let Some(cookie) = report.next_cookie {
                state.checkout = id;
                state.directory = Some(cookie);
                persist_snapshot_cursor(store, &state)?;
                return Ok(false);
            }
            snapshot_state.clear();
        } else {
            snapshot_state.clear();
        }
        state.checkout = id;
        state.directory = None;
    }
    if id_count == 8 {
        persist_snapshot_cursor(store, &state)?;
        return Ok(false);
    }

    let ending_generation: i64 = registry.query_row(
        "SELECT value FROM memory_snapshot_membership_generation WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    if ending_generation != cycle_generation
        || !root_snapshot_generation_matches(&managed_home, state.root_fingerprint)?
    {
        persist_snapshot_cursor(store, &CheckoutSnapshotCursor::default())?;
        return Ok(false);
    }
    // Reset before purge. If purge fails, retry begins with another complete
    // cycle rather than trusting a proof captured before the failure.
    persist_snapshot_cursor(store, &CheckoutSnapshotCursor::default())?;
    Ok(true)
}

fn root_snapshot_generation_matches(
    home: &SecureDir,
    expected: Option<ManagedDirFingerprint>,
) -> anyhow::Result<bool> {
    match (home.metadata("snapshots")?, expected) {
        (None, None) => Ok(true),
        (Some(_), Some(expected)) => {
            Ok(home.open_dir("snapshots")?.directory_fingerprint()? == expected)
        }
        _ => Ok(false),
    }
}

fn install_snapshot_membership_generation(connection: &rusqlite::Connection) -> anyhow::Result<()> {
    let transaction = connection.unchecked_transaction()?;
    transaction.execute_batch(r#"
        CREATE TABLE IF NOT EXISTS memory_snapshot_membership_generation(
            id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL);
        INSERT OR IGNORE INTO memory_snapshot_membership_generation VALUES(1,0);
        CREATE TRIGGER IF NOT EXISTS memory_snapshot_checkout_insert AFTER INSERT ON checkout_registry BEGIN
            UPDATE memory_snapshot_membership_generation SET value=value+1 WHERE id=1; END;
        CREATE TRIGGER IF NOT EXISTS memory_snapshot_checkout_delete AFTER DELETE ON checkout_registry BEGIN
            UPDATE memory_snapshot_membership_generation SET value=value+1 WHERE id=1; END;
        CREATE TRIGGER IF NOT EXISTS memory_snapshot_checkout_identity AFTER UPDATE OF checkout_id ON checkout_registry BEGIN
            UPDATE memory_snapshot_membership_generation SET value=value+1 WHERE id=1; END;
        CREATE TRIGGER IF NOT EXISTS memory_snapshot_repository_owner AFTER UPDATE ON repository_home BEGIN
            UPDATE memory_snapshot_membership_generation SET value=value+1 WHERE id=1; END;
    "#)?;
    transaction.commit()?;
    Ok(())
}

fn persist_snapshot_cursor(
    store: &MemoryStore,
    cursor: &CheckoutSnapshotCursor,
) -> anyhow::Result<()> {
    let encoded = serde_json::to_string(cursor)?;
    store.with_connection(|connection| {
        connection.execute(
            "INSERT INTO memory_maintenance_cursor VALUES('snapshots_v2',?1) ON CONFLICT(name) DO UPDATE SET value=excluded.value",
            [&encoded],
        ).map_err(|error| lattice_core::error::LatticeError::Storage(format!("snapshot maintenance cursor: {error}")))?;
        Ok(())
    })?;
    Ok(())
}

pub(crate) fn registry_path() -> anyhow::Result<PathBuf> {
    let root = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))
        .ok_or_else(|| anyhow::anyhow!("cannot locate memory lifecycle registry"))?;
    if !root.is_absolute() {
        anyhow::bail!("memory lifecycle state root must be absolute")
    }
    Ok(root.join("lattice/memory-retention-stores.json"))
}
fn validate_store(path: &Path) -> anyhow::Result<PathBuf> {
    let p = path.canonicalize()?;
    if !p.is_absolute() || !p.is_file() {
        anyhow::bail!("memory lifecycle accepts only an existing canonical database")
    };
    if path.symlink_metadata()?.file_type().is_symlink() {
        anyhow::bail!("memory lifecycle store path cannot be a symlink")
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata(&p)?.uid() != unsafe { libc::geteuid() } {
            anyhow::bail!("memory lifecycle store is not owned by the daemon user")
        }
    }
    let explicitly_configured = configured_shared_store_path()
        .and_then(|configured| configured.canonicalize().ok())
        .as_ref()
        == Some(&p);
    if !explicitly_configured {
        let home = p
            .parent()
            .ok_or_else(|| anyhow::anyhow!("memory store has no authority directory"))?;
        let registry = home.join("storage-registry.db");
        let metadata = registry
            .symlink_metadata()
            .map_err(|_| anyhow::anyhow!("memory store has no repository authority registry"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            anyhow::bail!("memory store authority registry is not a regular file")
        }
        let connection = rusqlite::Connection::open_with_flags(
            &registry,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let recorded: String = connection.query_row(
            "SELECT canonical_home FROM repository_home WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        if Path::new(&recorded) != home.canonicalize()? {
            anyhow::bail!("memory store repository authority does not match its directory")
        }
    }
    let connection =
        rusqlite::Connection::open_with_flags(&p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let has_retention: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='memory_retention_control'",
        [],
        |row| row.get(0),
    )?;
    if has_retention != 1 {
        anyhow::bail!("configured memory database has no retention schema")
    }
    Ok(p)
}

fn configured_shared_store_path() -> Option<PathBuf> {
    let env_org = std::env::var("LATTICE_ORGANIZATION_ID")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let env_path = std::env::var_os("LATTICE_SHARED_MEMORY_PATH").map(PathBuf::from);
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let text = std::fs::read_to_string(home.join(".lattice/config.toml")).unwrap_or_default();
    let mut in_memory = false;
    let mut config_org = None;
    let mut config_path = None;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_memory = line == "[memory]";
        } else if in_memory {
            let (key, value) = line.split_once('=')?;
            let value = value.trim().trim_matches('"').trim_matches('\'');
            match key.trim() {
                "organization_id" if !value.is_empty() => config_org = Some(value.to_owned()),
                "shared_store_path" if !value.is_empty() => {
                    config_path = Some(PathBuf::from(value))
                }
                _ => {}
            }
        }
    }
    (env_org.is_some() || config_org.is_some()).then(|| {
        env_path
            .or(config_path)
            .unwrap_or_else(|| home.join(".lattice/shared/memories.db"))
    })
}
fn read_registry(path: &Path) -> anyhow::Result<BTreeSet<PathBuf>> {
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let metadata = path.symlink_metadata()?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_REGISTRY_BYTES
    {
        anyhow::bail!("memory lifecycle registry is not a bounded regular file")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            anyhow::bail!("memory lifecycle registry is not private daemon-owned state")
        }
    }
    let r: Registry = serde_json::from_slice(&std::fs::read(path)?)?;
    if r.version != REGISTRY_VERSION {
        anyhow::bail!(
            "unsupported memory lifecycle registry version {}",
            r.version
        )
    }
    if r.stores.len() > MAX_REGISTERED_STORES {
        anyhow::bail!("memory lifecycle registry exceeds the registered-store limit")
    }
    let mut valid = BTreeSet::new();
    for p in r.stores {
        match validate_store(&p) {
            Ok(p) => {
                valid.insert(p);
            }
            Err(e) => {
                tracing::warn!(path=%p.display(),error=%e,"ignoring invalid persisted memory store")
            }
        }
    }
    Ok(valid)
}
fn write_registry(path: &Path, stores: &BTreeSet<PathBuf>) -> anyhow::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("registry has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = path.with_extension(format!("{}.{}.tmp", std::process::id(), now()));
    if stores.len() > MAX_REGISTERED_STORES {
        anyhow::bail!("memory lifecycle registry exceeds the registered-store limit")
    }
    let body = serde_json::to_vec(&Registry {
        version: REGISTRY_VERSION,
        stores: stores.clone(),
    })?;
    if body.len() as u64 > MAX_REGISTRY_BYTES {
        anyhow::bail!("memory lifecycle registry exceeds the byte limit")
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&body)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_directory_handle_is_bounded_reused_and_restart_resets_windows_cursor() {
        let root = tempfile::tempdir().unwrap();
        let snapshots = root.path().join("snapshots");
        std::fs::create_dir(&snapshots).unwrap();
        let mut worker = SnapshotWorkerState::default();
        let (_, first_changed) = worker
            .select("root".into(), SecureDir::open(&snapshots).unwrap())
            .unwrap();
        assert!(first_changed);
        assert_eq!(worker.active.iter().count(), 1);
        let (_, reopened_changed) = worker
            .select("root".into(), SecureDir::open(&snapshots).unwrap())
            .unwrap();
        assert!(
            !reopened_changed,
            "stable directory keeps its native handle"
        );

        let mut cursor = Some(SnapshotExpiryCursor {
            directory: Some(lattice_core::storage::ManagedDirCursor::Windows(7)),
            fingerprint: None,
            newest: Some((7, 7)),
            sweeping: false,
            changed_in_sweep: false,
        });
        reset_handle_bound_cursor(&mut cursor, true, true);
        assert!(cursor.is_none(), "fresh Windows worker restarts inventory");
        worker.clear();
        assert!(worker.active.is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_worker_handle_progresses_across_bounded_pages_and_restart_rescans() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        lattice_core::storage::StorageRegistry::open(&home, "repo").unwrap();
        let snapshots = home.join("snapshots");
        std::fs::create_dir(&snapshots).unwrap();
        for index in 0..4_100 {
            std::fs::write(snapshots.join(format!("unknown-{index:05}")), b"x").unwrap();
        }
        let path = home.join("memories.db");
        let store = MemoryStore::open(&path).unwrap();
        let managed_home = SecureDir::open(&home).unwrap();
        let mut worker = SnapshotWorkerState::default();
        let mut complete = false;
        for turn in 0..80 {
            if turn == 5 {
                worker = SnapshotWorkerState::default();
            }
            complete =
                expire_owned_snapshots(&path, &store, now(), &mut worker, &managed_home).unwrap();
            assert!(worker.active.iter().count() <= 1);
            if complete {
                break;
            }
        }
        assert!(
            complete,
            "stable Windows directory eventually completes after restart rescan"
        );
    }

    #[test]
    fn expiry_visits_registered_checkout_snapshots_and_preserves_unknown_roots() {
        use lattice_core::events::Snapshot;
        use lattice_core::graph::CodeGraph;
        use lattice_core::storage::StorageRegistry;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let checkout = dir.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        let mut registry = StorageRegistry::open(&home, "repo").unwrap();
        let checkout_id = format!("checkout_{}", "a".repeat(64));
        let lease = registry
            .register_and_lease(&checkout_id, &checkout, 1)
            .unwrap();
        let path = home.join("memories.db");
        let store = MemoryStore::open(&path).unwrap();
        let snapshots = home.join("checkouts").join(&checkout_id).join("snapshots");
        let unknown = home.join("checkouts/unknown/snapshots");
        std::fs::create_dir_all(&snapshots).unwrap();
        std::fs::create_dir_all(&unknown).unwrap();
        for (dir, row) in [(&snapshots, 9), (&snapshots, 10), (&unknown, 1)] {
            Snapshot::write(
                &dir.join(format!("snapshot-{row}-{row}.bin")),
                &CodeGraph::new(),
                row,
            )
            .unwrap();
        }
        expire_owned_snapshots(
            &path,
            &store,
            now() + 31 * 86400,
            &mut SnapshotWorkerState::default(),
            &SecureDir::open(&home).unwrap(),
        )
        .unwrap();
        assert!(!snapshots.join("snapshot-9-9.bin").exists());
        assert!(snapshots.join("snapshot-10-10.bin").exists());
        assert!(unknown.join("snapshot-1-1.bin").exists());
        drop(lease);
    }

    #[test]
    fn checkout_inserted_before_persisted_cursor_forces_a_complete_new_cycle() {
        use lattice_core::storage::StorageRegistry;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let mut registry = StorageRegistry::open(&home, "repo").unwrap();
        for id in 'b'..='j' {
            let id = id as u8 % 16;
            let id = format!("checkout_{}", format!("{id:x}").repeat(64));
            let checkout = dir.path().join(format!("checkout-{id}"));
            std::fs::create_dir_all(&checkout).unwrap();
            drop(registry.register_and_lease(&id, &checkout, 1).unwrap());
        }
        let path = home.join("memories.db");
        let store = MemoryStore::open(&path).unwrap();
        let mut snapshot_state = SnapshotWorkerState::default();
        let managed_home = SecureDir::open(&home).unwrap();
        assert!(
            !expire_owned_snapshots(&path, &store, now(), &mut snapshot_state, &managed_home,)
                .unwrap()
        );

        let earlier = dir.path().join("checkout-a");
        std::fs::create_dir_all(&earlier).unwrap();
        let earlier_id = format!("checkout_{}", "0".repeat(64));
        drop(
            registry
                .register_and_lease(&earlier_id, &earlier, 1)
                .unwrap(),
        );

        assert!(
            !expire_owned_snapshots(&path, &store, now(), &mut snapshot_state, &managed_home,)
                .unwrap(),
            "membership change must keep purge fenced at apparent end-of-cycle"
        );
        let encoded: String = store
            .with_connection(|connection| {
                Ok(connection
                    .query_row(
                        "SELECT value FROM memory_maintenance_cursor WHERE name='snapshots_v2'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap())
            })
            .unwrap();
        let restarted: CheckoutSnapshotCursor = serde_json::from_str(&encoded).unwrap();
        assert!(restarted.checkout.is_empty());
        assert!(!restarted.root_complete);
        assert!(restarted.membership_generation.is_none());
    }

    #[test]
    fn registry_inventory_ignores_missing_and_noncanonical_entries() {
        let dir = tempfile::tempdir().unwrap();
        let registry = dir.path().join("registry.json");
        let home = dir.path().join("home");
        lattice_core::storage::StorageRegistry::open(&home, "repo").unwrap();
        let valid = home.join("memories.db");
        MemoryStore::open(&valid).unwrap();
        let stores = BTreeSet::from([
            valid.clone(),
            dir.path().join("missing/memories.db"),
            dir.path().join("other.db"),
        ]);
        write_registry(&registry, &stores).unwrap();
        let loaded = read_registry(&registry).unwrap();
        assert_eq!(loaded, BTreeSet::from([valid.canonicalize().unwrap()]));
    }
    #[test]
    fn arbitrary_user_owned_database_is_not_a_retention_authority() {
        let dir = tempfile::tempdir().unwrap();
        let forged = dir.path().join("memories.db");
        MemoryStore::open(&forged).unwrap();
        assert!(validate_store(&forged).is_err());
    }
    #[test]
    fn maintenance_does_not_accelerate_retention_age() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        lattice_core::storage::StorageRegistry::open(&home, "repo").unwrap();
        let path = home.join("memories.db");
        let store = MemoryStore::open(&path).unwrap();
        store.with_connection(|c|{c.execute("INSERT INTO memories(id,content,memory_type,created_at,last_accessed,retention_grace_until)VALUES('m','x','fact',100,100,0),('n','y','fact',100,100,0),('o','z','fact',100,100,0)",[]).unwrap();Ok(())}).unwrap();
        drop(store);
        let mut p = RetentionPolicy::default();
        p.stale_after_secs = 90;
        p.purge_after_secs = 180;
        p.sweep_interval_secs = 3600;
        p.batch_size = 1;
        let mut snapshots = SnapshotWorkerState::default();
        let mut continuation = false;
        maintain_with_continuation(&path, &p, 189, &mut snapshots, &mut continuation).unwrap();
        assert!(continuation);
        assert_eq!(
            maintenance_delay(Duration::from_secs(3600), continuation),
            Duration::from_secs(10)
        );
        for _ in 0..20 {
            maintain_with_continuation(&path, &p, 189, &mut snapshots, &mut continuation).unwrap();
        }
        assert!(!continuation);
        assert_eq!(
            maintenance_delay(Duration::from_secs(3600), continuation),
            Duration::from_secs(3600)
        );
        let reopened = MemoryStore::open(&path).unwrap();
        reopened
            .with_connection(|c| {
                let stale: i64 = c
                    .query_row("SELECT SUM(retention_stale) FROM memories", [], |r| {
                        r.get(0)
                    })
                    .unwrap();
                assert_eq!(stale, 0);
                Ok(())
            })
            .unwrap();
    }
    #[test]
    fn snapshot_failure_prevents_authoritative_memory_purge() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        lattice_core::storage::StorageRegistry::open(&home, "repo").unwrap();
        let path = home.join("memories.db");
        let store = MemoryStore::open(&path).unwrap();
        store.with_connection(|c| { c.execute("INSERT INTO memories(id,content,memory_type,created_at,last_accessed,retention_grace_until,retention_stale)VALUES('m','must survive','fact',1,1,0,1)",[]).unwrap(); Ok(()) }).unwrap();
        let snapshots = home.join("snapshots");
        std::fs::create_dir_all(&snapshots).unwrap();
        std::fs::write(snapshots.join("snapshot-1-1.bin"), b"corrupt legacy copy").unwrap();
        drop(store);
        // The default 90/180-day windows make this row purge-eligible at now=300;
        // the failed snapshot must still prevent that authoritative deletion.
        assert!(maintain(
            &path,
            &RetentionPolicy::default(),
            300,
            &mut SnapshotWorkerState::default()
        )
        .is_err());
        let reopened = MemoryStore::open(&path).unwrap();
        reopened
            .with_connection(|c| {
                let present: i64 = c
                    .query_row("SELECT count(*) FROM memories WHERE id='m'", [], |r| {
                        r.get(0)
                    })
                    .unwrap();
                assert_eq!(present, 1);
                Ok(())
            })
            .unwrap();
    }
    #[test]
    fn persisted_inactive_store_is_eligible_after_restart_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        lattice_core::storage::StorageRegistry::open(&home, "repo").unwrap();
        let path = home.join("memories.db");
        let registry = dir.path().join("registry.json");
        let store = MemoryStore::open(&path).unwrap();
        store.with_connection(|c|{c.execute("INSERT INTO memories(id,content,memory_type,created_at,last_accessed,retention_grace_until)VALUES('m','x','fact',100,100,0)",[]).unwrap();Ok(())}).unwrap();
        drop(store);
        write_registry(&registry, &BTreeSet::from([path.clone()])).unwrap();
        let discovered = read_registry(&registry).unwrap();
        let mut p = RetentionPolicy::default();
        p.stale_after_secs = 90;
        p.purge_after_secs = 180;
        p.sweep_interval_secs = 1;
        for owner in discovered {
            maintain(&owner, &p, 190, &mut SnapshotWorkerState::default()).unwrap()
        }
        let reopened = MemoryStore::open(&path).unwrap();
        reopened
            .with_connection(|c| {
                assert_eq!(
                    c.query_row::<i64, _, _>(
                        "SELECT retention_stale FROM memories WHERE id='m'",
                        [],
                        |r| r.get(0)
                    )
                    .unwrap(),
                    1
                );
                Ok(())
            })
            .unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn symlink_store_registration_is_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("memories.db");
        MemoryStore::open(&real).unwrap();
        let link = dir.path().join("linked.db");
        symlink(&real, &link).unwrap();
        assert!(validate_store(&link).is_err());
    }
}
