//! Authority-bound routing for repository and organization memory stores.
//!
//! The underlying `MemoryStore` is deliberately a small SQLite primitive. This
//! router is the assistant-facing boundary: it prevents a repository store
//! from being used as an organization store, binds organization reads to a
//! trusted handler authority, and preserves the owning authority in every
//! returned identifier.

use super::{Memory, MemoryScope, MemoryStore, MemoryStructuredFields, MemoryVerificationStatus};
use crate::error::LatticeError;
use crate::events::BranchRef;
use crate::verification::ScopeFilter;
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;

const STORE_ROLE_TABLE: &str = "lattice_memory_store_metadata";
const STORE_AUDIT_TABLE: &str = "lattice_memory_scope_audit";

/// The authority that owns a record. It is part of the public memory identity,
/// so a shared memory can never be relabeled as belonging to the querying
/// repository.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MemoryAuthority {
    Repository(String),
    Organization(String),
}

impl MemoryAuthority {
    pub fn encoded(&self) -> String {
        match self {
            Self::Repository(id) => format!("repository:{id}"),
            Self::Organization(id) => format!("organization:{id}"),
        }
    }
}

/// Stable, authority-qualified external identifier for a memory record.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AuthorityQualifiedMemoryId {
    pub authority: MemoryAuthority,
    pub local_id: String,
}

impl AuthorityQualifiedMemoryId {
    pub fn encoded(&self) -> String {
        format!("{}:{}", self.authority.encoded(), self.local_id)
    }
}

/// Trusted, immutable memory authority supplied by daemon startup state.
///
/// `organization_id` is intentionally not request-derived. A request may name
/// an organization only when it exactly matches this configured authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryQueryAuthority {
    pub repository_id: String,
    pub checkout_id: String,
    pub branch: Option<String>,
    pub session_id: String,
    pub organization_id: Option<String>,
}

impl MemoryQueryAuthority {
    pub fn new(
        repository_id: impl Into<String>,
        checkout_id: impl Into<String>,
        branch: Option<String>,
        session_id: impl Into<String>,
        organization_id: Option<String>,
    ) -> Result<Self, LatticeError> {
        let authority = Self {
            repository_id: repository_id.into(),
            checkout_id: checkout_id.into(),
            branch,
            session_id: session_id.into(),
            organization_id,
        };
        if authority.repository_id.trim().is_empty()
            || authority.checkout_id.trim().is_empty()
            || authority.session_id.trim().is_empty()
            || authority
                .organization_id
                .as_deref()
                .is_some_and(|id| id.trim().is_empty())
        {
            return Err(LatticeError::Storage(
                "memory query authority requires non-empty repository, checkout, session, and organization identifiers"
                    .to_string(),
            ));
        }
        Ok(authority)
    }

    fn repository_scope_filter(&self) -> ScopeFilter {
        ScopeFilter::new(
            self.repository_id.clone(),
            self.branch.clone().map(|name| BranchRef { name }),
            None,
        )
        .for_session(self.session_id.clone())
    }

    fn organization_scope_filter(&self, organization_id: &str) -> ScopeFilter {
        ScopeFilter::new(
            self.repository_id.clone(),
            self.branch.clone().map(|name| BranchRef { name }),
            Some(organization_id.to_string()),
        )
        .for_session(self.session_id.clone())
    }
}

/// Physical store role. The database stores this role in metadata, so opening
/// a repository database as a shared database (or the reverse) fails loudly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryStoreRole {
    Repository { repository_id: String },
    Shared { organization_ids: Vec<String> },
}

impl MemoryStoreRole {
    fn kind(&self) -> &'static str {
        match self {
            Self::Repository { .. } => "repository",
            Self::Shared { .. } => "shared",
        }
    }
}

/// Source tier included in every merged recall record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryRecallTier {
    Repository,
    Organization,
}

