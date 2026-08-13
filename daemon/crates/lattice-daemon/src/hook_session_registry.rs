//! Durable, capture-content-free state for trusted hook-session delivery.
//!
//! The registry is deliberately not a capture store. It persists opaque
//! host-tuple fingerprints, daemon-resolved binding identity and Git-start
//! metadata, delivery hashes, ordering metadata, receipts, and an outbox
//! marker. It has no field capable of holding an event envelope, transcript,
//! command, summary, edited path, or other capture payload. Sanitized facts
//! belong in the repository store that owns them.

use std::fmt;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use thiserror::Error;

use crate::hook_session_binding::{
    HookBindingId, HookCapabilityVerifier, HookCheckoutIdentity, HookIntegrationId,
    HookInternalSessionId, HookRepositoryState, HookSessionCapability, HookSessionCryptography,
    HostSessionId,
};

pub const HOOK_REGISTRY_SCHEMA_VERSION: u32 = 2;
const APPLICATION_ID: i64 = 0x4c_48_53_52; // "LHSR"
const MAX_OPAQUE_ID_BYTES: usize = 4096;
const SHA256_BYTES: usize = 32;

/// Finite limits enforced independently of adapter behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HookRegistryConfig {
    pub reorder_window: u64,
    pub max_receipts_per_binding: usize,
    pub max_pending_per_binding: usize,
}

impl Default for HookRegistryConfig {
    fn default() -> Self {
        Self {
            reorder_window: 32,
            max_receipts_per_binding: 16_384,
            max_pending_per_binding: 256,
        }
    }
}

impl HookRegistryConfig {
    fn validate(self) -> Result<Self, HookRegistryError> {
        if self.reorder_window == 0
            || self.reorder_window > 256
            || self.max_receipts_per_binding == 0
            || self.max_receipts_per_binding > 65_536
            || self.max_pending_per_binding == 0
            || self.max_pending_per_binding > 4_096
        {
            return Err(HookRegistryError::InvalidConfiguration);
        }
        Ok(self)
    }
}

/// A bounded opaque identifier. Its value is never rendered by `Debug`.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RegistryId(Vec<u8>);

