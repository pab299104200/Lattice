//! Private, payload-bounded persistence for short-lived hook adapters.
//!
//! This is deliberately a local persistence primitive, not a transport client.
//! It knows how to protect a daemon-minted capability and allocate/replay
//! sanitized deliveries, but has no RPC method names, socket knowledge, or host
//! hook-envelope types.  In particular, the only queued content is the already
//! normalized `SessionCaptureEvent` or `SessionCaptureClose` value.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use lattice_core::memory::{
    CheckOutcome, ErrorStatus, SessionCaptureClose, SessionCaptureEvent, SessionCaptureFact,
};
use lattice_core::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::hook_session_binding::{
    HookCheckoutIdentity, HookIntegrationId, HookSessionCryptography, HostSessionId,
};

const CAPABILITY_SCHEMA_VERSION: u32 = 1;
const PENDING_SCHEMA_VERSION: u32 = 1;
const KEY_BYTES: usize = 32;
const DELIVERY_ID_BYTES: usize = 16;
const MAX_OPAQUE_ID_BYTES: usize = 4096;
const MAX_PENDING_BYTES: usize = 256 * 1024;
const MAX_PENDING_COUNT: usize = 256;
const MAX_PENDING_AGE_MS: i64 = 24 * 60 * 60 * 1_000;
const DEFAULT_CLOSE_RETRY_GRACE_MS: i64 = 5 * 60 * 1_000;

/// Bounded opaque value returned by the daemon.  It is never rendered by
/// `Debug`, including when an adapter reports a local persistence failure.
#[derive(Clone, Eq, PartialEq)]
pub struct HookClientOpaqueId(String);

impl HookClientOpaqueId {
    pub fn new(value: impl Into<String>) -> Result<Self, HookSessionClientError> {
        let value = value.into();
        validate_opaque(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for HookClientOpaqueId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HookClientOpaqueId(<redacted>)")
    }
}

/// The local lookup tuple.  Its host-session value is used solely while
/// deriving an HMAC filename and is never serialized.
#[derive(Clone, Eq, PartialEq)]
pub struct HookClientBindingKey {
    integration: String,
    host_session_id: String,
    repository_id: String,
    checkout_id: String,
}

impl HookClientBindingKey {
    pub fn new(
        integration: impl Into<String>,
        host_session_id: impl Into<String>,
        repository_id: impl Into<String>,
        checkout_id: impl Into<String>,
    ) -> Result<Self, HookSessionClientError> {
        let integration = integration.into();
        let host_session_id = host_session_id.into();
        let repository_id = repository_id.into();
        let checkout_id = checkout_id.into();
        HookIntegrationId::new(integration.clone())
            .map_err(|_| HookSessionClientError::InvalidInput)?;
        HostSessionId::new(host_session_id.clone())
            .map_err(|_| HookSessionClientError::InvalidInput)?;
        HookCheckoutIdentity::new(repository_id.clone(), checkout_id.clone())
            .map_err(|_| HookSessionClientError::InvalidInput)?;
        Ok(Self {
            integration,
            host_session_id,
            repository_id,
            checkout_id,
        })
    }

    fn fingerprint(
        &self,
        cryptography: &HookSessionCryptography,
    ) -> Result<String, HookSessionClientError> {
        let integration = HookIntegrationId::new(self.integration.clone())
            .map_err(|_| HookSessionClientError::InvalidInput)?;
        let host_session_id = HostSessionId::new(self.host_session_id.clone())
            .map_err(|_| HookSessionClientError::InvalidInput)?;
        let checkout =
            HookCheckoutIdentity::new(self.repository_id.clone(), self.checkout_id.clone())
                .map_err(|_| HookSessionClientError::InvalidInput)?;
        Ok(hex(cryptography
            .authority_fingerprint(&integration, &host_session_id, &checkout)
            .as_bytes()))
    }
}

impl fmt::Debug for HookClientBindingKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HookClientBindingKey(<redacted>)")
    }
}

/// Capability and daemon-independent expiry data returned by session open.
#[derive(Clone, Eq, PartialEq)]
pub struct HookClientBinding {
    binding_id: HookClientOpaqueId,
    capability: HookClientOpaqueId,
    integration: String,
    repository_id: String,
    checkout_id: String,
    idle_deadline_ms: i64,
    absolute_deadline_ms: i64,
}

impl HookClientBinding {
    pub fn new(
        binding_id: HookClientOpaqueId,
        capability: HookClientOpaqueId,
        integration: impl Into<String>,
        repository_id: impl Into<String>,
        checkout_id: impl Into<String>,
        idle_deadline_ms: i64,
        absolute_deadline_ms: i64,
    ) -> Result<Self, HookSessionClientError> {
        let integration = integration.into();
        let repository_id = repository_id.into();
        let checkout_id = checkout_id.into();
        HookIntegrationId::new(integration.clone())
            .map_err(|_| HookSessionClientError::InvalidInput)?;
        HookCheckoutIdentity::new(repository_id.clone(), checkout_id.clone())
            .map_err(|_| HookSessionClientError::InvalidInput)?;
        if idle_deadline_ms < 0 || absolute_deadline_ms < idle_deadline_ms {
            return Err(HookSessionClientError::InvalidInput);
        }
        Ok(Self {
            binding_id,
            capability,
            integration,
            repository_id,
            checkout_id,
            idle_deadline_ms,
            absolute_deadline_ms,
        })
    }

