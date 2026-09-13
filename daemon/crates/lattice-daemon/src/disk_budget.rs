//! Persistent per-user and per-class disk accounting for proven repository homes.
use anyhow::{bail, Context, Result};
use lattice_core::storage::{
    AccountingLimits, CachePolicy, SecureDir, StorageOperator, StorageRegistry,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[cfg(test)]
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_HOMES: usize = 1024;
const MAX_REGISTRY_BYTES: u64 = 1024 * 1024;
const USER_BUDGET_ENV: &str = "LATTICE_USER_CACHE_BUDGET_BYTES";
const CLASS_BUDGET_ENV: &str = "LATTICE_DISPOSABLE_CACHE_BUDGET_BYTES";
const DEFAULT_USER_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const DEFAULT_CLASS_BYTES: u64 = 6 * 1024 * 1024 * 1024;

const KNOWN_HOMES_VERSION: u16 = 2;
static REGISTRY_STAGE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
#[cfg(test)]
thread_local! {
    static BEFORE_REGISTRY_REPLACE: std::cell::RefCell<Option<Box<dyn FnOnce(&SecureDir, &str) -> std::io::Result<()>>>> = const { std::cell::RefCell::new(None) };
}
const REGISTRY_LEAF: &str = "resource-budget-homes.json";
const LOCK_LEAF: &str = "resource-budget-homes.lock";

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KnownHomes {
    version: u16,
    homes: BTreeMap<String, KnownHome>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KnownHome {
    path: PathBuf,
    device: u64,
    inode: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyKnownHomes {
    version: u16,
    homes: BTreeMap<String, PathBuf>,
}

/// Register a home only after `StorageRegistry` has proven its repository id
/// and canonical location, then account all known homes and reclaim only idle
/// typed checkout-cache bundles through their existing GC journal.
pub(crate) fn register_and_collect(current: &mut StorageRegistry, now: u64) -> Result<()> {
    let id = current.repository_id()?;
    let home = current.repository_home().canonicalize()?;
    let operator = current.operator()?;
    let identity = operator.home_identity()?;
    prove_named_home(&home, identity)?;
    let state = open_registry_directory()?;
    let lock = state.open_or_create_file(LOCK_LEAF)?;
    validate_private_file(&lock, "user cache budget lock")?;
    lock.lock().context("lock user cache budget registry")?;
    let mut known = read_known_for_registration_in(&state, REGISTRY_LEAF)?;
    known.version = KNOWN_HOMES_VERSION;
    known.homes.insert(
        id,
        KnownHome {
            path: home,
            device: identity.dev,
            inode: identity.ino,
        },
    );
    if known.homes.len() > MAX_HOMES {
        bail!("user cache registry exceeds {MAX_HOMES} proven repository homes");
    }
    write_known_in(&state, REGISTRY_LEAF, &known)?;
    lock.unlock()?;
    collect_known(&known, now)
}

fn prove_named_home(path: &Path, expected: lattice_core::storage::ManagedIdentity) -> Result<()> {
    let named = SecureDir::open(path).context("pin named repository home for registration")?;
    if named.identity()? != expected {
        bail!("repository storage home changed while registering its disk-budget authority");
    }
    Ok(())
}

fn collect_known(known: &KnownHomes, now: u64) -> Result<()> {
    let user_limit = env_bytes(USER_BUDGET_ENV, DEFAULT_USER_BYTES);
    let class_limit = env_bytes(CLASS_BUDGET_ENV, DEFAULT_CLASS_BYTES).min(user_limit);
    collect_known_with_limits(known, now, user_limit, class_limit, CachePolicy::default())
}

fn collect_known_with_limits(
    known: &KnownHomes,
    now: u64,
    user_limit: u64,
    class_limit: u64,
    policy: CachePolicy,
) -> Result<()> {
    let mut total_allocated = 0_u64;
    let mut cache_allocated = 0_u64;
    let mut verified = Vec::new();
    let mut incomplete = false;
    for (id, known_home) in known.homes.iter().take(MAX_HOMES) {
        let operator = match StorageOperator::open_existing(&known_home.path, id) {
            Ok(operator) => operator,
            Err(error) => {
                tracing::warn!(repository_id = id, %error, "Skipping unavailable or unproven disk-budget home");
                incomplete = true;
                continue;
            }
        };
        let identity = match operator.home_identity() {
            Ok(identity) => identity,
            Err(error) => {
                tracing::warn!(repository_id = id, %error, "Skipping disk-budget home with unreadable identity");
                incomplete = true;
                continue;
            }
        };
        if identity.dev != known_home.device || identity.ino != known_home.inode {
            tracing::warn!(
                repository_id = id,
                "Skipping replaced disk-budget home; explicit registration is required"
            );
            incomplete = true;
            continue;
        }
        let registry = match operator.open_registry() {
            Ok(registry) => registry,
            Err(error) => {
                tracing::warn!(repository_id = id, %error, "Skipping disk-budget home whose pinned registry cannot be opened");
                incomplete = true;
                continue;
            }
        };
        if let Err(error) = registry
            .advance_shared_accounting(256)
            .and_then(|_| registry.advance_inventory(now, &policy, 256))
        {
            tracing::warn!(repository_id = id, %error, "Bounded disk-budget accounting did not advance");
            incomplete = true;
            continue;
        }
        let status = match operator.status(now, &policy, AccountingLimits::default()) {
            Ok(status) => status,
            Err(error) => {
                tracing::warn!(repository_id = id, %error, "Skipping disk-budget home whose status is unavailable");
                incomplete = true;
                continue;
            }
        };
        if !status.complete {
            tracing::info!(
                repository_id = id,
                "User cache pressure is unknown while bounded repository accounting advances"
            );
            incomplete = true;
            continue;
        }
        total_allocated = total_allocated
            .saturating_add(status.classes.durable_knowledge.allocated_bytes)
            .saturating_add(status.classes.disposable_cache.allocated_bytes)
            .saturating_add(status.classes.telemetry.allocated_bytes);
        cache_allocated =
            cache_allocated.saturating_add(status.classes.disposable_cache.allocated_bytes);
        verified.push((operator, id.clone()));
    }
    if total_allocated <= user_limit && cache_allocated <= class_limit {
        if incomplete {
            tracing::info!("User cache pressure remains unknown because one or more registered homes were unavailable");
        }
        return Ok(());
    }
    if incomplete {
        tracing::info!(known_allocated_bytes = total_allocated, known_cache_bytes = cache_allocated,
            "Aggregate accounting is incomplete; proven homes alone exceed the budget, so their idle caches are eligible for collection");
    }
    // Oldest idle checkout bundles are selected inside each repository. Active
    // leases and every knowledge/telemetry/unknown file are excluded there.
    for (operator, _id) in verified {
        let mut registry = operator.open_registry()?;
        let pressure_policy = CachePolicy {
            high_bytes: 1,
            low_bytes: 0,
            idle_grace_secs: policy.idle_grace_secs,
            batch_files: policy.batch_files,
        };
        registry.advance_inventory(now, &pressure_policy, 256)?;
        let candidates = registry.plan_gc(now, &pressure_policy)?;
        if candidates.is_empty() {
            continue;
        }
        let report = registry.execute_gc(&candidates, now, &pressure_policy)?;
        total_allocated = total_allocated.saturating_sub(report.released_bytes);
        cache_allocated = cache_allocated.saturating_sub(report.released_bytes);
        if total_allocated <= user_limit && cache_allocated <= class_limit {
            break;
        }
    }
    Ok(())
}

fn registry_root() -> Result<PathBuf> {
    let root = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))
        .context("cannot locate user cache budget registry")?;
    if !root.is_absolute() {
        bail!("user cache budget state root must be absolute");
    }
    Ok(root)
}

fn open_registry_directory() -> Result<SecureDir> {
    let root = registry_root()?;
    let mut anchor = PathBuf::new();
    for component in root.components() {
        match component {
            std::path::Component::Prefix(prefix) => anchor.push(prefix.as_os_str()),
            std::path::Component::RootDir => anchor.push(component.as_os_str()),
            _ => break,
        }
    }
    let mut directory = SecureDir::open(&anchor)?;
    for component in root.components() {
        use std::path::Component;
        match component {
            Component::Prefix(_) | Component::RootDir => continue,
            Component::Normal(name) => {
                let name = name
                    .to_str()
                    .context("user cache budget state path is not UTF-8")?;
                directory = match directory.open_dir(name) {
                    Ok(next) => next,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        directory.create_dir(name)?
                    }
                    Err(error) => return Err(error.into()),
                };
            }
            _ => bail!("user cache budget state root has invalid components"),
        }
    }
    validate_state_directory(&directory, "user cache budget state root", false)?;
    let lattice = match directory.open_dir("lattice") {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            directory.create_dir("lattice")?
        }
        Err(error) => return Err(error.into()),
    };
    validate_state_directory(&lattice, "user cache budget directory", true)?;
    Ok(lattice)
}