impl RegistryId {
    pub fn from_bytes(value: impl Into<Vec<u8>>) -> Result<Self, HookRegistryError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_OPAQUE_ID_BYTES {
            return Err(HookRegistryError::InvalidIdentifier);
        }
        Ok(Self(value))
    }

    pub fn from_string(value: impl Into<String>) -> Result<Self, HookRegistryError> {
        Self::from_bytes(value.into().into_bytes())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for RegistryId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RegistryId(<redacted>)")
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct RegistryHash([u8; SHA256_BYTES]);

impl RegistryHash {
    pub fn from_bytes(value: [u8; SHA256_BYTES]) -> Self {
        Self(value)
    }

    pub fn as_bytes(&self) -> &[u8; SHA256_BYTES] {
        &self.0
    }
}

impl fmt::Debug for RegistryHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RegistryHash(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryBindingState {
    Open,
    Sealed,
    Expired,
    Revoked,
}

impl RegistryBindingState {
    fn parse(value: &str) -> rusqlite::Result<Self> {
        match value {
            "open" => Ok(Self::Open),
            "sealed" => Ok(Self::Sealed),
            "expired" => Ok(Self::Expired),
            "revoked" => Ok(Self::Revoked),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryDeliveryKind {
    Event,
    Close,
}

impl RegistryDeliveryKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Event => "event",
            Self::Close => "close",
        }
    }

    fn parse(value: &str) -> rusqlite::Result<Self> {
        match value {
            "event" => Ok(Self::Event),
            "close" => Ok(Self::Close),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryReceiptStatus {
    Pending,
    Reduced,
    Sealed,
}

impl RegistryReceiptStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Reduced => "reduced",
            Self::Sealed => "sealed",
        }
    }

    fn parse(value: &str) -> rusqlite::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "reduced" => Ok(Self::Reduced),
            "sealed" => Ok(Self::Sealed),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

#[derive(Debug)]
pub struct RegistrySessionResume {
    pub binding_id: HookBindingId,
    pub capability: HookSessionCapability,
}

/// Complete input to the registry's atomic session service. Identity and Git
/// state must already have been independently resolved by the daemon. The host
/// session value is used only by keyed fingerprinting and is never persisted.
#[derive(Debug)]
pub struct RegistryOpenRequest {
    pub integration: HookIntegrationId,
    pub host_session_id: HostSessionId,
    pub checkout: HookCheckoutIdentity,
    pub repository_state: HookRepositoryState,
    pub resume: Option<RegistrySessionResume>,
    pub now_ms: i64,
    pub idle_ttl_ms: i64,
    pub absolute_ttl_ms: i64,
    pub retention_ms: i64,
}

#[derive(Clone)]
pub struct RegistryOpenOutcome {
    pub binding_id: HookBindingId,
    pub capability: HookSessionCapability,
    pub internal_session_id: HookInternalSessionId,
    pub generation: u64,
    pub resumed: bool,
    pub idle_deadline_ms: i64,
    pub absolute_deadline_ms: i64,
}

#[derive(Debug)]
pub struct RegistryVerifyRequest {
    pub binding_id: HookBindingId,
    pub capability: HookSessionCapability,
    pub integration: HookIntegrationId,
    pub current_checkout: HookCheckoutIdentity,
    pub now_ms: i64,
    pub idle_ttl_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryVerification {
    pub binding_id: HookBindingId,
    pub internal_session_id: HookInternalSessionId,
    pub integration: HookIntegrationId,
    pub checkout: HookCheckoutIdentity,
    pub repository_state: HookRepositoryState,
    pub generation: u64,
    pub idle_deadline_ms: i64,
    pub absolute_deadline_ms: i64,
}

#[cfg(test)]
struct TestRegistryBinding {
    binding_id: RegistryId,
    authority_fingerprint: RegistryHash,
    capability_verifier: RegistryHash,
    generation: u64,
    created_at_ms: i64,
    idle_deadline_ms: i64,
    absolute_deadline_ms: i64,
    prune_after_ms: i64,
}

impl fmt::Debug for RegistryOpenOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistryOpenOutcome")
            .field("binding_id", &"<redacted>")
            .field("capability", &"<redacted>")
            .field("internal_session_id", &"<redacted>")
            .field("generation", &self.generation)
            .field("resumed", &self.resumed)
            .field("idle_deadline_ms", &self.idle_deadline_ms)
            .field("absolute_deadline_ms", &self.absolute_deadline_ms)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryBinding {
    pub schema_version: u32,
    pub row_version: u64,
    pub binding_id: RegistryId,
    pub authority_fingerprint: RegistryHash,
    pub capability_verifier: RegistryHash,
    pub internal_session_id: RegistryId,
    pub integration_id: RegistryId,
    pub repository_id: RegistryId,
    pub checkout_id: RegistryId,
    pub start_branch: Option<String>,
    pub start_revision: String,
    pub generation: u64,
    pub state: RegistryBindingState,
    pub created_at_ms: i64,
    pub last_seen_at_ms: i64,
    pub idle_deadline_ms: i64,
    pub absolute_deadline_ms: i64,
    pub prune_after_ms: i64,
    pub next_sequence: u64,
    pub closing_sequence: Option<u64>,
}

/// Content-free delivery metadata admitted to both the receipt table and the
/// durable outbox in one transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryAdmission {
    pub binding_id: RegistryId,
    pub delivery_id: RegistryId,
    pub sequence: u64,
    pub kind: RegistryDeliveryKind,
    pub event_schema_version: u32,
    pub normalized_hash: RegistryHash,
    pub admitted_at_ms: i64,
    pub idle_deadline_ms: i64,
    pub receipt_prune_after_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryReceipt {
    pub schema_version: u32,
    pub row_version: u64,
    pub binding_id: RegistryId,
    pub delivery_id: RegistryId,
    pub sequence: u64,
    pub kind: RegistryDeliveryKind,
    pub event_schema_version: u32,
    pub normalized_hash: RegistryHash,
    pub status: RegistryReceiptStatus,
    pub admitted_at_ms: i64,
    pub completed_at_ms: Option<i64>,
    pub prune_after_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryPendingDelivery {
    pub schema_version: u32,
    pub row_version: u64,
    pub binding_id: RegistryId,
    pub delivery_id: RegistryId,
    pub sequence: u64,
    pub kind: RegistryDeliveryKind,
    pub event_schema_version: u32,
    pub normalized_hash: RegistryHash,
    pub admitted_at_ms: i64,
    pub attempt_count: u64,
    pub available_after_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryAdmissionOutcome {
    pub receipt: RegistryReceipt,
    pub idempotent_replay: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryCompletion {
    pub binding_id: RegistryId,
    pub delivery_id: RegistryId,
    pub normalized_hash: RegistryHash,
    pub status: RegistryReceiptStatus,
    pub completed_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RegistryPruneResult {
    pub expired_bindings: u64,
    pub pruned_bindings: u64,
    pub pruned_receipts: u64,
}

#[derive(Debug, Error)]
pub enum HookRegistryError {
    #[error("hook-session registry configuration is invalid")]
    InvalidConfiguration,
    #[error("hook-session registry identifier is invalid")]
    InvalidIdentifier,
    #[error("hook-session registry timestamp or counter is invalid")]
    InvalidValue,
    #[error("hook-session registry schema version {found} is unsupported (expected {expected})")]
    UnsupportedSchema { found: u32, expected: u32 },
    #[error("hook-session binding was not found")]
    BindingNotFound,
    #[error("hook-session binding already exists with different authority")]
    BindingConflict,
    #[error("hook-session binding is already open and requires its capability to resume")]
    BindingAlreadyOpen,
    #[error("hook-session capability is invalid")]
    InvalidCapability,
    #[error("hook-session resume authority does not match the persisted binding tuple")]
    AuthorityMismatch,
    #[error("hook-session binding has expired")]
    Expired,
    #[error("hook-session binding is sealed")]
    Sealed,
    #[error("hook-session binding is revoked")]
    Revoked,
    #[error("hook-session delivery replay changed normalized metadata")]
    ReplayViolation,
    #[error("hook-session delivery order conflicts with persisted state")]
    OrderViolation,
    #[error("hook-session delivery is outside the allowed reorder window")]
    OrderWindowExceeded,
    #[error("hook-session receipt capacity is exhausted")]
    ReceiptCapacityExhausted,
    #[error("hook-session pending-delivery capacity is exhausted")]
    PendingCapacityExhausted,
    #[error("hook-session delivery completion is invalid")]
    InvalidCompletion,
    #[error("hook-session registry storage failed: {0}")]
    Storage(#[from] rusqlite::Error),
}

/// SQLite-backed durable registry. Mutating methods take `&mut self` so a
/// process has a single explicit transaction owner; SQLite remains the
/// inter-process correctness boundary.
pub struct HookSessionRegistry {
    connection: Connection,
    config: HookRegistryConfig,
}

impl fmt::Debug for HookSessionRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HookSessionRegistry")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl HookSessionRegistry {
    pub fn open(path: &Path, config: HookRegistryConfig) -> Result<Self, HookRegistryError> {
        let config = config.validate()?;
        let connection = Connection::open(path)?;
        configure_connection(&connection)?;
        initialize_schema(&connection)?;
        Ok(Self { connection, config })
    }

    pub fn open_in_memory(config: HookRegistryConfig) -> Result<Self, HookRegistryError> {
        let config = config.validate()?;
        let connection = Connection::open_in_memory()?;
        configure_connection(&connection)?;
        initialize_schema(&connection)?;
        Ok(Self { connection, config })
    }

    pub fn schema_version(&self) -> u32 {
        HOOK_REGISTRY_SCHEMA_VERSION
    }

    #[cfg(test)]
    fn insert_test_binding(
        &mut self,
        binding: TestRegistryBinding,
    ) -> Result<bool, HookRegistryError> {
        let changed = self.connection.execute(
            "INSERT OR IGNORE INTO hook_bindings
             (schema_version, row_version, binding_id, authority_fingerprint,
              capability_verifier, internal_session_id, integration_id,
              repository_id, checkout_id, start_branch, start_revision,
              generation, state, created_at_ms, last_seen_at_ms,
              idle_deadline_ms, absolute_deadline_ms, prune_after_ms,
              next_sequence)
             VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'main', 'abc123',
                     ?9, 'open', ?10, ?10, ?11, ?12, ?13, 1)",
            params![
                i64::from(HOOK_REGISTRY_SCHEMA_VERSION),
                binding.binding_id.as_bytes(),
                binding.authority_fingerprint.as_bytes(),
                binding.capability_verifier.as_bytes(),
                [3_u8; 16],
                b"test-adapter",
                b"test-repository",
                b"test-checkout",
                to_i64(binding.generation)?,
                binding.created_at_ms,
                binding.idle_deadline_ms,
                binding.absolute_deadline_ms,
                binding.prune_after_ms,
            ],
        )?;
        Ok(changed == 1)
    }

    /// Atomically opens a new durable binding or verifies and resumes the one
    /// open binding for the exact integration/host-session/repository/checkout
    /// tuple.
    ///
    /// This is the sole binding-creation seam: callers cannot supply verifier
    /// hashes, tuple fingerprints, internal session IDs, or generations.
    pub fn open_or_resume(
        &mut self,
        cryptography: &HookSessionCryptography,
        request: RegistryOpenRequest,
    ) -> Result<RegistryOpenOutcome, HookRegistryError> {
        validate_open_request(&request)?;
        let authority_fingerprint = cryptography.authority_fingerprint(
            &request.integration,
            &request.host_session_id,
            &request.checkout,
        );
        let authority_hash = RegistryHash::from_bytes(*authority_fingerprint.as_bytes());
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        expire_open_tuple_if_needed(&transaction, &authority_hash, request.now_ms)?;
        if let Some(existing) = load_open_binding_by_authority(&transaction, &authority_hash)? {
            let resume = request
                .resume
                .as_ref()
                .ok_or(HookRegistryError::BindingAlreadyOpen)?;
            if existing.binding_id.as_bytes() != resume.binding_id.as_bytes() {
                return Err(HookRegistryError::AuthorityMismatch);
            }
            verify_persisted_identity(&existing, &request)?;
            let expected =
                HookCapabilityVerifier::from_bytes(*existing.capability_verifier.as_bytes());
            if !cryptography.verify_capability(&resume.binding_id, &resume.capability, &expected) {
                return Err(HookRegistryError::InvalidCapability);
            }
            let requested_idle = request
                .now_ms
                .checked_add(request.idle_ttl_ms)
                .ok_or(HookRegistryError::InvalidValue)?;
            let idle_deadline_ms = requested_idle.min(existing.absolute_deadline_ms);
            transaction.execute(
                "UPDATE hook_bindings
                 SET row_version = row_version + 1, last_seen_at_ms = ?2,
                     idle_deadline_ms = ?3
                 WHERE binding_id = ?1 AND state = 'open'",
                params![
                    existing.binding_id.as_bytes(),
                    request.now_ms,
                    idle_deadline_ms
                ],
            )?;
            transaction.commit()?;
            return Ok(RegistryOpenOutcome {
                binding_id: resume.binding_id,
                capability: resume.capability,
                internal_session_id: hook_internal_session_id(&existing.internal_session_id)?,
                generation: existing.generation,
                resumed: true,
                idle_deadline_ms,
                absolute_deadline_ms: existing.absolute_deadline_ms,
            });
        }

        if let Some(resume) = &request.resume {
            let binding_id = registry_id(resume.binding_id.as_bytes())?;
            let existing = load_binding_from(&transaction, &binding_id)?
                .ok_or(HookRegistryError::BindingNotFound)?;
            let error = match existing.state {
                RegistryBindingState::Open => HookRegistryError::AuthorityMismatch,
                RegistryBindingState::Sealed => HookRegistryError::Sealed,
                RegistryBindingState::Expired => HookRegistryError::Expired,
                RegistryBindingState::Revoked => HookRegistryError::Revoked,
            };
            if existing.state == RegistryBindingState::Expired {
                transaction.commit()?;
            }
            return Err(error);
        }

        let prepared = cryptography
            .prepare_binding(
                &request.integration,
                &request.host_session_id,
                &request.checkout,
            )
            .map_err(|_| HookRegistryError::InvalidValue)?;
        let generation = next_generation(&transaction, &authority_hash)?;
        let idle_deadline_ms = request
            .now_ms
            .checked_add(request.idle_ttl_ms)
            .ok_or(HookRegistryError::InvalidValue)?;
        let absolute_deadline_ms = request
            .now_ms
            .checked_add(request.absolute_ttl_ms)
            .ok_or(HookRegistryError::InvalidValue)?;
        let prune_after_ms = absolute_deadline_ms
            .checked_add(request.retention_ms)
            .ok_or(HookRegistryError::InvalidValue)?;
        transaction.execute(
            "INSERT INTO hook_bindings
             (schema_version, row_version, binding_id, authority_fingerprint,
              capability_verifier, internal_session_id, integration_id,
              repository_id, checkout_id, start_branch, start_revision,
              generation, state, created_at_ms,
              last_seen_at_ms, idle_deadline_ms, absolute_deadline_ms,
              prune_after_ms, next_sequence)
             VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                     ?11, 'open', ?12, ?12, ?13, ?14, ?15, 1)",
            params![
                i64::from(HOOK_REGISTRY_SCHEMA_VERSION),
                prepared.binding_id.as_bytes(),
                prepared.authority_fingerprint.as_bytes(),
                prepared.capability_verifier.as_bytes(),
                prepared.internal_session_id.as_bytes(),
                request.integration.as_str().as_bytes(),
                request.checkout.repository_id().as_bytes(),
                request.checkout.checkout_id().as_bytes(),
                request.repository_state.branch(),
                request.repository_state.revision(),
                to_i64(generation)?,
                request.now_ms,
                idle_deadline_ms,
                absolute_deadline_ms,
                prune_after_ms,
            ],
        )?;
        transaction.commit()?;
        Ok(RegistryOpenOutcome {
            binding_id: prepared.binding_id,
            capability: prepared.capability,
            internal_session_id: prepared.internal_session_id,
            generation,
            resumed: false,
            idle_deadline_ms,
            absolute_deadline_ms,
        })
    }

    /// Verifies durable session authority and renews its idle lease in the same
    /// transaction. Callers use this only after an activity has passed its
    /// content-independent admission checks; malformed traffic must not renew
    /// a binding. The result contains only daemon-owned persisted authority.
    pub fn verify_and_renew(
        &mut self,
        cryptography: &HookSessionCryptography,
        request: RegistryVerifyRequest,
    ) -> Result<RegistryVerification, HookRegistryError> {
        if request.now_ms < 0 || request.idle_ttl_ms <= 0 {
            return Err(HookRegistryError::InvalidValue);
        }
        let binding_id = registry_id(request.binding_id.as_bytes())?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut binding = load_binding_from(&transaction, &binding_id)?
            .ok_or(HookRegistryError::BindingNotFound)?;
        let expected = HookCapabilityVerifier::from_bytes(*binding.capability_verifier.as_bytes());
        if !cryptography.verify_capability(&request.binding_id, &request.capability, &expected) {
            return Err(HookRegistryError::InvalidCapability);
        }
        if binding.integration_id.as_bytes() != request.integration.as_str().as_bytes()
            || binding.repository_id.as_bytes()
                != request.current_checkout.repository_id().as_bytes()
            || binding.checkout_id.as_bytes() != request.current_checkout.checkout_id().as_bytes()
        {
            return Err(HookRegistryError::AuthorityMismatch);
        }
        expire_binding_if_needed(&transaction, &mut binding, request.now_ms)?;
        let state_error = match binding.state {
            RegistryBindingState::Open => None,
            RegistryBindingState::Sealed => Some(HookRegistryError::Sealed),
            RegistryBindingState::Expired => Some(HookRegistryError::Expired),
            RegistryBindingState::Revoked => Some(HookRegistryError::Revoked),
        };
        if let Some(error) = state_error {
            transaction.commit()?;
            return Err(error);
        }
        let idle_deadline_ms = request
            .now_ms
            .checked_add(request.idle_ttl_ms)
            .ok_or(HookRegistryError::InvalidValue)?
            .min(binding.absolute_deadline_ms);
        transaction.execute(
            "UPDATE hook_bindings
             SET row_version = row_version + 1, last_seen_at_ms = ?2,
                 idle_deadline_ms = ?3
             WHERE binding_id = ?1 AND state = 'open'",
            params![
                binding.binding_id.as_bytes(),
                request.now_ms,
                idle_deadline_ms
            ],
        )?;
        transaction.commit()?;
        Ok(RegistryVerification {
            binding_id: request.binding_id,
            internal_session_id: hook_internal_session_id(&binding.internal_session_id)?,
            integration: request.integration,
            checkout: request.current_checkout,
            repository_state: HookRepositoryState::new(
                binding.start_branch,
                binding.start_revision,
            )
            .map_err(|_| HookRegistryError::InvalidIdentifier)?,
            generation: binding.generation,
            idle_deadline_ms,
            absolute_deadline_ms: binding.absolute_deadline_ms,
        })
    }

    pub fn binding(
        &self,
        binding_id: &RegistryId,
    ) -> Result<Option<RegistryBinding>, HookRegistryError> {
        load_binding_from(&self.connection, binding_id).map_err(Into::into)
    }

    pub fn receipt(
        &self,
        binding_id: &RegistryId,
        delivery_id: &RegistryId,
    ) -> Result<Option<RegistryReceipt>, HookRegistryError> {
        load_receipt_from(&self.connection, binding_id, delivery_id).map_err(Into::into)
    }

    /// Atomically inserts the idempotency receipt, a payload-free outbox row,
    /// and the renewed binding deadline.
    pub fn admit(
        &mut self,
        admission: RegistryAdmission,
    ) -> Result<RegistryAdmissionOutcome, HookRegistryError> {
        validate_admission(&admission)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut binding = load_binding_from(&transaction, &admission.binding_id)?
            .ok_or(HookRegistryError::BindingNotFound)?;

        if let Some(receipt) =
            load_receipt_from(&transaction, &admission.binding_id, &admission.delivery_id)?
        {
            let matches = receipt.sequence == admission.sequence
                && receipt.kind == admission.kind
                && receipt.event_schema_version == admission.event_schema_version
                && receipt.normalized_hash == admission.normalized_hash;
            if matches {
                transaction.commit()?;
                return Ok(RegistryAdmissionOutcome {
                    receipt,
                    idempotent_replay: true,
                });
            }
            revoke_in(&transaction, &admission.binding_id)?;
            transaction.commit()?;
            return Err(HookRegistryError::ReplayViolation);
        }

        expire_binding_if_needed(&transaction, &mut binding, admission.admitted_at_ms)?;
        let state_error = match binding.state {
            RegistryBindingState::Open => None,
            RegistryBindingState::Sealed => Some(HookRegistryError::Sealed),
            RegistryBindingState::Expired => Some(HookRegistryError::Expired),
            RegistryBindingState::Revoked => Some(HookRegistryError::Revoked),
        };
        if let Some(error) = state_error {
            // In particular, persist expiry discovered during admission rather
            // than rolling it back with the rejected delivery.
            transaction.commit()?;
            return Err(error);
        }
        if admission.sequence < binding.next_sequence {
            return Err(HookRegistryError::OrderViolation);
        }
        if admission.sequence.saturating_sub(binding.next_sequence) > self.config.reorder_window {
            return Err(HookRegistryError::OrderWindowExceeded);
        }
        if binding
            .closing_sequence
            .is_some_and(|closing| admission.sequence >= closing)
        {
            return Err(HookRegistryError::Sealed);
        }
        if admission.kind == RegistryDeliveryKind::Close {
            let later_exists: bool = transaction.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM hook_receipts
                    WHERE binding_id = ?1 AND sequence_number > ?2
                 )",
                params![admission.binding_id.as_bytes(), to_i64(admission.sequence)?],
                |row| row.get(0),
            )?;
            if later_exists {
                return Err(HookRegistryError::OrderViolation);
            }
        }

        let receipt_count = count_for(&transaction, "hook_receipts", &admission.binding_id)?;
        if receipt_count >= self.config.max_receipts_per_binding
            && admission.kind != RegistryDeliveryKind::Close
        {
            return Err(HookRegistryError::ReceiptCapacityExhausted);
        }
        let pending_count = count_for(&transaction, "hook_outbox", &admission.binding_id)?;
        if pending_count >= self.config.max_pending_per_binding
            && admission.kind != RegistryDeliveryKind::Close
        {
            return Err(HookRegistryError::PendingCapacityExhausted);
        }

        let sequence_conflict: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM hook_receipts
                WHERE binding_id = ?1 AND sequence_number = ?2
             )",
            params![admission.binding_id.as_bytes(), to_i64(admission.sequence)?],
            |row| row.get(0),
        )?;
        if sequence_conflict {
            revoke_in(&transaction, &admission.binding_id)?;
            transaction.commit()?;
            return Err(HookRegistryError::OrderViolation);
        }

        transaction.execute(
            "INSERT INTO hook_receipts
             (schema_version, row_version, binding_id, delivery_id,
              sequence_number, delivery_kind, event_schema_version,
              normalized_hash, receipt_status, admitted_at_ms, prune_after_ms)
             VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, ?9)",
            params![
                i64::from(HOOK_REGISTRY_SCHEMA_VERSION),
                admission.binding_id.as_bytes(),
                admission.delivery_id.as_bytes(),
                to_i64(admission.sequence)?,
                admission.kind.as_str(),
                i64::from(admission.event_schema_version),
                admission.normalized_hash.as_bytes(),
                admission.admitted_at_ms,
                admission.receipt_prune_after_ms,
            ],
        )?;
        transaction.execute(
            "INSERT INTO hook_outbox
             (schema_version, row_version, binding_id, delivery_id,
              sequence_number, delivery_kind, event_schema_version,
              normalized_hash, admitted_at_ms, attempt_count, available_after_ms)
             VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?8)",
            params![
                i64::from(HOOK_REGISTRY_SCHEMA_VERSION),
                admission.binding_id.as_bytes(),
                admission.delivery_id.as_bytes(),
                to_i64(admission.sequence)?,
                admission.kind.as_str(),
                i64::from(admission.event_schema_version),
                admission.normalized_hash.as_bytes(),
                admission.admitted_at_ms,
            ],
        )?;
        let renewed_idle = admission.idle_deadline_ms.min(binding.absolute_deadline_ms);
        transaction.execute(
            "UPDATE hook_bindings
             SET row_version = row_version + 1, last_seen_at_ms = ?2,
                 idle_deadline_ms = ?3,
                 closing_sequence = CASE WHEN ?4 = 'close' THEN ?5 ELSE closing_sequence END
             WHERE binding_id = ?1",
            params![
                admission.binding_id.as_bytes(),
                admission.admitted_at_ms,
                renewed_idle,
                admission.kind.as_str(),
                to_i64(admission.sequence)?,
            ],
        )?;
        let receipt =
            load_receipt_from(&transaction, &admission.binding_id, &admission.delivery_id)?
                .expect("inserted receipt is readable");
        transaction.commit()?;
        Ok(RegistryAdmissionOutcome {
            receipt,
            idempotent_replay: false,
        })
    }

    /// Returns available work in deterministic binding/sequence order. Calling
    /// this does not claim the rows; use `defer_pending` before handing work to
    /// another concurrent owner.
    pub fn pending(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<RegistryPendingDelivery>, HookRegistryError> {
        if limit == 0 || limit > 4_096 {
            return Err(HookRegistryError::InvalidValue);
        }
        let mut statement = self.connection.prepare(
            "SELECT schema_version, row_version, binding_id, delivery_id,
                    sequence_number, delivery_kind, event_schema_version,
                    normalized_hash, admitted_at_ms, attempt_count,
                    available_after_ms
             FROM hook_outbox
             WHERE available_after_ms <= ?1
             ORDER BY admitted_at_ms ASC, binding_id ASC, sequence_number ASC
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![now_ms, to_i64(limit as u64)?], pending_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Records a bounded retry/lease delay without changing receipt identity.
    pub fn defer_pending(
        &mut self,
        binding_id: &RegistryId,
        delivery_id: &RegistryId,
        expected_row_version: u64,
        available_after_ms: i64,
    ) -> Result<bool, HookRegistryError> {
        let changed = self.connection.execute(
            "UPDATE hook_outbox
             SET row_version = row_version + 1, attempt_count = attempt_count + 1,
                 available_after_ms = ?4
             WHERE binding_id = ?1 AND delivery_id = ?2 AND row_version = ?3",
            params![
                binding_id.as_bytes(),
                delivery_id.as_bytes(),
                to_i64(expected_row_version)?,
                available_after_ms,
            ],
        )?;
        Ok(changed == 1)
    }

    /// Atomically marks a receipt terminal, removes its outbox marker, and
    /// advances (or seals) the binding. Exact retries return the stored result.
    pub fn complete(
        &mut self,
        completion: RegistryCompletion,
    ) -> Result<RegistryReceipt, HookRegistryError> {
        if completion.status == RegistryReceiptStatus::Pending {
            return Err(HookRegistryError::InvalidCompletion);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let binding = load_binding_from(&transaction, &completion.binding_id)?
            .ok_or(HookRegistryError::BindingNotFound)?;
        let receipt = load_receipt_from(
            &transaction,
            &completion.binding_id,
            &completion.delivery_id,
        )?
        .ok_or(HookRegistryError::InvalidCompletion)?;
        if completion.completed_at_ms < receipt.admitted_at_ms {
            return Err(HookRegistryError::InvalidCompletion);
        }
        if receipt.normalized_hash != completion.normalized_hash {
            revoke_in(&transaction, &completion.binding_id)?;
            transaction.commit()?;
            return Err(HookRegistryError::ReplayViolation);
        }
        if receipt.status != RegistryReceiptStatus::Pending {
            if receipt.status == completion.status {
                transaction.commit()?;
                return Ok(receipt);
            }
            return Err(HookRegistryError::InvalidCompletion);
        }
        if receipt.sequence != binding.next_sequence {
            return Err(HookRegistryError::OrderViolation);
        }
        let expected_status = match receipt.kind {
            RegistryDeliveryKind::Event => RegistryReceiptStatus::Reduced,
            RegistryDeliveryKind::Close => RegistryReceiptStatus::Sealed,
        };
        if completion.status != expected_status {
            return Err(HookRegistryError::InvalidCompletion);
        }

        transaction.execute(
            "UPDATE hook_receipts
             SET row_version = row_version + 1, receipt_status = ?3,
                 completed_at_ms = ?4
             WHERE binding_id = ?1 AND delivery_id = ?2 AND receipt_status = 'pending'",
            params![
                completion.binding_id.as_bytes(),
                completion.delivery_id.as_bytes(),
                completion.status.as_str(),
                completion.completed_at_ms,
            ],
        )?;
        transaction.execute(
            "DELETE FROM hook_outbox WHERE binding_id = ?1 AND delivery_id = ?2",
            params![
                completion.binding_id.as_bytes(),
                completion.delivery_id.as_bytes()
            ],
        )?;
        transaction.execute(
            "UPDATE hook_bindings
             SET row_version = row_version + 1, next_sequence = next_sequence + 1,
                 state = CASE WHEN ?2 = 'sealed' THEN 'sealed' ELSE state END,
                 sealed_at_ms = CASE WHEN ?2 = 'sealed' THEN ?3 ELSE sealed_at_ms END
             WHERE binding_id = ?1",
            params![
                completion.binding_id.as_bytes(),
                completion.status.as_str(),
                completion.completed_at_ms,
            ],
        )?;
        let completed = load_receipt_from(
            &transaction,
            &completion.binding_id,
            &completion.delivery_id,
        )?
        .expect("completed receipt is readable");
        transaction.commit()?;
        Ok(completed)
    }

    pub fn revoke(&mut self, binding_id: &RegistryId) -> Result<(), HookRegistryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if load_binding_from(&transaction, binding_id)?.is_none() {
            return Err(HookRegistryError::BindingNotFound);
        }
        revoke_in(&transaction, binding_id)?;
        transaction.commit()?;
        Ok(())
    }

    /// Expires open rows from persisted wall-clock deadlines, drops work that
    /// can no longer be authenticated, then applies receipt/binding retention.
    pub fn expire_and_prune(
        &mut self,
        now_ms: i64,
    ) -> Result<RegistryPruneResult, HookRegistryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expired = transaction.execute(
            "UPDATE hook_bindings
             SET state = 'expired', row_version = row_version + 1
             WHERE state = 'open'
               AND (idle_deadline_ms <= ?1 OR absolute_deadline_ms <= ?1)",
            params![now_ms],
        )?;
        transaction.execute(
            "DELETE FROM hook_outbox
             WHERE binding_id IN (
                 SELECT binding_id FROM hook_bindings
                 WHERE state IN ('expired', 'revoked')
             )",
            [],
        )?;
        let receipts = transaction.execute(
            "DELETE FROM hook_receipts
             WHERE receipt_status != 'pending' AND prune_after_ms <= ?1",
            params![now_ms],
        )?;
        let bindings = transaction.execute(
            "DELETE FROM hook_bindings
             WHERE state != 'open' AND prune_after_ms <= ?1",
            params![now_ms],
        )?;
        transaction.commit()?;
        Ok(RegistryPruneResult {
            expired_bindings: expired as u64,
            pruned_bindings: bindings as u64,
            pruned_receipts: receipts as u64,
        })
    }
}

fn configure_connection(connection: &Connection) -> Result<(), rusqlite::Error> {
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;",
    )
}

fn initialize_schema(connection: &Connection) -> Result<(), HookRegistryError> {
    let mut found = connection.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))?;
    if found > HOOK_REGISTRY_SCHEMA_VERSION {
        return Err(HookRegistryError::UnsupportedSchema {
            found,
            expected: HOOK_REGISTRY_SCHEMA_VERSION,
        });
    }
    if found == 1 {
        let application_id =
            connection.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0))?;
        if application_id != APPLICATION_ID {
            return Err(HookRegistryError::UnsupportedSchema {
                found,
                expected: HOOK_REGISTRY_SCHEMA_VERSION,
            });
        }
        // D3 was not wired while schema v1 existed, and v1 did not retain the
        // identity needed to authenticate or migrate a binding. Preserving
        // unverifiable rows would create authority, so the v2 transition
        // deliberately invalidates that prerelease state.
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             DROP TABLE IF EXISTS hook_outbox;
             DROP TABLE IF EXISTS hook_receipts;
             DROP TABLE IF EXISTS hook_bindings;
             PRAGMA user_version = 0;
             COMMIT;",
        )?;
        found = 0;
    }
    if found == 0 {
        connection.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             PRAGMA application_id = {APPLICATION_ID};
             CREATE TABLE hook_bindings (
                 schema_version INTEGER NOT NULL CHECK (schema_version = 2),
                 row_version INTEGER NOT NULL CHECK (row_version > 0),
                 binding_id BLOB PRIMARY KEY NOT NULL CHECK (length(binding_id) BETWEEN 1 AND 256),
                 authority_fingerprint BLOB NOT NULL CHECK (length(authority_fingerprint) = 32),
                 capability_verifier BLOB NOT NULL CHECK (length(capability_verifier) = 32),
                 internal_session_id BLOB NOT NULL CHECK (length(internal_session_id) = 16),
                 integration_id BLOB NOT NULL CHECK (length(integration_id) BETWEEN 1 AND 128),
                 repository_id BLOB NOT NULL CHECK (length(repository_id) BETWEEN 1 AND 4096),
                 checkout_id BLOB NOT NULL CHECK (length(checkout_id) BETWEEN 1 AND 4096),
                 start_branch TEXT,
                 start_revision TEXT NOT NULL CHECK (length(start_revision) BETWEEN 1 AND 256),
                 generation INTEGER NOT NULL CHECK (generation > 0),
                 state TEXT NOT NULL CHECK (state IN ('open', 'sealed', 'expired', 'revoked')),
                 created_at_ms INTEGER NOT NULL,
                 last_seen_at_ms INTEGER NOT NULL,
                 idle_deadline_ms INTEGER NOT NULL,
                 absolute_deadline_ms INTEGER NOT NULL,
                 prune_after_ms INTEGER NOT NULL,
                 sealed_at_ms INTEGER,
                 next_sequence INTEGER NOT NULL CHECK (next_sequence > 0),
                 closing_sequence INTEGER,
                 CHECK (created_at_ms <= last_seen_at_ms),
                 CHECK (created_at_ms < idle_deadline_ms),
                 CHECK (created_at_ms < absolute_deadline_ms),
                 CHECK (absolute_deadline_ms <= prune_after_ms)
             );
             CREATE UNIQUE INDEX hook_bindings_one_open_tuple_idx
                 ON hook_bindings(authority_fingerprint) WHERE state = 'open';
             CREATE TABLE hook_receipts (
                 schema_version INTEGER NOT NULL CHECK (schema_version = 2),
                 row_version INTEGER NOT NULL CHECK (row_version > 0),
                 binding_id BLOB NOT NULL REFERENCES hook_bindings(binding_id) ON DELETE CASCADE,
                 delivery_id BLOB NOT NULL CHECK (length(delivery_id) BETWEEN 1 AND 256),
                 sequence_number INTEGER NOT NULL CHECK (sequence_number > 0),
                 delivery_kind TEXT NOT NULL CHECK (delivery_kind IN ('event', 'close')),
                 event_schema_version INTEGER NOT NULL CHECK (event_schema_version > 0),
                 normalized_hash BLOB NOT NULL CHECK (length(normalized_hash) = 32),
                 receipt_status TEXT NOT NULL CHECK (receipt_status IN ('pending', 'reduced', 'sealed')),
                 admitted_at_ms INTEGER NOT NULL,
                 completed_at_ms INTEGER,
                 prune_after_ms INTEGER NOT NULL,
                 PRIMARY KEY (binding_id, delivery_id),
                 UNIQUE (binding_id, sequence_number),
                 CHECK ((receipt_status = 'pending' AND completed_at_ms IS NULL)
                     OR (receipt_status != 'pending' AND completed_at_ms IS NOT NULL)),
                 CHECK (admitted_at_ms <= prune_after_ms)
             );
             CREATE TABLE hook_outbox (
                 schema_version INTEGER NOT NULL CHECK (schema_version = 2),
                 row_version INTEGER NOT NULL CHECK (row_version > 0),
                 binding_id BLOB NOT NULL,
                 delivery_id BLOB NOT NULL,
                 sequence_number INTEGER NOT NULL CHECK (sequence_number > 0),
                 delivery_kind TEXT NOT NULL CHECK (delivery_kind IN ('event', 'close')),
                 event_schema_version INTEGER NOT NULL CHECK (event_schema_version > 0),
                 normalized_hash BLOB NOT NULL CHECK (length(normalized_hash) = 32),
                 admitted_at_ms INTEGER NOT NULL,
                 attempt_count INTEGER NOT NULL CHECK (attempt_count >= 0),
                 available_after_ms INTEGER NOT NULL,
                 PRIMARY KEY (binding_id, delivery_id),
                 FOREIGN KEY (binding_id, delivery_id)
                     REFERENCES hook_receipts(binding_id, delivery_id) ON DELETE CASCADE
             );
             CREATE INDEX hook_outbox_available_idx
                 ON hook_outbox(available_after_ms, admitted_at_ms);
             CREATE INDEX hook_receipts_prune_idx
                 ON hook_receipts(prune_after_ms) WHERE receipt_status != 'pending';
             PRAGMA user_version = {HOOK_REGISTRY_SCHEMA_VERSION};
             COMMIT;"
        ))?;
    } else {
        let application_id =
            connection.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0))?;
        if application_id != APPLICATION_ID {
            return Err(HookRegistryError::UnsupportedSchema {
                found,
                expected: HOOK_REGISTRY_SCHEMA_VERSION,
            });
        }
    }
    Ok(())
}