    pub fn binding_id(&self) -> &HookClientOpaqueId {
        &self.binding_id
    }
    pub fn capability(&self) -> &HookClientOpaqueId {
        &self.capability
    }
    pub fn integration(&self) -> &str {
        &self.integration
    }
    pub fn repository_id(&self) -> &str {
        &self.repository_id
    }
    pub fn checkout_id(&self) -> &str {
        &self.checkout_id
    }
    pub fn idle_deadline_ms(&self) -> i64 {
        self.idle_deadline_ms
    }
    pub fn absolute_deadline_ms(&self) -> i64 {
        self.absolute_deadline_ms
    }
}

impl fmt::Debug for HookClientBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HookClientBinding")
            .field("binding_id", &"<redacted>")
            .field("capability", &"<redacted>")
            .field("integration", &self.integration)
            .field("repository_id", &"<redacted>")
            .field("checkout_id", &"<redacted>")
            .field("idle_deadline_ms", &self.idle_deadline_ms)
            .field("absolute_deadline_ms", &self.absolute_deadline_ms)
            .finish()
    }
}

/// Finite local queue policy, separate from daemon receipt and expiry policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HookClientQueueConfig {
    pub max_pending_count: usize,
    pub max_pending_bytes: usize,
    pub max_pending_age_ms: i64,
    pub close_retry_grace_ms: i64,
}

impl Default for HookClientQueueConfig {
    fn default() -> Self {
        Self {
            max_pending_count: MAX_PENDING_COUNT,
            max_pending_bytes: MAX_PENDING_BYTES,
            max_pending_age_ms: MAX_PENDING_AGE_MS,
            close_retry_grace_ms: DEFAULT_CLOSE_RETRY_GRACE_MS,
        }
    }
}

impl HookClientQueueConfig {
    fn validate(self) -> Result<Self, HookSessionClientError> {
        if self.max_pending_count == 0
            || self.max_pending_count > MAX_PENDING_COUNT
            || self.max_pending_bytes == 0
            || self.max_pending_bytes > MAX_PENDING_BYTES
            || self.max_pending_age_ms <= 0
            || self.max_pending_age_ms > MAX_PENDING_AGE_MS
            || self.close_retry_grace_ms <= 0
            || self.close_retry_grace_ms > MAX_PENDING_AGE_MS
        {
            return Err(HookSessionClientError::InvalidConfiguration);
        }
        Ok(self)
    }
}

/// A queued delivery safe to hand to a future authenticated transport adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingHookDelivery {
    pub delivery_id: HookClientOpaqueId,
    pub sequence: u64,
    pub payload: HookClientCapturePayload,
}

/// The complete, typed local capture payload.  It has no host envelope form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HookClientCapturePayload {
    Event(SessionCaptureEvent),
    Close(SessionCaptureClose),
}

impl HookClientCapturePayload {
    fn is_close(&self) -> bool {
        matches!(self, Self::Close(_))
    }
}

/// Result of an enqueue operation.  Overflow accounting is aggregate only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookClientEnqueueOutcome {
    pub delivery: PendingHookDelivery,
    pub dropped_non_close_events: usize,
}

#[derive(Debug, Error)]
pub enum HookSessionClientError {
    #[error("hook-session client input is invalid")]
    InvalidInput,
    #[error("hook-session client configuration is invalid")]
    InvalidConfiguration,
    #[error("hook-session client state is unsafe")]
    UnsafeState,
    #[error("hook-session client state is malformed")]
    MalformedState,
    #[error("hook-session client binding is missing")]
    BindingMissing,
    #[error("hook-session client binding is closed")]
    BindingClosed,
    #[error("hook-session client binding is expired")]
    BindingExpired,
    #[error("hook-session client pending queue cannot retain this delivery")]
    QueueCapacity,
    #[error("hook-session client delivery is missing")]
    DeliveryMissing,
    #[error("hook-session client entropy is unavailable")]
    EntropyUnavailable,
    #[error("hook-session client local I/O failed")]
    Io(#[from] std::io::Error),
    #[error("hook-session client serialization failed")]
    Serialization(#[from] serde_json::Error),
}

/// Protected local capability and delivery queue.
pub struct HookSessionClient {
    root: PathBuf,
    cryptography: HookSessionCryptography,
    queue: HookClientQueueConfig,
}

impl fmt::Debug for HookSessionClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HookSessionClient")
            .field("root", &"<private>")
            .field("queue", &self.queue)
            .finish()
    }
}

impl HookSessionClient {
    /// Opens the XDG state location: `$XDG_STATE_HOME/lattice/hook-bindings`,
    /// or `$HOME/.local/state/lattice/hook-bindings` when XDG is unset.
    pub fn open_default() -> Result<Self, HookSessionClientError> {
        let root = default_state_root()?.join("hook-bindings");
        Self::open_at(root, HookClientQueueConfig::default())
    }