#[cfg(test)]
fn read_known_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    let parent = path.parent().context("budget registry has no parent")?;
    let directory = lattice_core::storage::SecureDir::open(parent)?;
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("budget registry name is not UTF-8")?;
    let file = match directory.open_file(leaf, false) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).context("user cache budget registry is unsafe or unreadable")
        }
    };
    if file.metadata()?.len() > MAX_REGISTRY_BYTES {
        bail!("user cache budget registry is unsafe or oversized");
    }
    let mut bytes = Vec::new();
    file.take(MAX_REGISTRY_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REGISTRY_BYTES {
        bail!("user cache budget registry is unsafe or oversized");
    }
    Ok(Some(bytes))
}

fn parse_known(bytes: &[u8]) -> Result<KnownHomes> {
    let document: serde_json::Value = serde_json::from_slice(bytes)?;
    if document.get("version").and_then(|value| value.as_u64()) != Some(KNOWN_HOMES_VERSION as u64)
    {
        bail!("user cache budget registry version requires explicit active re-registration");
    }
    let value: KnownHomes = serde_json::from_value(document)?;
    if value.homes.len() > MAX_HOMES {
        bail!("user cache budget registry version or entry bound is invalid");
    }
    Ok(value)
}

#[cfg(test)]
fn read_known(path: &Path) -> Result<KnownHomes> {
    match read_known_bytes(path)? {
        Some(bytes) => parse_known(&bytes),
        None => Ok(KnownHomes::default()),
    }
}