fn validate_open_request(request: &RegistryOpenRequest) -> Result<(), HookRegistryError> {
    if request.now_ms < 0
        || request.idle_ttl_ms <= 0
        || request.absolute_ttl_ms < request.idle_ttl_ms
        || request.retention_ms < 0
        || request
            .now_ms
            .checked_add(request.absolute_ttl_ms)
            .and_then(|deadline| deadline.checked_add(request.retention_ms))
            .is_none()
    {
        return Err(HookRegistryError::InvalidValue);
    }
    Ok(())
}

fn validate_admission(admission: &RegistryAdmission) -> Result<(), HookRegistryError> {
    if admission.sequence == 0
        || admission.sequence > i64::MAX as u64
        || admission.event_schema_version == 0
        || admission.admitted_at_ms < 0
        || admission.admitted_at_ms >= admission.idle_deadline_ms
        || admission.admitted_at_ms > admission.receipt_prune_after_ms
    {
        return Err(HookRegistryError::InvalidValue);
    }
    Ok(())
}

fn to_i64(value: u64) -> Result<i64, HookRegistryError> {
    i64::try_from(value).map_err(|_| HookRegistryError::InvalidValue)
}

fn blob_hash(value: Vec<u8>) -> rusqlite::Result<RegistryHash> {
    let value: [u8; SHA256_BYTES] = value
        .try_into()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(RegistryHash(value))
}