    /// Opens an explicit private root, primarily for the later CLI adapter and
    /// tests.  The root itself must be (or becomes) a real 0700 directory.
    pub fn open_at(
        root: impl Into<PathBuf>,
        queue: HookClientQueueConfig,
    ) -> Result<Self, HookSessionClientError> {
        let root = root.into();
        queue.validate()?;
        ensure_private_directory(&root)?;
        let secret = load_or_create_key(&root.join("filename.key"))?;
        Ok(Self {
            root,
            cryptography: HookSessionCryptography::from_secret(secret),
            queue,
        })
    }

    /// Persist a newly minted binding.  Replacing an existing capability is
    /// forbidden: only an explicit daemon session-open/resume may decide which
    /// binding is current, and callers must first remove conclusively expired
    /// state.
    pub fn store_binding(
        &self,
        key: &HookClientBindingKey,
        binding: &HookClientBinding,
    ) -> Result<(), HookSessionClientError> {
        self.with_lock(|| {
            let fingerprint = key.fingerprint(&self.cryptography)?;
            let path = self.binding_path(&fingerprint);
            if path.exists() {
                return Err(HookSessionClientError::BindingClosed);
            }
            let record = CapabilityRecord::from_binding(binding);
            atomic_write_json(&self.root, &path, &record)
        })
    }

    pub fn load_binding(
        &self,
        key: &HookClientBindingKey,
        now_ms: i64,
    ) -> Result<HookClientBinding, HookSessionClientError> {
        self.with_lock(|| {
            let record = self.load_record(key)?;
            if now_ms > record.absolute_deadline_ms || now_ms > record.idle_deadline_ms {
                return Err(HookSessionClientError::BindingExpired);
            }
            record.into_binding()
        })
    }

    /// Persists daemon-renewed deadlines only when the returned binding is
    /// exactly the locally held capability and authority. This cannot replace
    /// a binding or reset local delivery bookkeeping.
    pub fn refresh_binding(
        &self,
        key: &HookClientBindingKey,
        renewed: &HookClientBinding,
    ) -> Result<(), HookSessionClientError> {
        self.with_lock(|| {
            let fingerprint = key.fingerprint(&self.cryptography)?;
            let mut record = self.load_record_by_fingerprint(&fingerprint)?;
            if record.binding_id != renewed.binding_id.0
                || record.capability != renewed.capability.0
                || record.integration != renewed.integration
                || record.repository_id != renewed.repository_id
                || record.checkout_id != renewed.checkout_id
                || renewed.absolute_deadline_ms != record.absolute_deadline_ms
                || renewed.idle_deadline_ms < record.idle_deadline_ms
                || renewed.idle_deadline_ms > renewed.absolute_deadline_ms
            {
                return Err(HookSessionClientError::InvalidInput);
            }
            record.idle_deadline_ms = renewed.idle_deadline_ms;
            atomic_write_json(&self.root, &self.binding_path(&fingerprint), &record)
        })
    }

    /// Atomically allocates a sequence and random delivery ID before persisting
    /// the typed payload.  A close is terminal locally: no later new event can
    /// be enqueued under this binding.
    pub fn enqueue(
        &self,
        key: &HookClientBindingKey,
        payload: HookClientCapturePayload,
        now_ms: i64,
    ) -> Result<HookClientEnqueueOutcome, HookSessionClientError> {
        self.with_lock(|| {
            let fingerprint = key.fingerprint(&self.cryptography)?;
            let mut record = self.load_record_by_fingerprint(&fingerprint)?;
            if now_ms > record.absolute_deadline_ms || now_ms > record.idle_deadline_ms {
                return Err(HookSessionClientError::BindingExpired);
            }
            if record.close_sequence.is_some() {
                return Err(HookSessionClientError::BindingClosed);
            }
            let sequence = record.next_sequence;
            let delivery_id = HookClientOpaqueId::new(hex(&random_bytes::<DELIVERY_ID_BYTES>()?))?;
            let pending =
                PendingRecord::new(sequence, delivery_id.0.clone(), payload.clone(), now_ms);
            let pending_bytes = serialized_len(&pending)?;
            if pending_bytes > self.queue.max_pending_bytes {
                return Err(HookSessionClientError::QueueCapacity);
            }

            let mut current = self.pending_records(&fingerprint, &record)?;
            let cutoff = now_ms.saturating_sub(self.queue.max_pending_age_ms);
            let mut dropped = 0;
            for item in current
                .iter()
                .filter(|item| item.created_at_ms < cutoff && !item.payload.is_close())
            {
                remove_private_file(&self.pending_path(&fingerprint, item.sequence))?;
                dropped += 1;
            }
            current.retain(|item| item.created_at_ms >= cutoff || item.payload.is_close());
            let mut bytes: usize = current
                .iter()
                .map(serialized_len)
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .sum();
            while current.len() >= self.queue.max_pending_count
                || bytes.saturating_add(pending_bytes) > self.queue.max_pending_bytes
            {
                let Some(index) = current.iter().position(|item| !item.payload.is_close()) else {
                    return Err(HookSessionClientError::QueueCapacity);
                };
                let removed = current.remove(index);
                bytes = bytes.saturating_sub(serialized_len(&removed)?);
                remove_private_file(&self.pending_path(&fingerprint, removed.sequence))?;
                dropped += 1;
            }

            atomic_write_json(
                &self.root,
                &self.pending_path(&fingerprint, sequence),
                &pending,
            )?;
            record.next_sequence = record
                .next_sequence
                .checked_add(1)
                .ok_or(HookSessionClientError::QueueCapacity)?;
            if payload.is_close() {
                record.close_sequence = Some(sequence);
            }
            atomic_write_json(&self.root, &self.binding_path(&fingerprint), &record)?;
            Ok(HookClientEnqueueOutcome {
                delivery: PendingHookDelivery {
                    delivery_id,
                    sequence,
                    payload,
                },
                dropped_non_close_events: dropped,
            })
        })
    }