fn read_known_in(directory: &SecureDir, leaf: &str) -> Result<KnownHomes> {
    match read_known_bytes_in(directory, leaf)? {
        Some(bytes) => parse_known(&bytes),
        None => Ok(KnownHomes::default()),
    }
}

#[cfg(test)]
fn read_known_for_registration(path: &Path) -> Result<KnownHomes> {
    let Some(bytes) = read_known_bytes(path)? else {
        return Ok(KnownHomes::default());
    };
    match parse_known(&bytes) {
        Ok(value) => Ok(value),
        Err(error) => {
            let legacy: LegacyKnownHomes =
                serde_json::from_slice(&bytes).with_context(|| error.to_string())?;
            if legacy.version == 1
                && legacy.homes.len() <= MAX_HOMES
                && legacy.homes.values().all(|home| home.is_absolute())
            {
                // Path-only entries never authorize collection. Active homes
                // must register their own retained directory authority again.
                return Ok(KnownHomes {
                    version: KNOWN_HOMES_VERSION,
                    homes: BTreeMap::new(),
                });
            }
            Err(error)
        }
    }
}

fn read_known_for_registration_in(directory: &SecureDir, leaf: &str) -> Result<KnownHomes> {
    let Some(bytes) = read_known_bytes_in(directory, leaf)? else {
        return Ok(KnownHomes::default());
    };
    parse_known_or_legacy(&bytes)
}

