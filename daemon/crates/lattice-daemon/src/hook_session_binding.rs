//! Boot-scoped authority for trusted, repository-local hook sessions.
//!
//! This module deliberately has no hook-envelope or transcript-shaped input.
//! Callers must normalize and sanitize an event before admission, then pass
//! only its cryptographic payload hash and delivery metadata here. The
//! authority binds a random capability to one integration, repository, and
//! exact checkout; enforces finite lifetimes and ordered, idempotent delivery;
//! and seals a session as soon as its close delivery becomes reducible.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::time::{Duration, Instant};

use thiserror::Error;

const TOKEN_BYTES: usize = 32;
const ID_BYTES: usize = 16;
const HASH_BYTES: usize = 32;
const MAX_IDENTITY_BYTES: usize = 4096;
const MAX_HOST_SESSION_BYTES: usize = 1024;
const MAX_INTEGRATION_BYTES: usize = 128;
const MAX_BRANCH_BYTES: usize = 1024;
const MAX_REVISION_BYTES: usize = 256;
const MAX_IDLE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_ABSOLUTE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_CLOSE_RETRY_GRACE: Duration = Duration::from_secs(60 * 60);
const MAX_REORDER_WINDOW: u64 = 256;
const MAX_RECEIPTS: usize = 65_536;

/// Finite policy limits for one daemon boot's hook-session authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HookSessionConfig {
    pub idle_ttl: Duration,
    pub absolute_ttl: Duration,
    pub close_retry_grace: Duration,
    pub reorder_window: u64,
    pub max_receipts_per_binding: usize,
}

impl Default for HookSessionConfig {
    fn default() -> Self {
        Self {
            idle_ttl: Duration::from_secs(30 * 60),
            absolute_ttl: Duration::from_secs(12 * 60 * 60),
            close_retry_grace: Duration::from_secs(5 * 60),
            reorder_window: 32,
            max_receipts_per_binding: 16_384,
        }
    }
}

impl HookSessionConfig {
    fn validate(self) -> Result<Self, HookSessionError> {
        if self.idle_ttl.is_zero()
            || self.idle_ttl > MAX_IDLE_TTL
            || self.absolute_ttl < self.idle_ttl
            || self.absolute_ttl > MAX_ABSOLUTE_TTL
            || self.close_retry_grace.is_zero()
            || self.close_retry_grace > MAX_CLOSE_RETRY_GRACE
            || self.reorder_window == 0
            || self.reorder_window > MAX_REORDER_WINDOW
            || self.max_receipts_per_binding == 0
            || self.max_receipts_per_binding > MAX_RECEIPTS
        {
            return Err(HookSessionError::InvalidConfiguration);
        }
        Ok(self)
    }
}

/// Exact daemon-resolved authority to which a hook capability is bound.
#[derive(Clone, Eq, PartialEq)]
pub struct HookCheckoutIdentity {
    repository_id: String,
    checkout_id: String,
}

impl HookCheckoutIdentity {
    pub fn new(
        repository_id: impl Into<String>,
        checkout_id: impl Into<String>,
    ) -> Result<Self, HookSessionError> {
        let repository_id = repository_id.into();
        let checkout_id = checkout_id.into();
        validate_bounded_identity(&repository_id, MAX_IDENTITY_BYTES)?;
        validate_bounded_identity(&checkout_id, MAX_IDENTITY_BYTES)?;
        Ok(Self {
            repository_id,
            checkout_id,
        })
    }

    pub fn repository_id(&self) -> &str {
        &self.repository_id
    }

    pub fn checkout_id(&self) -> &str {
        &self.checkout_id
    }
}

impl fmt::Debug for HookCheckoutIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HookCheckoutIdentity(<redacted>)")
    }
}

/// A bounded integration adapter identity, such as an adapter name/version.
#[derive(Clone, Eq, PartialEq)]
pub struct HookIntegrationId(String);

impl HookIntegrationId {
    pub fn new(value: impl Into<String>) -> Result<Self, HookSessionError> {
        let value = value.into();
        validate_bounded_identity(&value, MAX_INTEGRATION_BYTES)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for HookIntegrationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HookIntegrationId(<redacted>)")
    }
}

/// Opaque host identity used only to derive the registry key. It is not stored.
pub struct HostSessionId(String);

impl HostSessionId {
    pub fn new(value: impl Into<String>) -> Result<Self, HookSessionError> {
        let value = value.into();
        validate_bounded_identity(&value, MAX_HOST_SESSION_BYTES)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for HostSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HostSessionId(<redacted>)")
    }
}

macro_rules! redacted_id {
    ($name:ident, $size:expr, $label:literal) => {
        #[derive(Clone, Copy, Eq, Hash, PartialEq)]
        pub struct $name([u8; $size]);

        impl $name {
            pub fn from_bytes(bytes: [u8; $size]) -> Self {
                Self(bytes)
            }

            pub fn as_bytes(&self) -> &[u8; $size] {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(concat!($label, "(<redacted>)"))
            }
        }
    };
}

redacted_id!(HookBindingId, ID_BYTES, "HookBindingId");
redacted_id!(HookInternalSessionId, ID_BYTES, "HookInternalSessionId");
redacted_id!(HookDeliveryId, ID_BYTES, "HookDeliveryId");
redacted_id!(HookSessionCapability, TOKEN_BYTES, "HookSessionCapability");
redacted_id!(HookDaemonEpoch, ID_BYTES, "HookDaemonEpoch");
redacted_id!(
    HookAuthorityFingerprint,
    HASH_BYTES,
    "HookAuthorityFingerprint"
);
redacted_id!(HookCapabilityVerifier, HASH_BYTES, "HookCapabilityVerifier");

/// Daemon-observed Git state recorded at binding creation. `branch` is absent
/// for detached HEAD; `revision` is always required so persisted authority can
/// be attributed to an exact starting point without trusting hook input.
#[derive(Clone, Eq, PartialEq)]
pub struct HookRepositoryState {
    branch: Option<String>,
    revision: String,
}

impl fmt::Debug for HookRepositoryState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HookRepositoryState(<redacted>)")
    }
}

