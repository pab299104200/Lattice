use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lattice_core::identity::ContextHandleId;
use lattice_core::intelligence::ExpandContextSeed;
use serde::{Deserialize, Serialize};

use super::workflow_v2::RelevanceSignalScores;

const DEFAULT_CACHE_CAPACITY: usize = 128;
const DEFAULT_CACHE_TTL_SECS: u64 = 20 * 60;
const MAX_PERSISTED_CACHE_BYTES: u64 = 8 * 1024 * 1024;
static MANAGED_STAGE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
thread_local! {
    static MANAGED_PERSIST_FAILURE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
const FAIL_AFTER_CREATE: u64 = 1;
#[cfg(test)]
const FAIL_AFTER_WRITE: u64 = 2;
#[cfg(test)]
const FAIL_BEFORE_REPLACE: u64 = 3;

#[cfg(test)]
fn injected_persist_failure(point: u64) -> std::io::Result<()> {
    if MANAGED_PERSIST_FAILURE.with(|failure| failure.get()) == point {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "injected context cache persistence failure",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedContext {
    pub handle_id: ContextHandleId,
    pub origin: String,
    pub seed: ExpandContextSeed,
    /// Ordered references corresponding to the original expansion seed's
    /// memory positions. Cached lesson payload is never an authority source.
    #[serde(default)]
    pub memory_references: Vec<Option<CachedMemoryReference>>,
    #[serde(default)]
    pub relevance_detail: Option<CachedRelevanceDetail>,
    pub repo_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedMemoryReference {
    pub authority: String,
    pub memory_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedRelevanceDetail {
    pub key: String,
    pub kind: String,
    pub total_score: f64,
    pub ranking_signals: RelevanceSignalScores,
    #[serde(default)]
    pub memory_reference: Option<CachedMemoryReference>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandleRecord {
    pub legacy_handle: String,
    pub handle_id: ContextHandleId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    context: CachedContext,
    expires_at_epoch_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedContextCache {
    sequence: u64,
    entries: Vec<PersistedContextEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedContextEntry {
    handle: String,
    context: CachedContext,
    expires_at_epoch_secs: u64,
}

pub struct ContextHandleCache {
    entries: HashMap<String, CacheEntry>,
    order: VecDeque<String>,
    ttl: Duration,
    capacity: usize,
    sequence: u64,
    persistence_path: Option<PathBuf>,
    publication: Option<(
        lattice_core::storage::cache_publication::CachePublicationAuthority,
        String,
    )>,
    publication_error: Option<String>,
}

impl ContextHandleCache {
    pub fn new_with_persistence(path: impl Into<PathBuf>) -> Self {
        Self::new_with_limits_and_persistence(
            DEFAULT_CACHE_CAPACITY,
            Duration::from_secs(DEFAULT_CACHE_TTL_SECS),
            Some(path.into()),
        )
    }

    #[cfg(test)]
    pub(crate) fn new_with_limits(capacity: usize, ttl: Duration) -> Self {
        Self::new_with_limits_and_persistence(capacity, ttl, None)
    }

    fn new_with_limits_and_persistence(
        capacity: usize,
        ttl: Duration,
        persistence_path: Option<PathBuf>,
    ) -> Self {
        let capacity = capacity.max(1);
        let (publication, publication_error) = match persistence_path.as_ref().map(|path| {
            lattice_core::storage::cache_publication::CachePublicationAuthority::for_path(path)
        }) {
            Some(Ok(value)) => (value, None),
            Some(Err(error)) => (None, Some(error.to_string())),
            None => (None, None),
        };
        let mut cache = Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            ttl,
            capacity,
            sequence: 0,
            persistence_path,
            publication,
            publication_error,
        };
        cache.load_persisted();
        cache
    }

    pub fn insert(
        &mut self,
        origin: impl Into<String>,
        seed: ExpandContextSeed,
        workspace_id: &str,
        session_id: &str,
        repo_epoch: u64,
    ) -> HandleRecord {
        let authority = format!("repository:{workspace_id}");
        self.insert_with_memory_authority(
            origin,
            seed,
            workspace_id,
            session_id,
            repo_epoch,
            &authority,
        )
    }

    pub fn insert_with_memory_authority(
        &mut self,
        origin: impl Into<String>,
        seed: ExpandContextSeed,
        workspace_id: &str,
        session_id: &str,
        repo_epoch: u64,
        repository_authority: &str,
    ) -> HandleRecord {
        self.prune_expired();

        let handle = self.next_handle();
        let handle_id = ContextHandleId {
            workspace_id: workspace_id.to_string(),
            session_id: session_id.to_string(),
            ulid: next_ulid(&mut self.sequence),
        };
        let memory_references = seed
            .memories
            .iter()
            .map(|value| memory_reference(value, repository_authority))
            .collect();
        let mut seed = seed;
        seed.memories.clear();
        let context = CachedContext {
            handle_id: handle_id.clone(),
            origin: origin.into(),
            seed,
            memory_references,
            relevance_detail: None,
            repo_epoch,
        };
        let expires_at_epoch_secs = now_epoch_secs().saturating_add(self.ttl.as_secs().max(1));
        self.entries.insert(
            handle.clone(),
            CacheEntry {
                context,
                expires_at_epoch_secs,
            },
        );
        self.order.push_back(handle.clone());
        self.enforce_capacity();
        self.persist_best_effort();
        HandleRecord {
            legacy_handle: handle,
            handle_id,
        }
    }

    pub fn insert_relevance_detail(
        &mut self,
        origin: impl Into<String>,
        seed: ExpandContextSeed,
        workspace_id: &str,
        session_id: &str,
        repo_epoch: u64,
        detail: CachedRelevanceDetail,
    ) -> HandleRecord {
        let handle = self.insert(origin, seed, workspace_id, session_id, repo_epoch);
        if let Some(entry) = self.entries.get_mut(&handle.legacy_handle) {
            entry.context.memory_references.clear();
            entry.context.relevance_detail = Some(detail);
        }
        self.persist_best_effort();
        handle
    }

    pub fn get(&mut self, handle: &str) -> Option<CachedContext> {
        self.prune_expired();

        let entry = self.entries.get_mut(handle)?;
        entry.expires_at_epoch_secs = now_epoch_secs().saturating_add(self.ttl.as_secs().max(1));
        let context = entry.context.clone();
        self.persist_best_effort();
        Some(context)
    }

    pub fn peek(&mut self, handle: &str) -> Option<CachedContext> {
        self.prune_expired();
        self.entries.get(handle).map(|entry| entry.context.clone())
    }

    pub fn renew(&mut self, handle: &str) -> bool {
        self.prune_expired();
        let Some(entry) = self.entries.get_mut(handle) else {
            return false;
        };
        entry.expires_at_epoch_secs = now_epoch_secs().saturating_add(self.ttl.as_secs().max(1));
        self.persist_best_effort();
        true
    }

    fn next_handle(&mut self) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("ctx-{:x}-{:x}", now, self.sequence.wrapping_add(1))
    }

    fn prune_expired(&mut self) {
        let now = now_epoch_secs();
        self.entries
            .retain(|_, entry| entry.expires_at_epoch_secs > now);
        self.order
            .retain(|handle| self.entries.contains_key(handle));
    }

    fn enforce_capacity(&mut self) {
        while self.entries.len() > self.capacity {
            let Some(handle) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&handle);
        }
    }

    fn load_persisted(&mut self) {
        let Some(path) = self.persistence_path.as_ref() else {
            return;
        };

        let contents = if let Some((authority, leaf)) = &self.publication {
            let directory = match authority.cache_dir() {
                Ok(directory) => directory,
                Err(error) => {
                    tracing::warn!(%error, "Context cache load deferred: cache authority unavailable");
                    return;
                }
            };
            let file = match directory.open_file(leaf, false) {
                Ok(file) => file,
                Err(error) => {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!(%error, "Context cache load deferred: managed file unavailable");
                    }
                    return;
                }
            };
            match read_bounded(file) {
                Ok(contents) => contents,
                Err(error) => {
                    tracing::warn!(%error, "Context cache load deferred: managed file is invalid");
                    return;
                }
            }
        } else {
            if let Some(error) = &self.publication_error {
                tracing::warn!(%error, "Context cache load deferred: managed authority unavailable");
                return;
            }
            let file = match std::fs::File::open(path) {
                Ok(file) => file,
                Err(_) => return,
            };
            match read_bounded(file) {
                Ok(contents) => contents,
                Err(error) => {
                    tracing::warn!(%error, "Context cache load deferred: file is invalid");
                    return;
                }
            }
        };
        let Ok(persisted) = serde_json::from_str::<PersistedContextCache>(&contents) else {
            return;
        };

        self.sequence = persisted.sequence;
        self.entries.clear();
        self.order.clear();

        let now = now_epoch_secs();
        let mut migrated_legacy_payload = false;
        for mut entry in persisted.entries {
            if entry.expires_at_epoch_secs <= now {
                continue;
            }
            // Version-one cache files embedded memory payloads in the seed.
            // Reduce them to ordered references during bounded cache loading
            // and erase the payload before the entry can be served or saved.
            if !entry.context.seed.memories.is_empty() {
                migrated_legacy_payload = true;
                let fallback = format!("repository:{}", entry.context.handle_id.workspace_id);
                entry.context.memory_references = entry
                    .context
                    .seed
                    .memories
                    .iter()
                    .map(|value| memory_reference(value, &fallback))
                    .collect();
                entry.context.seed.memories.clear();
                if entry.context.origin == "relevance_detail" {
                    entry.context.seed.query = None;
                }
            }
            self.order.push_back(entry.handle.clone());
            self.entries.insert(
                entry.handle,
                CacheEntry {
                    context: entry.context,
                    expires_at_epoch_secs: entry.expires_at_epoch_secs,
                },
            );
        }
        self.enforce_capacity();
        if migrated_legacy_payload {
            self.persist_best_effort();
        }
    }

    fn persist_best_effort(&self) {
        let Some(path) = self.persistence_path.as_ref() else {
            return;
        };
        let persisted = PersistedContextCache {
            sequence: self.sequence,
            entries: self
                .order
                .iter()
                .filter_map(|handle| {
                    self.entries.get(handle).map(|entry| PersistedContextEntry {
                        handle: handle.clone(),
                        context: entry.context.clone(),
                        expires_at_epoch_secs: entry.expires_at_epoch_secs,
                    })
                })
                .collect(),
        };

        if let Some(error) = &self.publication_error {
            tracing::warn!(%error, "Context cache persistence deferred: managed authority unavailable");
            return;
        }

        let temp_path = temp_path_for(path);
        if let Ok(serialized) = serde_json::to_vec_pretty(&persisted) {
            if serialized.len() as u64 > MAX_PERSISTED_CACHE_BYTES {
                tracing::warn!("Context cache exceeds the 8 MiB persistence limit; keeping handles in memory only");
                return;
            }
            if let Some((authority, leaf)) = &self.publication {
                let _accounting = match authority.begin() {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::warn!(%error, "Context cache persistence deferred: accounting fence unavailable");
                        return;
                    }
                };
                let directory = match authority.cache_dir() {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::warn!(%error, "Context cache persistence deferred: cache authority unavailable");
                        return;
                    }
                };
                let stage = MANAGED_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let temp = format!(".{leaf}.{}.{stage}.tmp", std::process::id());
                // Keep the source identity from creation through every failure path. The
                // pinned directory and identity check make cleanup safe if the name is reused.
                let mut source_identity = None;
                let result = (|| -> std::io::Result<()> {
                    use std::io::Write;
                    let mut file = directory.open_new_file(&temp)?;
                    source_identity = Some(lattice_core::storage::SecureDir::file_identity(&file)?);
                    let source = source_identity
                        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
                    #[cfg(test)]
                    injected_persist_failure(FAIL_AFTER_CREATE)?;
                    file.write_all(&serialized)?;
                    file.sync_all()?;
                    drop(file);
                    #[cfg(test)]
                    injected_persist_failure(FAIL_AFTER_WRITE)?;
                    let destination = directory.metadata(leaf)?.map(|entry| entry.identity);
                    #[cfg(test)]
                    injected_persist_failure(FAIL_BEFORE_REPLACE)?;
                    directory.replace_from(&temp, &directory, leaf, source, destination)?;
                    directory.sync()
                })();
                if let Err(error) = result {
                    tracing::warn!(%error, "Context cache persistence failed");
                    if let Some(identity) = source_identity {
                        if let Err(cleanup_error) = directory.remove_file(&temp, identity) {
                            if cleanup_error.kind() == std::io::ErrorKind::NotFound {
                                tracing::debug!(stage = %temp, "Context cache staging already absent");
                            } else {
                                tracing::warn!(
                                    stage = %temp,
                                    %cleanup_error,
                                    "Context cache staging cleanup failed"
                                );
                            }
                        }
                    }
                }
                return;
            }
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::write(&temp_path, serialized).is_ok() {
                let _ = std::fs::rename(temp_path, path);
            }
        }
    }
}

fn memory_reference(
    value: &serde_json::Value,
    fallback_authority: &str,
) -> Option<CachedMemoryReference> {
    let identity = value.get("memory_id").or_else(|| value.get("id"))?;
    let (raw_id, workspace) = if let Some(id) = identity.as_str() {
        (id, None)
    } else {
        (
            identity.get("ulid")?.as_str()?,
            identity
                .get("workspace_id")
                .and_then(serde_json::Value::as_str),
        )
    };
    if raw_id.trim().is_empty() {
        return None;
    }
    if let Some(rest) = raw_id.strip_prefix("organization:") {
        let (organization, memory_id) = rest.split_once(':')?;
        if organization.is_empty() || memory_id.is_empty() {
            return None;
        }
        return Some(CachedMemoryReference {
            authority: format!("organization:{organization}"),
            memory_id: memory_id.to_string(),
        });
    }
    if let Some(rest) = raw_id.strip_prefix("repository:") {
        let (repository, memory_id) = rest.rsplit_once(':')?;
        if repository.is_empty() || memory_id.is_empty() {
            return None;
        }
        return Some(CachedMemoryReference {
            authority: format!("repository:{repository}"),
            memory_id: memory_id.to_string(),
        });
    }
    Some(CachedMemoryReference {
        authority: workspace
            .map(|workspace| format!("repository:{workspace}"))
            .unwrap_or_else(|| fallback_authority.to_string()),
        memory_id: raw_id.to_string(),
    })
}

fn read_bounded(mut file: std::fs::File) -> std::io::Result<String> {
    use std::io::Read;
    if file.metadata()?.len() > MAX_PERSISTED_CACHE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "context cache exceeds the 8 MiB load limit",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_PERSISTED_CACHE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PERSISTED_CACHE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "context cache exceeds the 8 MiB load limit",
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "context cache is not valid UTF-8",
        )
    })
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn temp_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|item| item.to_str())
        .unwrap_or("context_handles.json");
    path.with_file_name(format!("{}.tmp", file_name))
}