    /// Returns unacknowledged deliveries in strict local sequence order.  A
    /// transport adapter may retry this head before submitting a newer event.
    pub fn pending(
        &self,
        key: &HookClientBindingKey,
    ) -> Result<Vec<PendingHookDelivery>, HookSessionClientError> {
        self.with_lock(|| {
            let fingerprint = key.fingerprint(&self.cryptography)?;
            let record = self.load_record_by_fingerprint(&fingerprint)?;
            self.pending_records(&fingerprint, &record)?
                .into_iter()
                .map(PendingRecord::into_delivery)
                .collect()
        })
    }

    /// Records only that an opaque transport response accepted this delivery.
    /// Non-close payloads are erased immediately.  A close erases its payload
    /// but retains the capability record for its bounded duplicate-close grace.
    pub fn acknowledge(
        &self,
        key: &HookClientBindingKey,
        delivery_id: &HookClientOpaqueId,
        now_ms: i64,
    ) -> Result<(), HookSessionClientError> {
        self.with_lock(|| {
            let fingerprint = key.fingerprint(&self.cryptography)?;
            let mut record = self.load_record_by_fingerprint(&fingerprint)?;
            let current = self.pending_records(&fingerprint, &record)?;
            let pending = current
                .into_iter()
                .find(|pending| pending.delivery_id == delivery_id.0)
                .ok_or(HookSessionClientError::DeliveryMissing)?;
            remove_private_file(&self.pending_path(&fingerprint, pending.sequence))?;
            if pending.payload.is_close() {
                record.close_retire_after_ms =
                    Some(now_ms.saturating_add(self.queue.close_retry_grace_ms));
                atomic_write_json(&self.root, &self.binding_path(&fingerprint), &record)?;
            }
            Ok(())
        })
    }

    /// Removes a conclusively expired binding, or an acknowledged close after
    /// its retry grace.  It never infers success from a missing transport reply.
    pub fn prune(
        &self,
        key: &HookClientBindingKey,
        now_ms: i64,
    ) -> Result<bool, HookSessionClientError> {
        self.with_lock(|| {
            let fingerprint = key.fingerprint(&self.cryptography)?;
            let record = match self.load_record_by_fingerprint(&fingerprint) {
                Ok(record) => record,
                Err(HookSessionClientError::BindingMissing) => return Ok(false),
                Err(error) => return Err(error),
            };
            // A successful close owns its explicit duplicate-close grace even
            // if its former event authority would otherwise now be expired.
            let expired = record.close_retire_after_ms.is_none()
                && (now_ms > record.absolute_deadline_ms || now_ms > record.idle_deadline_ms);
            let close_complete = record
                .close_retire_after_ms
                .is_some_and(|deadline| now_ms >= deadline);
            if !expired && !close_complete {
                return Ok(false);
            }
            for pending in self.pending_records(&fingerprint, &record)? {
                remove_private_file(&self.pending_path(&fingerprint, pending.sequence))?;
            }
            remove_private_file(&self.binding_path(&fingerprint))?;
            Ok(true)
        })
    }