impl HookRepositoryState {
    pub fn new(
        branch: Option<String>,
        revision: impl Into<String>,
    ) -> Result<Self, HookSessionError> {
        if let Some(branch) = &branch {
            validate_bounded_identity(branch, MAX_BRANCH_BYTES)?;
        }
        let revision = revision.into();
        validate_bounded_identity(&revision, MAX_REVISION_BYTES)?;
        Ok(Self { branch, revision })
    }

    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }
}

/// The one cryptographic implementation shared by volatile authority and the
/// durable registry. The caller must retain this secret across daemon boots
/// when using it with persisted registry rows.
pub struct HookSessionCryptography {
    secret: [u8; TOKEN_BYTES],
}

impl fmt::Debug for HookSessionCryptography {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HookSessionCryptography(<redacted>)")
    }
}

#[derive(Clone)]
pub struct PreparedHookBinding {
    pub binding_id: HookBindingId,
    pub capability: HookSessionCapability,
    pub internal_session_id: HookInternalSessionId,
    pub authority_fingerprint: HookAuthorityFingerprint,
    pub capability_verifier: HookCapabilityVerifier,
}

impl fmt::Debug for PreparedHookBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedHookBinding")
            .field("binding_id", &"<redacted>")
            .field("capability", &"<redacted>")
            .field("internal_session_id", &"<redacted>")
            .field("authority_fingerprint", &"<redacted>")
            .field("capability_verifier", &"<redacted>")
            .finish()
    }
}

impl HookSessionCryptography {
    pub fn new() -> Result<Self, HookSessionError> {
        let mut secret = [0_u8; TOKEN_BYTES];
        secure_random(&mut secret)?;
        Ok(Self { secret })
    }

    /// Constructs the authority from daemon-owned persistent key material.
    /// The raw key is intentionally neither returned nor rendered.
    pub fn from_secret(secret: [u8; TOKEN_BYTES]) -> Self {
        Self { secret }
    }

    pub fn authority_fingerprint(
        &self,
        integration: &HookIntegrationId,
        host_session_id: &HostSessionId,
        checkout: &HookCheckoutIdentity,
    ) -> HookAuthorityFingerprint {
        HookAuthorityFingerprint(derive_session_key(
            &self.secret,
            integration,
            host_session_id,
            checkout,
        ))
    }

    pub fn prepare_binding(
        &self,
        integration: &HookIntegrationId,
        host_session_id: &HostSessionId,
        checkout: &HookCheckoutIdentity,
    ) -> Result<PreparedHookBinding, HookSessionError> {
        let mut binding_id = [0_u8; ID_BYTES];
        let mut capability = [0_u8; TOKEN_BYTES];
        let mut internal_session_id = [0_u8; ID_BYTES];
        secure_random(&mut binding_id)?;
        secure_random(&mut capability)?;
        secure_random(&mut internal_session_id)?;
        let binding_id = HookBindingId(binding_id);
        let capability = HookSessionCapability(capability);
        Ok(PreparedHookBinding {
            binding_id,
            capability,
            internal_session_id: HookInternalSessionId(internal_session_id),
            authority_fingerprint: self.authority_fingerprint(
                integration,
                host_session_id,
                checkout,
            ),
            capability_verifier: self.capability_verifier(&binding_id, &capability),
        })
    }

    pub fn capability_verifier(
        &self,
        binding_id: &HookBindingId,
        capability: &HookSessionCapability,
    ) -> HookCapabilityVerifier {
        HookCapabilityVerifier(capability_verifier(&self.secret, binding_id, capability))
    }

    pub fn verify_capability(
        &self,
        binding_id: &HookBindingId,
        capability: &HookSessionCapability,
        expected: &HookCapabilityVerifier,
    ) -> bool {
        let presented = self.capability_verifier(binding_id, capability);
        constant_time_eq(expected.as_bytes(), presented.as_bytes())
    }
}

/// Hash of a fully normalized, already-sanitized event.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct NormalizedEventHash([u8; HASH_BYTES]);