fn parse_known_or_legacy(bytes: &[u8]) -> Result<KnownHomes> {
    match parse_known(bytes) {
        Ok(value) => Ok(value),
        Err(error) => {
            let legacy: LegacyKnownHomes =
                serde_json::from_slice(bytes).with_context(|| error.to_string())?;
            if legacy.version == 1
                && legacy.homes.len() <= MAX_HOMES
                && legacy.homes.values().all(|home| home.is_absolute())
            {
                return Ok(KnownHomes {
                    version: KNOWN_HOMES_VERSION,
                    homes: BTreeMap::new(),
                });
            }
            Err(error)
        }
    }
}

fn read_known_bytes_in(directory: &SecureDir, leaf: &str) -> Result<Option<Vec<u8>>> {
    let file = match directory.open_file(leaf, false) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).context("user cache budget registry is unsafe or unreadable")
        }
    };
    validate_private_file(&file, "user cache budget registry")?;
    if file.metadata()?.len() > MAX_REGISTRY_BYTES {
        bail!("user cache budget registry is unsafe or oversized");
    }
    let mut bytes = Vec::new();
    file.take(MAX_REGISTRY_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REGISTRY_BYTES {
        bail!("user cache budget registry is unsafe or oversized");
    }
    Ok(Some(bytes))
}