fn blob_id(value: Vec<u8>) -> rusqlite::Result<RegistryId> {
    RegistryId::from_bytes(value).map_err(|_| rusqlite::Error::InvalidQuery)
}

fn u64_column(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| rusqlite::Error::InvalidQuery)
}

fn u32_column(value: i64) -> rusqlite::Result<u32> {
    u32::try_from(value).map_err(|_| rusqlite::Error::InvalidQuery)
}

fn load_binding_from(
    connection: &Connection,
    binding_id: &RegistryId,
) -> rusqlite::Result<Option<RegistryBinding>> {
    connection
        .query_row(
            "SELECT schema_version, row_version, binding_id,
                    authority_fingerprint, capability_verifier,
                    internal_session_id, integration_id, repository_id,
                    checkout_id, start_branch, start_revision, generation,
                    state, created_at_ms, last_seen_at_ms, idle_deadline_ms,
                    absolute_deadline_ms, prune_after_ms, next_sequence,
                    closing_sequence
             FROM hook_bindings WHERE binding_id = ?1",
            params![binding_id.as_bytes()],
            |row| {
                Ok(RegistryBinding {
                    schema_version: u32_column(row.get(0)?)?,
                    row_version: u64_column(row.get(1)?)?,
                    binding_id: blob_id(row.get(2)?)?,
                    authority_fingerprint: blob_hash(row.get(3)?)?,
                    capability_verifier: blob_hash(row.get(4)?)?,
                    internal_session_id: blob_id(row.get(5)?)?,
                    integration_id: blob_id(row.get(6)?)?,
                    repository_id: blob_id(row.get(7)?)?,
                    checkout_id: blob_id(row.get(8)?)?,
                    start_branch: row.get(9)?,
                    start_revision: row.get(10)?,
                    generation: u64_column(row.get(11)?)?,
                    state: RegistryBindingState::parse(&row.get::<_, String>(12)?)?,
                    created_at_ms: row.get(13)?,
                    last_seen_at_ms: row.get(14)?,
                    idle_deadline_ms: row.get(15)?,
                    absolute_deadline_ms: row.get(16)?,
                    prune_after_ms: row.get(17)?,
                    next_sequence: u64_column(row.get(18)?)?,
                    closing_sequence: row.get::<_, Option<i64>>(19)?.map(u64_column).transpose()?,
                })
            },
        )
        .optional()
}

