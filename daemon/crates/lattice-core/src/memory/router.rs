//! Authority-bound routing for repository and organization memory stores.
//!
//! The underlying `MemoryStore` is deliberately a small SQLite primitive. This
//! router is the assistant-facing boundary: it prevents a repository store
//! from being used as an organization store, binds organization reads to a
//! trusted handler authority, and preserves the owning authority in every
//! returned identifier.

use super::model::MemoryProvenance;
use super::session_capture::{
    SessionCaptureDeletionResult, SessionCaptureRetentionPolicy, SessionCaptureSelector,
};
use super::session_digest::{
    extract_default_session_digest_candidates, SessionDigest, SessionDigestCandidate,
    SESSION_DIGEST_EXTRACTOR_VERSION,
};
use super::{Memory, MemoryScope, MemoryStore, MemoryStructuredFields, MemoryVerificationStatus};
use crate::error::LatticeError;
use crate::events::BranchRef;
use crate::verification::ScopeFilter;
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;

const STORE_ROLE_TABLE: &str = "lattice_memory_store_metadata";
const STORE_AUDIT_TABLE: &str = "lattice_memory_scope_audit";
const REPOSITORY_ID_METADATA_KEY: &str = "repository_id";
const SHARED_STORE_QUERY_WORKSPACE_ID: &str = "__lattice_shared_memory_router__";
const ORIGIN_CHECKOUT_PROVENANCE_SOURCE: &str = "lattice.memory.origin_checkout.v1";

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
        // ScopeFilter is a union of repository, branch, session, and
        // organization visibility. A shared store must admit organization rows
        // only, so keep its non-organization branches impossible and enforce
        // the exact scope again after querying malformed legacy rows.
        ScopeFilter::new(
            SHARED_STORE_QUERY_WORKSPACE_ID,
            None,
            Some(organization_id.to_string()),
        )
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