/// A memory normalized for deterministic merged recall.
#[derive(Debug, Clone)]
pub struct MemoryRecallResult {
    pub memory: Memory,
    pub memory_id: AuthorityQualifiedMemoryId,
    pub source_tier: MemoryRecallTier,
    pub assertion_key: String,
    pub origin_repository_id: Option<String>,
    pub origin_checkout_id: Option<String>,
    pub cross_repo: bool,
    pub origin_verification_status: MemoryVerificationStatus,
    pub effective_verification_status: MemoryVerificationStatus,
    pub trust_reason: String,
}

/// Router over a repository-private store and optional organization-shared
/// store. It intentionally has no unscoped assistant-facing query API.
pub struct MemoryStoreRouter<'a> {
    repository_store: &'a MemoryStore,
    shared_store: Option<&'a MemoryStore>,
    authority: MemoryQueryAuthority,
}

impl<'a> MemoryStoreRouter<'a> {
    pub fn new(
        repository_store: &'a MemoryStore,
        shared_store: Option<&'a MemoryStore>,
        authority: MemoryQueryAuthority,
    ) -> Result<Self, LatticeError> {
        ensure_store_role(
            repository_store,
            &MemoryStoreRole::Repository {
                repository_id: authority.repository_id.clone(),
            },
        )?;
        if let Some(store) = shared_store {
            let organization_ids = authority.organization_id.iter().cloned().collect();
            ensure_store_role(store, &MemoryStoreRole::Shared { organization_ids })?;
        }
        Ok(Self {
            repository_store,
            shared_store,
            authority,
        })
    }

    pub fn authority(&self) -> &MemoryQueryAuthority {
        &self.authority
    }

    /// Save a memory through its owning store. Organization requests cannot
    /// grant or widen organization authority: an optional supplied ID must
    /// exactly match daemon configuration.
    pub fn remember(
        &self,
        mut memory: Memory,
        fields: &MemoryStructuredFields,
        requested_organization_id: Option<&str>,
    ) -> Result<AuthorityQualifiedMemoryId, LatticeError> {
        match memory.scope {
            MemoryScope::Organization => {
                let configured = self.require_organization("remember")?;
                if requested_organization_id.is_some_and(|id| id != configured)
                    || memory
                        .scope_organization_id
                        .as_deref()
                        .is_some_and(|id| id != configured)
                {
                    self.audit_denial("remember", "organization authority does not match daemon configuration")?;
                    return Err(LatticeError::Storage(
                        "organization memory request does not match configured organization authority"
                            .to_string(),
                    ));
                }
                let store = self.shared_store.ok_or_else(|| {
                    LatticeError::Storage(
                        "organization memory is configured but the shared memory store is unavailable"
                            .to_string(),
                    )
                })?;
                memory.scope_organization_id = Some(configured.to_string());
                memory.workspace_id = Some(self.authority.repository_id.clone());
                memory.branch = None;
                memory.verification_status = MemoryVerificationStatus::Unverified;
                let id = store.store(memory)?;
                store.update_structured_fields(&id, fields)?;
                Ok(AuthorityQualifiedMemoryId {
                    authority: MemoryAuthority::Organization(configured.to_string()),
                    local_id: id,
                })
            }
            _ => {
                if requested_organization_id.is_some() {
                    self.audit_denial("remember", "non-organization memory supplied organization authority")?;
                    return Err(LatticeError::Storage(
                        "only organization-scoped memory may name an organization".to_string(),
                    ));
                }
                memory.workspace_id = Some(self.authority.repository_id.clone());
                let id = self.repository_store.store(memory)?;
                self.repository_store.update_structured_fields(&id, fields)?;
                Ok(AuthorityQualifiedMemoryId {
                    authority: MemoryAuthority::Repository(self.authority.repository_id.clone()),
                    local_id: id,
                })
            }
        }
    }