fn load_open_binding_by_authority(
    connection: &Connection,
    authority_fingerprint: &RegistryHash,
) -> rusqlite::Result<Option<RegistryBinding>> {
    let binding_id = connection
        .query_row(
            "SELECT binding_id FROM hook_bindings
             WHERE authority_fingerprint = ?1 AND state = 'open'",
            params![authority_fingerprint.as_bytes()],
            |row| blob_id(row.get(0)?),
        )
        .optional()?;
    binding_id
        .as_ref()
        .map(|binding_id| load_binding_from(connection, binding_id))
        .transpose()
        .map(Option::flatten)
}

fn expire_open_tuple_if_needed(
    connection: &Connection,
    authority_fingerprint: &RegistryHash,
    now_ms: i64,
) -> rusqlite::Result<()> {
    connection.execute(
        "UPDATE hook_bindings
         SET state = 'expired', row_version = row_version + 1
         WHERE authority_fingerprint = ?1 AND state = 'open'
           AND (idle_deadline_ms <= ?2 OR absolute_deadline_ms <= ?2)",
        params![authority_fingerprint.as_bytes(), now_ms],
    )?;
    connection.execute(
        "DELETE FROM hook_outbox
         WHERE binding_id IN (
             SELECT binding_id FROM hook_bindings
             WHERE authority_fingerprint = ?1 AND state = 'expired'
         )",
        params![authority_fingerprint.as_bytes()],
    )?;
    Ok(())
}

