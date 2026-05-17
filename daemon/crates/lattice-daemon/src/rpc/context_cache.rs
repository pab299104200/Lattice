use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lattice_core::identity::ContextHandleId;
use lattice_core::intelligence::ExpandContextSeed;
use serde::{Deserialize, Serialize};

const DEFAULT_CACHE_CAPACITY: usize = 128;
const DEFAULT_CACHE_TTL_SECS: u64 = 20 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedContext {
    pub handle_id: ContextHandleId,
    pub origin: String,
    pub seed: ExpandContextSeed,
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
        let mut cache = Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            ttl,
            capacity,
            sequence: 0,
            persistence_path,
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
    ) -> HandleRecord {
        self.prune_expired();

        let handle = self.next_handle();
        let handle_id = ContextHandleId {
            workspace_id: workspace_id.to_string(),
            session_id: session_id.to_string(),
            ulid: next_ulid(&mut self.sequence),
        };
        let context = CachedContext {
            handle_id: handle_id.clone(),
            origin: origin.into(),
            seed,
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

    pub fn get(&mut self, handle: &str) -> Option<CachedContext> {
        self.prune_expired();

        let entry = self.entries.get_mut(handle)?;
        entry.expires_at_epoch_secs = now_epoch_secs().saturating_add(self.ttl.as_secs().max(1));
        let context = entry.context.clone();
        self.persist_best_effort();
        Some(context)
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

        let Ok(contents) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(persisted) = serde_json::from_str::<PersistedContextCache>(&contents) else {
            return;
        };

        self.sequence = persisted.sequence;
        self.entries.clear();
        self.order.clear();

        let now = now_epoch_secs();
        for entry in persisted.entries {
            if entry.expires_at_epoch_secs <= now {
                continue;
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

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let temp_path = temp_path_for(path);
        if let Ok(serialized) = serde_json::to_vec_pretty(&persisted) {
            if std::fs::write(&temp_path, serialized).is_ok() {
                let _ = std::fs::rename(temp_path, path);
            }
        }
    }
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
    use serde_json::json;
    use std::fs;

    fn seed() -> ExpandContextSeed {
        ExpandContextSeed {
            query: Some("fix auth".to_string()),
            files: vec!["src/auth.ts".to_string()],
            symbols: vec!["loginUser".to_string()],
            tests: vec!["tests/auth.test.ts".to_string()],
            memories: vec![json!({"content": "auth pattern"})],
        }
    }

    fn temp_file_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("{}-{}.json", name, unique))
    }

    #[test]
    fn test_cache_round_trip() {
        let mut cache = ContextHandleCache::new_with_limits(4, Duration::from_secs(60));
        let handle = cache.insert("prepare_change", seed(), "workspace-a", "session-a");

        let entry = cache
            .get(&handle.legacy_handle)
            .expect("expected cached entry");
        assert_eq!(entry.origin, "prepare_change");
        assert_eq!(entry.seed.files, vec!["src/auth.ts".to_string()]);
        assert_eq!(entry.handle_id.workspace_id, "workspace-a");
    }

    #[test]
    fn test_cache_evicts_oldest_when_over_capacity() {
        let mut cache = ContextHandleCache::new_with_limits(1, Duration::from_secs(60));
        let first = cache.insert("prepare_change", seed(), "workspace-a", "session-a");
        let second = cache.insert(
            "get_working_set_context",
            seed(),
            "workspace-a",
            "session-a",
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
            cache.insert("prepare_change", seed(), "workspace-a", "session-a")
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
}