    fn with_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, HookSessionClientError>,
    ) -> Result<T, HookSessionClientError> {
        ensure_private_directory(&self.root)?;
        let lock = open_private_lock(&self.root.join("lock"))?;
        #[cfg(unix)]
        {
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(HookSessionClientError::Io(std::io::Error::last_os_error()));
            }
        }
        operation()
    }

    fn load_record(
        &self,
        key: &HookClientBindingKey,
    ) -> Result<CapabilityRecord, HookSessionClientError> {
        let fingerprint = key.fingerprint(&self.cryptography)?;
        self.load_record_by_fingerprint(&fingerprint)
    }

    fn load_record_by_fingerprint(
        &self,
        fingerprint: &str,
    ) -> Result<CapabilityRecord, HookSessionClientError> {
        let record: CapabilityRecord = read_private_json(&self.binding_path(fingerprint))?;
        record.validate()
    }

    fn pending_records(
        &self,
        fingerprint: &str,
        record: &CapabilityRecord,
    ) -> Result<Vec<PendingRecord>, HookSessionClientError> {
        let mut pending = Vec::new();
        for sequence in 1..record.next_sequence {
            let path = self.pending_path(fingerprint, sequence);
            match read_private_json(&path) {
                Ok(record) => pending.push(record),
                Err(HookSessionClientError::BindingMissing) => {}
                Err(error) => return Err(error),
            }
        }
        pending.sort_by_key(|item: &PendingRecord| item.sequence);
        if pending
            .iter()
            .any(|item| item.sequence >= record.next_sequence)
            || pending
                .windows(2)
                .any(|window| window[0].sequence == window[1].sequence)
        {
            return Err(HookSessionClientError::MalformedState);
        }
        for item in &pending {
            item.validate()?;
        }
        Ok(pending)
    }

    fn binding_path(&self, fingerprint: &str) -> PathBuf {
        self.root.join(format!("binding-{fingerprint}.json"))
    }

    fn pending_path(&self, fingerprint: &str, sequence: u64) -> PathBuf {
        self.root
            .join(format!("pending-{fingerprint}-{sequence:020}.json"))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapabilityRecord {
    schema_version: u32,
    binding_id: String,
    capability: String,
    integration: String,
    repository_id: String,
    checkout_id: String,
    idle_deadline_ms: i64,
    absolute_deadline_ms: i64,
    next_sequence: u64,
    close_sequence: Option<u64>,
    close_retire_after_ms: Option<i64>,
}

impl CapabilityRecord {
    fn from_binding(binding: &HookClientBinding) -> Self {
        Self {
            schema_version: CAPABILITY_SCHEMA_VERSION,
            binding_id: binding.binding_id.0.clone(),
            capability: binding.capability.0.clone(),
            integration: binding.integration.clone(),
            repository_id: binding.repository_id.clone(),
            checkout_id: binding.checkout_id.clone(),
            idle_deadline_ms: binding.idle_deadline_ms,
            absolute_deadline_ms: binding.absolute_deadline_ms,
            // Sequence zero is reserved/invalid on the daemon wire contract.
            next_sequence: 1,
            close_sequence: None,
            close_retire_after_ms: None,
        }
    }

    fn validate(self) -> Result<Self, HookSessionClientError> {
        if self.schema_version != CAPABILITY_SCHEMA_VERSION
            || self.next_sequence == u64::MAX
            || self
                .close_sequence
                .is_some_and(|sequence| sequence >= self.next_sequence)
            || self.idle_deadline_ms < 0
            || self.absolute_deadline_ms < self.idle_deadline_ms
        {
            return Err(HookSessionClientError::MalformedState);
        }
        HookClientOpaqueId::new(self.binding_id.clone())?;
        HookClientOpaqueId::new(self.capability.clone())?;
        HookIntegrationId::new(self.integration.clone())
            .map_err(|_| HookSessionClientError::MalformedState)?;
        HookCheckoutIdentity::new(self.repository_id.clone(), self.checkout_id.clone())
            .map_err(|_| HookSessionClientError::MalformedState)?;
        Ok(self)
    }

    fn into_binding(self) -> Result<HookClientBinding, HookSessionClientError> {
        HookClientBinding::new(
            HookClientOpaqueId::new(self.binding_id)?,
            HookClientOpaqueId::new(self.capability)?,
            self.integration,
            self.repository_id,
            self.checkout_id,
            self.idle_deadline_ms,
            self.absolute_deadline_ms,
        )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingRecord {
    schema_version: u32,
    sequence: u64,
    delivery_id: String,
    created_at_ms: i64,
    payload: StoredCapturePayload,
}

impl PendingRecord {
    fn new(
        sequence: u64,
        delivery_id: String,
        payload: HookClientCapturePayload,
        created_at_ms: i64,
    ) -> Self {
        Self {
            schema_version: PENDING_SCHEMA_VERSION,
            sequence,
            delivery_id,
            created_at_ms,
            payload: payload.into(),
        }
    }

    fn validate(&self) -> Result<(), HookSessionClientError> {
        if self.schema_version != PENDING_SCHEMA_VERSION || self.created_at_ms < 0 {
            return Err(HookSessionClientError::MalformedState);
        }
        HookClientOpaqueId::new(self.delivery_id.clone())?;
        self.payload.validate()
    }

    fn into_delivery(self) -> Result<PendingHookDelivery, HookSessionClientError> {
        self.validate()?;
        Ok(PendingHookDelivery {
            delivery_id: HookClientOpaqueId::new(self.delivery_id)?,
            sequence: self.sequence,
            payload: self.payload.try_into()?,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredCapturePayload {
    Event { event: StoredCaptureEvent },
    Close { close: StoredCaptureClose },
}

impl StoredCapturePayload {
    fn is_close(&self) -> bool {
        matches!(self, Self::Close { .. })
    }

    fn validate(&self) -> Result<(), HookSessionClientError> {
        match self {
            Self::Event { event } => event.to_event().map(|_| ()),
            Self::Close { close } => close.to_close().map(|_| ()),
        }
    }
}

impl From<HookClientCapturePayload> for StoredCapturePayload {
    fn from(payload: HookClientCapturePayload) -> Self {
        match payload {
            HookClientCapturePayload::Event(event) => Self::Event {
                event: event.into(),
            },
            HookClientCapturePayload::Close(close) => Self::Close {
                close: close.into(),
            },
        }
    }
}

impl TryFrom<StoredCapturePayload> for HookClientCapturePayload {
    type Error = HookSessionClientError;

    fn try_from(payload: StoredCapturePayload) -> Result<Self, Self::Error> {
        match payload {
            StoredCapturePayload::Event { event } => Ok(Self::Event(event.to_event()?)),
            StoredCapturePayload::Close { close } => Ok(Self::Close(close.to_close()?)),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCaptureEvent {
    schema_version: u32,
    fact: StoredCaptureFact,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredCaptureFact {
    EditedPath {
        path: String,
    },
    Check {
        label: String,
        outcome: CheckOutcome,
    },
    Error {
        category: String,
        fingerprint: String,
        status: ErrorStatus,
        summary: Option<String>,
    },
}

impl From<SessionCaptureEvent> for StoredCaptureEvent {
    fn from(event: SessionCaptureEvent) -> Self {
        let fact = match event.fact {
            SessionCaptureFact::EditedPath { path } => StoredCaptureFact::EditedPath { path },
            SessionCaptureFact::Check { label, outcome } => {
                StoredCaptureFact::Check { label, outcome }
            }
            SessionCaptureFact::Error {
                category,
                fingerprint,
                status,
                summary,
            } => StoredCaptureFact::Error {
                category,
                fingerprint,
                status,
                summary,
            },
        };
        Self {
            schema_version: event.schema_version,
            fact,
        }
    }
}

impl StoredCaptureEvent {
    fn to_event(&self) -> Result<SessionCaptureEvent, HookSessionClientError> {
        // Re-admit through core's parser so a corrupt local file cannot smuggle
        // an authority-shaped or unsanitized fact to the later transport.
        let value = match &self.fact {
            StoredCaptureFact::EditedPath { path } => serde_json::json!({
                "schema_version": self.schema_version, "kind": "edited_path", "path": path,
            }),
            StoredCaptureFact::Check { label, outcome } => serde_json::json!({
                "schema_version": self.schema_version, "kind": "check", "label": label, "outcome": outcome,
            }),
            StoredCaptureFact::Error {
                category,
                fingerprint,
                status,
                summary,
            } => {
                let mut value = serde_json::json!({
                    "schema_version": self.schema_version, "kind": "error", "category": category,
                    "fingerprint": fingerprint, "status": status,
                });
                if let Some(summary) = summary {
                    value["summary"] = serde_json::Value::String(summary.clone());
                }
                value
            }
        };
        lattice_core::memory::parse_session_capture_event(&serde_json::to_string(&value)?)
            .map_err(|_| HookSessionClientError::MalformedState)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCaptureClose {
    schema_version: u32,
    received_at: DateTime<Utc>,
    final_summary: Option<String>,
}

impl From<SessionCaptureClose> for StoredCaptureClose {
    fn from(close: SessionCaptureClose) -> Self {
        Self {
            schema_version: close.schema_version,
            received_at: close.received_at,
            final_summary: close.final_summary,
        }
    }
}

impl StoredCaptureClose {
    fn to_close(&self) -> Result<SessionCaptureClose, HookSessionClientError> {
        let mut value = serde_json::json!({ "schema_version": self.schema_version });
        if let Some(summary) = &self.final_summary {
            value["final_summary"] = serde_json::Value::String(summary.clone());
        }
        // The persisted local value has already been normalized.  Parsing it
        // again protects the wire boundary; its timestamp is retained solely
        // for local replay identity and the daemon will still own admission.
        lattice_core::memory::parse_session_capture_close(
            &serde_json::to_string(&value)?,
            self.received_at,
        )
        .map_err(|_| HookSessionClientError::MalformedState)
    }
}

fn validate_opaque(value: &str) -> Result<(), HookSessionClientError> {
    if value.is_empty()
        || value.len() > MAX_OPAQUE_ID_BYTES
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(HookSessionClientError::InvalidInput);
    }
    Ok(())
}

fn serialized_len<T: Serialize>(value: &T) -> Result<usize, HookSessionClientError> {
    Ok(serde_json::to_vec(value)?.len())
}

fn default_state_root() -> Result<PathBuf, HookSessionClientError> {
    if let Some(root) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            return Err(HookSessionClientError::InvalidInput);
        }
        return Ok(root.join("lattice"));
    }
    let home = std::env::var_os("HOME").ok_or(HookSessionClientError::InvalidInput)?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return Err(HookSessionClientError::InvalidInput);
    }
    Ok(home.join(".local/state/lattice"))
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> Result<(), HookSessionClientError> {
    match fs::symlink_metadata(path) {
        Ok(_) => return validate_private_directory(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(HookSessionClientError::Io(error)),
    }
    let parent = path.parent().ok_or(HookSessionClientError::UnsafeState)?;
    fs::create_dir_all(parent)?;
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(HookSessionClientError::Io(error)),
    }
    validate_private_directory(path)
}

#[cfg(not(unix))]
fn ensure_private_directory(_path: &Path) -> Result<(), HookSessionClientError> {
    Err(HookSessionClientError::UnsafeState)
}

#[cfg(unix)]
fn validate_private_directory(path: &Path) -> Result<(), HookSessionClientError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(HookSessionClientError::UnsafeState);
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_file_metadata(metadata: &fs::Metadata) -> Result<(), HookSessionClientError> {
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err(HookSessionClientError::UnsafeState);
    }
    Ok(())
}

#[cfg(unix)]
fn open_private_read(path: &Path) -> Result<File, HookSessionClientError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    validate_private_file_metadata(&file.metadata()?)?;
    Ok(file)
}

#[cfg(unix)]
fn open_private_lock(path: &Path) -> Result<File, HookSessionClientError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    validate_private_file_metadata(&file.metadata()?)?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_private_lock(_path: &Path) -> Result<File, HookSessionClientError> {
    Err(HookSessionClientError::UnsafeState)
}

fn read_private_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
) -> Result<T, HookSessionClientError> {
    let mut file = match open_private_read(path) {
        Ok(file) => file,
        Err(HookSessionClientError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(HookSessionClientError::BindingMissing)
        }
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(|_| HookSessionClientError::MalformedState)
}

fn atomic_write_json<T: Serialize>(
    root: &Path,
    destination: &Path,
    value: &T,
) -> Result<(), HookSessionClientError> {
    validate_private_directory(root)?;
    let bytes = serde_json::to_vec(value)?;
    let temporary = root.join(format!(
        ".hook-session-{}.tmp",
        hex(&random_bytes::<DELIVERY_ID_BYTES>()?)
    ));
    let write_result = (|| -> Result<(), HookSessionClientError> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        validate_private_file_metadata(&file.metadata()?)?;
        fs::rename(&temporary, destination)?;
        File::open(root)?.sync_all()?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn remove_private_file(path: &Path) -> Result<(), HookSessionClientError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_private_file_metadata(&metadata)?;
            fs::remove_file(path)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(HookSessionClientError::Io(error)),
    }
}

fn load_or_create_key(path: &Path) -> Result<[u8; KEY_BYTES], HookSessionClientError> {
    match read_private_key(path) {
        Ok(key) => return Ok(key),
        Err(HookSessionClientError::BindingMissing) => {}
        Err(error) => return Err(error),
    }
    let key = random_bytes::<KEY_BYTES>()?;
    let root = path.parent().ok_or(HookSessionClientError::UnsafeState)?;
    let temporary = root.join(format!(
        ".filename-key-{}.tmp",
        hex(&random_bytes::<DELIVERY_ID_BYTES>()?)
    ));
    let result = (|| -> Result<(), HookSessionClientError> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(&key)?;
        file.sync_all()?;
        match fs::hard_link(&temporary, path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(HookSessionClientError::Io(error)),
        }
        File::open(root)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result?;
    read_private_key(path)
}

fn read_private_key(path: &Path) -> Result<[u8; KEY_BYTES], HookSessionClientError> {
    let mut file = match open_private_read(path) {
        Ok(file) => file,
        Err(HookSessionClientError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(HookSessionClientError::BindingMissing)
        }
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() != KEY_BYTES as u64 {
        return Err(HookSessionClientError::MalformedState);
    }
    let mut key = [0; KEY_BYTES];
    file.read_exact(&mut key)?;
    Ok(key)
}

#[cfg(unix)]
fn random_bytes<const N: usize>() -> Result<[u8; N], HookSessionClientError> {
    let mut bytes = [0; N];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(|_| HookSessionClientError::EntropyUnavailable)?;
    Ok(bytes)
}

#[cfg(not(unix))]
fn random_bytes<const N: usize>() -> Result<[u8; N], HookSessionClientError> {
    Err(HookSessionClientError::EntropyUnavailable)
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(HEX[(byte >> 4) as usize] as char);
        value.push(HEX[(byte & 0x0f) as usize] as char);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lattice-hook-client-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn key() -> HookClientBindingKey {
        HookClientBindingKey::new(
            "codex-1",
            "host-session-private",
            "repo-private",
            "/work/private",
        )
        .unwrap()
    }

    fn binding() -> HookClientBinding {
        HookClientBinding::new(
            HookClientOpaqueId::new("binding-private").unwrap(),
            HookClientOpaqueId::new("capability-private").unwrap(),
            "codex-1",
            "repo-private",
            "/work/private",
            10_000,
            20_000,
        )
        .unwrap()
    }

    fn event(path: &str) -> HookClientCapturePayload {
        HookClientCapturePayload::Event(
            lattice_core::memory::parse_session_capture_event(&format!(
                r#"{{"schema_version":1,"kind":"edited_path","path":"{path}"}}"#
            ))
            .unwrap(),
        )
    }

    fn close() -> HookClientCapturePayload {
        HookClientCapturePayload::Close(
            lattice_core::memory::parse_session_capture_close(
                r#"{"schema_version":1,"final_summary":"finished safely"}"#,
                DateTime::from_unix_seconds(1_000),
            )
            .unwrap(),
        )
    }

    fn client(label: &str) -> (HookSessionClient, PathBuf) {
        let root = fixture_root(label);
        let client = HookSessionClient::open_at(&root, HookClientQueueConfig::default()).unwrap();
        (client, root)
    }

    #[test]
    fn opaque_filenames_do_not_disclose_host_session_or_checkout() {
        let (client, root) = client("privacy");
        let key = key();
        client.store_binding(&key, &binding()).unwrap();
        let names: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(names.iter().any(|name| name.starts_with("binding-")));
        assert!(names
            .iter()
            .all(|name| !name.contains("host-session-private") && !name.contains("work/private")));
        assert!(!format!("{key:?}").contains("host-session-private"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_permissive_capability_file_fail_closed() {
        use std::os::unix::fs::symlink;
        let (client, root) = client("unsafe");
        let key = key();
        client.store_binding(&key, &binding()).unwrap();
        let fingerprint = key.fingerprint(&client.cryptography).unwrap();
        let record = client.binding_path(&fingerprint);
        fs::remove_file(&record).unwrap();
        symlink("/dev/null", &record).unwrap();
        assert!(matches!(
            client.load_binding(&key, 1),
            Err(HookSessionClientError::UnsafeState) | Err(HookSessionClientError::Io(_))
        ));
        fs::remove_file(&record).unwrap();
        client.store_binding(&key, &binding()).unwrap();
        fs::set_permissions(&record, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            client.load_binding(&key, 1),
            Err(HookSessionClientError::UnsafeState)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_process_shaped_clients_allocate_unique_monotonic_sequences() {
        let root = fixture_root("concurrent");
        let initial = HookSessionClient::open_at(&root, HookClientQueueConfig::default()).unwrap();
        let key = key();
        initial.store_binding(&key, &binding()).unwrap();
        let barrier = Arc::new(Barrier::new(9));
        let mut workers = Vec::new();
        for index in 0..8 {
            let barrier = barrier.clone();
            let root = root.clone();
            let key = key.clone();
            workers.push(thread::spawn(move || {
                let client =
                    HookSessionClient::open_at(root, HookClientQueueConfig::default()).unwrap();
                barrier.wait();
                client
                    .enqueue(&key, event(&format!("src/{index}.rs")), 1)
                    .unwrap()
                    .delivery
                    .sequence
            }));
        }
        barrier.wait();
        let mut sequences: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        sequences.sort_unstable();
        assert_eq!(sequences, (1..=8).collect::<Vec<_>>());
        assert_eq!(
            initial
                .pending(&key)
                .unwrap()
                .iter()
                .map(|item| item.sequence)
                .collect::<Vec<_>>(),
            sequences
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pending_order_is_stable_and_close_retains_binding_for_grace() {
        let (client, root) = client("ordering");
        let key = key();
        client.store_binding(&key, &binding()).unwrap();
        let first = client.enqueue(&key, event("src/a.rs"), 1).unwrap().delivery;
        let second = client.enqueue(&key, event("src/b.rs"), 2).unwrap().delivery;
        let closing = client.enqueue(&key, close(), 3).unwrap().delivery;
        assert_eq!(
            client
                .pending(&key)
                .unwrap()
                .iter()
                .map(|item| item.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(matches!(
            client.enqueue(&key, event("src/c.rs"), 4),
            Err(HookSessionClientError::BindingClosed)
        ));
        client.acknowledge(&key, &first.delivery_id, 4).unwrap();
        client.acknowledge(&key, &second.delivery_id, 4).unwrap();
        client.acknowledge(&key, &closing.delivery_id, 4).unwrap();
        assert!(client.load_binding(&key, 5).is_ok());
        assert!(!client
            .prune(&key, 4 + DEFAULT_CLOSE_RETRY_GRACE_MS - 1)
            .unwrap());
        assert!(client
            .prune(&key, 4 + DEFAULT_CLOSE_RETRY_GRACE_MS)
            .unwrap());
        assert!(matches!(
            client.pending(&key),
            Err(HookSessionClientError::BindingMissing)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persisted_records_have_only_typed_capture_schema() {
        let (client, root) = client("schema");
        let key = key();
        client.store_binding(&key, &binding()).unwrap();
        client.enqueue(&key, event("src/a.rs"), 1).unwrap();
        let contents = fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read_to_string(entry.path()).ok())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!contents.contains("transcript") && !contents.contains("host-session-private"));
        assert!(contents.contains("edited_path"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refresh_preserves_queue_and_rejects_authority_replacement() {
        let (client, root) = client("refresh");
        let key = key();
        let original = binding();
        client.store_binding(&key, &original).unwrap();
        let delivery = client.enqueue(&key, event("src/a.rs"), 1).unwrap().delivery;
        let renewed = HookClientBinding::new(
            original.binding_id.clone(),
            original.capability.clone(),
            original.integration.clone(),
            original.repository_id.clone(),
            original.checkout_id.clone(),
            15_000,
            20_000,
        )
        .unwrap();
        client.refresh_binding(&key, &renewed).unwrap();
        assert_eq!(
            client
                .load_binding(&key, 11_000)
                .unwrap()
                .idle_deadline_ms(),
            15_000
        );
        assert_eq!(
            client.pending(&key).unwrap()[0].delivery_id,
            delivery.delivery_id
        );
        let replaced = HookClientBinding::new(
            HookClientOpaqueId::new("other-binding-private").unwrap(),
            original.capability.clone(),
            original.integration.clone(),
            original.repository_id.clone(),
            original.checkout_id.clone(),
            15_000,
            20_000,
        )
        .unwrap();
        assert!(matches!(
            client.refresh_binding(&key, &replaced),
            Err(HookSessionClientError::InvalidInput)
        ));
        fs::remove_dir_all(root).unwrap();
    }
}