fn next_generation(
    connection: &Connection,
    authority_fingerprint: &RegistryHash,
) -> Result<u64, HookRegistryError> {
    let previous: i64 = connection.query_row(
        "SELECT COALESCE(MAX(generation), 0) FROM hook_bindings
         WHERE authority_fingerprint = ?1",
        params![authority_fingerprint.as_bytes()],
        |row| row.get(0),
    )?;
    u64_column(previous)?
        .checked_add(1)
        .ok_or(HookRegistryError::InvalidValue)
}

fn verify_persisted_identity(
    existing: &RegistryBinding,
    request: &RegistryOpenRequest,
) -> Result<(), HookRegistryError> {
    let matches = existing.integration_id.as_bytes() == request.integration.as_str().as_bytes()
        && existing.repository_id.as_bytes() == request.checkout.repository_id().as_bytes()
        && existing.checkout_id.as_bytes() == request.checkout.checkout_id().as_bytes();
    if matches {
        Ok(())
    } else {
        Err(HookRegistryError::BindingConflict)
    }
}

fn registry_id(bytes: &[u8]) -> Result<RegistryId, HookRegistryError> {
    RegistryId::from_bytes(bytes.to_vec())
}

fn hook_internal_session_id(
    identifier: &RegistryId,
) -> Result<HookInternalSessionId, HookRegistryError> {
    let bytes: [u8; 16] = identifier
        .as_bytes()
        .try_into()
        .map_err(|_| HookRegistryError::InvalidIdentifier)?;
    Ok(HookInternalSessionId::from_bytes(bytes))
}

fn load_receipt_from(
    connection: &Connection,
    binding_id: &RegistryId,
    delivery_id: &RegistryId,
) -> rusqlite::Result<Option<RegistryReceipt>> {
    connection
        .query_row(
            "SELECT schema_version, row_version, binding_id, delivery_id,
                    sequence_number, delivery_kind, event_schema_version,
                    normalized_hash, receipt_status, admitted_at_ms,
                    completed_at_ms, prune_after_ms
             FROM hook_receipts WHERE binding_id = ?1 AND delivery_id = ?2",
            params![binding_id.as_bytes(), delivery_id.as_bytes()],
            receipt_from_row,
        )
        .optional()
}

fn receipt_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RegistryReceipt> {
    Ok(RegistryReceipt {
        schema_version: u32_column(row.get(0)?)?,
        row_version: u64_column(row.get(1)?)?,
        binding_id: blob_id(row.get(2)?)?,
        delivery_id: blob_id(row.get(3)?)?,
        sequence: u64_column(row.get(4)?)?,
        kind: RegistryDeliveryKind::parse(&row.get::<_, String>(5)?)?,
        event_schema_version: u32_column(row.get(6)?)?,
        normalized_hash: blob_hash(row.get(7)?)?,
        status: RegistryReceiptStatus::parse(&row.get::<_, String>(8)?)?,
        admitted_at_ms: row.get(9)?,
        completed_at_ms: row.get(10)?,
        prune_after_ms: row.get(11)?,
    })
}

fn pending_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RegistryPendingDelivery> {
    Ok(RegistryPendingDelivery {
        schema_version: u32_column(row.get(0)?)?,
        row_version: u64_column(row.get(1)?)?,
        binding_id: blob_id(row.get(2)?)?,
        delivery_id: blob_id(row.get(3)?)?,
        sequence: u64_column(row.get(4)?)?,
        kind: RegistryDeliveryKind::parse(&row.get::<_, String>(5)?)?,
        event_schema_version: u32_column(row.get(6)?)?,
        normalized_hash: blob_hash(row.get(7)?)?,
        admitted_at_ms: row.get(8)?,
        attempt_count: u64_column(row.get(9)?)?,
        available_after_ms: row.get(10)?,
    })
}

fn expire_binding_if_needed(
    connection: &Connection,
    binding: &mut RegistryBinding,
    now_ms: i64,
) -> Result<(), rusqlite::Error> {
    if binding.state == RegistryBindingState::Open
        && (now_ms >= binding.idle_deadline_ms || now_ms >= binding.absolute_deadline_ms)
    {
        connection.execute(
            "UPDATE hook_bindings
             SET state = 'expired', row_version = row_version + 1
             WHERE binding_id = ?1 AND state = 'open'",
            params![binding.binding_id.as_bytes()],
        )?;
        connection.execute(
            "DELETE FROM hook_outbox WHERE binding_id = ?1",
            params![binding.binding_id.as_bytes()],
        )?;
        binding.state = RegistryBindingState::Expired;
    }
    Ok(())
}