    /// Merged, bounded recall. Each store is searched independently before
    /// merging; a shared tier is never opened or searched without configured
    /// organization authority.
    pub fn recall(
        &self,
        keyword: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryRecallResult>, LatticeError> {
        let limit = limit.max(1);
        let oversample = limit.saturating_mul(2).clamp(8, 64);
        let repository = self
            .repository_store
            .query(keyword, oversample, &self.authority.repository_scope_filter())?;
        let mut ranked = repository
            .into_iter()
            .enumerate()
            .map(|(query_rank, memory)| self.normalize(memory, MemoryRecallTier::Repository, query_rank))
            .collect::<Vec<_>>();

        if let (Some(organization_id), Some(shared_store)) =
            (self.authority.organization_id.as_deref(), self.shared_store)
        {
            let shared = shared_store.query(
                keyword,
                oversample,
                &self.authority.organization_scope_filter(organization_id),
            )?;
            ranked.extend(shared.into_iter().enumerate().map(|(query_rank, memory)| {
                self.normalize(memory, MemoryRecallTier::Organization, query_rank)
            }));
        }

        ranked.sort_by(|left, right| recall_order(left, right));
        ranked.dedup_by(|left, right| left.result.memory_id == right.result.memory_id);
        ranked.truncate(limit);
        Ok(ranked.into_iter().map(|ranked| ranked.result).collect())
    }

    /// Move legacy organization rows out of the repository database. Copying
    /// first and invalidating only after the shared write makes the operation
    /// restart-safe and idempotent.
    pub fn migrate_legacy_organization_memories(&self) -> Result<usize, LatticeError> {
        let configured = self.require_organization("migrate organization memories")?;
        let shared_store = self.shared_store.ok_or_else(|| {
            LatticeError::Storage("shared memory store is unavailable".to_string())
        })?;
        let legacy = self
            .repository_store
            .query_unscoped_admin(None, usize::MAX)?
            .into_iter()
            .filter(|memory| {
                memory.scope == MemoryScope::Organization
                    && memory.scope_organization_id.as_deref() == Some(configured)
            })
            .collect::<Vec<_>>();
        let mut migrated = 0;
        for memory in legacy {
            if shared_store.get_by_id(&memory.id)?.is_none() {
                let fields = self
                    .repository_store
                    .get_structured_fields(&memory.id)?
                    .unwrap_or_default();
                shared_store.store(memory.clone())?;
                shared_store.update_structured_fields(&memory.id, &fields)?;
            }
            self.repository_store.invalidate(&memory.id)?;
            migrated += 1;
        }
        Ok(migrated)
    }

    fn normalize(
        &self,
        memory: Memory,
        tier: MemoryRecallTier,
        query_rank: usize,
    ) -> RankedRecall {
        let authority = match tier {
            MemoryRecallTier::Repository => {
                MemoryAuthority::Repository(self.authority.repository_id.clone())
            }
            MemoryRecallTier::Organization => MemoryAuthority::Organization(
                memory.scope_organization_id.clone().unwrap_or_default(),
            ),
        };
        let origin_repository_id = match tier {
            MemoryRecallTier::Repository => Some(self.authority.repository_id.clone()),
            MemoryRecallTier::Organization => memory.workspace_id.clone(),
        };
        let cross_repo = matches!(tier, MemoryRecallTier::Organization)
            && origin_repository_id.as_deref() != Some(self.authority.repository_id.as_str());
        let origin_verification_status = memory.verification_status;
        let effective_verification_status = if cross_repo {
            MemoryVerificationStatus::Unverified
        } else {
            origin_verification_status
        };
        let trust_reason = if cross_repo {
            "organization memory is advisory: origin repository evidence has not been verified in this repository".to_string()
        } else {
            "memory verification applies to the current repository authority".to_string()
        };
        RankedRecall {
            query_rank,
            result: MemoryRecallResult {
                assertion_key: assertion_key(&memory),
                memory_id: AuthorityQualifiedMemoryId {
                    authority,
                    local_id: memory.id.clone(),
                },
                memory,
                source_tier: tier,
                origin_repository_id,
                origin_checkout_id: None,
                cross_repo,
                origin_verification_status,
                effective_verification_status,
                trust_reason,
            },
        }
    }

    fn require_organization(&self, operation: &str) -> Result<&str, LatticeError> {
        self.authority.organization_id.as_deref().ok_or_else(|| {
            let _ = self.audit_denial(operation, "no configured organization authority");
            LatticeError::Storage(format!(
                "{operation} requires a configured organization authority; configure [memory].organization_id"
            ))
        })
    }

    fn audit_denial(&self, operation: &str, reason: &str) -> Result<(), LatticeError> {
        self.repository_store.with_connection(|conn| {
            conn.execute(
                &format!(
                    "INSERT INTO {STORE_AUDIT_TABLE} (operation, repository_id, checkout_id, organization_id, reason) VALUES (?1, ?2, ?3, ?4, ?5)"
                ),
                params![operation, self.authority.repository_id, self.authority.checkout_id, self.authority.organization_id, reason],
            )
            .map_err(|error| {
                LatticeError::Storage(format!("failed to record memory scope denial: {error}"))
            })?;
            Ok(())
        })
    }
}

struct RankedRecall {
    query_rank: usize,
    result: MemoryRecallResult,
}

fn recall_order(left: &RankedRecall, right: &RankedRecall) -> Ordering {
    left.query_rank
        .cmp(&right.query_rank)
        .then_with(|| verification_rank(right.result.effective_verification_status).cmp(&verification_rank(left.result.effective_verification_status)))
        .then_with(|| tier_rank(right.result.source_tier).cmp(&tier_rank(left.result.source_tier)))
        .then_with(|| right.result.memory.confidence.partial_cmp(&left.result.memory.confidence).unwrap_or(Ordering::Equal))
        .then_with(|| right.result.memory.access_count.cmp(&left.result.memory.access_count))
        .then_with(|| right.result.memory.created_at.cmp(&left.result.memory.created_at))
        .then_with(|| left.result.memory_id.cmp(&right.result.memory_id))
}

fn verification_rank(status: MemoryVerificationStatus) -> u8 {
    match status {
        MemoryVerificationStatus::Verified => 7,
        MemoryVerificationStatus::InReview => 6,
        MemoryVerificationStatus::Unverified => 5,
        MemoryVerificationStatus::Expired => 2,
        MemoryVerificationStatus::Superseded => 1,
        MemoryVerificationStatus::Stale
        | MemoryVerificationStatus::Contradicted
        | MemoryVerificationStatus::Invalidated => 0,
    }
}

fn tier_rank(tier: MemoryRecallTier) -> u8 {
    match tier {
        MemoryRecallTier::Repository => 1,
        MemoryRecallTier::Organization => 0,
    }
}

fn assertion_key(memory: &Memory) -> String {
    let slot = memory
        .refresh_key
        .as_deref()
        .unwrap_or(memory.content.as_str())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(b"lattice.assertion-key.v1\\0");
    hasher.update(memory.memory_type.as_str().as_bytes());
    hasher.update(b"\\0");
    hasher.update(slot.as_bytes());
    format!("v1:{:x}", hasher.finalize())
}

fn ensure_store_role(store: &MemoryStore, role: &MemoryStoreRole) -> Result<(), LatticeError> {
    store.with_connection(|conn| {
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {STORE_ROLE_TABLE} (key TEXT PRIMARY KEY, value TEXT NOT NULL);\
             CREATE TABLE IF NOT EXISTS {STORE_AUDIT_TABLE} (\
                audit_id INTEGER PRIMARY KEY AUTOINCREMENT,\
                operation TEXT NOT NULL, repository_id TEXT NOT NULL, checkout_id TEXT NOT NULL,\
                organization_id TEXT, reason TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT (unixepoch())\
             );"
        ))
        .map_err(|error| LatticeError::Storage(format!("failed to initialize memory store role metadata: {error}")))?;
        let existing = conn
            .query_row(
                &format!("SELECT value FROM {STORE_ROLE_TABLE} WHERE key = 'role'"),
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| LatticeError::Storage(format!("failed to load memory store role metadata: {error}")))?;
        match existing {
            Some(found) if found != role.kind() => Err(LatticeError::Storage(format!(
                "memory store role mismatch: database is '{found}', requested '{}'",
                role.kind()
            ))),
            Some(_) => Ok(()),
            None => {
                conn.execute(
                    &format!("INSERT INTO {STORE_ROLE_TABLE} (key, value) VALUES ('role', ?1)"),
                    params![role.kind()],
                )
                .map_err(|error| LatticeError::Storage(format!("failed to persist memory store role: {error}")))?;
                Ok(())
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryClass, MemoryType};

    fn authority(repository: &str, organization: Option<&str>) -> MemoryQueryAuthority {
        MemoryQueryAuthority::new(
            repository,
            format!("checkout-{repository}"),
            Some("main".to_string()),
            "session-1",
            organization.map(str::to_string),
        )
        .unwrap()
    }

    fn memory(scope: MemoryScope, organization: Option<&str>, content: &str) -> Memory {
        Memory {
            id: String::new(), session_id: "session-1".to_string(), content: content.to_string(),
            memory_type: MemoryType::Observation, scope, confidence: 0.8,
            linked_symbols: vec![], linked_files: vec![], workspace_id: None, branch: None,
            scope_organization_id: organization.map(str::to_string), refresh_key: Some("claim".to_string()),
            source_query: None, created_at: 0, last_accessed: 0, access_count: 0,
            is_stale: false, stale_reason: None, verification_status: MemoryVerificationStatus::Verified,
        }
    }

    fn fields() -> MemoryStructuredFields {
        MemoryStructuredFields { memory_class: MemoryClass::Observation, ..Default::default() }
    }

    #[test]
    fn organization_memory_is_shared_but_cross_repo_is_unverified_and_advisory() {
        let repo_a = MemoryStore::open_in_memory().unwrap();
        let repo_b = MemoryStore::open_in_memory().unwrap();
        let shared = MemoryStore::open_in_memory().unwrap();
        let router_a = MemoryStoreRouter::new(&repo_a, Some(&shared), authority("repo-a", Some("cadres"))).unwrap();
        let stored = router_a.remember(memory(MemoryScope::Organization, Some("cadres"), "shared deployment rule"), &fields(), Some("cadres")).unwrap();
        assert_eq!(stored.authority, MemoryAuthority::Organization("cadres".to_string()));
        assert!(repo_a.query_unscoped_admin(None, 10).unwrap().is_empty());

        let router_b = MemoryStoreRouter::new(&repo_b, Some(&shared), authority("repo-b", Some("cadres"))).unwrap();
        let result = router_b.recall(Some("deployment"), 10).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].cross_repo);
        assert_eq!(result[0].effective_verification_status, MemoryVerificationStatus::Unverified);
        assert_eq!(result[0].memory_id.authority, MemoryAuthority::Organization("cadres".to_string()));
        assert!(result[0].trust_reason.contains("advisory"));
    }