/// Durable outcome of one repository-owned automatic session capture batch.
/// Exact retries return the same qualified IDs and counts with `replayed` set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDigestCaptureResult {
    pub delivery_key: String,
    pub memory_ids: Vec<AuthorityQualifiedMemoryId>,
    pub candidate_count: usize,
    pub committed_count: usize,
    pub dropped_observation_count: usize,
    pub replayed: bool,
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
                    self.audit_denial(
                        "remember",
                        "organization authority does not match daemon configuration",
                    )?;
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
                let mut shared_fields = fields.clone();
                shared_fields.verification_status = MemoryVerificationStatus::Unverified;
                record_origin_checkout(&mut shared_fields, &self.authority.checkout_id);
                let id = store.store(memory)?;
                store.update_structured_fields(&id, &shared_fields)?;
                Ok(AuthorityQualifiedMemoryId {
                    authority: MemoryAuthority::Organization(configured.to_string()),
                    local_id: id,
                })
            }
            _ => {
                if requested_organization_id.is_some() {
                    self.audit_denial(
                        "remember",
                        "non-organization memory supplied organization authority",
                    )?;
                    return Err(LatticeError::Storage(
                        "only organization-scoped memory may name an organization".to_string(),
                    ));
                }
                memory.workspace_id = Some(self.authority.repository_id.clone());
                let id = self.repository_store.store(memory)?;
                self.repository_store
                    .update_structured_fields(&id, fields)?;
                Ok(AuthorityQualifiedMemoryId {
                    authority: MemoryAuthority::Repository(self.authority.repository_id.clone()),
                    local_id: id,
                })
            }
        }
    }

    /// Persist the deterministic candidate batch for an authority-bound
    /// session digest in one repository-store transaction.
    ///
    /// Capture has no organization target or shared-store fallback. Candidate
    /// normalization is verified again at this boundary so a caller cannot
    /// retain an idempotency key while changing its claim or evidence.
    pub fn capture_session_digest_candidate_batch(
        &self,
        digest: &SessionDigest,
        candidates: &[SessionDigestCandidate],
    ) -> Result<SessionDigestCaptureResult, LatticeError> {
        if digest.repository_id != self.authority.repository_id
            || digest.session_id != self.authority.session_id
            || digest.checkout_id.as_deref() != Some(self.authority.checkout_id.as_str())
            || digest.branch != self.authority.branch
        {
            self.audit_denial(
                "capture session digest",
                "session digest authority does not match the repository router",
            )?;
            return Err(LatticeError::Storage(
                "automatic session capture authority does not match the repository router"
                    .to_string(),
            ));
        }

        let expected = extract_default_session_digest_candidates(digest);
        if candidates != expected.as_slice() {
            self.audit_denial(
                "capture session digest",
                "candidate batch differs from deterministic normalized extraction",
            )?;
            return Err(LatticeError::Storage(
                "automatic session capture candidate batch is not the deterministic normalized batch"
                    .to_string(),
            ));
        }

        let persisted = self
            .repository_store
            .persist_session_digest_candidate_batch(
                digest,
                candidates,
                SESSION_DIGEST_EXTRACTOR_VERSION,
            )?;
        Ok(SessionDigestCaptureResult {
            delivery_key: persisted.delivery_key,
            memory_ids: persisted
                .memory_ids
                .into_iter()
                .map(|local_id| AuthorityQualifiedMemoryId {
                    authority: MemoryAuthority::Repository(self.authority.repository_id.clone()),
                    local_id,
                })
                .collect(),
            candidate_count: persisted.candidate_count,
            committed_count: persisted.committed_count,
            dropped_observation_count: persisted.dropped_observation_count,
            replayed: persisted.replayed,
        })
    }

    /// Apply repository-local age-and-count retention to automatic captures.
    pub fn prune_session_captures(
        &self,
        policy: SessionCaptureRetentionPolicy,
        now: crate::DateTime<crate::Utc>,
    ) -> Result<SessionCaptureDeletionResult, LatticeError> {
        self.repository_store.prune_session_captures(
            &self.authority.repository_id,
            policy,
            now.unix_seconds(),
        )
    }

    /// Delete automatic captures selected by an opaque session or capture ID.
    /// The repository qualifier is a strict lease and cannot be supplied from
    /// another repository authority.
    pub fn delete_session_captures(
        &self,
        selector: &SessionCaptureSelector,
        deleted_at: crate::DateTime<crate::Utc>,
    ) -> Result<SessionCaptureDeletionResult, LatticeError> {
        if selector.repository_id() != self.authority.repository_id {
            self.audit_denial(
                "delete session captures",
                "capture selector repository does not match the repository router",
            )?;
            return Err(LatticeError::Storage(
                "session capture selector does not match repository authority".to_string(),
            ));
        }
        self.repository_store.delete_session_captures(
            &self.authority.repository_id,
            selector,
            deleted_at.unix_seconds(),
        )
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
        let repository = self.repository_store.query_for_checkout(
            keyword,
            oversample,
            &self.authority.repository_scope_filter(),
            &self.authority.checkout_id,
        )?;
        let mut ranked = repository
            .into_iter()
            .enumerate()
            .map(|(query_rank, memory)| {
                self.normalize(memory, MemoryRecallTier::Repository, query_rank)
            })
            .collect::<Vec<_>>();

        if let (Some(organization_id), Some(shared_store)) =
            (self.authority.organization_id.as_deref(), self.shared_store)
        {
            let shared = shared_store.query(
                keyword,
                oversample,
                &self.authority.organization_scope_filter(organization_id),
            )?;
            ranked.extend(
                shared
                    .into_iter()
                    .filter(|memory| {
                        memory.scope == MemoryScope::Organization
                            && memory.scope_organization_id.as_deref() == Some(organization_id)
                    })
                    .filter(recall_eligible)
                    .enumerate()
                    .map(|(query_rank, memory)| {
                        self.normalize(memory, MemoryRecallTier::Organization, query_rank)
                    }),
            );
        }

        ranked.retain(|ranked| recall_eligible(&ranked.result.memory));

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

    fn normalize(&self, memory: Memory, tier: MemoryRecallTier, query_rank: usize) -> RankedRecall {
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
        .then_with(|| {
            verification_rank(right.result.effective_verification_status).cmp(&verification_rank(
                left.result.effective_verification_status,
            ))
        })
        .then_with(|| tier_rank(right.result.source_tier).cmp(&tier_rank(left.result.source_tier)))
        .then_with(|| {
            right
                .result
                .memory
                .confidence
                .partial_cmp(&left.result.memory.confidence)
                .unwrap_or(Ordering::Equal)
        })
        .then_with(|| {
            right
                .result
                .memory
                .access_count
                .cmp(&left.result.memory.access_count)
        })
        .then_with(|| {
            right
                .result
                .memory
                .created_at
                .cmp(&left.result.memory.created_at)
        })
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

fn recall_eligible(memory: &Memory) -> bool {
    !memory.is_stale
        && !matches!(
            memory.verification_status,
            MemoryVerificationStatus::Stale
                | MemoryVerificationStatus::Contradicted
                | MemoryVerificationStatus::Superseded
                | MemoryVerificationStatus::Expired
                | MemoryVerificationStatus::Invalidated
        )
}

fn record_origin_checkout(fields: &mut MemoryStructuredFields, checkout_id: &str) {
    if fields.provenance.iter().any(|entry| {
        entry.source == ORIGIN_CHECKOUT_PROVENANCE_SOURCE
            && entry.reference.as_deref() == Some(checkout_id)
    }) {
        return;
    }
    fields.provenance.push(MemoryProvenance {
        source: ORIGIN_CHECKOUT_PROVENANCE_SOURCE.to_string(),
        reference: Some(checkout_id.to_string()),
        captured_at: None,
        note: Some(
            "origin checkout identity recorded by the authority-bound memory router".to_string(),
        ),
    });
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
            Some(_) => ensure_role_identity(conn, role),
            None => {
                conn.execute(
                    &format!("INSERT INTO {STORE_ROLE_TABLE} (key, value) VALUES ('role', ?1)"),
                    params![role.kind()],
                )
                .map_err(|error| LatticeError::Storage(format!("failed to persist memory store role: {error}")))?;
                ensure_role_identity(conn, role)
            }
        }
    })
}