impl NormalizedEventHash {
    pub fn from_bytes(bytes: [u8; HASH_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; HASH_BYTES] {
        &self.0
    }
}

impl fmt::Debug for NormalizedEventHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NormalizedEventHash(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookBindingState {
    Open,
    Sealed,
    Expired,
    Revoked,
}

#[derive(Debug)]
pub struct HookSessionOpenRequest {
    pub integration: HookIntegrationId,
    pub host_session_id: HostSessionId,
    pub checkout: HookCheckoutIdentity,
}

#[derive(Clone)]
pub struct MintedHookSession {
    pub binding_id: HookBindingId,
    pub capability: HookSessionCapability,
    pub internal_session_id: HookInternalSessionId,
    pub daemon_epoch: HookDaemonEpoch,
    pub generation: u64,
    pub idle_ttl: Duration,
    pub absolute_ttl: Duration,
}

impl fmt::Debug for MintedHookSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MintedHookSession")
            .field("binding_id", &"<redacted>")
            .field("capability", &"<redacted>")
            .field("internal_session_id", &"<redacted>")
            .field("daemon_epoch", &"<redacted>")
            .field("generation", &self.generation)
            .field("idle_ttl", &self.idle_ttl)
            .field("absolute_ttl", &self.absolute_ttl)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookSessionVerification {
    pub binding_id: HookBindingId,
    pub internal_session_id: HookInternalSessionId,
    pub integration: HookIntegrationId,
    pub checkout: HookCheckoutIdentity,
    pub generation: u64,
}

#[derive(Debug)]
pub struct HookSessionVerifyRequest {
    pub binding_id: HookBindingId,
    pub capability: HookSessionCapability,
    pub integration: HookIntegrationId,
    /// Fresh daemon resolution of the process's exact checkout.
    pub current_checkout: HookCheckoutIdentity,
}

#[derive(Debug)]
pub struct HookSessionResumeRequest {
    pub binding_id: HookBindingId,
    pub capability: HookSessionCapability,
    pub integration: HookIntegrationId,
    pub host_session_id: HostSessionId,
    /// Fresh daemon resolution of the process's exact checkout.
    pub current_checkout: HookCheckoutIdentity,
}

#[derive(Clone, Debug)]
pub struct ResumedHookSession {
    pub verification: HookSessionVerification,
    pub idle_ttl: Duration,
}

/// Content-free lifecycle timing for registry persistence and health reporting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HookBindingTiming {
    pub age: Duration,
    pub idle_age: Duration,
    pub idle_remaining: Duration,
    pub absolute_remaining: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookDeliveryKind {
    Event,
    Close,
}

#[derive(Debug)]
pub struct HookEventAdmission {
    pub authority: HookSessionVerifyRequest,
    pub delivery_id: HookDeliveryId,
    pub sequence: u64,
    pub normalized_hash: NormalizedEventHash,
}

#[derive(Debug)]
pub struct HookSessionSealRequest {
    pub authority: HookSessionVerifyRequest,
    pub delivery_id: HookDeliveryId,
    pub sequence: u64,
    pub normalized_hash: NormalizedEventHash,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookDeliveryStatus {
    Buffered,
    Reduced,
    Sealed,
}

/// Content-free metadata for a delivery now ready to reduce in sequence order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReducibleHookDelivery {
    pub delivery_id: HookDeliveryId,
    pub sequence: u64,
    pub kind: HookDeliveryKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookAdmissionOutcome {
    pub status: HookDeliveryStatus,
    pub idempotent_replay: bool,
    pub next_sequence: u64,
    pub newly_reducible: Vec<ReducibleHookDelivery>,
}

#[derive(Debug, Error, Clone, Copy, Eq, PartialEq)]
pub enum HookSessionError {
    #[error("hook-session configuration is invalid")]
    InvalidConfiguration,
    #[error("hook-session identity is invalid")]
    InvalidIdentity,
    #[error("secure random generation is unavailable")]
    EntropyUnavailable,
    #[error("hook-session binding was not found")]
    BindingNotFound,
    #[error("hook-session binding is already open")]
    BindingAlreadyOpen,
    #[error("hook-session capability is invalid")]
    InvalidCapability,
    #[error("hook-session integration does not match")]
    IntegrationMismatch,
    #[error("hook-session checkout does not match")]
    CheckoutMismatch,
    #[error("hook-session repository does not match")]
    RepositoryMismatch,
    #[error("hook-session binding has expired")]
    Expired,
    #[error("hook-session binding is sealed")]
    Sealed,
    #[error("hook-session binding is revoked")]
    Revoked,
    #[error("hook-session delivery is outside the allowed order window")]
    OrderWindowExceeded,
    #[error("hook-session delivery order conflicts with an admitted delivery")]
    OrderViolation,
    #[error("hook-session delivery replay changed normalized content")]
    ReplayViolation,
    #[error("hook-session receipt capacity is exhausted")]
    ReceiptCapacityExhausted,
}

/// In-memory, daemon-boot authority. Capabilities from another instance fail.
pub struct HookSessionAuthority {
    config: HookSessionConfig,
    cryptography: HookSessionCryptography,
    epoch: HookDaemonEpoch,
    bindings: HashMap<HookBindingId, BindingRecord>,
    session_keys: HashMap<SessionKey, HookBindingId>,
    generations: HashMap<SessionKey, u64>,
}

impl fmt::Debug for HookSessionAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HookSessionAuthority")
            .field("config", &self.config)
            .field("cryptography", &"<redacted>")
            .field("epoch", &"<redacted>")
            .field("binding_count", &self.bindings.len())
            .finish()
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct SessionKey([u8; HASH_BYTES]);

struct BindingRecord {
    session_key: SessionKey,
    verifier: [u8; HASH_BYTES],
    integration: HookIntegrationId,
    checkout: HookCheckoutIdentity,
    internal_session_id: HookInternalSessionId,
    generation: u64,
    state: HookBindingState,
    created_at: Instant,
    last_seen_at: Instant,
    idle_deadline: Instant,
    absolute_deadline: Instant,
    sealed_at: Option<Instant>,
    next_sequence: u64,
    closing_sequence: Option<u64>,
    pending: BTreeMap<u64, PendingDelivery>,
    receipts: HashMap<HookDeliveryId, DeliveryReceipt>,
    sequence_receipts: HashMap<u64, HookDeliveryId>,
}

#[derive(Clone, Copy)]
struct PendingDelivery {
    delivery_id: HookDeliveryId,
    kind: HookDeliveryKind,
}

#[derive(Clone, Copy)]
struct DeliveryReceipt {
    sequence: u64,
    normalized_hash: NormalizedEventHash,
    kind: HookDeliveryKind,
    status: HookDeliveryStatus,
}

impl HookSessionAuthority {
    pub fn new(config: HookSessionConfig) -> Result<Self, HookSessionError> {
        let config = config.validate()?;
        let mut secret = [0_u8; TOKEN_BYTES];
        let mut epoch = [0_u8; ID_BYTES];
        secure_random(&mut secret)?;
        secure_random(&mut epoch)?;
        Ok(Self::from_material(config, secret, epoch))
    }

    fn from_material(
        config: HookSessionConfig,
        secret: [u8; TOKEN_BYTES],
        epoch: [u8; ID_BYTES],
    ) -> Self {
        Self {
            config,
            cryptography: HookSessionCryptography::from_secret(secret),
            epoch: HookDaemonEpoch(epoch),
            bindings: HashMap::new(),
            session_keys: HashMap::new(),
            generations: HashMap::new(),
        }
    }

    pub fn daemon_epoch(&self) -> HookDaemonEpoch {
        self.epoch
    }

    /// Mint a new capability. An existing open tuple must resume by presenting
    /// its capability; it is never silently replaced or reconstructed.
    pub fn mint(
        &mut self,
        request: HookSessionOpenRequest,
    ) -> Result<MintedHookSession, HookSessionError> {
        self.mint_at(request, Instant::now())
    }

    pub fn resume(
        &mut self,
        request: HookSessionResumeRequest,
    ) -> Result<ResumedHookSession, HookSessionError> {
        self.resume_at(request, Instant::now())
    }

    pub fn verify(
        &mut self,
        request: &HookSessionVerifyRequest,
    ) -> Result<HookSessionVerification, HookSessionError> {
        self.verify_at(request, Instant::now(), false)
    }

    pub fn consume(
        &mut self,
        request: HookEventAdmission,
    ) -> Result<HookAdmissionOutcome, HookSessionError> {
        self.admit(
            request.authority,
            request.delivery_id,
            request.sequence,
            request.normalized_hash,
            HookDeliveryKind::Event,
            Instant::now(),
        )
    }

    pub fn seal(
        &mut self,
        request: HookSessionSealRequest,
    ) -> Result<HookAdmissionOutcome, HookSessionError> {
        self.admit(
            request.authority,
            request.delivery_id,
            request.sequence,
            request.normalized_hash,
            HookDeliveryKind::Close,
            Instant::now(),
        )
    }

    pub fn revoke(&mut self, binding_id: HookBindingId) -> Result<(), HookSessionError> {
        let binding = self
            .bindings
            .get_mut(&binding_id)
            .ok_or(HookSessionError::BindingNotFound)?;
        binding.state = HookBindingState::Revoked;
        binding.pending.clear();
        Ok(())
    }

    pub fn state(
        &mut self,
        binding_id: HookBindingId,
    ) -> Result<HookBindingState, HookSessionError> {
        let now = Instant::now();
        let binding = self
            .bindings
            .get_mut(&binding_id)
            .ok_or(HookSessionError::BindingNotFound)?;
        expire_if_needed(binding, now);
        Ok(binding.state)
    }

    pub fn timing(
        &mut self,
        binding_id: HookBindingId,
    ) -> Result<HookBindingTiming, HookSessionError> {
        let now = Instant::now();
        let binding = self
            .bindings
            .get_mut(&binding_id)
            .ok_or(HookSessionError::BindingNotFound)?;
        expire_if_needed(binding, now);
        Ok(HookBindingTiming {
            age: now.saturating_duration_since(binding.created_at),
            idle_age: now.saturating_duration_since(binding.last_seen_at),
            idle_remaining: binding.idle_deadline.saturating_duration_since(now),
            absolute_remaining: binding.absolute_deadline.saturating_duration_since(now),
        })
    }

    fn mint_at(
        &mut self,
        request: HookSessionOpenRequest,
        now: Instant,
    ) -> Result<MintedHookSession, HookSessionError> {
        let session_key = SessionKey(derive_session_key(
            &self.cryptography.secret,
            &request.integration,
            &request.host_session_id,
            &request.checkout,
        ));
        if let Some(existing_id) = self.session_keys.get(&session_key).copied() {
            if let Some(existing) = self.bindings.get_mut(&existing_id) {
                expire_if_needed(existing, now);
                if existing.state == HookBindingState::Open {
                    return Err(HookSessionError::BindingAlreadyOpen);
                }
            }
        }

        let generation = self
            .generations
            .get(&session_key)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(HookSessionError::InvalidConfiguration)?;
        let prepared = self.cryptography.prepare_binding(
            &request.integration,
            &request.host_session_id,
            &request.checkout,
        )?;
        let binding_id = prepared.binding_id;
        let capability = prepared.capability;
        let internal_session_id = prepared.internal_session_id;
        let verifier = *prepared.capability_verifier.as_bytes();
        let idle_deadline = now + self.config.idle_ttl;
        let absolute_deadline = now + self.config.absolute_ttl;

        self.bindings.insert(
            binding_id,
            BindingRecord {
                session_key,
                verifier,
                integration: request.integration,
                checkout: request.checkout,
                internal_session_id,
                generation,
                state: HookBindingState::Open,
                created_at: now,
                last_seen_at: now,
                idle_deadline,
                absolute_deadline,
                sealed_at: None,
                next_sequence: 1,
                closing_sequence: None,
                pending: BTreeMap::new(),
                receipts: HashMap::new(),
                sequence_receipts: HashMap::new(),
            },
        );
        self.session_keys.insert(session_key, binding_id);
        self.generations.insert(session_key, generation);

        Ok(MintedHookSession {
            binding_id,
            capability,
            internal_session_id,
            daemon_epoch: self.epoch,
            generation,
            idle_ttl: self.config.idle_ttl,
            absolute_ttl: self.config.absolute_ttl,
        })
    }

    fn resume_at(
        &mut self,
        request: HookSessionResumeRequest,
        now: Instant,
    ) -> Result<ResumedHookSession, HookSessionError> {
        let expected_key = SessionKey(derive_session_key(
            &self.cryptography.secret,
            &request.integration,
            &request.host_session_id,
            &request.current_checkout,
        ));
        let verify_request = HookSessionVerifyRequest {
            binding_id: request.binding_id,
            capability: request.capability,
            integration: request.integration,
            current_checkout: request.current_checkout,
        };
        let verification = self.verify_at(&verify_request, now, false)?;
        let binding = self
            .bindings
            .get_mut(&verify_request.binding_id)
            .expect("verified binding exists");
        if !constant_time_eq(&binding.session_key.0, &expected_key.0) {
            return Err(HookSessionError::InvalidCapability);
        }
        binding.last_seen_at = now;
        binding.idle_deadline = (now + self.config.idle_ttl).min(binding.absolute_deadline);
        Ok(ResumedHookSession {
            verification,
            idle_ttl: binding.idle_deadline.saturating_duration_since(now),
        })
    }

    fn verify_at(
        &mut self,
        request: &HookSessionVerifyRequest,
        now: Instant,
        allow_sealed_close_retry: bool,
    ) -> Result<HookSessionVerification, HookSessionError> {
        let binding = self
            .bindings
            .get_mut(&request.binding_id)
            .ok_or(HookSessionError::BindingNotFound)?;

        let presented = self
            .cryptography
            .capability_verifier(&request.binding_id, &request.capability);
        if !constant_time_eq(&binding.verifier, presented.as_bytes()) {
            return Err(HookSessionError::InvalidCapability);
        }
        if binding.integration != request.integration {
            return Err(HookSessionError::IntegrationMismatch);
        }
        if binding.checkout.repository_id != request.current_checkout.repository_id {
            return Err(HookSessionError::RepositoryMismatch);
        }
        if binding.checkout.checkout_id != request.current_checkout.checkout_id {
            return Err(HookSessionError::CheckoutMismatch);
        }

        expire_if_needed(binding, now);
        match binding.state {
            HookBindingState::Open => {}
            HookBindingState::Sealed if allow_sealed_close_retry => {
                let within_grace = binding
                    .sealed_at
                    .and_then(|sealed| sealed.checked_add(self.config.close_retry_grace))
                    .is_some_and(|deadline| now <= deadline);
                if !within_grace {
                    return Err(HookSessionError::Sealed);
                }
            }
            HookBindingState::Sealed => return Err(HookSessionError::Sealed),
            HookBindingState::Expired => return Err(HookSessionError::Expired),
            HookBindingState::Revoked => return Err(HookSessionError::Revoked),
        }

        Ok(HookSessionVerification {
            binding_id: request.binding_id,
            internal_session_id: binding.internal_session_id,
            integration: binding.integration.clone(),
            checkout: binding.checkout.clone(),
            generation: binding.generation,
        })
    }

    fn admit(
        &mut self,
        authority: HookSessionVerifyRequest,
        delivery_id: HookDeliveryId,
        sequence: u64,
        normalized_hash: NormalizedEventHash,
        kind: HookDeliveryKind,
        now: Instant,
    ) -> Result<HookAdmissionOutcome, HookSessionError> {
        // A sealed binding is entered only for a byte-identical close retry.
        let allow_sealed = kind == HookDeliveryKind::Close;
        self.verify_at(&authority, now, allow_sealed)?;
        let binding = self
            .bindings
            .get_mut(&authority.binding_id)
            .expect("verified binding exists");

        if let Some(receipt) = binding.receipts.get(&delivery_id).copied() {
            if !constant_time_eq(
                receipt.normalized_hash.as_bytes(),
                normalized_hash.as_bytes(),
            ) || receipt.sequence != sequence
                || receipt.kind != kind
            {
                binding.state = HookBindingState::Revoked;
                binding.pending.clear();
                return Err(HookSessionError::ReplayViolation);
            }
            if binding.state == HookBindingState::Sealed && receipt.kind != HookDeliveryKind::Close
            {
                return Err(HookSessionError::Sealed);
            }
            if binding.state == HookBindingState::Open {
                renew_idle(binding, now, self.config.idle_ttl);
            }
            return Ok(HookAdmissionOutcome {
                status: receipt.status,
                idempotent_replay: true,
                next_sequence: binding.next_sequence,
                newly_reducible: Vec::new(),
            });
        }

        if binding.state == HookBindingState::Sealed {
            return Err(HookSessionError::Sealed);
        }
        if sequence == 0 || sequence < binding.next_sequence {
            return Err(HookSessionError::OrderViolation);
        }
        if sequence.saturating_sub(binding.next_sequence) > self.config.reorder_window {
            return Err(HookSessionError::OrderWindowExceeded);
        }
        if let Some(close_sequence) = binding.closing_sequence {
            if sequence >= close_sequence {
                return Err(HookSessionError::Sealed);
            }
        }
        if kind == HookDeliveryKind::Close
            && binding
                .pending
                .keys()
                .next_back()
                .is_some_and(|pending| *pending > sequence)
        {
            return Err(HookSessionError::OrderViolation);
        }
        // Reserve the ability to close a full session. A close may consume one
        // final receipt beyond the ordinary event ceiling, but never more.
        if binding.receipts.len() >= self.config.max_receipts_per_binding
            && kind != HookDeliveryKind::Close
        {
            return Err(HookSessionError::ReceiptCapacityExhausted);
        }
        if binding.sequence_receipts.contains_key(&sequence) {
            binding.state = HookBindingState::Revoked;
            binding.pending.clear();
            return Err(HookSessionError::OrderViolation);
        }

        if kind == HookDeliveryKind::Close {
            binding.closing_sequence = Some(sequence);
        }
        binding
            .pending
            .insert(sequence, PendingDelivery { delivery_id, kind });
        binding.sequence_receipts.insert(sequence, delivery_id);
        binding.receipts.insert(
            delivery_id,
            DeliveryReceipt {
                sequence,
                normalized_hash,
                kind,
                status: HookDeliveryStatus::Buffered,
            },
        );
        renew_idle(binding, now, self.config.idle_ttl);

        let mut newly_reducible = Vec::new();
        while let Some(pending) = binding.pending.remove(&binding.next_sequence) {
            let reducible = ReducibleHookDelivery {
                delivery_id: pending.delivery_id,
                sequence: binding.next_sequence,
                kind: pending.kind,
            };
            binding.next_sequence = binding.next_sequence.saturating_add(1);
            let status = if pending.kind == HookDeliveryKind::Close {
                HookDeliveryStatus::Sealed
            } else {
                HookDeliveryStatus::Reduced
            };
            if let Some(receipt) = binding.receipts.get_mut(&pending.delivery_id) {
                receipt.status = status;
            }
            newly_reducible.push(reducible);
            if pending.kind == HookDeliveryKind::Close {
                binding.state = HookBindingState::Sealed;
                binding.sealed_at = Some(now);
                binding.pending.clear();
                break;
            }
        }

        let status = binding
            .receipts
            .get(&delivery_id)
            .expect("new receipt exists")
            .status;
        Ok(HookAdmissionOutcome {
            status,
            idempotent_replay: false,
            next_sequence: binding.next_sequence,
            newly_reducible,
        })
    }
}

fn validate_bounded_identity(value: &str, maximum: usize) -> Result<(), HookSessionError> {
    if value.is_empty()
        || value.len() > maximum
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(HookSessionError::InvalidIdentity);
    }
    Ok(())
}

fn expire_if_needed(binding: &mut BindingRecord, now: Instant) {
    if binding.state == HookBindingState::Open
        && (now >= binding.idle_deadline || now >= binding.absolute_deadline)
    {
        binding.state = HookBindingState::Expired;
        binding.pending.clear();
    }
}

fn renew_idle(binding: &mut BindingRecord, now: Instant, idle_ttl: Duration) {
    binding.last_seen_at = now;
    binding.idle_deadline = (now + idle_ttl).min(binding.absolute_deadline);
}

fn derive_session_key(
    secret: &[u8; TOKEN_BYTES],
    integration: &HookIntegrationId,
    host_session_id: &HostSessionId,
    checkout: &HookCheckoutIdentity,
) -> [u8; HASH_BYTES] {
    let mut message = Vec::with_capacity(
        64 + integration.0.len()
            + host_session_id.0.len()
            + checkout.repository_id.len()
            + checkout.checkout_id.len(),
    );
    append_field(&mut message, b"lattice-hook-session-key-v1");
    append_field(&mut message, integration.0.as_bytes());
    append_field(&mut message, host_session_id.0.as_bytes());
    append_field(&mut message, checkout.repository_id.as_bytes());
    append_field(&mut message, checkout.checkout_id.as_bytes());
    hmac_sha256(secret, &message)
}

fn capability_verifier(
    secret: &[u8; TOKEN_BYTES],
    binding_id: &HookBindingId,
    capability: &HookSessionCapability,
) -> [u8; HASH_BYTES] {
    let mut message = Vec::with_capacity(96);
    append_field(&mut message, b"lattice-hook-capability-v1");
    append_field(&mut message, binding_id.as_bytes());
    append_field(&mut message, capability.as_bytes());
    hmac_sha256(secret, &message)
}

fn append_field(target: &mut Vec<u8>, field: &[u8]) {
    target.extend_from_slice(&(field.len() as u64).to_be_bytes());
    target.extend_from_slice(field);
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (&left, &right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(unix)]
fn secure_random(bytes: &mut [u8]) -> Result<(), HookSessionError> {
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(bytes))
        .map_err(|_| HookSessionError::EntropyUnavailable)
}

#[cfg(not(unix))]
fn secure_random(_bytes: &mut [u8]) -> Result<(), HookSessionError> {
    Err(HookSessionError::EntropyUnavailable)
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; HASH_BYTES] {
    const BLOCK: usize = 64;
    let mut normalized_key = [0_u8; BLOCK];
    if key.len() > BLOCK {
        normalized_key[..HASH_BYTES].copy_from_slice(&sha256(key));
    } else {
        normalized_key[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; BLOCK];
    let mut outer_pad = [0x5c_u8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= normalized_key[index];
        outer_pad[index] ^= normalized_key[index];
    }
    let mut inner = Vec::with_capacity(BLOCK + message.len());
    inner.extend_from_slice(&inner_pad);
    inner.extend_from_slice(message);
    let inner_hash = sha256(&inner);
    let mut outer = Vec::with_capacity(BLOCK + HASH_BYTES);
    outer.extend_from_slice(&outer_pad);
    outer.extend_from_slice(&inner_hash);
    sha256(&outer)
}

fn sha256(input: &[u8]) -> [u8; HASH_BYTES] {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const ROUND: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(input.len() + 72);
    padded.extend_from_slice(input);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = INITIAL;
    for chunk in padded.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (index, bytes) in chunk.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes(bytes.try_into().expect("four-byte chunk"));
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(sum1)
                .wrapping_add(choice)
                .wrapping_add(ROUND[index])
                .wrapping_add(words[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sum0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (current, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *current = current.wrapping_add(value);
        }
    }

    let mut digest = [0_u8; HASH_BYTES];
    for (target, word) in digest.chunks_exact_mut(4).zip(state) {
        target.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authority() -> HookSessionAuthority {
        HookSessionAuthority::from_material(
            HookSessionConfig::default(),
            [0x11; TOKEN_BYTES],
            [0x22; ID_BYTES],
        )
    }

    fn checkout(repository: &str, checkout: &str) -> HookCheckoutIdentity {
        HookCheckoutIdentity::new(repository, checkout).unwrap()
    }

    fn open(authority: &mut HookSessionAuthority) -> MintedHookSession {
        authority
            .mint_at(
                HookSessionOpenRequest {
                    integration: HookIntegrationId::new("codex/v1").unwrap(),
                    host_session_id: HostSessionId::new("opaque-host-session").unwrap(),
                    checkout: checkout("repo-A", "checkout-A"),
                },
                Instant::now(),
            )
            .unwrap()
    }

    fn verify_request(minted: &MintedHookSession) -> HookSessionVerifyRequest {
        HookSessionVerifyRequest {
            binding_id: minted.binding_id,
            capability: minted.capability,
            integration: HookIntegrationId::new("codex/v1").unwrap(),
            current_checkout: checkout("repo-A", "checkout-A"),
        }
    }

    fn hash(byte: u8) -> NormalizedEventHash {
        NormalizedEventHash::from_bytes([byte; HASH_BYTES])
    }

    fn delivery(byte: u8) -> HookDeliveryId {
        HookDeliveryId::from_bytes([byte; ID_BYTES])
    }

    #[test]
    fn sha256_and_hmac_match_standard_vectors() {
        assert_eq!(
            sha256(b"abc"),
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
        assert_eq!(
            hmac_sha256(b"key", b"The quick brown fox jumps over the lazy dog"),
            [
                0xf7, 0xbc, 0x83, 0xf4, 0x30, 0x53, 0x84, 0x24, 0xb1, 0x32, 0x98, 0xe6, 0xaa, 0x6f,
                0xb1, 0x43, 0xef, 0x4d, 0x59, 0xa1, 0x49, 0x46, 0x17, 0x59, 0x97, 0x47, 0x9d, 0xbc,
                0x2d, 0x1a, 0x3c, 0xd8,
            ]
        );
    }

    #[test]
    fn in_memory_authority_retains_only_verifier_and_not_cross_instance_state() {
        let mut first = authority();
        let minted = open(&mut first);
        first.verify(&verify_request(&minted)).unwrap();

        let mut second = HookSessionAuthority::from_material(
            HookSessionConfig::default(),
            [0x33; TOKEN_BYTES],
            [0x44; ID_BYTES],
        );
        assert_eq!(
            second.verify(&verify_request(&minted)),
            Err(HookSessionError::BindingNotFound)
        );
        assert_ne!(
            first.bindings[&minted.binding_id].verifier,
            *minted.capability.as_bytes()
        );
    }

    #[test]
    fn durable_cryptography_prepares_and_verifies_one_tuple_without_epoch_state() {
        let first = HookSessionCryptography::from_secret([0x61; TOKEN_BYTES]);
        let integration = HookIntegrationId::new("codex/v1").unwrap();
        let host = HostSessionId::new("host-session").unwrap();
        let checkout = checkout("repo-A", "checkout-A");
        let prepared = first
            .prepare_binding(&integration, &host, &checkout)
            .unwrap();
        assert!(first.verify_capability(
            &prepared.binding_id,
            &prepared.capability,
            &prepared.capability_verifier
        ));

        // A daemon restart reconstructs the same verifier authority from its
        // protected persistent secret; no boot epoch participates.
        let restarted = HookSessionCryptography::from_secret([0x61; TOKEN_BYTES]);
        assert!(restarted.verify_capability(
            &prepared.binding_id,
            &prepared.capability,
            &prepared.capability_verifier
        ));
        assert_eq!(
            restarted.authority_fingerprint(&integration, &host, &checkout),
            prepared.authority_fingerprint
        );

        let wrong_secret = HookSessionCryptography::from_secret([0x62; TOKEN_BYTES]);
        assert!(!wrong_secret.verify_capability(
            &prepared.binding_id,
            &prepared.capability,
            &prepared.capability_verifier
        ));
        assert_ne!(
            restarted.authority_fingerprint(
                &integration,
                &HostSessionId::new("other-host-session").unwrap(),
                &checkout,
            ),
            prepared.authority_fingerprint
        );
    }

    #[test]
    fn exact_repository_checkout_and_integration_are_enforced() {
        let mut authority = authority();
        let minted = open(&mut authority);
        let mut wrong = verify_request(&minted);
        wrong.integration = HookIntegrationId::new("claude/v1").unwrap();
        assert_eq!(
            authority.verify(&wrong),
            Err(HookSessionError::IntegrationMismatch)
        );

        let mut wrong = verify_request(&minted);
        wrong.current_checkout = checkout("repo-B", "checkout-A");
        assert_eq!(
            authority.verify(&wrong),
            Err(HookSessionError::RepositoryMismatch)
        );

        let mut wrong = verify_request(&minted);
        wrong.current_checkout = checkout("repo-A", "sibling-worktree");
        assert_eq!(
            authority.verify(&wrong),
            Err(HookSessionError::CheckoutMismatch)
        );
    }

    #[test]
    fn wrong_capability_is_rejected_without_secret_in_errors_or_debug() {
        let mut authority = authority();
        let minted = open(&mut authority);
        let mut request = verify_request(&minted);
        request.capability = HookSessionCapability::from_bytes([0xfe; TOKEN_BYTES]);
        let error = authority.verify(&request).unwrap_err();
        assert_eq!(error, HookSessionError::InvalidCapability);
        assert_eq!(error.to_string(), "hook-session capability is invalid");
        assert!(!format!("{request:?}").contains("fefefe"));
        assert!(!format!("{minted:?}").contains("111111"));
    }

    #[test]
    fn resume_requires_host_tuple_and_renews_only_idle_deadline() {
        let mut authority = authority();
        let minted = open(&mut authority);
        let absolute = authority.bindings[&minted.binding_id].absolute_deadline;
        let created = authority.bindings[&minted.binding_id].created_at;
        let now = created + Duration::from_secs(60);
        authority
            .resume_at(
                HookSessionResumeRequest {
                    binding_id: minted.binding_id,
                    capability: minted.capability,
                    integration: HookIntegrationId::new("codex/v1").unwrap(),
                    host_session_id: HostSessionId::new("opaque-host-session").unwrap(),
                    current_checkout: checkout("repo-A", "checkout-A"),
                },
                now,
            )
            .unwrap();
        let record = &authority.bindings[&minted.binding_id];
        assert_eq!(record.absolute_deadline, absolute);
        assert_eq!(record.idle_deadline, now + authority.config.idle_ttl);

        let error = authority
            .resume_at(
                HookSessionResumeRequest {
                    binding_id: minted.binding_id,
                    capability: minted.capability,
                    integration: HookIntegrationId::new("codex/v1").unwrap(),
                    host_session_id: HostSessionId::new("forged-host-session").unwrap(),
                    current_checkout: checkout("repo-A", "checkout-A"),
                },
                now,
            )
            .unwrap_err();
        assert_eq!(error, HookSessionError::InvalidCapability);
    }

    #[test]
    fn out_of_order_events_reduce_in_sequence_and_gaps_do_not_invent_events() {
        let mut authority = authority();
        let minted = open(&mut authority);
        let now = Instant::now();
        let buffered = authority
            .admit(
                verify_request(&minted),
                delivery(2),
                2,
                hash(2),
                HookDeliveryKind::Event,
                now,
            )
            .unwrap();
        assert_eq!(buffered.status, HookDeliveryStatus::Buffered);
        assert!(buffered.newly_reducible.is_empty());
        assert_eq!(buffered.next_sequence, 1);

        let reduced = authority
            .admit(
                verify_request(&minted),
                delivery(1),
                1,
                hash(1),
                HookDeliveryKind::Event,
                now,
            )
            .unwrap();
        assert_eq!(reduced.status, HookDeliveryStatus::Reduced);
        assert_eq!(
            reduced
                .newly_reducible
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(reduced.next_sequence, 3);
    }

    #[test]
    fn identical_delivery_is_idempotent_and_changed_replay_revokes() {
        let mut authority = authority();
        let minted = open(&mut authority);
        let now = Instant::now();
        authority
            .admit(
                verify_request(&minted),
                delivery(1),
                1,
                hash(1),
                HookDeliveryKind::Event,
                now,
            )
            .unwrap();
        let replay = authority
            .admit(
                verify_request(&minted),
                delivery(1),
                1,
                hash(1),
                HookDeliveryKind::Event,
                now,
            )
            .unwrap();
        assert!(replay.idempotent_replay);
        assert!(replay.newly_reducible.is_empty());

        assert_eq!(
            authority.admit(
                verify_request(&minted),
                delivery(1),
                1,
                hash(9),
                HookDeliveryKind::Event,
                now,
            ),
            Err(HookSessionError::ReplayViolation)
        );
        assert_eq!(
            authority.state(minted.binding_id).unwrap(),
            HookBindingState::Revoked
        );
    }

    #[test]
    fn close_waits_for_gap_then_seals_and_only_exact_close_can_retry() {
        let mut authority = authority();
        let minted = open(&mut authority);
        let now = Instant::now();
        let close = authority
            .admit(
                verify_request(&minted),
                delivery(3),
                3,
                hash(3),
                HookDeliveryKind::Close,
                now,
            )
            .unwrap();
        assert_eq!(close.status, HookDeliveryStatus::Buffered);
        authority
            .admit(
                verify_request(&minted),
                delivery(1),
                1,
                hash(1),
                HookDeliveryKind::Event,
                now,
            )
            .unwrap();
        let sealing = authority
            .admit(
                verify_request(&minted),
                delivery(2),
                2,
                hash(2),
                HookDeliveryKind::Event,
                now,
            )
            .unwrap();
        assert_eq!(
            sealing
                .newly_reducible
                .iter()
                .map(|event| (event.sequence, event.kind))
                .collect::<Vec<_>>(),
            vec![(2, HookDeliveryKind::Event), (3, HookDeliveryKind::Close)]
        );
        assert_eq!(
            authority.state(minted.binding_id).unwrap(),
            HookBindingState::Sealed
        );

        let retry = authority
            .admit(
                verify_request(&minted),
                delivery(3),
                3,
                hash(3),
                HookDeliveryKind::Close,
                now,
            )
            .unwrap();
        assert!(retry.idempotent_replay);
        assert_eq!(retry.status, HookDeliveryStatus::Sealed);
        assert_eq!(
            authority.admit(
                verify_request(&minted),
                delivery(4),
                4,
                hash(4),
                HookDeliveryKind::Event,
                now,
            ),
            Err(HookSessionError::Sealed)
        );
    }

    #[test]
    fn idle_and_absolute_expiry_fail_closed_without_sealing() {
        let config = HookSessionConfig {
            idle_ttl: Duration::from_secs(10),
            absolute_ttl: Duration::from_secs(20),
            ..HookSessionConfig::default()
        };
        let mut authority =
            HookSessionAuthority::from_material(config, [0x11; TOKEN_BYTES], [0x22; ID_BYTES]);
        let minted = open(&mut authority);
        let created = authority.bindings[&minted.binding_id].created_at;
        assert_eq!(
            authority.verify_at(
                &verify_request(&minted),
                created + Duration::from_secs(11),
                false,
            ),
            Err(HookSessionError::Expired)
        );
        assert_eq!(
            authority.bindings[&minted.binding_id].state,
            HookBindingState::Expired
        );
        assert!(authority.bindings[&minted.binding_id].sealed_at.is_none());

        let mut authority =
            HookSessionAuthority::from_material(config, [0x11; TOKEN_BYTES], [0x22; ID_BYTES]);
        let minted = open(&mut authority);
        let created = authority.bindings[&minted.binding_id].created_at;
        // Traffic can keep moving the idle deadline, but not the absolute one.
        authority
            .admit(
                verify_request(&minted),
                delivery(1),
                1,
                hash(1),
                HookDeliveryKind::Event,
                created + Duration::from_secs(9),
            )
            .unwrap();
        authority
            .admit(
                verify_request(&minted),
                delivery(2),
                2,
                hash(2),
                HookDeliveryKind::Event,
                created + Duration::from_secs(18),
            )
            .unwrap();
        assert_eq!(
            authority.verify_at(
                &verify_request(&minted),
                created + Duration::from_secs(20),
                false,
            ),
            Err(HookSessionError::Expired)
        );
    }

    #[test]
    fn finite_limits_and_identity_validation_are_enforced() {
        assert_eq!(
            HookSessionAuthority::new(HookSessionConfig {
                reorder_window: MAX_REORDER_WINDOW + 1,
                ..HookSessionConfig::default()
            })
            .unwrap_err(),
            HookSessionError::InvalidConfiguration
        );
        assert_eq!(
            HookIntegrationId::new("bad\nidentity"),
            Err(HookSessionError::InvalidIdentity)
        );

        let mut authority = authority();
        let minted = open(&mut authority);
        assert_eq!(
            authority.admit(
                verify_request(&minted),
                delivery(1),
                authority.config.reorder_window + 2,
                hash(1),
                HookDeliveryKind::Event,
                Instant::now(),
            ),
            Err(HookSessionError::OrderWindowExceeded)
        );
    }

    #[test]
    fn completed_tuple_gets_new_generation_and_capability() {
        let mut authority = authority();
        let first = open(&mut authority);
        authority
            .admit(
                verify_request(&first),
                delivery(1),
                1,
                hash(1),
                HookDeliveryKind::Close,
                Instant::now(),
            )
            .unwrap();
        let second = open(&mut authority);
        assert_eq!(second.generation, 2);
        assert_ne!(first.binding_id, second.binding_id);
        assert_ne!(first.capability, second.capability);
    }

    #[test]
    fn public_consume_and_seal_api_returns_only_reduction_metadata() {
        let mut authority = authority();
        let minted = open(&mut authority);
        let event = authority
            .consume(HookEventAdmission {
                authority: verify_request(&minted),
                delivery_id: delivery(1),
                sequence: 1,
                normalized_hash: hash(1),
            })
            .unwrap();
        assert_eq!(event.status, HookDeliveryStatus::Reduced);
        assert_eq!(event.newly_reducible.len(), 1);

        let close = authority
            .seal(HookSessionSealRequest {
                authority: verify_request(&minted),
                delivery_id: delivery(2),
                sequence: 2,
                normalized_hash: hash(2),
            })
            .unwrap();
        assert_eq!(close.status, HookDeliveryStatus::Sealed);
        assert_eq!(close.newly_reducible.len(), 1);
        assert_eq!(close.newly_reducible[0].kind, HookDeliveryKind::Close);
    }

    #[test]
    fn public_surface_has_no_raw_host_fields() {
        let source = include_str!("hook_session_binding.rs");
        // This single module-level prose occurrence documents the prohibition;
        // no identifier or serialized field contains either forbidden shape.
        assert!(!source.contains(&["transcript", "_path"].concat()));
        assert!(!source.contains(&["event", "_text"].concat()));
    }
}