    #[test]
    fn request_cannot_widen_organization_authority() {
        let repository = MemoryStore::open_in_memory().unwrap();
        let shared = MemoryStore::open_in_memory().unwrap();
        let router = MemoryStoreRouter::new(&repository, Some(&shared), authority("repo-a", Some("cadres"))).unwrap();
        let error = router.remember(memory(MemoryScope::Organization, Some("other"), "private"), &fields(), Some("other")).unwrap_err();
        assert!(error.to_string().contains("does not match"));
        assert!(shared.query_unscoped_admin(None, 10).unwrap().is_empty());
    }

    #[test]
    fn shared_store_role_cannot_be_reopened_as_repository_store() {
        let repository = MemoryStore::open_in_memory().unwrap();
        let shared = MemoryStore::open_in_memory().unwrap();
        MemoryStoreRouter::new(&repository, Some(&shared), authority("repo-a", Some("cadres"))).unwrap();
        let other_shared = MemoryStore::open_in_memory().unwrap();
        let error = match MemoryStoreRouter::new(
            &shared,
            Some(&other_shared),
            authority("repo-b", Some("cadres")),
        ) {
            Ok(_) => panic!("shared store must not be reopened as repository store"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("role mismatch"));
    }

    #[test]
    fn recall_never_queries_shared_store_without_configured_authority() {
        let repository = MemoryStore::open_in_memory().unwrap();
        let shared = MemoryStore::open_in_memory().unwrap();
        let router = MemoryStoreRouter::new(&repository, Some(&shared), authority("repo-a", None)).unwrap();
        shared.store(memory(MemoryScope::Organization, Some("cadres"), "must remain hidden")).unwrap();
        assert!(router.recall(Some("hidden"), 10).unwrap().is_empty());
    }
}