fn revoke_in(connection: &Connection, binding_id: &RegistryId) -> rusqlite::Result<()> {
    connection.execute(
        "UPDATE hook_bindings
         SET state = 'revoked', row_version = row_version + 1
         WHERE binding_id = ?1",
        params![binding_id.as_bytes()],
    )?;
    connection.execute(
        "DELETE FROM hook_outbox WHERE binding_id = ?1",
        params![binding_id.as_bytes()],
    )?;
    Ok(())
}

fn count_for(
    connection: &Connection,
    table: &'static str,
    binding_id: &RegistryId,
) -> Result<usize, HookRegistryError> {
    let query = match table {
        "hook_receipts" => "SELECT COUNT(*) FROM hook_receipts WHERE binding_id = ?1",
        "hook_outbox" => "SELECT COUNT(*) FROM hook_outbox WHERE binding_id = ?1",
        _ => unreachable!("only internal registry tables are counted"),
    };
    let count = connection.query_row(query, params![binding_id.as_bytes()], |row| {
        row.get::<_, i64>(0)
    })?;
    usize::try_from(count).map_err(|_| HookRegistryError::InvalidValue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn id(value: &str) -> RegistryId {
        RegistryId::from_string(value).unwrap()
    }

    fn hash(value: u8) -> RegistryHash {
        RegistryHash::from_bytes([value; SHA256_BYTES])
    }

    fn binding() -> TestRegistryBinding {
        TestRegistryBinding {
            binding_id: id("binding-1"),
            authority_fingerprint: hash(1),
            capability_verifier: hash(2),
            generation: 1,
            created_at_ms: 10,
            idle_deadline_ms: 100,
            absolute_deadline_ms: 1_000,
            prune_after_ms: 2_000,
        }
    }

    fn admission(delivery: &str, sequence: u64, value: u8) -> RegistryAdmission {
        RegistryAdmission {
            binding_id: id("binding-1"),
            delivery_id: id(delivery),
            sequence,
            kind: RegistryDeliveryKind::Event,
            event_schema_version: 1,
            normalized_hash: hash(value),
            admitted_at_ms: 20,
            idle_deadline_ms: 110,
            receipt_prune_after_ms: 1_500,
        }
    }

    fn cryptography() -> HookSessionCryptography {
        HookSessionCryptography::from_secret([0x51; 32])
    }

    fn open_request(now_ms: i64, resume: Option<RegistrySessionResume>) -> RegistryOpenRequest {
        RegistryOpenRequest {
            integration: HookIntegrationId::new("codex/v1").unwrap(),
            host_session_id: HostSessionId::new("opaque-host-session").unwrap(),
            checkout: HookCheckoutIdentity::new("repository-A", "checkout-A").unwrap(),
            repository_state: HookRepositoryState::new(Some("feature/d3".to_string()), "abc123")
                .unwrap(),
            resume,
            now_ms,
            idle_ttl_ms: 100,
            absolute_ttl_ms: 1_000,
            retention_ms: 1_000,
        }
    }

    fn temp_db(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lattice-hook-registry-{label}-{}-{nonce}.sqlite",
            std::process::id()
        ))
    }

    fn clean_db(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let candidate = PathBuf::from(format!("{}{}", path.display(), suffix));
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn open_or_resume_is_atomic_persistent_and_renews_only_idle() {
        let path = temp_db("open-resume");
        let crypto = cryptography();
        let opened = {
            let mut registry = HookSessionRegistry::open(&path, Default::default()).unwrap();
            let opened = registry
                .open_or_resume(&crypto, open_request(10, None))
                .unwrap();
            assert!(!opened.resumed);
            assert_eq!(opened.generation, 1);
            assert_eq!(opened.idle_deadline_ms, 110);
            assert_eq!(opened.absolute_deadline_ms, 1_010);
            let row = registry
                .binding(&registry_id(opened.binding_id.as_bytes()).unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(
                row.internal_session_id.as_bytes(),
                opened.internal_session_id.as_bytes()
            );
            assert_eq!(row.integration_id.as_bytes(), b"codex/v1");
            assert_eq!(row.repository_id.as_bytes(), b"repository-A");
            assert_eq!(row.checkout_id.as_bytes(), b"checkout-A");
            assert_eq!(row.start_branch.as_deref(), Some("feature/d3"));
            assert_eq!(row.start_revision, "abc123");
            opened
        };

        let mut registry = HookSessionRegistry::open(&path, Default::default()).unwrap();
        assert!(matches!(
            registry.open_or_resume(&crypto, open_request(20, None)),
            Err(HookRegistryError::BindingAlreadyOpen)
        ));
        let wrong = RegistrySessionResume {
            binding_id: opened.binding_id,
            capability: HookSessionCapability::from_bytes([0xff; 32]),
        };
        assert!(matches!(
            registry.open_or_resume(&crypto, open_request(20, Some(wrong))),
            Err(HookRegistryError::InvalidCapability)
        ));
        let before = registry
            .binding(&registry_id(opened.binding_id.as_bytes()).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(before.last_seen_at_ms, 10);
        assert_eq!(before.idle_deadline_ms, 110);

        let resumed = registry
            .open_or_resume(
                &crypto,
                open_request(
                    50,
                    Some(RegistrySessionResume {
                        binding_id: opened.binding_id,
                        capability: opened.capability,
                    }),
                ),
            )
            .unwrap();
        assert!(resumed.resumed);
        assert_eq!(resumed.internal_session_id, opened.internal_session_id);
        assert_eq!(resumed.generation, 1);
        assert_eq!(resumed.idle_deadline_ms, 150);
        assert_eq!(resumed.absolute_deadline_ms, 1_010);
        clean_db(&path);
    }

    #[test]
    fn durable_verification_returns_persisted_authority_and_renews_lease() {
        let path = temp_db("verify-renew");
        let crypto = cryptography();
        let opened = {
            let mut registry = HookSessionRegistry::open(&path, Default::default()).unwrap();
            registry
                .open_or_resume(&crypto, open_request(10, None))
                .unwrap()
        };
        let mut registry = HookSessionRegistry::open(&path, Default::default()).unwrap();
        let verified = registry
            .verify_and_renew(
                &crypto,
                RegistryVerifyRequest {
                    binding_id: opened.binding_id,
                    capability: opened.capability,
                    integration: HookIntegrationId::new("codex/v1").unwrap(),
                    current_checkout: HookCheckoutIdentity::new("repository-A", "checkout-A")
                        .unwrap(),
                    now_ms: 90,
                    idle_ttl_ms: 100,
                },
            )
            .unwrap();
        assert_eq!(verified.internal_session_id, opened.internal_session_id);
        assert_eq!(verified.repository_state.branch(), Some("feature/d3"));
        assert_eq!(verified.repository_state.revision(), "abc123");
        assert_eq!(verified.idle_deadline_ms, 190);
        assert_eq!(verified.absolute_deadline_ms, 1_010);

        let error = registry
            .verify_and_renew(
                &crypto,
                RegistryVerifyRequest {
                    binding_id: opened.binding_id,
                    capability: opened.capability,
                    integration: HookIntegrationId::new("codex/v1").unwrap(),
                    current_checkout: HookCheckoutIdentity::new("repository-A", "sibling-checkout")
                        .unwrap(),
                    now_ms: 100,
                    idle_ttl_ms: 100,
                },
            )
            .unwrap_err();
        assert!(matches!(error, HookRegistryError::AuthorityMismatch));
        clean_db(&path);
    }

    #[test]
    fn expired_tuple_opens_new_generation_without_reusing_internal_session() {
        let crypto = cryptography();
        let mut registry = HookSessionRegistry::open_in_memory(Default::default()).unwrap();
        let first = registry
            .open_or_resume(&crypto, open_request(10, None))
            .unwrap();
        let second = registry
            .open_or_resume(&crypto, open_request(110, None))
            .unwrap();
        assert_eq!(second.generation, 2);
        assert_ne!(second.binding_id, first.binding_id);
        assert_ne!(second.internal_session_id, first.internal_session_id);
        assert_ne!(second.capability, first.capability);
        assert_eq!(
            registry
                .binding(&registry_id(first.binding_id.as_bytes()).unwrap())
                .unwrap()
                .unwrap()
                .state,
            RegistryBindingState::Expired
        );
    }

    #[test]
    fn admission_and_completion_are_atomic_and_versioned() {
        let mut registry = HookSessionRegistry::open_in_memory(Default::default()).unwrap();
        assert!(registry.insert_test_binding(binding()).unwrap());
        assert!(!registry.insert_test_binding(binding()).unwrap());

        let outcome = registry.admit(admission("delivery-1", 1, 7)).unwrap();
        assert!(!outcome.idempotent_replay);
        assert_eq!(outcome.receipt.schema_version, 2);
        assert_eq!(outcome.receipt.row_version, 1);
        assert_eq!(registry.pending(20, 10).unwrap().len(), 1);

        let receipt = registry
            .complete(RegistryCompletion {
                binding_id: id("binding-1"),
                delivery_id: id("delivery-1"),
                normalized_hash: hash(7),
                status: RegistryReceiptStatus::Reduced,
                completed_at_ms: 30,
            })
            .unwrap();
        assert_eq!(receipt.status, RegistryReceiptStatus::Reduced);
        assert_eq!(receipt.row_version, 2);
        assert!(registry.pending(30, 10).unwrap().is_empty());
        assert_eq!(
            registry
                .binding(&id("binding-1"))
                .unwrap()
                .unwrap()
                .next_sequence,
            2
        );
    }

    #[test]
    fn pending_outbox_and_receipt_survive_restart() {
        let path = temp_db("restart");
        {
            let mut registry = HookSessionRegistry::open(&path, Default::default()).unwrap();
            registry.insert_test_binding(binding()).unwrap();
            registry.admit(admission("delivery-1", 1, 7)).unwrap();
        }
        {
            let registry = HookSessionRegistry::open(&path, Default::default()).unwrap();
            let pending = registry.pending(20, 10).unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].delivery_id, id("delivery-1"));
            assert_eq!(
                registry
                    .receipt(&id("binding-1"), &id("delivery-1"))
                    .unwrap()
                    .unwrap()
                    .status,
                RegistryReceiptStatus::Pending
            );
        }
        clean_db(&path);
    }

    #[test]
    fn exact_replay_is_idempotent_and_changed_hash_revokes() {
        let mut registry = HookSessionRegistry::open_in_memory(Default::default()).unwrap();
        registry.insert_test_binding(binding()).unwrap();
        registry.admit(admission("delivery-1", 1, 7)).unwrap();
        let replay = registry.admit(admission("delivery-1", 1, 7)).unwrap();
        assert!(replay.idempotent_replay);
        assert_eq!(registry.pending(20, 10).unwrap().len(), 1);

        let error = registry.admit(admission("delivery-1", 1, 8)).unwrap_err();
        assert!(matches!(error, HookRegistryError::ReplayViolation));
        assert_eq!(
            registry.binding(&id("binding-1")).unwrap().unwrap().state,
            RegistryBindingState::Revoked
        );
        assert!(registry.pending(20, 10).unwrap().is_empty());
    }

    #[test]
    fn failed_out_of_order_completion_leaves_receipt_and_outbox_pending() {
        let mut registry = HookSessionRegistry::open_in_memory(Default::default()).unwrap();
        registry.insert_test_binding(binding()).unwrap();
        registry.admit(admission("delivery-2", 2, 8)).unwrap();

        let error = registry
            .complete(RegistryCompletion {
                binding_id: id("binding-1"),
                delivery_id: id("delivery-2"),
                normalized_hash: hash(8),
                status: RegistryReceiptStatus::Reduced,
                completed_at_ms: 30,
            })
            .unwrap_err();
        assert!(matches!(error, HookRegistryError::OrderViolation));
        assert_eq!(registry.pending(30, 10).unwrap().len(), 1);
        assert_eq!(
            registry
                .receipt(&id("binding-1"), &id("delivery-2"))
                .unwrap()
                .unwrap()
                .status,
            RegistryReceiptStatus::Pending
        );
        assert_eq!(
            registry
                .binding(&id("binding-1"))
                .unwrap()
                .unwrap()
                .next_sequence,
            1
        );
    }

    #[test]
    fn expiry_discovered_by_admission_is_committed() {
        let path = temp_db("admission-expiry");
        {
            let mut registry = HookSessionRegistry::open(&path, Default::default()).unwrap();
            let mut row = binding();
            row.idle_deadline_ms = 30;
            registry.insert_test_binding(row).unwrap();
            let mut late = admission("late", 1, 7);
            late.admitted_at_ms = 30;
            late.idle_deadline_ms = 40;
            let error = registry.admit(late).unwrap_err();
            assert!(matches!(error, HookRegistryError::Expired));
        }
        {
            let registry = HookSessionRegistry::open(&path, Default::default()).unwrap();
            assert_eq!(
                registry.binding(&id("binding-1")).unwrap().unwrap().state,
                RegistryBindingState::Expired
            );
            assert!(registry
                .receipt(&id("binding-1"), &id("late"))
                .unwrap()
                .is_none());
        }
        clean_db(&path);
    }

    #[test]
    fn expiry_is_conservative_across_restart_and_pruning_is_bounded() {
        let path = temp_db("expiry");
        {
            let mut registry = HookSessionRegistry::open(&path, Default::default()).unwrap();
            let mut row = binding();
            row.idle_deadline_ms = 30;
            registry.insert_test_binding(row).unwrap();
            registry.admit(admission("delivery-1", 1, 7)).unwrap();
        }
        {
            let mut registry = HookSessionRegistry::open(&path, Default::default()).unwrap();
            let result = registry.expire_and_prune(110).unwrap();
            assert_eq!(result.expired_bindings, 1);
            assert_eq!(
                registry.binding(&id("binding-1")).unwrap().unwrap().state,
                RegistryBindingState::Expired
            );
            assert!(registry.pending(110, 10).unwrap().is_empty());
            assert_eq!(registry.expire_and_prune(1_999).unwrap().pruned_bindings, 0);
            assert_eq!(registry.expire_and_prune(2_000).unwrap().pruned_bindings, 1);
            assert!(registry.binding(&id("binding-1")).unwrap().is_none());
        }
        clean_db(&path);
    }

    #[test]
    fn close_seals_transactionally_and_replays_after_restart() {
        let path = temp_db("close");
        {
            let mut registry = HookSessionRegistry::open(&path, Default::default()).unwrap();
            registry.insert_test_binding(binding()).unwrap();
            let mut close = admission("close-1", 1, 9);
            close.kind = RegistryDeliveryKind::Close;
            registry.admit(close).unwrap();
            registry
                .complete(RegistryCompletion {
                    binding_id: id("binding-1"),
                    delivery_id: id("close-1"),
                    normalized_hash: hash(9),
                    status: RegistryReceiptStatus::Sealed,
                    completed_at_ms: 30,
                })
                .unwrap();
        }
        {
            let mut registry = HookSessionRegistry::open(&path, Default::default()).unwrap();
            let mut close = admission("close-1", 1, 9);
            close.kind = RegistryDeliveryKind::Close;
            let replay = registry.admit(close).unwrap();
            assert!(replay.idempotent_replay);
            assert_eq!(replay.receipt.status, RegistryReceiptStatus::Sealed);
            assert_eq!(
                registry.binding(&id("binding-1")).unwrap().unwrap().state,
                RegistryBindingState::Sealed
            );
        }
        clean_db(&path);
    }

    #[test]
    fn future_schema_is_refused_without_mutation() {
        let path = temp_db("schema");
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 99).unwrap();
        drop(connection);
        let error = HookSessionRegistry::open(&path, Default::default()).unwrap_err();
        assert!(matches!(
            error,
            HookRegistryError::UnsupportedSchema {
                found: 99,
                expected: HOOK_REGISTRY_SCHEMA_VERSION
            }
        ));
        clean_db(&path);
    }

    #[test]
    fn source_has_no_payload_or_transcript_storage_columns() {
        let source = include_str!("hook_session_registry.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            "payload TEXT",
            "payload BLOB",
            "transcript TEXT",
            "transcript BLOB",
            "summary TEXT",
            "command TEXT",
            "path TEXT",
        ] {
            assert!(
                !source.contains(forbidden),
                "found forbidden storage: {forbidden}"
            );
        }
    }
}