fn write_known_in(directory: &SecureDir, leaf: &str, known: &KnownHomes) -> Result<()> {
    let bytes = serde_json::to_vec(known)?;
    if bytes.len() as u64 > MAX_REGISTRY_BYTES {
        bail!("user cache budget registry exceeds byte bound");
    }
    let sequence = REGISTRY_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = format!(".{leaf}.{}.{sequence}.tmp", std::process::id());
    let mut file = directory.open_new_file(&temp)?;
    let source = SecureDir::file_identity(&file)?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        #[cfg(test)]
        BEFORE_REGISTRY_REPLACE.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook(directory, &temp)?;
            }
            Ok::<(), std::io::Error>(())
        })?;
        let destination = directory.metadata(leaf)?.map(|entry| entry.identity);
        directory.replace_from(&temp, directory, leaf, source, destination)?;
        directory.sync()
    })();
    if let Err(error) = result {
        let _ = directory.remove_file(&temp, source);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_file(file: &std::fs::File, label: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        bail!("{label} must be user-owned, private, regular, and singly linked");
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_file(file: &std::fs::File, label: &str) -> Result<()> {
    if !file.metadata()?.is_file() {
        bail!("{label} must be a regular file");
    }
    Ok(())
}

#[cfg(unix)]
fn validate_state_directory(directory: &SecureDir, label: &str, private: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(directory.path())?;
    let identity = directory.identity()?;
    if !metadata.is_dir()
        || metadata.dev() != identity.dev
        || metadata.ino() != identity.ino
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & if private { 0o077 } else { 0o022 } != 0
    {
        bail!("{label} must be the pinned user-owned private directory");
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_state_directory(directory: &SecureDir, label: &str, private: bool) -> Result<()> {
    let _ = private;
    if !std::fs::metadata(directory.path())?.is_dir() {
        bail!("{label} must be a directory");
    }
    Ok(())
}

fn env_bytes(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout_id(value: u8) -> String {
        format!("checkout_{value:064x}")
    }

    fn repository(id: &str, seen: u64) -> (tempfile::TempDir, tempfile::TempDir, StorageRegistry) {
        let home = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let mut registry = StorageRegistry::open(home.path(), id).unwrap();
        drop(
            registry
                .register_and_lease(&checkout_id(1), checkout.path(), seen)
                .unwrap(),
        );
        fs::write(
            home.path()
                .join("checkouts")
                .join(checkout_id(1))
                .join("cache/graph.db"),
            vec![1_u8; 8192],
        )
        .unwrap();
        (home, checkout, registry)
    }

    fn known(entries: &[(&str, &Path)]) -> KnownHomes {
        KnownHomes {
            version: KNOWN_HOMES_VERSION,
            homes: entries
                .iter()
                .map(|(id, home)| {
                    let operator = StorageOperator::open_existing(home, id).unwrap();
                    let identity = operator.home_identity().unwrap();
                    (
                        (*id).to_owned(),
                        KnownHome {
                            path: home.canonicalize().unwrap(),
                            device: identity.dev,
                            inode: identity.ino,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn two_repository_aggregate_collects_idle_only_and_preserves_protected_files() {
        let (idle_home, _idle_checkout, idle_registry) = repository("idle-repo", 1);
        let (active_home, active_checkout, mut active_registry) = repository("active-repo", 1);
        let active_lease = active_registry
            .register_and_lease(&checkout_id(1), active_checkout.path(), 1)
            .unwrap();
        let knowledge = idle_home.path().join("memories.db");
        let unknown = idle_home.path().join("operator-note.bin");
        fs::write(&knowledge, vec![2_u8; 4096]).unwrap();
        fs::write(&unknown, vec![3_u8; 4096]).unwrap();
        let idle_cache = idle_home
            .path()
            .join("checkouts")
            .join(checkout_id(1))
            .join("cache");
        let active_cache = active_home
            .path()
            .join("checkouts")
            .join(checkout_id(1))
            .join("cache");
        let status_policy = CachePolicy::default();
        idle_registry
            .advance_inventory(10, &status_policy, 256)
            .unwrap();
        active_registry
            .advance_inventory(10, &status_policy, 256)
            .unwrap();
        let idle_before = StorageOperator::open_existing(idle_home.path(), "idle-repo")
            .unwrap()
            .status(10, &status_policy, AccountingLimits::default())
            .unwrap()
            .classes
            .disposable_cache
            .allocated_bytes;
        let active_before = StorageOperator::open_existing(active_home.path(), "active-repo")
            .unwrap()
            .status(10, &status_policy, AccountingLimits::default())
            .unwrap()
            .classes
            .disposable_cache
            .allocated_bytes;
        assert!(idle_before > 0 && active_before > 0);
        drop(idle_registry);

        let homes = known(&[
            ("active-repo", active_home.path()),
            ("idle-repo", idle_home.path()),
        ]);
        let policy = CachePolicy {
            high_bytes: u64::MAX,
            low_bytes: u64::MAX - 1,
            idle_grace_secs: 1,
            batch_files: 8,
        };
        collect_known_with_limits(&homes, 10, 1, 1, policy.clone()).unwrap();

        assert!(!idle_cache.exists(), "idle typed cache was reclaimed");
        assert!(
            !idle_home.path().join("embedding-objects").exists(),
            "GC must not allocate unused embedding stores"
        );
        assert!(
            !idle_home.path().join("symbol-bodies").exists(),
            "GC must not allocate unused content stores"
        );
        assert!(active_cache.join("graph.db").exists(), "active lease won");
        assert_eq!(fs::read(&knowledge).unwrap(), vec![2_u8; 4096]);
        assert_eq!(fs::read(&unknown).unwrap(), vec![3_u8; 4096]);
        StorageRegistry::open(idle_home.path(), "idle-repo")
            .unwrap()
            .advance_inventory(11, &status_policy, 256)
            .unwrap();
        let idle_after = StorageOperator::open_existing(idle_home.path(), "idle-repo")
            .unwrap()
            .status(11, &status_policy, AccountingLimits::default())
            .unwrap()
            .classes
            .disposable_cache
            .allocated_bytes;
        let active_after = StorageOperator::open_existing(active_home.path(), "active-repo")
            .unwrap()
            .status(11, &status_policy, AccountingLimits::default())
            .unwrap()
            .classes
            .disposable_cache
            .allocated_bytes;
        assert!(idle_after < idle_before);
        assert_eq!(active_after, active_before);
        // Remaining protected/active bytes keep pressure present; retry is
        // bounded and stable rather than deleting another class.
        collect_known_with_limits(&homes, 11, 1, 1, policy).unwrap();
        assert!(active_cache.join("graph.db").exists());
        drop(active_lease);
    }

    #[cfg(unix)]
    #[test]
    fn registry_authority_requires_private_leaf_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let directory = SecureDir::open(root.path()).unwrap();
        validate_state_directory(&directory, "fixture", true).unwrap();
        let file = directory.open_new_file("registry").unwrap();
        validate_private_file(&file, "fixture").unwrap();
        file.set_permissions(fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(validate_private_file(&file, "fixture").is_err());
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_state_directory(&directory, "fixture", true).is_err());
        validate_state_directory(&directory, "state ancestor", false).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn legacy_registration_rejects_symlink_without_reading_target() {
        let state = tempfile::tempdir().unwrap();
        let target = state.path().join("foreign.json");
        fs::write(&target, br#"{"version":1,"homes":{}}"#).unwrap();
        let link = state.path().join("registry.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_known_for_registration(&link).is_err());
        assert_eq!(fs::read(&target).unwrap(), br#"{"version":1,"homes":{}}"#);
    }

    #[test]
    fn forged_stale_and_oversized_registries_are_rejected() {
        let (home, _checkout, registry) = repository("real-repo", 1);
        drop(registry);
        let mut forged = known(&[("real-repo", home.path())]);
        let entry = forged.homes.remove("real-repo").unwrap();
        forged.homes.insert("forged-repo".into(), entry);
        collect_known_with_limits(&forged, 10, 1, 1, CachePolicy::default()).unwrap();
        assert!(home
            .path()
            .join("checkouts")
            .join(checkout_id(1))
            .join("cache/graph.db")
            .exists());

        let state = tempfile::tempdir().unwrap();
        let file = state.path().join("registry.json");
        fs::write(&file, vec![b'x'; MAX_REGISTRY_BYTES as usize + 1]).unwrap();
        assert!(read_known(&file).is_err());
        assert!(read_known_for_registration(&file).is_err());

        let mut bounded = KnownHomes {
            version: KNOWN_HOMES_VERSION,
            homes: BTreeMap::new(),
        };
        let identity = StorageOperator::open_existing(home.path(), "real-repo")
            .unwrap()
            .home_identity()
            .unwrap();
        for value in 0..=MAX_HOMES {
            bounded.homes.insert(
                format!("repo-{value}"),
                KnownHome {
                    path: home.path().to_path_buf(),
                    device: identity.dev,
                    inode: identity.ino,
                },
            );
        }
        let bytes = serde_json::to_vec(&bounded).unwrap();
        fs::write(&file, bytes).unwrap();
        assert!(read_known(&file).is_err());
    }

    #[test]
    fn registered_home_replacement_is_rejected_before_writable_open() {
        let (home, _checkout, registry) = repository("repo", 1);
        drop(registry);
        let known = known(&[("repo", home.path())]);
        let original = home.path().to_path_buf();
        let displaced = original.with_extension("displaced");
        fs::rename(&original, &displaced).unwrap();
        fs::create_dir(&original).unwrap();
        fs::copy(
            displaced.join("storage-registry.db"),
            original.join("storage-registry.db"),
        )
        .unwrap();
        fs::write(original.join("replacement-marker"), b"preserve").unwrap();

        collect_known_with_limits(&known, 10, 1, 1, CachePolicy::default()).unwrap();
        assert_eq!(
            fs::read(original.join("replacement-marker")).unwrap(),
            b"preserve"
        );
        assert!(!original.join("embedding-objects").exists());
        assert!(!original.join("symbol-bodies").exists());

        fs::remove_dir_all(&original).unwrap();
        fs::rename(&displaced, &original).unwrap();
    }

    #[test]
    fn registration_refuses_path_replaced_after_registry_was_pinned() {
        let (home, _checkout, registry) = repository("repo", 1);
        let operator = registry.operator().unwrap();
        let identity = operator.home_identity().unwrap();
        let original = home.path().to_path_buf();
        let displaced = original.with_extension("registration-displaced");
        fs::rename(&original, &displaced).unwrap();
        fs::create_dir(&original).unwrap();
        assert!(prove_named_home(&original, identity).is_err());
        fs::remove_dir_all(&original).unwrap();
        fs::rename(&displaced, &original).unwrap();
    }

    #[test]
    fn stale_home_does_not_block_proven_home_reclamation() {
        let (stale_home, _stale_checkout, stale_registry) = repository("stale", 1);
        let (valid_home, _valid_checkout, valid_registry) = repository("valid", 1);
        valid_registry
            .advance_inventory(10, &CachePolicy::default(), 256)
            .unwrap();
        drop(stale_registry);
        drop(valid_registry);
        let homes = known(&[("stale", stale_home.path()), ("valid", valid_home.path())]);
        let stale_path = stale_home.path().to_path_buf();
        let displaced = stale_path.with_extension("displaced");
        fs::rename(&stale_path, &displaced).unwrap();
        fs::create_dir(&stale_path).unwrap();
        fs::write(stale_path.join("foreign"), b"preserve").unwrap();
        let valid_cache = valid_home
            .path()
            .join("checkouts")
            .join(checkout_id(1))
            .join("cache");
        let policy = CachePolicy {
            high_bytes: 1,
            low_bytes: 0,
            idle_grace_secs: 1,
            batch_files: 8,
        };
        collect_known_with_limits(&homes, 10, 1, 1, policy).unwrap();
        assert!(!valid_cache.exists());
        assert_eq!(fs::read(stale_path.join("foreign")).unwrap(), b"preserve");
        fs::remove_dir_all(&stale_path).unwrap();
        fs::rename(&displaced, &stale_path).unwrap();
    }

    #[test]
    fn legacy_path_only_registry_requires_explicit_reregistration() {
        let state = tempfile::tempdir().unwrap();
        let file = state.path().join("registry.json");
        fs::write(&file, br#"{"version":1,"homes":{"repo":"/tmp/old"}}"#).unwrap();
        assert!(read_known(&file)
            .unwrap_err()
            .to_string()
            .contains("version"));
        let reset = read_known_for_registration(&file).unwrap();
        assert_eq!(reset.version, KNOWN_HOMES_VERSION);
        assert!(reset.homes.is_empty());
    }

    #[test]
    fn failed_registry_publish_cleans_only_its_owned_stage() {
        let state = tempfile::tempdir().unwrap();
        let directory = SecureDir::open(state.path()).unwrap();
        fs::write(state.path().join(REGISTRY_LEAF), b"prior").unwrap();
        BEFORE_REGISTRY_REPLACE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|directory, stage| {
                let owned = directory.metadata(stage)?.unwrap().identity;
                directory.rename_to(stage, directory, "displaced-owned-stage", owned)?;
                let mut foreign = directory.open_new_file(stage)?;
                foreign.write_all(b"foreign")?;
                Ok(())
            }));
        });
        let known = KnownHomes {
            version: KNOWN_HOMES_VERSION,
            homes: BTreeMap::new(),
        };
        assert!(write_known_in(&directory, REGISTRY_LEAF, &known).is_err());
        assert_eq!(
            fs::read(state.path().join(REGISTRY_LEAF)).unwrap(),
            b"prior"
        );
        let foreign = fs::read_dir(state.path())
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .unwrap();
        assert_eq!(fs::read(foreign.path()).unwrap(), b"foreign");
    }
}