const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn next_ulid(sequence: &mut u64) -> String {
    *sequence = sequence.wrapping_add(1);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let entropy = (millis << 80) | (((*sequence as u128) << 16) | ((*sequence as u128) & 0xffff));
    encode_crockford_base32(entropy)
}

fn encode_crockford_base32(mut value: u128) -> String {
    let mut encoded = ['0'; 26];
    for slot in encoded.iter_mut().rev() {
        let index = (value & 0x1f) as usize;
        *slot = CROCKFORD_BASE32[index] as char;
        value >>= 5;
    }
    encoded.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use serde_json::json;
    use std::fs;

    const CHECKOUT: &str =
        "checkout_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn seed() -> ExpandContextSeed {
        ExpandContextSeed {
            query: Some("fix auth".to_string()),
            files: vec!["src/auth.ts".to_string()],
            symbols: vec!["loginUser".to_string()],
            tests: vec!["tests/auth.test.ts".to_string()],
            memories: vec![json!({"id": "memory-auth", "content": "auth pattern"})],
        }
    }

    fn temp_file_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("{}-{}.json", name, unique))
    }

    fn managed_fixture() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().expect("create managed fixture");
        let checkout = tempfile::tempdir().expect("create checkout fixture");
        let mut registry = lattice_core::storage::StorageRegistry::open(root.path(), "repository")
            .expect("open storage registry");
        drop(
            registry
                .register_and_lease(CHECKOUT, checkout.path(), 1)
                .expect("register checkout"),
        );
        let path = root
            .path()
            .join("checkouts")
            .join(CHECKOUT)
            .join("cache/context_handles.json");
        (root, path)
    }

    #[test]
    fn test_cache_round_trip() {
        let mut cache = ContextHandleCache::new_with_limits(4, Duration::from_secs(60));
        let handle = cache.insert("prepare_change", seed(), "workspace-a", "session-a", 7);

        let entry = cache
            .get(&handle.legacy_handle)
            .expect("expected cached entry");
        assert_eq!(entry.origin, "prepare_change");
        assert_eq!(entry.seed.files, vec!["src/auth.ts".to_string()]);
        assert!(entry.seed.memories.is_empty());
        assert_eq!(
            entry.memory_references[0].as_ref().unwrap().memory_id,
            "memory-auth"
        );
        assert_eq!(entry.handle_id.workspace_id, "workspace-a");
    }

    #[test]
    fn test_cache_evicts_oldest_when_over_capacity() {
        let mut cache = ContextHandleCache::new_with_limits(1, Duration::from_secs(60));
        let first = cache.insert("prepare_change", seed(), "workspace-a", "session-a", 7);
        let second = cache.insert(
            "get_working_set_context",
            seed(),
            "workspace-a",
            "session-a",
            7,
        );

        assert!(
            cache.get(&first.legacy_handle).is_none(),
            "oldest handle should be evicted"
        );
        assert!(
            cache.get(&second.legacy_handle).is_some(),
            "newest handle should remain"
        );
    }

    #[test]
    fn test_cache_persists_and_restores_handles() {
        let path = temp_file_path("lattice-context-cache");
        let handle = {
            let mut cache = ContextHandleCache::new_with_limits_and_persistence(
                4,
                Duration::from_secs(60),
                Some(path.clone()),
            );
            cache.insert("prepare_change", seed(), "workspace-a", "session-a", 7)
        };

        let mut restored = ContextHandleCache::new_with_limits_and_persistence(
            4,
            Duration::from_secs(60),
            Some(path.clone()),
        );
        let entry = restored
            .get(&handle.legacy_handle)
            .expect("expected persisted handle after restart");
        assert_eq!(entry.origin, "prepare_change");
        assert_eq!(entry.seed.symbols, vec!["loginUser".to_string()]);
        assert_eq!(entry.handle_id.session_id, "session-a");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_cache_skips_expired_persisted_handles() {
        let path = temp_file_path("lattice-context-cache-expired");
        let persisted = PersistedContextCache {
            sequence: 3,
            entries: vec![PersistedContextEntry {
                handle: "ctx-old".to_string(),
                context: CachedContext {
                    handle_id: ContextHandleId {
                        workspace_id: "workspace-a".to_string(),
                        session_id: "session-a".to_string(),
                        ulid: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
                    },
                    origin: "prepare_change".to_string(),
                    seed: seed(),
                    memory_references: Vec::new(),
                    relevance_detail: None,
                    repo_epoch: 7,
                },
                expires_at_epoch_secs: now_epoch_secs().saturating_sub(5),
            }],
        };
        fs::write(&path, serde_json::to_vec(&persisted).expect("serialize")).expect("write cache");

        let mut restored = ContextHandleCache::new_with_limits_and_persistence(
            4,
            Duration::from_secs(60),
            Some(path.clone()),
        );
        assert!(
            restored.get("ctx-old").is_none(),
            "expired handle should not be restored"
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn legacy_persisted_memory_payload_is_immediately_rewritten_as_a_reference() {
        let path = temp_file_path("lattice-context-cache-legacy-memory");
        let persisted = serde_json::json!({
            "sequence": 3,
            "entries": [{
                "handle": "ctx-old",
                "context": {
                    "handle_id": {"workspace_id":"workspace-a","session_id":"session-a","ulid":"01ARZ3NDEKTSV4RRFFQ69G5FAV"},
                    "origin": "relevance_detail",
                    "seed": {"query":"legacy secret lesson","files":[],"symbols":[],"tests":[],"memories":[{"id":"memory-auth","content":"legacy secret lesson"}]},
                    "repo_epoch": 7
                },
                "expires_at_epoch_secs": now_epoch_secs() + 60
            }]
        });
        fs::write(&path, serde_json::to_vec(&persisted).unwrap()).unwrap();

        let mut restored = ContextHandleCache::new_with_limits_and_persistence(
            4,
            Duration::from_secs(60),
            Some(path.clone()),
        );
        let rewritten = fs::read_to_string(&path).expect("rewritten cache");
        assert!(!rewritten.contains("legacy secret lesson"), "{rewritten}");
        let context = restored.peek("ctx-old").expect("migrated handle");
        assert!(context.seed.memories.is_empty());
        assert_eq!(
            context.memory_references[0].as_ref().unwrap().memory_id,
            "memory-auth"
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn cached_memory_references_preserve_positions_and_authorities_without_payloads() {
        let mut cache = ContextHandleCache::new_with_limits(4, Duration::from_secs(60));
        let mut expansion = seed();
        expansion.memories = vec![
            json!({"id":"repo-memory","content":"repository lesson"}),
            json!({"id":"organization:team:org-memory","content":"organization lesson"}),
            json!({"content":"unidentified lesson"}),
        ];
        let handle = cache.insert_with_memory_authority(
            "prepare_change",
            expansion,
            "workspace-path",
            "session-a",
            7,
            "repository:repo-a",
        );
        let context = cache.peek(&handle.legacy_handle).unwrap();
        assert!(context.seed.memories.is_empty());
        assert_eq!(context.memory_references.len(), 3);
        assert_eq!(
            context.memory_references[0].as_ref().unwrap().authority,
            "repository:repo-a"
        );
        assert_eq!(
            context.memory_references[1].as_ref().unwrap().authority,
            "organization:team"
        );
        assert!(context.memory_references[2].is_none());
    }

    #[test]
    fn typed_relevance_snapshot_persists_numeric_signals_without_lesson_prose() {
        let path = temp_file_path("lattice-context-cache-relevance");
        let mut cache = ContextHandleCache::new_with_limits_and_persistence(
            4,
            Duration::from_secs(60),
            Some(path.clone()),
        );
        let signals: RelevanceSignalScores = serde_json::from_value(json!({
            "task_type_compatibility":0.1,"graph_proximity_to_anchors":0.2,
            "exact_identifier_match":0.3,"semantic_similarity":0.4,
            "verification_status":0.5,"freshness":0.6,"scope":0.7,
            "evidence_strength":0.8,"contradiction_supersession_state":0.9,
            "past_usefulness":0.1,"recent_successful_reuse":0.2,
            "user_preference_compatibility":0.3,"token_cost":0.4
        }))
        .unwrap();
        let handle = cache.insert_relevance_detail(
            "relevance_detail",
            ExpandContextSeed {
                query: None,
                files: vec!["src/auth.rs".into()],
                symbols: vec![],
                tests: vec![],
                memories: vec![json!({"content":"lesson prose sentinel"})],
            },
            "workspace-a",
            "session-a",
            7,
            CachedRelevanceDetail {
                key: "organization:cadres:memory-a".into(),
                kind: "memory".into(),
                total_score: 0.75,
                ranking_signals: signals,
                memory_reference: Some(CachedMemoryReference {
                    authority: "organization:cadres".into(),
                    memory_id: "memory-a".into(),
                }),
            },
        );
        let serialized = fs::read_to_string(&path).unwrap();
        assert!(
            !serialized.contains("lesson prose sentinel"),
            "{serialized}"
        );
        assert!(!serialized.contains("inclusion_reason"), "{serialized}");
        let restored = cache.peek(&handle.legacy_handle).unwrap();
        assert_eq!(restored.relevance_detail.unwrap().total_score, 0.75);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn managed_persistence_cleans_owned_stage_after_each_injected_failure() {
        for failure in [FAIL_AFTER_CREATE, FAIL_AFTER_WRITE, FAIL_BEFORE_REPLACE] {
            let (root, path) = managed_fixture();
            fs::write(&path, b"prior destination").expect("write prior destination");
            MANAGED_PERSIST_FAILURE.with(|value| value.set(failure));

            let mut cache = ContextHandleCache::new_with_limits_and_persistence(
                4,
                Duration::from_secs(60),
                Some(path.clone()),
            );
            cache.insert("prepare_change", seed(), "workspace-a", "session-a", 7);
            MANAGED_PERSIST_FAILURE.with(|value| value.set(0));

            assert_eq!(
                fs::read(&path).expect("read destination"),
                b"prior destination"
            );
            let stages = fs::read_dir(path.parent().expect("cache parent"))
                .expect("read cache directory")
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".context_handles.json.")
                })
                .count();
            assert_eq!(stages, 0, "failure point {failure} left a staging file");

            let control = Connection::open(root.path().join("storage-registry.db"))
                .expect("open registry")
                .query_row(
                    "SELECT invalidated FROM storage_inventory_control WHERE id=1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("read inventory state");
            assert_eq!(
                control, 1,
                "publication failure must leave accounting invalidated"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn managed_load_rejects_symlink_and_oversized_files() {
        let (_root, path) = managed_fixture();
        let foreign = path.with_extension("foreign");
        fs::write(&foreign, b"foreign").expect("write foreign file");
        std::os::unix::fs::symlink(&foreign, &path).expect("create cache symlink");
        let mut cache = ContextHandleCache::new_with_limits_and_persistence(
            4,
            Duration::from_secs(60),
            Some(path.clone()),
        );
        assert!(cache.entries.is_empty());
        assert!(fs::symlink_metadata(&path)
            .expect("inspect cache symlink")
            .file_type()
            .is_symlink());

        fs::remove_file(&path).expect("remove symlink");
        fs::write(&path, vec![b'x'; (MAX_PERSISTED_CACHE_BYTES + 1) as usize])
            .expect("write oversized cache");
        let mut cache = ContextHandleCache::new_with_limits_and_persistence(
            4,
            Duration::from_secs(60),
            Some(path.clone()),
        );
        assert!(cache.entries.is_empty());
        assert_eq!(
            fs::metadata(&path).expect("inspect oversized cache").len(),
            MAX_PERSISTED_CACHE_BYTES + 1
        );
    }

    #[test]
    fn managed_load_does_not_follow_replaced_root_path() {
        let (root, path) = managed_fixture();
        let original = root.path().with_extension("original");
        fs::rename(root.path(), &original).expect("rename managed root");
        fs::create_dir(root.path()).expect("create replacement root");
        fs::create_dir_all(path.parent().expect("cache parent")).expect("create foreign cache");
        let persisted = PersistedContextCache {
            sequence: 1,
            entries: vec![PersistedContextEntry {
                handle: "foreign".to_string(),
                context: CachedContext {
                    handle_id: ContextHandleId {
                        workspace_id: "foreign".to_string(),
                        session_id: "foreign".to_string(),
                        ulid: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
                    },
                    origin: "foreign".to_string(),
                    seed: seed(),
                    memory_references: Vec::new(),
                    relevance_detail: None,
                    repo_epoch: 1,
                },
                expires_at_epoch_secs: now_epoch_secs() + 60,
            }],
        };
        fs::write(
            &path,
            serde_json::to_vec(&persisted).expect("serialize foreign cache"),
        )
        .expect("write foreign cache");
        let mut cache = ContextHandleCache::new_with_limits_and_persistence(
            4,
            Duration::from_secs(60),
            Some(path),
        );
        assert!(cache.entries.is_empty());
    }
}