fn ensure_role_identity(
    conn: &rusqlite::Connection,
    role: &MemoryStoreRole,
) -> Result<(), LatticeError> {
    let MemoryStoreRole::Repository { repository_id } = role else {
        return Ok(());
    };
    let existing = conn
        .query_row(
            &format!("SELECT value FROM {STORE_ROLE_TABLE} WHERE key = ?1"),
            params![REPOSITORY_ID_METADATA_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| {
            LatticeError::Storage(format!(
                "failed to load repository memory-store authority: {error}"
            ))
        })?;
    match existing {
        Some(found) if found != *repository_id => Err(LatticeError::Storage(format!(
            "repository memory store authority mismatch: database is '{found}', requested '{repository_id}'"
        ))),
        Some(_) => Ok(()),
        None => {
            conn.execute(
                &format!("INSERT INTO {STORE_ROLE_TABLE} (key, value) VALUES (?1, ?2)"),
                params![REPOSITORY_ID_METADATA_KEY, repository_id],
            )
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "failed to persist repository memory-store authority: {error}"
                ))
            })?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::session_capture::{SessionCaptureRetentionPolicy, SessionCaptureSelector};
    use crate::memory::store::{
        MEMORY_FTS_STATE_TABLE, SESSION_CAPTURE_TOMBSTONES_TABLE,
        SESSION_CAPTURE_TOMBSTONE_PROVENANCE_TABLE, SESSION_DIGEST_CAPTURE_COMMITS_TABLE,
        SESSION_DIGEST_DELIVERIES_TABLE,
    };
    use crate::memory::{
        bind_session_digest_authority, parse_session_digest, MemoryClass, MemoryLinkRecord,
        MemoryType, SessionDigestAuthority,
    };
    use crate::DateTime;
    use std::time::Duration;

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
            id: String::new(),
            session_id: "session-1".to_string(),
            content: content.to_string(),
            memory_type: MemoryType::Observation,
            scope,
            confidence: 0.8,
            linked_symbols: vec![],
            linked_files: vec![],
            workspace_id: None,
            branch: None,
            scope_organization_id: organization.map(str::to_string),
            refresh_key: Some("claim".to_string()),
            source_query: None,
            created_at: 0,
            last_accessed: 0,
            access_count: 0,
            is_stale: false,
            stale_reason: None,
            verification_status: MemoryVerificationStatus::Verified,
        }
    }

    fn fields() -> MemoryStructuredFields {
        MemoryStructuredFields {
            memory_class: MemoryClass::Observation,
            ..Default::default()
        }
    }

    fn capture_authority(checkout: &str, branch: Option<&str>) -> MemoryQueryAuthority {
        MemoryQueryAuthority::new(
            "repo-capture",
            checkout,
            branch.map(str::to_string),
            "session-capture",
            Some("cadres".to_string()),
        )
        .unwrap()
    }

    fn capture_digest(checkout: &str, branch: Option<&str>) -> SessionDigest {
        let received_at = DateTime::from_unix_seconds(1_700_000_010);
        let content = parse_session_digest(
            r#"{
                "schema_version":1,
                "ended_at":"2023-11-14T22:13:20Z",
                "edited_paths":["src/capture.rs"],
                "final_summary":"Implemented atomic session capture.",
                "observations":[
                    {"kind":"check","label":"lattice-core tests","outcome":"passed"}
                ]
            }"#,
            received_at,
        )
        .unwrap();
        bind_session_digest_authority(
            content,
            &SessionDigestAuthority {
                session_id: "session-capture".to_string(),
                repository_id: "repo-capture".to_string(),
                checkout_id: Some(checkout.to_string()),
                branch: branch.map(str::to_string),
                revision: "0123456789abcdef".to_string(),
                segment: 7,
            },
        )
        .unwrap()
    }

    fn capture_authority_for_session(session_id: &str, checkout: &str) -> MemoryQueryAuthority {
        MemoryQueryAuthority::new(
            "repo-capture",
            checkout,
            Some("main".to_string()),
            session_id,
            None,
        )
        .unwrap()
    }

    fn capture_digest_at(
        session_id: &str,
        checkout: &str,
        segment: u64,
        received_at: i64,
    ) -> SessionDigest {
        let content = parse_session_digest(
            r#"{
                "schema_version":1,
                "ended_at":"2023-11-14T22:13:20Z",
                "edited_paths":["src/capture.rs"],
                "observations":[]
            }"#,
            DateTime::from_unix_seconds(received_at),
        )
        .unwrap();
        bind_session_digest_authority(
            content,
            &SessionDigestAuthority {
                session_id: session_id.to_string(),
                repository_id: "repo-capture".to_string(),
                checkout_id: Some(checkout.to_string()),
                branch: Some("main".to_string()),
                revision: format!("revision-{segment}"),
                segment,
            },
        )
        .unwrap()
    }

    fn table_count(store: &MemoryStore, table: &str) -> i64 {
        store
            .with_connection(|connection| {
                connection
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })
                    .map_err(|error| LatticeError::Storage(error.to_string()))
            })
            .unwrap()
    }

    #[test]
    fn organization_memory_is_shared_but_cross_repo_is_unverified_and_advisory() {
        let repo_a = MemoryStore::open_in_memory().unwrap();
        let repo_b = MemoryStore::open_in_memory().unwrap();
        let shared = MemoryStore::open_in_memory().unwrap();
        let router_a =
            MemoryStoreRouter::new(&repo_a, Some(&shared), authority("repo-a", Some("cadres")))
                .unwrap();
        let stored = router_a
            .remember(
                memory(
                    MemoryScope::Organization,
                    Some("cadres"),
                    "shared deployment rule",
                ),
                &fields(),
                Some("cadres"),
            )
            .unwrap();
        assert_eq!(
            stored.authority,
            MemoryAuthority::Organization("cadres".to_string())
        );
        assert!(repo_a.query_unscoped_admin(None, 10).unwrap().is_empty());

        let router_b =
            MemoryStoreRouter::new(&repo_b, Some(&shared), authority("repo-b", Some("cadres")))
                .unwrap();
        let result = router_b.recall(Some("deployment"), 10).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].cross_repo);
        assert_eq!(
            result[0].effective_verification_status,
            MemoryVerificationStatus::Unverified
        );
        assert_eq!(
            result[0].memory_id.authority,
            MemoryAuthority::Organization("cadres".to_string())
        );
        assert!(result[0].trust_reason.contains("advisory"));
    }

    #[test]
    fn organization_writes_are_unverified_and_record_origin_checkout() {
        let repository = MemoryStore::open_in_memory().unwrap();
        let shared = MemoryStore::open_in_memory().unwrap();
        let router = MemoryStoreRouter::new(
            &repository,
            Some(&shared),
            authority("repo-a", Some("cadres")),
        )
        .unwrap();
        let mut structured = fields();
        structured.verification_status = MemoryVerificationStatus::Verified;

        let stored = router
            .remember(
                memory(MemoryScope::Organization, Some("cadres"), "shared rule"),
                &structured,
                Some("cadres"),
            )
            .unwrap();

        let persisted = shared.get_by_id(&stored.local_id).unwrap().unwrap();
        assert_eq!(
            persisted.verification_status,
            MemoryVerificationStatus::Unverified
        );
        let persisted_fields = shared
            .get_structured_fields(&stored.local_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            persisted_fields.verification_status,
            MemoryVerificationStatus::Unverified
        );
        assert!(persisted_fields.provenance.iter().any(|entry| {
            entry.source == ORIGIN_CHECKOUT_PROVENANCE_SOURCE
                && entry.reference.as_deref() == Some("checkout-repo-a")
        }));
    }

    #[test]
    fn request_cannot_widen_organization_authority() {
        let repository = MemoryStore::open_in_memory().unwrap();
        let shared = MemoryStore::open_in_memory().unwrap();
        let router = MemoryStoreRouter::new(
            &repository,
            Some(&shared),
            authority("repo-a", Some("cadres")),
        )
        .unwrap();
        let error = router
            .remember(
                memory(MemoryScope::Organization, Some("other"), "private"),
                &fields(),
                Some("other"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("does not match"));
        assert!(shared.query_unscoped_admin(None, 10).unwrap().is_empty());
    }

    #[test]
    fn shared_store_role_cannot_be_reopened_as_repository_store() {
        let repository = MemoryStore::open_in_memory().unwrap();
        let shared = MemoryStore::open_in_memory().unwrap();
        MemoryStoreRouter::new(
            &repository,
            Some(&shared),
            authority("repo-a", Some("cadres")),
        )
        .unwrap();
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
    fn repository_store_cannot_be_reopened_for_a_different_repository() {
        let repository = MemoryStore::open_in_memory().unwrap();
        MemoryStoreRouter::new(&repository, None, authority("repo-a", None)).unwrap();

        let error = match MemoryStoreRouter::new(&repository, None, authority("repo-b", None)) {
            Ok(_) => panic!("repository store must retain its configured repository authority"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("authority mismatch"));
    }

    #[test]
    fn recall_never_queries_shared_store_without_configured_authority() {
        let repository = MemoryStore::open_in_memory().unwrap();
        let shared = MemoryStore::open_in_memory().unwrap();
        let router =
            MemoryStoreRouter::new(&repository, Some(&shared), authority("repo-a", None)).unwrap();
        shared
            .store(memory(
                MemoryScope::Organization,
                Some("cadres"),
                "must remain hidden",
            ))
            .unwrap();
        assert!(router.recall(Some("hidden"), 10).unwrap().is_empty());
    }

    #[test]
    fn shared_recall_excludes_malformed_non_organization_rows() {
        let repository = MemoryStore::open_in_memory().unwrap();
        let shared = MemoryStore::open_in_memory().unwrap();
        let router = MemoryStoreRouter::new(
            &repository,
            Some(&shared),
            authority("repo-a", Some("cadres")),
        )
        .unwrap();
        let mut malformed_session = memory(MemoryScope::Session, None, "shared deployment secret");
        malformed_session.workspace_id = Some("repo-a".to_string());
        shared.store(malformed_session).unwrap();
        let mut malformed_repo = memory(MemoryScope::Repo, None, "shared deployment secret");
        malformed_repo.workspace_id = Some("repo-a".to_string());
        shared.store(malformed_repo).unwrap();
        router
            .remember(
                memory(
                    MemoryScope::Organization,
                    Some("cadres"),
                    "shared deployment rule",
                ),
                &fields(),
                Some("cadres"),
            )
            .unwrap();

        let recalled = router.recall(Some("deployment"), 10).unwrap();
        assert_eq!(recalled.len(), 1);
        assert_eq!(recalled[0].source_tier, MemoryRecallTier::Organization);
        assert_eq!(recalled[0].memory.content, "shared deployment rule");
    }

    #[test]
    fn ineligible_shared_lifecycle_states_do_not_surface_in_normal_recall() {
        let repository = MemoryStore::open_in_memory().unwrap();
        let shared = MemoryStore::open_in_memory().unwrap();
        let router = MemoryStoreRouter::new(
            &repository,
            Some(&shared),
            authority("repo-a", Some("cadres")),
        )
        .unwrap();
        let stored = router
            .remember(
                memory(
                    MemoryScope::Organization,
                    Some("cadres"),
                    "obsolete deployment rule",
                ),
                &fields(),
                Some("cadres"),
            )
            .unwrap();
        shared
            .set_verification_state(
                &stored.local_id,
                MemoryVerificationStatus::Contradicted,
                false,
                None,
                0,
                None,
            )
            .unwrap();

        assert!(router.recall(Some("deployment"), 10).unwrap().is_empty());
    }

    #[test]
    fn session_digest_batch_is_atomic_deterministic_and_exactly_replayable() {
        let repository = MemoryStore::open_in_memory().unwrap();
        let shared = MemoryStore::open_in_memory().unwrap();
        let router = MemoryStoreRouter::new(
            &repository,
            Some(&shared),
            capture_authority("checkout-a", Some("main")),
        )
        .unwrap();
        let digest = capture_digest("checkout-a", Some("main"));
        let candidates = extract_default_session_digest_candidates(&digest);

        let first = router
            .capture_session_digest_candidate_batch(&digest, &candidates)
            .unwrap();
        let replay = router
            .capture_session_digest_candidate_batch(&digest, &candidates)
            .unwrap();

        assert!(!first.replayed);
        assert!(replay.replayed);
        assert_eq!(replay.delivery_key, first.delivery_key);
        assert_eq!(replay.memory_ids, first.memory_ids);
        assert_eq!(replay.candidate_count, first.candidate_count);
        assert_eq!(replay.committed_count, first.committed_count);
        assert_eq!(first.committed_count, candidates.len());
        assert_eq!(table_count(&repository, SESSION_DIGEST_DELIVERIES_TABLE), 1);
        assert_eq!(
            table_count(&repository, SESSION_DIGEST_CAPTURE_COMMITS_TABLE),
            candidates.len() as i64
        );
        assert_eq!(
            table_count(&repository, "memories"),
            candidates.len() as i64
        );
        assert_eq!(
            table_count(&repository, "memory_evidence"),
            candidates.len() as i64
        );
        assert_eq!(
            table_count(&repository, "memories_fts"),
            candidates.len() as i64
        );
        assert_eq!(table_count(&shared, "memories"), 0);
        assert_eq!(table_count(&shared, SESSION_DIGEST_DELIVERIES_TABLE), 0);

        repository
            .with_connection(|connection| {
                let unsafe_journal_fields: i64 = connection
                    .query_row(
                        &format!(
                            "SELECT COUNT(*) FROM pragma_table_info('{SESSION_DIGEST_DELIVERIES_TABLE}')
                             WHERE name IN ('claim', 'summary', 'evidence', 'edited_paths')"
                        ),
                        [],
                        |row| row.get(0),
                    )
                    .map_err(|error| LatticeError::Storage(error.to_string()))?;
                assert_eq!(unsafe_journal_fields, 0);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn changed_normalized_candidate_with_same_key_is_rejected() {
        let repository = MemoryStore::open_in_memory().unwrap();
        let router = MemoryStoreRouter::new(
            &repository,
            None,
            capture_authority("checkout-a", Some("main")),
        )
        .unwrap();
        let digest = capture_digest("checkout-a", Some("main"));
        let candidates = extract_default_session_digest_candidates(&digest);
        router
            .capture_session_digest_candidate_batch(&digest, &candidates)
            .unwrap();

        let mut changed = candidates.clone();
        changed[0].claim.push_str(" changed");
        let error = repository
            .persist_session_digest_candidate_batch(
                &digest,
                &changed,
                SESSION_DIGEST_EXTRACTOR_VERSION,
            )
            .unwrap_err();
        assert!(error.to_string().contains("different normalized content"));
        assert_eq!(table_count(&repository, SESSION_DIGEST_DELIVERIES_TABLE), 1);
        assert_eq!(
            table_count(&repository, "memories"),
            candidates.len() as i64
        );
    }

    #[test]
    fn automatic_capture_is_exact_checkout_and_branch_applicable() {
        let repository = MemoryStore::open_in_memory().unwrap();
        let digest = capture_digest("checkout-a", Some("main"));
        let candidates = extract_default_session_digest_candidates(&digest);
        MemoryStoreRouter::new(
            &repository,
            None,
            capture_authority("checkout-a", Some("main")),
        )
        .unwrap()
        .capture_session_digest_candidate_batch(&digest, &candidates)
        .unwrap();

        let mut legacy = memory(MemoryScope::Branch, None, "legacy branch observation");
        legacy.workspace_id = Some("repo-capture".to_string());
        legacy.branch = Some("main".to_string());
        repository.store(legacy).unwrap();

        let same_checkout = MemoryStoreRouter::new(
            &repository,
            None,
            capture_authority("checkout-a", Some("main")),
        )
        .unwrap()
        .recall(None, 20)
        .unwrap();
        assert_eq!(same_checkout.len(), candidates.len() + 1);

        let other_checkout = MemoryStoreRouter::new(
            &repository,
            None,
            capture_authority("checkout-b", Some("main")),
        )
        .unwrap()
        .recall(None, 20)
        .unwrap();
        assert_eq!(other_checkout.len(), 1);
        assert_eq!(
            other_checkout[0].memory.content,
            "legacy branch observation"
        );

        let other_branch = MemoryStoreRouter::new(
            &repository,
            None,
            capture_authority("checkout-a", Some("feature")),
        )
        .unwrap()
        .recall(None, 20)
        .unwrap();
        assert!(other_branch.is_empty());
    }

    #[test]
    fn injected_capture_failures_roll_back_every_persistence_component() {
        for failure_step in 1..=6 {
            let repository = MemoryStore::open_in_memory().unwrap();
            repository.set_capture_failure_after_step(Some(failure_step));
            let digest = capture_digest("checkout-a", Some("main"));
            let candidates = extract_default_session_digest_candidates(&digest);
            let router = MemoryStoreRouter::new(
                &repository,
                None,
                capture_authority("checkout-a", Some("main")),
            )
            .unwrap();

            let error = router
                .capture_session_digest_candidate_batch(&digest, &candidates)
                .unwrap_err();
            assert!(error.to_string().contains("injected"));
            for table in [
                "memories",
                "memory_evidence",
                "memories_fts",
                SESSION_DIGEST_DELIVERIES_TABLE,
                SESSION_DIGEST_CAPTURE_COMMITS_TABLE,
            ] {
                assert_eq!(
                    table_count(&repository, table),
                    0,
                    "failure step {failure_step} left rows in {table}"
                );
            }
            let fts_dirty = repository
                .with_connection(|connection| {
                    connection
                        .query_row(
                            &format!(
                                "SELECT is_dirty FROM {MEMORY_FTS_STATE_TABLE} WHERE singleton = 1"
                            ),
                            [],
                            |row| row.get::<_, i64>(0),
                        )
                        .map_err(|error| LatticeError::Storage(error.to_string()))
                })
                .unwrap();
            assert_eq!(fts_dirty, 0, "failure step {failure_step} left FTS dirty");
        }
    }

    #[test]
    fn capture_rejects_mismatched_checkout_without_touching_shared_store() {
        let repository = MemoryStore::open_in_memory().unwrap();
        let shared = MemoryStore::open_in_memory().unwrap();
        let router = MemoryStoreRouter::new(
            &repository,
            Some(&shared),
            capture_authority("checkout-a", Some("main")),
        )
        .unwrap();
        let digest = capture_digest("checkout-b", Some("main"));
        let candidates = extract_default_session_digest_candidates(&digest);

        let error = router
            .capture_session_digest_candidate_batch(&digest, &candidates)
            .unwrap_err();
        assert!(error.to_string().contains("authority"));
        assert_eq!(table_count(&repository, "memories"), 0);
        assert_eq!(table_count(&shared, "memories"), 0);
        assert_eq!(table_count(&shared, SESSION_DIGEST_DELIVERIES_TABLE), 0);
    }

    #[test]
    fn capture_retention_applies_repository_local_age_and_count_bounds() {
        let repository = MemoryStore::open_in_memory().unwrap();
        let mut capture_ids = Vec::new();
        for (segment, received_at) in [(1, 100), (2, 200), (3, 300)] {
            let digest = capture_digest_at("retention-session", "checkout-a", segment, received_at);
            let candidates = extract_default_session_digest_candidates(&digest);
            let result = MemoryStoreRouter::new(
                &repository,
                None,
                capture_authority_for_session("retention-session", "checkout-a"),
            )
            .unwrap()
            .capture_session_digest_candidate_batch(&digest, &candidates)
            .unwrap();
            capture_ids.push(result.delivery_key);
        }

        let router = MemoryStoreRouter::new(
            &repository,
            None,
            capture_authority_for_session("retention-session", "checkout-a"),
        )
        .unwrap();
        let result = router
            .prune_session_captures(
                SessionCaptureRetentionPolicy::new(Duration::from_secs(150), 1).unwrap(),
                DateTime::from_unix_seconds(350),
            )
            .unwrap();

        assert_eq!(result.deleted_capture_ids, capture_ids[..2]);
        assert_eq!(result.deleted_memory_count, 2);
        assert_eq!(table_count(&repository, SESSION_DIGEST_DELIVERIES_TABLE), 1);
        assert_eq!(table_count(&repository, "memories"), 1);
        assert_eq!(
            table_count(&repository, SESSION_CAPTURE_TOMBSTONES_TABLE),
            2
        );
        assert_eq!(
            router
                .prune_session_captures(
                    SessionCaptureRetentionPolicy::new(Duration::from_secs(150), 1).unwrap(),
                    DateTime::from_unix_seconds(350),
                )
                .unwrap(),
            SessionCaptureDeletionResult::default()
        );
    }

    #[test]
    fn operator_deletion_prunes_dependencies_and_retains_derived_records_as_tombstones() {
        let repository = MemoryStore::open_in_memory().unwrap();
        let authority = capture_authority_for_session("delete-session", "checkout-a");
        let digest = capture_digest_at("delete-session", "checkout-a", 1, 500);
        let candidates = extract_default_session_digest_candidates(&digest);
        let router = MemoryStoreRouter::new(&repository, None, authority).unwrap();
        let captured = router
            .capture_session_digest_candidate_batch(&digest, &candidates)
            .unwrap();
        let source_memory_id = captured.memory_ids[0].local_id.clone();

        let mut derived = memory(MemoryScope::Repo, None, "durable derived decision");
        derived.workspace_id = Some("repo-capture".to_string());
        derived.session_id = "later-session".to_string();
        let derived_id = repository.store(derived).unwrap();
        let mut derived_fields = fields();
        derived_fields.linked_memories = vec![source_memory_id.clone()];
        repository
            .update_structured_fields(&derived_id, &derived_fields)
            .unwrap();
        repository
            .insert_memory_link(&MemoryLinkRecord {
                link_id: "derived-capture-link".to_string(),
                source_memory_id: derived_id.clone(),
                target_memory_id: source_memory_id.clone(),
                link_type: "derived_from".to_string(),
                reason: "test provenance".to_string(),
                created_at: 501,
                verification_status: "unverified".to_string(),
            })
            .unwrap();
        repository
            .enqueue_verification_job("repo-capture", &source_memory_id)
            .unwrap();
        repository
            .with_connection(|connection| {
                connection
                    .execute(
                        "INSERT INTO consolidation_jobs
                            (job_id, workspace_id, kind, mode, status, enqueued_at)
                         VALUES ('capture-job', 'repo-capture', 'session_digest',
                                 'manual_review', 'proposed', 501)",
                        [],
                    )
                    .map_err(|error| LatticeError::Storage(error.to_string()))?;
                connection
                    .execute(
                        "INSERT INTO consolidation_proposals
                            (proposal_id, job_id, target_memory_id, proposal_kind,
                             prior_state, proposed_state, evidence, decision)
                         VALUES ('capture-proposal', 'capture-job', ?1, 'update_memory',
                                 ?2, '{}', ?3, 'pending')",
                        params![
                            source_memory_id,
                            serde_json::json!({
                                "memory": {"id": source_memory_id, "content": "deleted claim"}
                            })
                            .to_string(),
                            serde_json::json!({"source_memory_ids": [source_memory_id]})
                                .to_string(),
                        ],
                    )
                    .map_err(|error| LatticeError::Storage(error.to_string()))?;
                Ok(())
            })
            .unwrap();

        let selector = SessionCaptureSelector::session("repo-capture", "delete-session").unwrap();
        let result = router
            .delete_session_captures(&selector, DateTime::from_unix_seconds(600))
            .unwrap();
        assert_eq!(
            result.deleted_capture_ids,
            vec![captured.delivery_key.clone()]
        );
        assert_eq!(result.deleted_memory_count, 1);
        assert_eq!(result.retained_derived_memory_count, 1);
        assert_eq!(result.retained_proposal_count, 1);
        assert!(repository.get_by_id(&source_memory_id).unwrap().is_none());
        assert!(repository.get_by_id(&derived_id).unwrap().is_some());
        assert_eq!(table_count(&repository, "memory_links"), 0);
        assert_eq!(table_count(&repository, "verification_jobs"), 0);
        assert_eq!(table_count(&repository, "consolidation_jobs"), 1);
        assert_eq!(table_count(&repository, "consolidation_proposals"), 1);
        assert_eq!(
            table_count(&repository, SESSION_CAPTURE_TOMBSTONE_PROVENANCE_TABLE),
            2
        );

        repository
            .with_connection(|connection| {
                let provenance_json: String = connection
                    .query_row(
                        "SELECT provenance_json FROM memories WHERE id = ?1",
                        params![derived_id],
                        |row| row.get(0),
                    )
                    .map_err(|error| LatticeError::Storage(error.to_string()))?;
                let provenance: Vec<MemoryProvenance> =
                    serde_json::from_str(&provenance_json).unwrap();
                assert!(provenance.iter().any(|entry| {
                    entry.source == "lattice.deleted_session_capture.v1"
                        && entry.reference.as_deref() == Some(captured.delivery_key.as_str())
                        && entry.captured_at == Some(600)
                }));
                let (target, prior): (Option<String>, String) = connection
                    .query_row(
                        "SELECT target_memory_id, prior_state
                         FROM consolidation_proposals WHERE proposal_id = 'capture-proposal'",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(|error| LatticeError::Storage(error.to_string()))?;
                assert!(target.is_none());
                assert!(!prior.contains("deleted claim"));
                assert!(prior.contains("deleted_session_capture"));
                Ok(())
            })
            .unwrap();

        assert_eq!(
            router
                .delete_session_captures(&selector, DateTime::from_unix_seconds(700))
                .unwrap(),
            SessionCaptureDeletionResult::default()
        );
        assert_eq!(
            table_count(&repository, SESSION_CAPTURE_TOMBSTONES_TABLE),
            1
        );

        let replay_error = router
            .capture_session_digest_candidate_batch(&digest, &candidates)
            .unwrap_err();
        assert!(replay_error.to_string().contains("cannot be replayed"));
        assert_eq!(table_count(&repository, SESSION_DIGEST_DELIVERIES_TABLE), 0);
        assert!(repository.get_by_id(&source_memory_id).unwrap().is_none());
    }

    #[test]
    fn operator_deletion_refuses_cross_repository_selector() {
        let repository = MemoryStore::open_in_memory().unwrap();
        let authority = capture_authority_for_session("delete-session", "checkout-a");
        let digest = capture_digest_at("delete-session", "checkout-a", 1, 500);
        let candidates = extract_default_session_digest_candidates(&digest);
        let router = MemoryStoreRouter::new(&repository, None, authority).unwrap();
        router
            .capture_session_digest_candidate_batch(&digest, &candidates)
            .unwrap();

        let selector = SessionCaptureSelector::session("repo-other", "delete-session").unwrap();
        let error = router
            .delete_session_captures(&selector, DateTime::from_unix_seconds(600))
            .unwrap_err();
        assert!(error.to_string().contains("repository authority"));
        assert_eq!(table_count(&repository, SESSION_DIGEST_DELIVERIES_TABLE), 1);
        assert_eq!(table_count(&repository, "memories"), 1);
    }
}
