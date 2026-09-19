//! Authenticated, shard-independent hook-session capture routing.

use anyhow::{Context, Result};
use lattice_core::memory::{
    parse_session_capture_close, parse_session_capture_event, reduce_session_capture,
    DaemonSessionCaptureEvent, DeliveryBinding, MemoryClass, MemoryQueryAuthority,
    MemoryRecallResult, MemoryRecallTier, MemoryStore, MemoryStoreRouter, MemoryVerificationStatus,
    SessionCaptureFact, SessionDigestAuthority, SESSION_CAPTURE_SCHEMA_VERSION,
};
use lattice_core::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::adoption_metrics::{
    AdoptionMetricsStore, CaptureMetricRecord, CaptureOutcome, MemoryInjectionActionRecord,
    MemoryInjectionRecord,
};
use crate::hook_enforcement::{evaluate_plan, FollowupGaps, IndexState, PlanState};
use crate::hook_session_binding::{
    HookBindingId, HookCheckoutIdentity, HookIntegrationId, HookRepositoryState,
    HookSessionCapability, HookSessionCryptography, HostSessionId,
};
use crate::hook_session_registry::{
    HookRegistryConfig, HookRegistryError, HookSessionRegistry, RegistryAdmission,
    RegistryCompletion, RegistryDeliveryKind, RegistryHash, RegistryId, RegistryOpenRequest,
    RegistryReceiptStatus, RegistrySessionResume, RegistryVerification, RegistryVerifyRequest,
};
use crate::hook_workflow_state::{HookWorkflowState, WorkflowStep};
use crate::transport::ProxyRequest;
use crate::workspace_identity::WorkspaceIdentity;

const KEY_BYTES: usize = 32;
const MAX_PARAMS_BYTES: usize = 16 * 1024;
const IDLE_TTL_MS: i64 = 30 * 60 * 1_000;
const ABSOLUTE_TTL_MS: i64 = 12 * 60 * 60 * 1_000;
const RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const RECOVERY_BATCH: usize = 64;
const MAX_JOURNAL_ROWS_PER_BINDING: i64 = 16_385;
const RETRY_BASE_MS: i64 = 250;
const RETRY_MAX_MS: i64 = 30_000;
const PRESENTATION_CANDIDATE_LIMIT: usize = 64;
const SESSION_PRESENTATION_LIMIT: usize = 5;
const PROMPT_PRESENTATION_LIMIT: usize = 5;
const SESSION_PRESENTATION_BUDGET_TOKENS: usize = 1_500;
const PROMPT_PRESENTATION_BUDGET_TOKENS: usize = 1_200;
const POST_TOOL_PRESENTATION_BUDGET_TOKENS: usize = 240;
const MIN_PRESENTATION_RELEVANCE_BPS: u16 = 2_500;
const MAX_PROMPT_BYTES: usize = 8 * 1024;
const MAX_REQUEST_ID_BYTES: usize = 128;
// The route itself is the first and only extractor for the bounded hook facts.
// Keep this numeric metric dimension independent of the user-controlled
// integration identifier and of the string stored with session digests.
const CAPTURE_EXTRACTOR_VERSION: u32 = 1;

pub(crate) const HOOK_SESSION_OPEN_METHOD: &str = "hook/session_open";
pub(crate) const HOOK_EVENT_METHOD: &str = "hook/event";
pub(crate) const HOOK_TURN_SUMMARY_METHOD: &str = "hook/turn_summary";
pub(crate) const HOOK_SESSION_CLOSE_METHOD: &str = "hook/session_close";

#[derive(Debug)]
pub(crate) enum HookSessionRouteError {
    InvalidRequest,
    AuthorityRejected,
    Unavailable,
}

impl HookSessionRouteError {
    pub(crate) fn json_rpc_error(&self) -> (i32, String) {
        match self {
            Self::InvalidRequest => (-32602, "hook request is invalid".into()),
            Self::AuthorityRejected => (-32001, "hook request rejected".into()),
            Self::Unavailable => (-32603, "hook service is unavailable".into()),
        }
    }
}

pub(crate) struct HookSessionRoute {
    cryptography: HookSessionCryptography,
    registry: Mutex<HookSessionRegistry>,
    workflow: HookWorkflowState,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookSessionOpenParams {
    integration: String,
    host_session_id: String,
    #[serde(default)]
    resume: Option<HookSessionResumeParams>,
    #[serde(default)]
    presentation: Option<HookPresentationParams>,
    #[serde(default)]
    enforcement: Option<HookEnforcementParams>,
}

/// An enforcing workspace's adapter asks for a workflow decision on the same
/// authenticated open that carries its presentation request. The adapter has
/// already classified paths locally; only a category and a bounded count
/// cross the wire.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookEnforcementParams {
    event: HookEnforcementEvent,
    #[serde(default)]
    session_source: Option<HookSessionSource>,
    #[serde(default)]
    product_paths: Option<u32>,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum HookEnforcementEvent {
    SessionStart,
    PreToolUse,
    ShellEdit,
    Stop,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum HookSessionSource {
    Startup,
    Resume,
    Clear,
    Compact,
}

#[derive(Serialize)]
struct HookEnforcementResult {
    /// `allow` or `deny`. Only a pre-tool-use request can be denied.
    decision: &'static str,
    plan_state: &'static str,
    index_state: &'static str,
    /// Set only when this request claimed the session's single reminder.
    #[serde(skip_serializing_if = "Option::is_none")]
    followup: Option<HookFollowupResult>,
}

#[derive(Serialize)]
struct HookFollowupResult {
    stale_docs: bool,
    remember: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookPresentationParams {
    kind: HookPresentationKind,
    request_id: String,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    acted_on_injection_id: Option<String>,
    #[serde(default)]
    acknowledge_delivery: Option<HookMemoryDeliveryReceipt>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum HookPresentationKind {
    SessionStart,
    UserPromptSubmit,
    PostToolUse,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookDeliveryParams {
    binding_id: String,
    capability: String,
    integration: String,
    delivery_id: String,
    sequence: u64,
    event: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookSessionResumeParams {
    binding_id: String,
    capability: String,
}

#[derive(Serialize)]
struct HookSessionOpenResult {
    binding_id: String,
    capability: String,
    generation: u64,
    resumed: bool,
    idle_deadline_ms: i64,
    absolute_deadline_ms: i64,
    repository_id: String,
    checkout_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    presentation: Option<HookPresentationResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enforcement: Option<HookEnforcementResult>,
}

#[derive(Serialize)]
struct HookPresentationResult {
    injection_id: String,
    context: String,
    memory_deliveries: Vec<HookMemoryDeliveryReceipt>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HookMemoryDeliveryReceipt {
    authority: String,
    delivery_id: String,
    payload_hash: String,
    #[serde(skip_deserializing, default = "ack_required")]
    ack_required: bool,
}

fn ack_required() -> bool {
    true
}

impl HookSessionRoute {
    pub(crate) fn open_default() -> Result<Self> {
        let state_root = default_state_root()?;
        ensure_private_directory(&state_root)?;
        Self::open_at(&state_root.join("hook-sessions"))
    }

    pub(crate) fn open_at(directory: &Path) -> Result<Self> {
        ensure_private_directory(directory)?;
        let secret = load_or_create_key(&directory.join("authority.key"))?;
        let registry_path = directory.join("sessions.db");
        ensure_private_database_file(&registry_path)?;
        let registry = HookSessionRegistry::open(&registry_path, HookRegistryConfig::default())
            .context("failed to open hook-session registry")?;
        validate_private_file(&registry_path, "hook-session registry")?;
        let workflow_path = directory.join("workflow.db");
        ensure_private_database_file(&workflow_path)?;
        let workflow = HookWorkflowState::open(&workflow_path)
            .context("failed to open hook workflow state")?;
        validate_private_file(&workflow_path, "hook workflow state")?;
        Ok(Self {
            cryptography: HookSessionCryptography::from_secret(secret),
            registry: Mutex::new(registry),
            workflow,
        })
    }

    /// Record that the daemon served a workflow step for the checkout this
    /// connection resolved to. Called by the transport layer after a public
    /// tool call succeeds, for MCP and CLI clients alike. A connection that
    /// does not name exactly one checkout records nothing: a fact that cannot
    /// be attributed to a checkout must not satisfy any checkout's gate.
    pub(crate) fn record_workflow_step(&self, hello: &ProxyRequest, step: WorkflowStep) {
        let Ok(identity) = resolve_authority(hello) else {
            return;
        };
        let Ok(now) = now_ms() else {
            return;
        };
        let checkout_id = identity.checkout_root.to_string_lossy();
        if let Err(error) = self.workflow.record_step(&checkout_id, step, now) {
            tracing::warn!(%error, "failed to record hook workflow step");
        }
    }

    #[cfg(test)]
    pub(crate) fn handle_open(
        &self,
        hello: &ProxyRequest,
        params: Value,
    ) -> Result<Value, HookSessionRouteError> {
        self.handle_open_in(hello, params, IndexState::NotLoaded)
    }

    /// `index_state` is what the transport layer observed about this
    /// workspace's shard without loading it.
    pub(crate) fn handle_open_in(
        &self,
        hello: &ProxyRequest,
        params: Value,
        index_state: IndexState,
    ) -> Result<Value, HookSessionRouteError> {
        let encoded_len = serde_json::to_vec(&params)
            .map_err(|_| HookSessionRouteError::InvalidRequest)?
            .len();
        if encoded_len > MAX_PARAMS_BYTES {
            return Err(HookSessionRouteError::InvalidRequest);
        }
        let params: HookSessionOpenParams =
            serde_json::from_value(params).map_err(|_| HookSessionRouteError::InvalidRequest)?;
        let resolved = resolve_authority(hello)?;
        let integration = HookIntegrationId::new(params.integration)
            .map_err(|_| HookSessionRouteError::InvalidRequest)?;
        let metric_client = capture_metric_integration(integration.as_str());
        let host_session_id = HostSessionId::new(params.host_session_id)
            .map_err(|_| HookSessionRouteError::InvalidRequest)?;
        let checkout = HookCheckoutIdentity::new(
            resolved.repository_id.clone(),
            resolved.checkout_root.to_string_lossy().to_string(),
        )
        .map_err(|_| HookSessionRouteError::Unavailable)?;
        let repository_state = resolve_repository_state(&resolved.checkout_root)?;
        let resume = params.resume.map(decode_resume).transpose()?;
        // Binding identifiers are re-minted whenever an idle binding expires.
        // The keyed fingerprint of integration, host session and checkout is
        // the only content-free identity that is stable for a host session.
        let workflow_session = *self
            .cryptography
            .authority_fingerprint(&integration, &host_session_id, &checkout)
            .as_bytes();
        let presentation_request = params.presentation;
        let enforcement_request = params.enforcement;
        let request = registry_open_request(
            integration,
            host_session_id,
            checkout,
            repository_state,
            resume,
        )?;
        let outcome = self
            .registry
            .lock()
            .map_err(|_| HookSessionRouteError::Unavailable)?
            .open_or_resume(&self.cryptography, request)
            .map_err(map_registry_error)?;
        let presentation = presentation_request
            .map(|request| present_hook_memory(&resolved, &outcome, metric_client, request))
            .transpose()?
            .flatten();
        let enforcement = enforcement_request
            .map(|request| {
                self.decide_enforcement(&resolved, &workflow_session, request, &index_state)
            })
            .transpose()?;
        serde_json::to_value(HookSessionOpenResult {
            binding_id: encode_hex(outcome.binding_id.as_bytes()),
            capability: encode_hex(outcome.capability.as_bytes()),
            generation: outcome.generation,
            resumed: outcome.resumed,
            idle_deadline_ms: outcome.idle_deadline_ms,
            absolute_deadline_ms: outcome.absolute_deadline_ms,
            repository_id: resolved.repository_id,
            checkout_id: resolved.checkout_root.to_string_lossy().to_string(),
            presentation,
            enforcement,
        })
        .map_err(|_| HookSessionRouteError::Unavailable)
    }

    /// The workflow decision for one authenticated hook request. See
    /// `docs/hook-enforcement.md`, "Freshness rule" and "Fail-open rule".
    fn decide_enforcement(
        &self,
        identity: &WorkspaceIdentity,
        session_id: &[u8],
        request: HookEnforcementParams,
        index_state: &IndexState,
    ) -> Result<HookEnforcementResult, HookSessionRouteError> {
        let unavailable = |error: anyhow::Error| {
            tracing::warn!(%error, "hook workflow state is unavailable");
            HookSessionRouteError::Unavailable
        };
        let now = now_ms()?;
        let checkout_id = identity.checkout_root.to_string_lossy().to_string();
        if request.event == HookEnforcementEvent::SessionStart
            && matches!(
                request.session_source,
                Some(
                    HookSessionSource::Startup
                        | HookSessionSource::Clear
                        | HookSessionSource::Compact
                )
            )
        {
            self.workflow
                .mark_context_reset(session_id, now)
                .map_err(unavailable)?;
        }
        let session = self
            .workflow
            .session(session_id, now)
            .map_err(unavailable)?;
        let latest_plan = self
            .workflow
            .latest_step(
                &checkout_id,
                WorkflowStep::PrepareChange,
                session.not_before_ms,
            )
            .map_err(unavailable)?;
        let plan_state = evaluate_plan(
            now,
            latest_plan,
            session.last_covered_edit_ms,
            session.not_before_ms,
        );
        let covered = plan_state == PlanState::Current;
        let mut decision = "allow";
        let mut followup = None;
        match request.event {
            HookEnforcementEvent::SessionStart => {}
            HookEnforcementEvent::PreToolUse => {
                // A degraded index cannot give a real plan, so the gate fails
                // open and the adapter tells the agent instead.
                if covered || index_state.degraded() {
                    self.workflow
                        .record_product_edits(session_id, 1, covered, now)
                        .map_err(unavailable)?;
                } else {
                    decision = "deny";
                }
            }
            HookEnforcementEvent::ShellEdit => {
                let count = u64::from(request.product_paths.unwrap_or(0))
                    .min(crate::hook_enforcement::MAX_SHELL_CHANGED_PATHS as u64);
                if count > 0 {
                    self.workflow
                        .record_product_edits(session_id, count, covered, now)
                        .map_err(unavailable)?;
                }
            }
            HookEnforcementEvent::Stop => {
                let gaps: FollowupGaps = self
                    .workflow
                    .followup_gaps(&checkout_id, &session)
                    .map_err(unavailable)?;
                if gaps.any()
                    && !session.followup_reminded
                    && self
                        .workflow
                        .claim_followup_reminder(session_id, now)
                        .map_err(unavailable)?
                {
                    followup = Some(HookFollowupResult {
                        stale_docs: gaps.stale_docs,
                        remember: gaps.remember,
                    });
                }
            }
        }
        Ok(HookEnforcementResult {
            decision,
            plan_state: plan_state.wire(),
            index_state: index_state.wire(),
            followup,
        })
    }

    pub(crate) fn handle_event(
        &self,
        hello: &ProxyRequest,
        params: Value,
    ) -> Result<Value, HookSessionRouteError> {
        self.handle_delivery(hello, params, RegistryDeliveryKind::Event)
    }

    pub(crate) fn handle_close(
        &self,
        hello: &ProxyRequest,
        params: Value,
    ) -> Result<Value, HookSessionRouteError> {
        self.handle_delivery(hello, params, RegistryDeliveryKind::Close)
    }

    pub(crate) fn handle_turn_summary(
        &self,
        hello: &ProxyRequest,
        params: Value,
    ) -> Result<Value, HookSessionRouteError> {
        self.handle_delivery(hello, params, RegistryDeliveryKind::TurnSummary)
    }

    fn handle_delivery(
        &self,
        hello: &ProxyRequest,
        params: Value,
        kind: RegistryDeliveryKind,
    ) -> Result<Value, HookSessionRouteError> {
        let encoded_len = serde_json::to_vec(&params)
            .map_err(|_| HookSessionRouteError::InvalidRequest)?
            .len();
        if encoded_len > MAX_PARAMS_BYTES {
            return Err(HookSessionRouteError::InvalidRequest);
        }
        let params: HookDeliveryParams =
            serde_json::from_value(params).map_err(|_| HookSessionRouteError::InvalidRequest)?;
        let identity = resolve_authority(hello)?;
        if params.sequence == 0 {
            record_capture_metric(
                &identity,
                CaptureMetricContext {
                    integration: capture_metric_integration(&params.integration),
                    schema_version: capture_schema_version(&params.event),
                },
                CaptureOutcome::Rejected,
                false,
            );
            return Err(HookSessionRouteError::InvalidRequest);
        }
        let metric = CaptureMetricContext {
            integration: capture_metric_integration(&params.integration),
            schema_version: capture_schema_version(&params.event),
        };
        let result = (|| {
            let binding_id = HookBindingId::from_bytes(decode_hex::<16>(&params.binding_id)?);
            let capability =
                HookSessionCapability::from_bytes(decode_hex::<32>(&params.capability)?);
            let integration = HookIntegrationId::new(params.integration)
                .map_err(|_| HookSessionRouteError::InvalidRequest)?;
            let delivery_id =
                RegistryId::from_bytes(decode_hex::<16>(&params.delivery_id)?.to_vec())
                    .map_err(map_registry_error)?;
            let checkout = HookCheckoutIdentity::new(
                identity.repository_id.clone(),
                identity.checkout_root.to_string_lossy().to_string(),
            )
            .map_err(|_| HookSessionRouteError::Unavailable)?;
            let admitted_at_ms = now_ms()?;

            // Capability, integration and exact checkout are authenticated before
            // the event body is interpreted or any repository content is touched.
            let mut registry = self
                .registry
                .lock()
                .map_err(|_| HookSessionRouteError::Unavailable)?;
            let verification = registry
                .verify_and_renew(
                    &self.cryptography,
                    RegistryVerifyRequest {
                        binding_id,
                        capability,
                        integration,
                        current_checkout: checkout,
                        now_ms: admitted_at_ms,
                        idle_ttl_ms: IDLE_TTL_MS,
                        renew_idle: false,
                        replay_delivery_id: (kind == RegistryDeliveryKind::Close)
                            .then_some(delivery_id.clone()),
                    },
                )
                .map_err(map_registry_error)?;
            let current_state = resolve_repository_state(&identity.checkout_root)?;
            let payload_json = serde_json::to_string(&params.event)
                .map_err(|_| HookSessionRouteError::InvalidRequest)?;
            let (normalized_json, hash_json) = match kind {
                RegistryDeliveryKind::Event | RegistryDeliveryKind::TurnSummary => {
                    let event = parse_session_capture_event(&payload_json)
                        .map_err(|_| HookSessionRouteError::InvalidRequest)?;
                    match kind {
                        RegistryDeliveryKind::Event => {
                            if matches!(event.fact, SessionCaptureFact::TurnSummary { .. }) {
                                return Err(HookSessionRouteError::InvalidRequest);
                            }
                            validate_event_path(&identity.checkout_root, &event.fact)?;
                        }
                        RegistryDeliveryKind::TurnSummary => {
                            if !matches!(event.fact, SessionCaptureFact::TurnSummary { .. }) {
                                return Err(HookSessionRouteError::InvalidRequest);
                            }
                        }
                        RegistryDeliveryKind::Close => unreachable!(),
                    }
                    let normalized_value = normalized_event_value(&event)?;
                    let normalized = serde_json::to_string(&normalized_value)
                        .map_err(|_| HookSessionRouteError::Unavailable)?;
                    (normalized.clone(), normalized)
                }
                RegistryDeliveryKind::Close => {
                    let close = parse_session_capture_close(
                        &payload_json,
                        DateTime::<Utc>::from_unix_seconds(admitted_at_ms / 1_000),
                    )
                    .map_err(|_| HookSessionRouteError::InvalidRequest)?;
                    let stable_value = match close.final_summary {
                        Some(summary) => serde_json::json!({
                            "schema_version": close.schema_version,
                            "final_summary": summary,
                        }),
                        None => serde_json::json!({"schema_version": close.schema_version}),
                    };
                    let stable = serde_json::to_string(&stable_value)
                        .map_err(|_| HookSessionRouteError::Unavailable)?;
                    (stable.clone(), stable)
                }
            };
            let normalized_hash = RegistryHash::from_bytes(sha256(hash_json.as_bytes()));
            let binding_registry_id = RegistryId::from_bytes(binding_id.as_bytes().to_vec())
                .map_err(map_registry_error)?;
            let journal = CaptureJournal::open(&identity)?;
            if let Err(error) = journal.stage(JournalDelivery {
                binding_id: &binding_registry_id,
                delivery_id: &delivery_id,
                sequence: params.sequence,
                kind,
                normalized_hash,
                normalized_json: &normalized_json,
                branch: current_state.branch(),
                revision: current_state.revision(),
                received_at_ms: admitted_at_ms,
            }) {
                if matches!(error, HookSessionRouteError::AuthorityRejected) {
                    let _ = registry.revoke(&binding_registry_id);
                }
                return Err(error);
            }
            let outcome = registry
                .admit(RegistryAdmission {
                    binding_id: binding_registry_id.clone(),
                    delivery_id: delivery_id.clone(),
                    sequence: params.sequence,
                    kind,
                    event_schema_version: SESSION_CAPTURE_SCHEMA_VERSION,
                    normalized_hash,
                    admitted_at_ms,
                    idle_deadline_ms: admitted_at_ms
                        .checked_add(IDLE_TTL_MS)
                        .ok_or(HookSessionRouteError::Unavailable)?,
                    receipt_prune_after_ms: admitted_at_ms
                        .checked_add(RETENTION_MS)
                        .ok_or(HookSessionRouteError::Unavailable)?,
                })
                .map_err(map_registry_error)?;
            drain_binding(
                &mut registry,
                &identity,
                &verification,
                &binding_registry_id,
                admitted_at_ms,
            )?;
            let receipt = registry
                .receipt(&binding_registry_id, &delivery_id)
                .map_err(map_registry_error)?
                .ok_or(HookSessionRouteError::Unavailable)?;
            Ok(serde_json::json!({
                "delivery_id": params.delivery_id,
                "sequence": params.sequence,
                "status": match receipt.status {
                    RegistryReceiptStatus::Pending => "pending",
                    RegistryReceiptStatus::Reduced => "reduced",
                    RegistryReceiptStatus::Sealed => "sealed",
                },
                "replayed": outcome.idempotent_replay,
            }))
        })();
        record_delivery_metric(&identity, metric, &result);
        result
    }
}

fn normalized_event_value(
    event: &lattice_core::memory::SessionCaptureEvent,
) -> Result<Value, HookSessionRouteError> {
    let value = match &event.fact {
        SessionCaptureFact::EditedPath { path } => serde_json::json!({
            "schema_version": event.schema_version,
            "kind": "edited_path",
            "path": path,
        }),
        SessionCaptureFact::Check { label, outcome } => serde_json::json!({
            "schema_version": event.schema_version,
            "kind": "check",
            "label": label,
            "outcome": outcome,
        }),
        SessionCaptureFact::Error {
            category,
            fingerprint,
            status,
            summary,
        } => {
            let mut value = serde_json::json!({
                "schema_version": event.schema_version,
                "kind": "error",
                "category": category,
                "fingerprint": fingerprint,
                "status": status,
            });
            if let Some(summary) = summary {
                value["summary"] = Value::String(summary.clone());
            }
            value
        }
        SessionCaptureFact::TurnSummary { summary } => serde_json::json!({
            "schema_version": event.schema_version,
            "kind": "turn_summary",
            "summary": summary,
        }),
    };
    Ok(value)
}

#[derive(Clone, Copy)]
struct CaptureMetricContext {
    integration: &'static str,
    schema_version: u32,
}

/// Capture metrics are intentionally only admitted after the adapter's
/// checkout authority has been resolved. This prevents a rejected, untrusted
/// request from creating a ledger at an attacker-selected location.
fn record_delivery_metric(
    identity: &WorkspaceIdentity,
    context: CaptureMetricContext,
    result: &Result<Value, HookSessionRouteError>,
) {
    let (outcome, idempotent_replay) = match result {
        Ok(receipt) => (
            match receipt.get("status").and_then(Value::as_str) {
                Some("pending") => CaptureOutcome::Queued,
                Some("reduced" | "sealed") => CaptureOutcome::Captured,
                // The route creates only these receipt states. Do not turn a
                // future, malformed response into a successful capture.
                _ => CaptureOutcome::StoreUnavailable,
            },
            receipt
                .get("replayed")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
        Err(HookSessionRouteError::InvalidRequest | HookSessionRouteError::AuthorityRejected) => {
            (CaptureOutcome::Rejected, false)
        }
        // After authority and bounded delivery parameters are admitted, an
        // unavailable route dependency means the delivery was not committed.
        Err(HookSessionRouteError::Unavailable) => (CaptureOutcome::StoreUnavailable, false),
    };
    record_capture_metric(identity, context, outcome, idempotent_replay);
}

fn record_capture_metric(
    identity: &WorkspaceIdentity,
    context: CaptureMetricContext,
    outcome: CaptureOutcome,
    idempotent_replay: bool,
) {
    // The ledger deliberately receives no binding, delivery, session,
    // capability, path, hash, payload, or error value. Metrics are
    // best-effort: a ledger outage must not change capture acknowledgement.
    if let Err(error) = AdoptionMetricsStore::new(&identity.repository_root).record_capture_outcome(
        CaptureMetricRecord {
            integration: context.integration.to_string(),
            schema_version: context.schema_version,
            extractor_version: CAPTURE_EXTRACTOR_VERSION,
            outcome,
        },
        idempotent_replay,
    ) {
        tracing::warn!(
            %error,
            outcome = outcome.label(),
            "failed to record content-free hook capture telemetry"
        );
    }
}

fn capture_metric_integration(integration: &str) -> &'static str {
    match integration {
        "codex/v1" | "codex-hooks/v1" => "codex",
        "claude-code/v1" | "claude-code-hooks/v1" => "claude-code",
        "cursor/v1" | "cursor-hooks/v1" => "cursor",
        "generic/v1" | "generic-hooks/v1" => "generic",
        _ => "other",
    }
}

fn capture_schema_version(event: &Value) -> u32 {
    event
        .get("schema_version")
        .and_then(Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .unwrap_or_default()
}

struct HookMemoryStores {
    repository: MemoryStore,
    shared: Option<MemoryStore>,
    organization_id: Option<String>,
}

#[derive(Clone)]
struct RankedHookMemory {
    result: MemoryRecallResult,
    class: MemoryClass,
    relevance_bps: u16,
}

struct HookMemoryClasses {
    repository: BTreeMap<String, MemoryClass>,
    shared: BTreeMap<String, MemoryClass>,
}

fn present_hook_memory(
    identity: &WorkspaceIdentity,
    outcome: &crate::hook_session_registry::RegistryOpenOutcome,
    client: &'static str,
    request: HookPresentationParams,
) -> Result<Option<HookPresentationResult>, HookSessionRouteError> {
    validate_presentation_request(identity, &request)?;
    let session_id = encode_hex(outcome.internal_session_id.as_bytes());
    let metrics = AdoptionMetricsStore::new(&identity.repository_root);

    // An action is admitted only as an explicit exact-id claim on a later
    // request that has already authenticated by opening/resuming this lease.
    // There is deliberately no time/proximity or edited-file inference here.
    if let Some(injection_id) = request.acted_on_injection_id.as_deref() {
        if !valid_injection_id(injection_id) {
            return Err(HookSessionRouteError::InvalidRequest);
        }
        let action_metric_id = format!(
            "hook-action-v1:{}:{}:{}",
            session_id, request.request_id, injection_id
        );
        if let Err(error) = metrics.record_memory_injection_action_once(
            &action_metric_id,
            MemoryInjectionActionRecord {
                injection_id: injection_id.to_string(),
                acted_count: 1,
            },
        ) {
            // Telemetry is observational. The authenticated action claim and
            // memory presentation remain valid even when its metric cannot be
            // persisted; do not turn an acknowledged claim into a false
            // delivery failure.
            tracing::warn!(%error, "failed to record hook memory action telemetry");
        }
    }

    let repository_state = resolve_repository_state(&identity.checkout_root)?;
    let stores = HookMemoryStores::open(identity)?;
    let authority = MemoryQueryAuthority::new(
        identity.repository_id.clone(),
        identity.checkout_root.to_string_lossy().to_string(),
        repository_state.branch().map(str::to_owned),
        session_id.clone(),
        stores.organization_id.clone(),
    )
    .map_err(|_| HookSessionRouteError::Unavailable)?;
    let router = MemoryStoreRouter::new(&stores.repository, stores.shared.as_ref(), authority)
        .map_err(|_| HookSessionRouteError::Unavailable)?;
    if let Some(receipt) = request.acknowledge_delivery.as_ref() {
        acknowledge_hook_delivery(&stores, &router, &session_id, receipt)?;
    }
    let dirty_files = match request.kind {
        HookPresentationKind::SessionStart => dirty_working_set(&identity.checkout_root)?,
        _ => Vec::new(),
    };
    // Scope, checkout and lifecycle filtering happen in the router before the
    // bounded candidate set. Never start from an unfiltered recent-memory scan.
    let retrieval_query = presentation_query(&request, &dirty_files);
    let candidates = match retrieval_query.as_deref() {
        Some(query) => router
            .recall(Some(query), PRESENTATION_CANDIDATE_LIMIT)
            .map_err(|_| HookSessionRouteError::Unavailable)?,
        None => Vec::new(),
    };
    let classes = stores.memory_classes(&candidates)?;
    let mut ranked = Vec::new();
    for result in candidates {
        let class = classes.class_for(&result)?;
        let relevance_bps = presentation_relevance(
            &request,
            &result,
            repository_state.branch(),
            &dirty_files,
            class,
        );
        if passes_presentation_gate(relevance_bps) {
            ranked.push(RankedHookMemory {
                result,
                class,
                relevance_bps,
            });
        }
    }
    ranked.sort_by(|left, right| {
        right
            .relevance_bps
            .cmp(&left.relevance_bps)
            .then_with(|| {
                right
                    .result
                    .memory
                    .confidence
                    .partial_cmp(&left.result.memory.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.result.memory_id.cmp(&right.result.memory_id))
    });
    let (limit, budget_tokens, channel) = match request.kind {
        HookPresentationKind::SessionStart => (
            SESSION_PRESENTATION_LIMIT,
            SESSION_PRESENTATION_BUDGET_TOKENS,
            "hook-session-start",
        ),
        HookPresentationKind::UserPromptSubmit => (
            PROMPT_PRESENTATION_LIMIT,
            PROMPT_PRESENTATION_BUDGET_TOKENS,
            "hook-user-prompt-submit",
        ),
        HookPresentationKind::PostToolUse => (
            1,
            POST_TOOL_PRESENTATION_BUDGET_TOKENS,
            "hook-post-tool-use",
        ),
    };
    ranked.truncate(limit);
    if ranked.is_empty() {
        return Ok(None);
    }
    let injection_id = stable_injection_id(outcome, &request, &ranked);
    let (context, shown_ids) =
        render_hook_presentation(request.kind, &injection_id, &ranked, budget_tokens);
    let shown_count = shown_ids.len();
    if shown_count == 0 {
        return Ok(None);
    }
    if let Err(error) = metrics.record_memory_injection_once(
        &format!("hook-injection-v1:{injection_id}"),
        MemoryInjectionRecord {
            session_id: session_id.clone(),
            client: client.to_string(),
            channel: channel.to_string(),
            injection_id: injection_id.clone(),
            shown_count: shown_count as u64,
        },
    ) {
        // A rendered briefing remains true regardless of telemetry storage.
        // The stable injection id still binds retries to the same presentation.
        tracing::warn!(%error, injection_id = %injection_id, "failed to record hook memory injection telemetry");
    }
    // Rendering is prefix-preserving. Bind receipts only to that rendered
    // prefix; candidates clipped by the token budget were never delivered.
    let shown = ranked
        .iter()
        .filter(|entry| shown_ids.contains(&entry.result.memory_id.encoded()))
        .cloned()
        .collect::<Vec<_>>();
    let memory_deliveries =
        attempt_hook_deliveries(&stores, &router, &session_id, &injection_id, &shown)?;
    Ok(Some(HookPresentationResult {
        injection_id,
        context,
        memory_deliveries,
    }))
}

fn attempt_hook_deliveries(
    stores: &HookMemoryStores,
    router: &MemoryStoreRouter<'_>,
    session_id: &str,
    injection_id: &str,
    ranked: &[RankedHookMemory],
) -> Result<Vec<HookMemoryDeliveryReceipt>, HookSessionRouteError> {
    let owners = [
        (
            MemoryRecallTier::Repository,
            &stores.repository,
            format!("repository:{}", router.authority().repository_id),
        ),
        (
            MemoryRecallTier::Organization,
            stores.shared.as_ref().unwrap_or(&stores.repository),
            router
                .authority()
                .organization_id
                .as_ref()
                .map(|id| format!("organization:{id}"))
                .unwrap_or_default(),
        ),
    ];
    owners
        .into_iter()
        .filter_map(|(tier, store, authority)| {
            if authority.is_empty() {
                return None;
            }
            let selected = ranked
                .iter()
                .filter(|entry| entry.result.source_tier == tier)
                .collect::<Vec<_>>();
            if selected.is_empty() {
                return None;
            }
            let projection = selected.iter().map(|entry| serde_json::json!({
            "id": entry.result.memory_id.encoded(), "content": entry.result.memory.content,
        })).collect::<Vec<_>>();
            let payload_hash = format!(
                "sha256:{}",
                encode_hex(&sha256(
                    &serde_json::to_vec(&projection).unwrap_or_default()
                ))
            );
            let delivery_id = format!(
                "hdel_{}",
                encode_hex(&sha256(format!("{injection_id}\0{authority}").as_bytes())[..16])
            );
            let ids = selected
                .iter()
                .map(|entry| entry.result.memory.id.clone())
                .collect::<Vec<_>>();
            let binding = DeliveryBinding {
                delivery_id: &delivery_id,
                repository_id: &authority,
                session_id,
                payload_hash: &payload_hash,
            };
            Some(
                match store.attempt_memory_delivery(&binding, &ids, now_epoch_seconds()) {
                    Ok(()) => Ok(HookMemoryDeliveryReceipt {
                        authority,
                        delivery_id,
                        payload_hash,
                        ack_required: true,
                    }),
                    Err(error) => {
                        tracing::warn!(%error, "failed to record attempted hook memory delivery");
                        Err(HookSessionRouteError::Unavailable)
                    }
                },
            )
        })
        .collect::<Result<Vec<_>, _>>()
}

fn acknowledge_hook_delivery(
    stores: &HookMemoryStores,
    router: &MemoryStoreRouter<'_>,
    session_id: &str,
    receipt: &HookMemoryDeliveryReceipt,
) -> Result<(), HookSessionRouteError> {
    let repository_authority = format!("repository:{}", router.authority().repository_id);
    let organization_authority = router
        .authority()
        .organization_id
        .as_ref()
        .map(|id| format!("organization:{id}"));
    let store = if receipt.authority == repository_authority {
        &stores.repository
    } else if organization_authority.as_deref() == Some(receipt.authority.as_str()) {
        stores
            .shared
            .as_ref()
            .ok_or(HookSessionRouteError::Unavailable)?
    } else {
        return Err(HookSessionRouteError::InvalidRequest);
    };
    let binding = DeliveryBinding {
        delivery_id: &receipt.delivery_id,
        repository_id: &receipt.authority,
        session_id,
        payload_hash: &receipt.payload_hash,
    };
    let now = now_epoch_seconds();
    let acknowledged = store
        .acknowledge_memory_delivery(&binding, now)
        .map_err(|_| HookSessionRouteError::InvalidRequest)?;
    if acknowledged == 0
        && !store
            .memory_delivery_acknowledgement_was_recorded(&binding, now)
            .map_err(|_| HookSessionRouteError::InvalidRequest)?
    {
        return Err(HookSessionRouteError::InvalidRequest);
    }
    Ok(())
}

fn now_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

impl HookMemoryStores {
    fn open(identity: &WorkspaceIdentity) -> Result<Self, HookSessionRouteError> {
        std::fs::create_dir_all(&identity.repository_lattice_dir)
            .map_err(|_| HookSessionRouteError::Unavailable)?;
        let repository = MemoryStore::open(&identity.memories_path())
            .map_err(|_| HookSessionRouteError::Unavailable)?;
        let config = hook_shared_memory_config()?;
        let (shared, organization_id) = match config {
            Some((organization_id, path)) => (
                Some(MemoryStore::open(&path).map_err(|_| HookSessionRouteError::Unavailable)?),
                Some(organization_id),
            ),
            None => (None, None),
        };
        Ok(Self {
            repository,
            shared,
            organization_id,
        })
    }

    fn memory_classes(
        &self,
        results: &[MemoryRecallResult],
    ) -> Result<HookMemoryClasses, HookSessionRouteError> {
        let repository_ids = results
            .iter()
            .filter(|result| result.source_tier == MemoryRecallTier::Repository)
            .map(|result| result.memory.id.as_str())
            .collect::<Vec<_>>();
        let shared_ids = results
            .iter()
            .filter(|result| result.source_tier == MemoryRecallTier::Organization)
            .map(|result| result.memory.id.as_str())
            .collect::<Vec<_>>();
        let repository = load_memory_classes(&self.repository, &repository_ids)?;
        let shared = match (self.shared.as_ref(), shared_ids.is_empty()) {
            (_, true) => BTreeMap::new(),
            (Some(store), false) => load_memory_classes(store, &shared_ids)?,
            (None, false) => return Err(HookSessionRouteError::Unavailable),
        };
        Ok(HookMemoryClasses { repository, shared })
    }
}

impl HookMemoryClasses {
    fn class_for(&self, result: &MemoryRecallResult) -> Result<MemoryClass, HookSessionRouteError> {
        let classes = match result.source_tier {
            MemoryRecallTier::Repository => &self.repository,
            MemoryRecallTier::Organization => &self.shared,
        };
        classes
            .get(&result.memory.id)
            .copied()
            .ok_or(HookSessionRouteError::Unavailable)
    }
}

fn load_memory_classes(
    store: &MemoryStore,
    ids: &[&str],
) -> Result<BTreeMap<String, MemoryClass>, HookSessionRouteError> {
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    store
        .with_connection(|connection| {
            let placeholders = std::iter::repeat("?")
                .take(ids.len())
                .collect::<Vec<_>>()
                .join(",");
            let mut statement = connection
                .prepare(&format!(
                    "SELECT id, memory_class FROM memories WHERE id IN ({placeholders})"
                ))
                .map_err(|error| lattice_core::LatticeError::Storage(error.to_string()))?;
            let rows = statement
                .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|error| lattice_core::LatticeError::Storage(error.to_string()))?;
            let mut classes = BTreeMap::new();
            for row in rows {
                let (id, class) =
                    row.map_err(|error| lattice_core::LatticeError::Storage(error.to_string()))?;
                classes.insert(id, MemoryClass::from_str(&class));
            }
            Ok(classes)
        })
        .map_err(|_| HookSessionRouteError::Unavailable)
}

fn validate_presentation_request(
    identity: &WorkspaceIdentity,
    request: &HookPresentationParams,
) -> Result<(), HookSessionRouteError> {
    if request.request_id.is_empty()
        || request.request_id.len() > MAX_REQUEST_ID_BYTES
        || !request
            .request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(HookSessionRouteError::InvalidRequest);
    }
    match request.kind {
        HookPresentationKind::SessionStart
            if request.prompt.is_some() || request.path.is_some() =>
        {
            Err(HookSessionRouteError::InvalidRequest)
        }
        HookPresentationKind::UserPromptSubmit => {
            let prompt = request
                .prompt
                .as_deref()
                .filter(|prompt| !prompt.trim().is_empty())
                .ok_or(HookSessionRouteError::InvalidRequest)?;
            if prompt.len() > MAX_PROMPT_BYTES || request.path.is_some() {
                return Err(HookSessionRouteError::InvalidRequest);
            }
            Ok(())
        }
        HookPresentationKind::PostToolUse => {
            if request.prompt.is_some() {
                return Err(HookSessionRouteError::InvalidRequest);
            }
            let path = request
                .path
                .as_deref()
                .ok_or(HookSessionRouteError::InvalidRequest)?;
            validate_event_path(
                &identity.checkout_root,
                &SessionCaptureFact::EditedPath {
                    path: path.to_string(),
                },
            )
        }
        HookPresentationKind::SessionStart => Ok(()),
    }
}

fn presentation_relevance(
    request: &HookPresentationParams,
    result: &MemoryRecallResult,
    branch: Option<&str>,
    dirty_files: &[String],
    class: MemoryClass,
) -> u16 {
    if matches!(request.kind, HookPresentationKind::PostToolUse) {
        if !matches!(
            class,
            MemoryClass::Decision | MemoryClass::Constraint | MemoryClass::AntiPattern
        ) {
            return 0;
        }
        let Some(path) = request.path.as_deref() else {
            return 0;
        };
        return if result
            .memory
            .linked_files
            .iter()
            .any(|linked| same_repo_path(linked, path))
        {
            10_000
        } else {
            0
        };
    }

    let mut score = (result.memory.confidence.clamp(0.0, 1.0) * 1_000.0).round() as u16;
    let subject = match request.kind {
        HookPresentationKind::UserPromptSubmit => request.prompt.as_deref().unwrap_or_default(),
        HookPresentationKind::SessionStart => "",
        HookPresentationKind::PostToolUse => unreachable!(),
    };
    let terms = query_terms(subject);
    if !terms.is_empty() {
        let haystack = format!(
            "{} {} {}",
            result.memory.content,
            result.memory.linked_files.join(" "),
            result.memory.linked_symbols.join(" ")
        )
        .to_ascii_lowercase();
        let matched = terms
            .iter()
            .filter(|term| haystack.contains(term.as_str()))
            .count();
        score = score.saturating_add(((matched * 7_000) / terms.len()) as u16);
    }
    if matches!(request.kind, HookPresentationKind::SessionStart) {
        if branch.is_some() && result.memory.branch.as_deref() == branch {
            score = score.saturating_add(2_500);
        }
        if result.memory.linked_files.iter().any(|linked| {
            dirty_files
                .iter()
                .any(|dirty| same_repo_path(linked, dirty))
        }) {
            score = score.saturating_add(7_000);
        }
    }
    score.min(10_000)
}

fn passes_presentation_gate(relevance_bps: u16) -> bool {
    relevance_bps >= MIN_PRESENTATION_RELEVANCE_BPS
}

fn stable_injection_id(
    outcome: &crate::hook_session_registry::RegistryOpenOutcome,
    request: &HookPresentationParams,
    ranked: &[RankedHookMemory],
) -> String {
    let mut material = Vec::new();
    material.extend_from_slice(b"lattice.hook-injection.v1\0");
    material.extend_from_slice(outcome.binding_id.as_bytes());
    material.extend_from_slice(&outcome.generation.to_be_bytes());
    material.extend_from_slice(request.request_id.as_bytes());
    material.push(match request.kind {
        HookPresentationKind::SessionStart => 1,
        HookPresentationKind::UserPromptSubmit => 2,
        HookPresentationKind::PostToolUse => 3,
    });
    for memory in ranked {
        material.extend_from_slice(memory.result.memory_id.encoded().as_bytes());
        material.push(0);
    }
    format!("hinj_{}", encode_hex(&sha256(&material)[..16]))
}

fn render_hook_presentation(
    kind: HookPresentationKind,
    injection_id: &str,
    ranked: &[RankedHookMemory],
    budget_tokens: usize,
) -> (String, Vec<String>) {
    let char_budget = budget_tokens.saturating_mul(4);
    if matches!(kind, HookPresentationKind::PostToolUse) {
        let memory = &ranked[0];
        let prefix = format!(
            "Lattice memory warning [injection_id={injection_id}] [{}] ({}; {}): ",
            memory.result.memory_id.encoded(),
            memory.class.as_str(),
            presentation_trust_note(&memory.result),
        );
        let available = char_budget.saturating_sub(prefix.chars().count());
        let content = clipped_one_line(&memory.result.memory.content, available);
        return (
            clipped_text(&format!("{prefix}{content}"), char_budget),
            vec![memory.result.memory_id.encoded()],
        );
    }

    let mut output = format!("Lattice memory context [injection_id={injection_id}]:");
    let mut shown_ids = Vec::new();
    for memory in ranked {
        let prefix = format!(
            "\n- [{}] {} ({}) : ",
            memory.result.memory_id.encoded(),
            memory.class.as_str(),
            presentation_trust_note(&memory.result),
        );
        let used = output.chars().count() + prefix.chars().count();
        if used >= char_budget {
            break;
        }
        let content = clipped_one_line(&memory.result.memory.content, char_budget - used);
        if content.is_empty() {
            break;
        }
        output.push_str(&prefix);
        output.push_str(&content);
        shown_ids.push(memory.result.memory_id.encoded());
    }
    (clipped_text(&output, char_budget), shown_ids)
}

fn presentation_query(request: &HookPresentationParams, dirty_files: &[String]) -> Option<String> {
    let value = match request.kind {
        HookPresentationKind::UserPromptSubmit => {
            request.prompt.as_deref().unwrap_or_default().to_string()
        }
        HookPresentationKind::PostToolUse => {
            request.path.as_deref().unwrap_or_default().to_string()
        }
        HookPresentationKind::SessionStart => dirty_files.join(" "),
    };
    let terms = query_terms(&value);
    (!terms.is_empty()).then(|| terms.join(" "))
}

fn presentation_trust_note(result: &MemoryRecallResult) -> String {
    let status = result.effective_verification_status.as_str();
    if result.cross_repo {
        format!("advisory {status}; {}", result.trust_reason)
    } else if result.memory.verification_status == MemoryVerificationStatus::Contradicted {
        "conflicts with current evidence; verify before use".to_string()
    } else if result.memory.verification_status == MemoryVerificationStatus::Unverified {
        "hypothesis; validate with a focused check".to_string()
    } else {
        format!("{status}; {}", result.trust_reason)
    }
}

fn clipped_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return "…".to_string();
    }
    let mut clipped = value.chars().take(max_chars - 1).collect::<String>();
    clipped.push('…');
    clipped
}

fn clipped_one_line(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    if max_chars == 1 {
        return "…".to_string();
    }
    let mut clipped = normalized.chars().take(max_chars - 1).collect::<String>();
    clipped.push('…');
    clipped
}

fn query_terms(value: &str) -> Vec<String> {
    let mut terms = value
        .split(|character: char| !(character.is_alphanumeric() || "/._-".contains(character)))
        .filter(|term| term.chars().count() >= 3)
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    terms.sort();
    terms.dedup();
    terms.truncate(32);
    terms
}

fn same_repo_path(left: &str, right: &str) -> bool {
    left.trim_start_matches("./").replace('\\', "/")
        == right.trim_start_matches("./").replace('\\', "/")
}

fn valid_injection_id(value: &str) -> bool {
    value.len() == 37
        && value.starts_with("hinj_")
        && value[5..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn dirty_working_set(checkout_root: &Path) -> Result<Vec<String>, HookSessionRouteError> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(checkout_root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|_| HookSessionRouteError::Unavailable)?;
    if !output.status.success() {
        return Err(HookSessionRouteError::Unavailable);
    }
    let mut paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|record| {
            (record.len() > 3).then(|| String::from_utf8_lossy(&record[3..]).into_owned())
        })
        .take(64)
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Loads the same operator-owned authority contract as the D2 MCP runtime.
/// Request payloads and repository files never grant organization access.
fn hook_shared_memory_config() -> Result<Option<(String, PathBuf)>, HookSessionRouteError> {
    let mut configured_organization = None;
    let mut configured_path = None;
    if let Some(home) = std::env::var_os("HOME") {
        let config_path = PathBuf::from(&home).join(".lattice/config.toml");
        if let Ok(text) = std::fs::read_to_string(config_path) {
            let mut in_memory = false;
            for raw in text.lines() {
                let line = raw.split('#').next().unwrap_or_default().trim();
                if line.starts_with('[') && line.ends_with(']') {
                    in_memory = line == "[memory]";
                    continue;
                }
                if !in_memory || line.is_empty() {
                    continue;
                }
                let Some((key, value)) = line.split_once('=') else {
                    return Err(HookSessionRouteError::Unavailable);
                };
                let value = value.trim().trim_matches('"').trim_matches('\'');
                match key.trim() {
                    "organization_id" => configured_organization = Some(value.to_string()),
                    "shared_store_path" => configured_path = Some(PathBuf::from(value)),
                    _ => {}
                }
            }
        }
    }
    let organization_id = match std::env::var("LATTICE_ORGANIZATION_ID") {
        Ok(value) if !value.trim().is_empty() => Some(value),
        Ok(_) => return Err(HookSessionRouteError::Unavailable),
        Err(std::env::VarError::NotPresent) => configured_organization,
        Err(_) => return Err(HookSessionRouteError::Unavailable),
    };
    let Some(organization_id) = organization_id else {
        return Ok(None);
    };
    let path = match std::env::var_os("LATTICE_SHARED_MEMORY_PATH") {
        Some(value) => PathBuf::from(value),
        None => configured_path
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".lattice/shared/memories.db"))
            })
            .ok_or(HookSessionRouteError::Unavailable)?,
    };
    if !path.is_absolute() {
        return Err(HookSessionRouteError::Unavailable);
    }
    Ok(Some((organization_id, path)))
}

struct JournalDelivery<'a> {
    binding_id: &'a RegistryId,
    delivery_id: &'a RegistryId,
    sequence: u64,
    kind: RegistryDeliveryKind,
    normalized_hash: RegistryHash,
    normalized_json: &'a str,
    branch: Option<&'a str>,
    revision: &'a str,
    received_at_ms: i64,
}

struct JournalRow {
    sequence: u64,
    kind: RegistryDeliveryKind,
    normalized_hash: RegistryHash,
    normalized_json: String,
    branch: Option<String>,
    revision: String,
    received_at_ms: i64,
}

struct CaptureJournal {
    connection: Connection,
}

impl CaptureJournal {
    fn open(identity: &WorkspaceIdentity) -> Result<Self, HookSessionRouteError> {
        let directory = &identity.repository_lattice_dir;
        if directory.exists() {
            let metadata = std::fs::symlink_metadata(directory)
                .map_err(|_| HookSessionRouteError::Unavailable)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(HookSessionRouteError::AuthorityRejected);
            }
        } else {
            std::fs::create_dir_all(directory).map_err(|_| HookSessionRouteError::Unavailable)?;
        }
        let canonical = directory
            .canonicalize()
            .map_err(|_| HookSessionRouteError::Unavailable)?;
        let repository_root = identity
            .repository_root
            .canonicalize()
            .map_err(|_| HookSessionRouteError::Unavailable)?;
        if !canonical.starts_with(&repository_root) {
            return Err(HookSessionRouteError::AuthorityRejected);
        }
        let journal_path = canonical.join("hook-capture.db");
        ensure_private_database_file(&journal_path)
            .map_err(|_| HookSessionRouteError::Unavailable)?;
        let connection =
            Connection::open(&journal_path).map_err(|_| HookSessionRouteError::Unavailable)?;
        validate_private_file(&journal_path, "hook capture journal")
            .map_err(|_| HookSessionRouteError::Unavailable)?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 CREATE TABLE IF NOT EXISTS hook_capture_journal (
                    binding_id BLOB NOT NULL,
                    delivery_id BLOB NOT NULL,
                    sequence_number INTEGER NOT NULL CHECK(sequence_number > 0),
                    delivery_kind TEXT NOT NULL CHECK(delivery_kind IN ('event','turn_summary','close')),
                    normalized_hash BLOB NOT NULL CHECK(length(normalized_hash) = 32),
                    normalized_json TEXT NOT NULL,
                    branch TEXT,
                    revision TEXT NOT NULL,
                    received_at_ms INTEGER NOT NULL,
                    PRIMARY KEY(binding_id, delivery_id),
                    UNIQUE(binding_id, sequence_number)
                 );",
            )
            .map_err(|_| HookSessionRouteError::Unavailable)?;
        let schema_sql: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'hook_capture_journal'",
                [],
                |row| row.get(0),
            )
            .map_err(|_| HookSessionRouteError::Unavailable)?;
        if !schema_sql.contains("turn_summary") {
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     ALTER TABLE hook_capture_journal RENAME TO hook_capture_journal_v2;
                     CREATE TABLE hook_capture_journal (
                        binding_id BLOB NOT NULL,
                        delivery_id BLOB NOT NULL,
                        sequence_number INTEGER NOT NULL CHECK(sequence_number > 0),
                        delivery_kind TEXT NOT NULL CHECK(delivery_kind IN ('event','turn_summary','close')),
                        normalized_hash BLOB NOT NULL CHECK(length(normalized_hash) = 32),
                        normalized_json TEXT NOT NULL,
                        branch TEXT,
                        revision TEXT NOT NULL,
                        received_at_ms INTEGER NOT NULL,
                        PRIMARY KEY(binding_id, delivery_id),
                        UNIQUE(binding_id, sequence_number)
                     );
                     INSERT INTO hook_capture_journal
                     SELECT * FROM hook_capture_journal_v2;
                     DROP TABLE hook_capture_journal_v2;
                     COMMIT;",
                )
                .map_err(|_| HookSessionRouteError::Unavailable)?;
        }
        if let Ok(cutoff) = now_ms().map(|now| now.saturating_sub(RETENTION_MS)) {
            connection
                .execute(
                    "DELETE FROM hook_capture_journal WHERE received_at_ms <= ?1",
                    params![cutoff],
                )
                .map_err(|_| HookSessionRouteError::Unavailable)?;
        }
        Ok(Self { connection })
    }

    fn stage(&self, delivery: JournalDelivery<'_>) -> Result<(), HookSessionRouteError> {
        let sequence =
            i64::try_from(delivery.sequence).map_err(|_| HookSessionRouteError::InvalidRequest)?;
        let existing = self
            .connection
            .query_row(
                "SELECT sequence_number, delivery_kind, normalized_hash
                 FROM hook_capture_journal
                 WHERE binding_id = ?1 AND delivery_id = ?2",
                params![
                    delivery.binding_id.as_bytes(),
                    delivery.delivery_id.as_bytes()
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| HookSessionRouteError::Unavailable)?;
        let kind = delivery_kind_str(delivery.kind);
        if let Some((stored_sequence, stored_kind, stored_hash)) = existing {
            if stored_sequence == sequence
                && stored_kind == kind
                && stored_hash.as_slice() == delivery.normalized_hash.as_bytes()
            {
                return Ok(());
            }
            return Err(HookSessionRouteError::AuthorityRejected);
        }
        let count: i64 = self
            .connection
            .query_row(
                "SELECT COUNT(*) FROM hook_capture_journal WHERE binding_id = ?1",
                params![delivery.binding_id.as_bytes()],
                |row| row.get(0),
            )
            .map_err(|_| HookSessionRouteError::Unavailable)?;
        if count >= MAX_JOURNAL_ROWS_PER_BINDING {
            return Err(HookSessionRouteError::Unavailable);
        }
        self.connection
            .execute(
                "INSERT INTO hook_capture_journal
                 (binding_id, delivery_id, sequence_number, delivery_kind,
                  normalized_hash, normalized_json, branch, revision, received_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    delivery.binding_id.as_bytes(),
                    delivery.delivery_id.as_bytes(),
                    sequence,
                    kind,
                    delivery.normalized_hash.as_bytes(),
                    delivery.normalized_json,
                    delivery.branch,
                    delivery.revision,
                    delivery.received_at_ms,
                ],
            )
            .map_err(|error| match error {
                rusqlite::Error::SqliteFailure(ref failure, _)
                    if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    HookSessionRouteError::AuthorityRejected
                }
                _ => HookSessionRouteError::Unavailable,
            })?;
        Ok(())
    }

    fn row(
        &self,
        binding_id: &RegistryId,
        delivery_id: &RegistryId,
    ) -> Result<Option<JournalRow>, HookSessionRouteError> {
        self.connection
            .query_row(
                "SELECT sequence_number, delivery_kind, normalized_hash,
                        normalized_json, branch, revision, received_at_ms
                 FROM hook_capture_journal
                 WHERE binding_id = ?1 AND delivery_id = ?2",
                params![binding_id.as_bytes(), delivery_id.as_bytes()],
                journal_row,
            )
            .optional()
            .map_err(|_| HookSessionRouteError::Unavailable)
    }

    fn events_before(
        &self,
        binding_id: &RegistryId,
        close_sequence: u64,
    ) -> Result<Vec<JournalRow>, HookSessionRouteError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT sequence_number, delivery_kind, normalized_hash,
                        normalized_json, branch, revision, received_at_ms
                 FROM hook_capture_journal
                 WHERE binding_id = ?1 AND delivery_kind IN ('event', 'turn_summary')
                   AND sequence_number < ?2
                 ORDER BY sequence_number ASC",
            )
            .map_err(|_| HookSessionRouteError::Unavailable)?;
        let rows = statement
            .query_map(
                params![
                    binding_id.as_bytes(),
                    i64::try_from(close_sequence).unwrap_or(i64::MAX)
                ],
                journal_row,
            )
            .map_err(|_| HookSessionRouteError::Unavailable)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|_| HookSessionRouteError::Unavailable)
    }

    fn delete_binding(&self, binding_id: &RegistryId) -> Result<(), HookSessionRouteError> {
        self.connection
            .execute(
                "DELETE FROM hook_capture_journal WHERE binding_id = ?1",
                params![binding_id.as_bytes()],
            )
            .map_err(|_| HookSessionRouteError::Unavailable)?;
        Ok(())
    }
}

fn journal_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<JournalRow> {
    let kind = match row.get::<_, String>(1)?.as_str() {
        "event" => RegistryDeliveryKind::Event,
        "turn_summary" => RegistryDeliveryKind::TurnSummary,
        "close" => RegistryDeliveryKind::Close,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    let hash: Vec<u8> = row.get(2)?;
    let hash: [u8; 32] = hash.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(JournalRow {
        sequence: u64::try_from(row.get::<_, i64>(0)?)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        kind,
        normalized_hash: RegistryHash::from_bytes(hash),
        normalized_json: row.get(3)?,
        branch: row.get(4)?,
        revision: row.get(5)?,
        received_at_ms: row.get(6)?,
    })
}

fn delivery_kind_str(kind: RegistryDeliveryKind) -> &'static str {
    match kind {
        RegistryDeliveryKind::Event => "event",
        RegistryDeliveryKind::TurnSummary => "turn_summary",
        RegistryDeliveryKind::Close => "close",
    }
}

fn drain_binding(
    registry: &mut HookSessionRegistry,
    identity: &WorkspaceIdentity,
    verification: &RegistryVerification,
    binding_id: &RegistryId,
    now_ms: i64,
) -> Result<(), HookSessionRouteError> {
    let journal = CaptureJournal::open(identity)?;
    for pending in registry
        .pending_for_binding(binding_id, now_ms, RECOVERY_BATCH)
        .map_err(map_registry_error)?
    {
        let binding = registry
            .binding(binding_id)
            .map_err(map_registry_error)?
            .ok_or(HookSessionRouteError::AuthorityRejected)?;
        if pending.sequence != binding.next_sequence {
            break;
        }
        let Some(row) = journal.row(binding_id, &pending.delivery_id)? else {
            defer_pending(registry, &pending, now_ms)?;
            return Err(HookSessionRouteError::Unavailable);
        };
        if row.sequence != pending.sequence
            || row.kind != pending.kind
            || row.normalized_hash != pending.normalized_hash
        {
            registry.revoke(binding_id).map_err(map_registry_error)?;
            return Err(HookSessionRouteError::AuthorityRejected);
        }
        if pending.kind == RegistryDeliveryKind::Close {
            if reduce_close(identity, verification, binding_id, &journal, &row).is_err() {
                defer_pending(registry, &pending, now_ms)?;
                return Err(HookSessionRouteError::Unavailable);
            }
        }
        registry
            .complete(RegistryCompletion {
                binding_id: binding_id.clone(),
                delivery_id: pending.delivery_id,
                normalized_hash: pending.normalized_hash,
                status: match pending.kind {
                    RegistryDeliveryKind::Event | RegistryDeliveryKind::TurnSummary => {
                        RegistryReceiptStatus::Reduced
                    }
                    RegistryDeliveryKind::Close => RegistryReceiptStatus::Sealed,
                },
                completed_at_ms: now_ms.max(pending.admitted_at_ms),
            })
            .map_err(map_registry_error)?;
        if pending.kind == RegistryDeliveryKind::Close {
            journal.delete_binding(binding_id)?;
        }
    }
    Ok(())
}

fn defer_pending(
    registry: &mut HookSessionRegistry,
    pending: &crate::hook_session_registry::RegistryPendingDelivery,
    now_ms: i64,
) -> Result<(), HookSessionRouteError> {
    let shift = pending.attempt_count.min(7) as u32;
    let delay = RETRY_BASE_MS
        .checked_mul(1_i64.checked_shl(shift).unwrap_or(i64::MAX))
        .unwrap_or(RETRY_MAX_MS)
        .min(RETRY_MAX_MS);
    registry
        .defer_pending(
            &pending.binding_id,
            &pending.delivery_id,
            pending.row_version,
            now_ms.saturating_add(delay),
        )
        .map_err(map_registry_error)?;
    Ok(())
}

fn reduce_close(
    identity: &WorkspaceIdentity,
    verification: &RegistryVerification,
    binding_id: &RegistryId,
    journal: &CaptureJournal,
    close_row: &JournalRow,
) -> Result<(), HookSessionRouteError> {
    let close = parse_session_capture_close(
        &close_row.normalized_json,
        DateTime::<Utc>::from_unix_seconds(close_row.received_at_ms / 1_000),
    )
    .map_err(|_| HookSessionRouteError::Unavailable)?;
    let mut segment = 1_u64;
    let mut previous_branch = verification.repository_state.branch().map(str::to_owned);
    let mut previous_revision = verification.repository_state.revision().to_owned();
    let session_id = encode_hex(verification.internal_session_id.as_bytes());
    let checkout_id = verification.checkout.checkout_id().to_owned();
    let repository_id = verification.checkout.repository_id().to_owned();
    let mut events = Vec::new();
    for row in journal.events_before(binding_id, close_row.sequence)? {
        if row.branch != previous_branch || row.revision != previous_revision {
            segment = segment
                .checked_add(1)
                .ok_or(HookSessionRouteError::Unavailable)?;
            previous_branch = row.branch.clone();
            previous_revision = row.revision.clone();
        }
        let event = parse_session_capture_event(&row.normalized_json)
            .map_err(|_| HookSessionRouteError::Unavailable)?;
        events.push(DaemonSessionCaptureEvent {
            authority: SessionDigestAuthority {
                session_id: session_id.clone(),
                repository_id: repository_id.clone(),
                checkout_id: Some(checkout_id.clone()),
                branch: row.branch,
                revision: row.revision,
                segment,
            },
            event,
        });
    }
    if close_row.branch != previous_branch || close_row.revision != previous_revision {
        segment = segment
            .checked_add(1)
            .ok_or(HookSessionRouteError::Unavailable)?;
    }
    let close_authority = SessionDigestAuthority {
        session_id: session_id.clone(),
        repository_id: repository_id.clone(),
        checkout_id: Some(checkout_id.clone()),
        branch: close_row.branch.clone(),
        revision: close_row.revision.clone(),
        segment,
    };
    let digests = reduce_session_capture(&events, &close, &close_authority)
        .map_err(|_| HookSessionRouteError::Unavailable)?;
    let store = MemoryStore::open(&identity.memories_path())
        .map_err(|_| HookSessionRouteError::Unavailable)?;
    for digest in &digests {
        let authority = MemoryQueryAuthority::new(
            repository_id.clone(),
            checkout_id.clone(),
            digest.branch.clone(),
            session_id.clone(),
            None,
        )
        .map_err(|_| HookSessionRouteError::Unavailable)?;
        let router = MemoryStoreRouter::new(&store, None, authority)
            .map_err(|_| HookSessionRouteError::Unavailable)?;
        let candidates = lattice_core::memory::extract_default_session_digest_candidates(digest);
        router
            .capture_session_digest_candidate_batch(digest, &candidates)
            .map_err(|_| HookSessionRouteError::Unavailable)?;
    }
    Ok(())
}

fn validate_event_path(
    checkout_root: &Path,
    fact: &SessionCaptureFact,
) -> Result<(), HookSessionRouteError> {
    let SessionCaptureFact::EditedPath { path } = fact else {
        return Ok(());
    };
    let root = checkout_root
        .canonicalize()
        .map_err(|_| HookSessionRouteError::AuthorityRejected)?;
    let mut cursor = root.clone();
    for component in Path::new(path).components() {
        let std::path::Component::Normal(component) = component else {
            return Err(HookSessionRouteError::InvalidRequest);
        };
        cursor.push(component);
        match std::fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                cursor = cursor
                    .canonicalize()
                    .map_err(|_| HookSessionRouteError::AuthorityRejected)?;
                if !cursor.starts_with(&root) {
                    return Err(HookSessionRouteError::AuthorityRejected);
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return Err(HookSessionRouteError::AuthorityRejected),
        }
    }
    let existing = nearest_existing_ancestor(&cursor)?;
    let canonical = existing
        .canonicalize()
        .map_err(|_| HookSessionRouteError::AuthorityRejected)?;
    if !canonical.starts_with(root) {
        return Err(HookSessionRouteError::AuthorityRejected);
    }
    Ok(())
}

fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf, HookSessionRouteError> {
    let mut candidate = path.to_path_buf();
    loop {
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !candidate.pop() {
                    return Err(HookSessionRouteError::AuthorityRejected);
                }
            }
            Err(_) => return Err(HookSessionRouteError::AuthorityRejected),
        }
    }
}

fn registry_open_request(
    integration: HookIntegrationId,
    host_session_id: HostSessionId,
    checkout: HookCheckoutIdentity,
    repository_state: HookRepositoryState,
    resume: Option<RegistrySessionResume>,
) -> Result<RegistryOpenRequest, HookSessionRouteError> {
    Ok(RegistryOpenRequest {
        integration,
        host_session_id,
        checkout,
        repository_state,
        resume,
        now_ms: now_ms()?,
        idle_ttl_ms: IDLE_TTL_MS,
        absolute_ttl_ms: ABSOLUTE_TTL_MS,
        retention_ms: RETENTION_MS,
    })
}

fn resolve_authority(hello: &ProxyRequest) -> Result<WorkspaceIdentity, HookSessionRouteError> {
    if hello.workspace_roots.len() != 1
        || !hello.focus_files.is_empty()
        || !hello.focus_dirs.is_empty()
    {
        return Err(HookSessionRouteError::AuthorityRejected);
    }
    let hello_root = PathBuf::from(&hello.workspace_roots[0])
        .canonicalize()
        .map_err(|_| HookSessionRouteError::AuthorityRejected)?;
    let identity = WorkspaceIdentity::resolve(&hello_root)
        .map_err(|_| HookSessionRouteError::AuthorityRejected)?;
    if hello_root != identity.checkout_root {
        return Err(HookSessionRouteError::AuthorityRejected);
    }
    Ok(identity)
}

fn resolve_repository_state(root: &Path) -> Result<HookRepositoryState, HookSessionRouteError> {
    let snapshot = crate::repo_state::resolve_repo_state(root)
        .ok_or(HookSessionRouteError::AuthorityRejected)?;
    let revision = snapshot
        .head_oid
        .ok_or(HookSessionRouteError::AuthorityRejected)?;
    let branch = snapshot
        .head_ref
        .and_then(|value| value.strip_prefix("refs/heads/").map(str::to_owned));
    HookRepositoryState::new(branch, revision).map_err(|_| HookSessionRouteError::Unavailable)
}

fn decode_resume(
    resume: HookSessionResumeParams,
) -> Result<RegistrySessionResume, HookSessionRouteError> {
    Ok(RegistrySessionResume {
        binding_id: HookBindingId::from_bytes(decode_hex::<16>(&resume.binding_id)?),
        capability: HookSessionCapability::from_bytes(decode_hex::<32>(&resume.capability)?),
    })
}

fn map_registry_error(error: HookRegistryError) -> HookSessionRouteError {
    match error {
        HookRegistryError::BindingAlreadyOpen
        | HookRegistryError::BindingConflict
        | HookRegistryError::BindingNotFound
        | HookRegistryError::InvalidCapability
        | HookRegistryError::AuthorityMismatch
        | HookRegistryError::Expired
        | HookRegistryError::Sealed
        | HookRegistryError::Revoked
        | HookRegistryError::ReplayViolation
        | HookRegistryError::OrderViolation => HookSessionRouteError::AuthorityRejected,
        HookRegistryError::InvalidConfiguration
        | HookRegistryError::InvalidIdentifier
        | HookRegistryError::InvalidValue
        | HookRegistryError::OrderWindowExceeded => HookSessionRouteError::InvalidRequest,
        _ => HookSessionRouteError::Unavailable,
    }
}

fn now_ms() -> Result<i64, HookSessionRouteError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| HookSessionRouteError::Unavailable)?
        .as_millis();
    i64::try_from(millis).map_err(|_| HookSessionRouteError::Unavailable)
}

fn default_state_root() -> Result<PathBuf> {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        let state = PathBuf::from(state);
        if !state.is_absolute() {
            anyhow::bail!("XDG_STATE_HOME must be absolute");
        }
        return Ok(state.join("lattice"));
    }
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    if !home.is_absolute() {
        anyhow::bail!("HOME must be absolute");
    }
    Ok(home.join(".local/state/lattice"))
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        return validate_private_directory(path);
    }
    let parent = path.context_parent()?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create hook-session state parent {}",
            parent.display()
        )
    })?;
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("failed to create hook-session state directory"),
    }
    validate_private_directory(path)
}

#[cfg(not(unix))]
fn ensure_private_directory(_path: &Path) -> Result<()> {
    anyhow::bail!("hook-session state requires Unix ownership and mode checks")
}

trait PathContextExt {
    fn context_parent(&self) -> Result<&Path>;
}

impl PathContextExt for Path {
    fn context_parent(&self) -> Result<&Path> {
        self.parent()
            .context("hook-session state directory has no parent")
    }
}

#[cfg(unix)]
fn validate_private_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!("hook-session state path is not a real directory");
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        anyhow::bail!("hook-session state directory has unsafe ownership");
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        anyhow::bail!("hook-session state directory must have mode 0700");
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_file(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("{label} is not a regular file");
    }
    validate_private_file_metadata(&metadata, label)
}

#[cfg(unix)]
fn validate_private_file_metadata(metadata: &std::fs::Metadata, label: &str) -> Result<()> {
    if !metadata.is_file() {
        anyhow::bail!("{label} is not a regular file");
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        anyhow::bail!("{label} has unsafe ownership");
    }
    if metadata.permissions().mode() & 0o777 != 0o600 {
        anyhow::bail!("{label} must have mode 0600");
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_file(_path: &Path, _label: &str) -> Result<()> {
    anyhow::bail!("hook-session state requires Unix ownership and mode checks")
}

#[cfg(unix)]
fn open_private_read(path: &Path, label: &str) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("failed to open {label}"))?;
    validate_private_file_metadata(&file.metadata()?, label)?;
    Ok(file)
}

#[cfg(unix)]
fn ensure_private_database_file(path: &Path) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        open_private_read(path, "hook-session registry")?;
        return Ok(());
    }
    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            open_private_read(path, "hook-session registry")?;
        }
        Err(error) => return Err(error).context("failed to create hook-session registry"),
    }
    validate_private_file(path, "hook-session registry")
}

#[cfg(not(unix))]
fn ensure_private_database_file(_path: &Path) -> Result<()> {
    anyhow::bail!("hook-session state requires Unix ownership and mode checks")
}

#[cfg(unix)]
fn load_or_create_key(path: &Path) -> Result<[u8; KEY_BYTES]> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => return read_key(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to inspect hook-session authority key"),
    }
    let parent = path.context_parent()?;
    let temporary = parent.join(format!(".authority-{}.tmp", random_suffix()?));
    let create_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        let secret = random_key()?;
        file.write_all(&secret)?;
        file.sync_all()?;
        match std::fs::hard_link(&temporary, path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).context("failed to install hook-session authority key")
            }
        }
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temporary);
    create_result?;
    read_key(path)
}

#[cfg(not(unix))]
fn load_or_create_key(_path: &Path) -> Result<[u8; KEY_BYTES]> {
    anyhow::bail!("hook-session state requires Unix ownership and mode checks")
}

fn read_key(path: &Path) -> Result<[u8; KEY_BYTES]> {
    let mut file = open_private_read(path, "hook-session authority key")?;
    if file.metadata()?.len() != KEY_BYTES as u64 {
        anyhow::bail!("hook-session authority key has an invalid length");
    }
    let mut secret = [0_u8; KEY_BYTES];
    file.read_exact(&mut secret)?;
    Ok(secret)
}

fn random_key() -> Result<[u8; KEY_BYTES]> {
    let mut secret = [0_u8; KEY_BYTES];
    File::open("/dev/urandom")?.read_exact(&mut secret)?;
    Ok(secret)
}

fn random_suffix() -> Result<String> {
    Ok(encode_hex(&random_key()?[..16]))
}

fn sha256(input: &[u8]) -> [u8; 32] {
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
    let mut padded = input.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    let mut state = INITIAL;
    for chunk in padded.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (index, bytes) in chunk.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes(bytes.try_into().expect("four byte word"));
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
            let temp1 = h
                .wrapping_add(e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25))
                .wrapping_add((e & f) ^ ((!e) & g))
                .wrapping_add(ROUND[index])
                .wrapping_add(words[index]);
            let temp2 = (a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22))
                .wrapping_add((a & b) ^ (a & c) ^ (b & c));
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
    let mut digest = [0_u8; 32];
    for (target, word) in digest.chunks_exact_mut(4).zip(state) {
        target.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

fn decode_hex<const N: usize>(value: &str) -> Result<[u8; N], HookSessionRouteError> {
    if value.len() != N * 2 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HookSessionRouteError::InvalidRequest);
    }
    let mut result = [0_u8; N];
    for (index, slot) in result.iter_mut().enumerate() {
        let offset = index * 2;
        *slot = (hex_nibble(value.as_bytes()[offset])? << 4)
            | hex_nibble(value.as_bytes()[offset + 1])?;
    }
    Ok(result)
}

fn hex_nibble(byte: u8) -> Result<u8, HookSessionRouteError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(HookSessionRouteError::InvalidRequest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_delivery_acknowledges_only_the_exact_presented_receipt() {
        let stores = HookMemoryStores {
            repository: MemoryStore::open_in_memory().expect("repository store"),
            shared: None,
            organization_id: None,
        };
        stores
            .repository
            .with_connection(|connection| {
                connection
                    .execute(
                        "INSERT INTO memories(id,content,memory_type,created_at,last_accessed,retention_grace_until) VALUES('m','hook delivery','fact',1,1,0)",
                        [],
                    )
                    .map(|_| ())
                    .map_err(|error| lattice_core::error::LatticeError::Storage(error.to_string()))
            })
            .expect("seed memory");
        let authority = MemoryQueryAuthority::new(
            "repo".to_string(),
            "checkout".to_string(),
            None,
            "hook-session".to_string(),
            None,
        )
        .expect("authority");
        let router = MemoryStoreRouter::new(&stores.repository, None, authority).expect("router");
        let receipt = HookMemoryDeliveryReceipt {
            authority: "repository:repo".to_string(),
            delivery_id: "hdel_receipt".to_string(),
            payload_hash: "sha256:payload".to_string(),
            ack_required: true,
        };
        let binding = DeliveryBinding {
            delivery_id: &receipt.delivery_id,
            repository_id: &receipt.authority,
            session_id: "hook-session",
            payload_hash: &receipt.payload_hash,
        };
        stores
            .repository
            .attempt_memory_delivery(&binding, &["m".to_string()], now_epoch_seconds())
            .expect("record final hook delivery");
        let mut wrong = receipt.clone();
        wrong.payload_hash = "sha256:wrong".to_string();
        assert!(matches!(
            acknowledge_hook_delivery(&stores, &router, "hook-session", &wrong),
            Err(HookSessionRouteError::InvalidRequest)
        ));
        acknowledge_hook_delivery(&stores, &router, "hook-session", &receipt)
            .expect("exact hook acknowledgement");
        acknowledge_hook_delivery(&stores, &router, "hook-session", &receipt)
            .expect("idempotent hook acknowledgement replay");
    }

    #[cfg(unix)]
    #[test]
    fn durable_route_reuses_key_and_rejects_unsafe_state() {
        let directory = test_directory("durable");
        HookSessionRoute::open_at(&directory).expect("create durable route");
        HookSessionRoute::open_at(&directory).expect("reopen durable route");
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(directory.join("authority.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(directory.join("sessions.db"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::set_permissions(
            directory.join("authority.key"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(HookSessionRoute::open_at(&directory).is_err());
        std::fs::remove_dir_all(directory).unwrap();

        let symlink_directory = test_directory("symlink");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&symlink_directory)
            .unwrap();
        let target = symlink_directory.join("key-target");
        std::fs::write(&target, [7_u8; KEY_BYTES]).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, symlink_directory.join("authority.key")).unwrap();
        assert!(HookSessionRoute::open_at(&symlink_directory).is_err());
        std::fs::remove_dir_all(symlink_directory).unwrap();
    }

    #[test]
    fn hex_decoder_is_exact_and_lowercase_only() {
        assert_eq!(decode_hex::<2>("00af").unwrap(), [0, 175]);
        assert!(decode_hex::<2>("00AF").is_err());
        assert!(decode_hex::<2>("00").is_err());
    }

    #[test]
    fn presentation_gate_includes_exact_threshold_and_budget_is_hard() {
        assert!(!passes_presentation_gate(
            MIN_PRESENTATION_RELEVANCE_BPS - 1
        ));
        assert!(passes_presentation_gate(MIN_PRESENTATION_RELEVANCE_BPS));
        assert_eq!(clipped_text("abcdef", 5), "abcd…");
        assert_eq!(clipped_text("abcde", 5), "abcde");
    }

    #[test]
    fn hook_render_reports_only_ids_that_fit_the_final_budget() {
        let ranked = (0..2)
            .map(|ordinal| RankedHookMemory {
                result: MemoryRecallResult {
                    memory: lattice_core::memory::Memory {
                        id: format!("m{ordinal}"),
                        session_id: "prior".into(),
                        content: "x".repeat(200),
                        memory_type: lattice_core::memory::MemoryType::Pattern,
                        scope: lattice_core::memory::MemoryScope::Repo,
                        confidence: 1.0,
                        linked_symbols: vec![],
                        linked_files: vec![],
                        workspace_id: Some("repo".into()),
                        branch: None,
                        scope_organization_id: None,
                        refresh_key: None,
                        source_query: None,
                        created_at: 1,
                        last_accessed: 1,
                        access_count: 0,
                        is_stale: false,
                        stale_reason: None,
                        verification_status: MemoryVerificationStatus::Verified,
                    },
                    memory_id: lattice_core::memory::AuthorityQualifiedMemoryId {
                        authority: lattice_core::memory::MemoryAuthority::Repository("repo".into()),
                        local_id: format!("m{ordinal}"),
                    },
                    source_tier: MemoryRecallTier::Repository,
                    assertion_key: format!("a{ordinal}"),
                    origin_repository_id: Some("repo".into()),
                    origin_checkout_id: Some("checkout".into()),
                    cross_repo: false,
                    origin_verification_status: MemoryVerificationStatus::Verified,
                    effective_verification_status: MemoryVerificationStatus::Verified,
                    trust_reason: "verified".into(),
                    retention_stale: false,
                },
                class: MemoryClass::Decision,
                relevance_bps: 10_000,
            })
            .collect::<Vec<_>>();
        let (rendered, shown_ids) =
            render_hook_presentation(HookPresentationKind::SessionStart, "hinj", &ranked, 70);
        assert!(rendered.contains("m0"));
        assert!(!rendered.contains("m1"));
        assert_eq!(shown_ids, vec![ranked[0].result.memory_id.encoded()]);
    }

    #[test]
    fn binding_capability_resumes_after_route_reopen() {
        let directory = test_directory("restart-state");
        let checkout = committed_repository("restart-checkout");
        let identity = WorkspaceIdentity::resolve(&checkout).unwrap();
        let hello = ProxyRequest {
            workspace_roots: vec![identity.checkout_root.to_string_lossy().to_string()],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };
        let params = serde_json::json!({
            "integration": "codex/v1",
            "host_session_id": "persistent-host-session",
        });
        let opened = HookSessionRoute::open_at(&directory)
            .unwrap()
            .handle_open(&hello, params.clone())
            .unwrap();

        let reopened = HookSessionRoute::open_at(&directory).unwrap();
        assert!(matches!(
            reopened.handle_open(&hello, params.clone()),
            Err(HookSessionRouteError::AuthorityRejected)
        ));
        let mut resume = params;
        resume["resume"] = serde_json::json!({
            "binding_id": opened["binding_id"],
            "capability": opened["capability"],
        });
        let resumed = reopened.handle_open(&hello, resume).unwrap();
        assert_eq!(resumed["resumed"], true);
        assert_eq!(resumed["binding_id"], opened["binding_id"]);

        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
        std::fs::remove_dir_all(checkout).unwrap();
    }

    #[test]
    fn turn_summary_is_nonterminal_and_session_end_preserves_order_and_seals_idempotently() {
        let directory = test_directory("capture-state");
        let checkout = committed_repository("capture-checkout");
        let identity = WorkspaceIdentity::resolve(&checkout).unwrap();
        let hello = ProxyRequest {
            workspace_roots: vec![identity.checkout_root.to_string_lossy().to_string()],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };
        let route = HookSessionRoute::open_at(&directory).unwrap();
        let opened = route
            .handle_open(
                &hello,
                serde_json::json!({
                    "integration": "codex/v1",
                    "host_session_id": "capture-host-session",
                }),
            )
            .unwrap();
        let authority = |sequence: u64, delivery_id: &str, event: Value| {
            serde_json::json!({
                "binding_id": opened["binding_id"],
                "capability": opened["capability"],
                "integration": "codex/v1",
                "delivery_id": delivery_id,
                "sequence": sequence,
                "event": event,
            })
        };

        let second = route
            .handle_event(
                &hello,
                authority(
                    2,
                    "22222222222222222222222222222222",
                    serde_json::json!({
                        "schema_version": 1,
                        "kind": "edited_path",
                        "path": "fixture.txt",
                    }),
                ),
            )
            .unwrap();
        assert_eq!(second["status"], "pending");

        let first = route
            .handle_event(
                &hello,
                authority(
                    1,
                    "11111111111111111111111111111111",
                    serde_json::json!({
                        "schema_version": 1,
                        "kind": "edited_path",
                        "path": "fixture.txt",
                    }),
                ),
            )
            .unwrap();
        assert_eq!(first["status"], "reduced");
        let binding_id = RegistryId::from_bytes(
            decode_hex::<16>(opened["binding_id"].as_str().unwrap())
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        let second_id = RegistryId::from_bytes([0x22; 16].to_vec()).unwrap();
        assert_eq!(
            route
                .registry
                .lock()
                .unwrap()
                .receipt(&binding_id, &second_id)
                .unwrap()
                .unwrap()
                .status,
            RegistryReceiptStatus::Reduced
        );

        let summary_on_event_route = route.handle_event(
            &hello,
            authority(
                3,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                serde_json::json!({
                    "schema_version": 1,
                    "kind": "turn_summary",
                    "summary": "must use dedicated route",
                }),
            ),
        );
        assert!(matches!(
            summary_on_event_route,
            Err(HookSessionRouteError::InvalidRequest)
        ));
        let edit_on_summary_route = route.handle_turn_summary(
            &hello,
            authority(
                3,
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                serde_json::json!({
                    "schema_version": 1,
                    "kind": "edited_path",
                    "path": "fixture.txt",
                }),
            ),
        );
        assert!(matches!(
            edit_on_summary_route,
            Err(HookSessionRouteError::InvalidRequest)
        ));

        let summary = route
            .handle_turn_summary(
                &hello,
                authority(
                    3,
                    "33333333333333333333333333333333",
                    serde_json::json!({
                        "schema_version": 1,
                        "kind": "turn_summary",
                        "summary": "Completed capture routing.",
                    }),
                ),
            )
            .unwrap();
        assert_eq!(summary["status"], "reduced");

        let close_params = authority(
            4,
            "44444444444444444444444444444444",
            serde_json::json!({"schema_version": 1}),
        );
        let closed = route.handle_close(&hello, close_params.clone()).unwrap();
        assert_eq!(closed["status"], "sealed");
        assert_eq!(closed["replayed"], false);
        let replayed = route.handle_close(&hello, close_params).unwrap();
        assert_eq!(replayed["status"], "sealed");
        assert_eq!(replayed["replayed"], true);

        let rejected = route.handle_event(
            &hello,
            authority(
                5,
                "55555555555555555555555555555555",
                serde_json::json!({"not": "parsed after seal"}),
            ),
        );
        assert!(matches!(
            rejected,
            Err(HookSessionRouteError::AuthorityRejected)
        ));

        let connection = Connection::open(identity.memories_path()).unwrap();
        let memory_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            memory_count, 0,
            "edits and a generic summary are navigation facts, not a demonstrated lesson"
        );

        let next_generation = route
            .handle_open(
                &hello,
                serde_json::json!({
                    "integration": "codex/v1",
                    "host_session_id": "capture-host-session",
                }),
            )
            .unwrap();
        assert_eq!(next_generation["generation"], 2);
        assert_ne!(next_generation["binding_id"], opened["binding_id"]);
        let next_event = serde_json::json!({
            "binding_id": next_generation["binding_id"],
            "capability": next_generation["capability"],
            "integration": "codex/v1",
            "delivery_id": "66666666666666666666666666666666",
            "sequence": 1,
            "event": {
                "schema_version": 1,
                "kind": "edited_path",
                "path": "fixture.txt",
            },
        });
        assert_eq!(
            route.handle_event(&hello, next_event).unwrap()["status"],
            "reduced"
        );

        let capture_health =
            crate::adoption_metrics::capture_health_for_workspace(&identity.repository_root)
                .expect("read capture counters");
        assert_eq!(capture_health.total_attempts, 8);
        assert_eq!(capture_health.outcomes.get("queued"), Some(&1));
        assert_eq!(capture_health.outcomes.get("captured"), Some(&4));
        assert_eq!(capture_health.outcomes.get("rejected"), Some(&3));
        // The close replay is an exact transport retry and must not create a
        // duplicate capture metric.
        assert_eq!(capture_health.outcomes.values().sum::<u64>(), 8);

        let metrics = crate::adoption_metrics::render_metrics_for_workspace(
            &identity.repository_root,
            90,
            true,
        )
        .expect("read content-free capture metrics");
        for forbidden in [
            "capture-host-session",
            opened["binding_id"].as_str().unwrap(),
            opened["capability"].as_str().unwrap(),
            "11111111111111111111111111111111",
            "33333333333333333333333333333333",
            "fixture.txt",
            "Completed capture routing.",
        ] {
            assert!(
                !metrics.contains(forbidden),
                "capture metrics retained forbidden value {forbidden:?}"
            );
        }

        drop(route);
        std::fs::remove_dir_all(directory).unwrap();
        std::fs::remove_dir_all(checkout).unwrap();
    }

    #[test]
    fn sealed_resolved_error_and_passing_check_capture_one_lesson_across_replay() {
        let directory = test_directory("lesson-capture-state");
        let checkout = committed_repository("lesson-capture-checkout");
        let identity = WorkspaceIdentity::resolve(&checkout).unwrap();
        let hello = ProxyRequest {
            workspace_roots: vec![identity.checkout_root.to_string_lossy().to_string()],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };
        let route = HookSessionRoute::open_at(&directory).unwrap();
        let opened = route
            .handle_open(
                &hello,
                serde_json::json!({
                    "integration":"codex/v1", "host_session_id":"lesson-capture-session"
                }),
            )
            .unwrap();
        let fingerprint = format!("sha256:{}", "6".repeat(64));
        let events = [
            serde_json::json!({"schema_version":1,"kind":"edited_path","path":"fixture.txt"}),
            serde_json::json!({"schema_version":1,"kind":"error","category":"storage","fingerprint":fingerprint,"status":"observed","summary":"Partial publication left the persisted file without its required header."}),
            serde_json::json!({"schema_version":1,"kind":"error","category":"storage","fingerprint":fingerprint,"status":"resolved","summary":"Write the header and payload in one atomic transaction before publication."}),
            serde_json::json!({"schema_version":1,"kind":"check","label":"atomic_publication_regression","outcome":"passed"}),
        ];
        for (index, event) in events.into_iter().enumerate() {
            let result = route
                .handle_event(
                    &hello,
                    serde_json::json!({
                        "binding_id":opened["binding_id"],"capability":opened["capability"],
                        "integration":"codex/v1","delivery_id":format!("{:032x}", index+1),
                        "sequence":index+1,"event":event
                    }),
                )
                .unwrap();
            assert_eq!(result["status"], "reduced");
        }
        let close = serde_json::json!({
            "binding_id":opened["binding_id"],"capability":opened["capability"],
            "integration":"codex/v1","delivery_id":format!("{:032x}", 5),
            "sequence":5,"event":{"schema_version":1,"final_summary":"Publish header and payload together in one atomic transaction. The regression check passed."}
        });
        assert_eq!(
            route.handle_close(&hello, close.clone()).unwrap()["replayed"],
            false
        );
        assert_eq!(route.handle_close(&hello, close).unwrap()["replayed"], true);
        let connection = Connection::open(identity.memories_path()).unwrap();
        let (count, content): (i64, String) = connection
            .query_row(
                "SELECT COUNT(*), COALESCE(MAX(content),'') FROM memories",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 1, "sealed correction captures exactly once");
        assert!(content.contains("atomic transaction"));
        drop(connection);
        drop(route);
        std::fs::remove_dir_all(directory).unwrap();
        std::fs::remove_dir_all(checkout).unwrap();
    }

    #[test]
    fn bound_presentations_are_relevant_bounded_idempotent_and_explicitly_attributed() {
        use lattice_core::memory::{
            Memory, MemoryScope, MemoryStructuredFields, MemoryType, MemoryVerificationStatus,
        };

        let directory = test_directory("presentation-state");
        let checkout = committed_repository("presentation-checkout");
        let identity = WorkspaceIdentity::resolve(&checkout).unwrap();
        let hello = ProxyRequest {
            workspace_roots: vec![identity.checkout_root.to_string_lossy().to_string()],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };
        std::fs::create_dir_all(&identity.repository_lattice_dir).unwrap();
        let store = MemoryStore::open(&identity.memories_path()).unwrap();
        let constraint_id = store
            .store(Memory {
                id: "constraint-fixture".to_string(),
                session_id: "prior-session".to_string(),
                content: "Keep fixture writes atomic; partial writes corrupt recovery.".to_string(),
                memory_type: MemoryType::Observation,
                scope: MemoryScope::Repo,
                confidence: 0.92,
                linked_symbols: Vec::new(),
                linked_files: vec!["fixture.txt".to_string()],
                workspace_id: Some(identity.repository_id.clone()),
                branch: Some("main".to_string()),
                scope_organization_id: None,
                refresh_key: None,
                source_query: None,
                created_at: 1,
                last_accessed: 1,
                access_count: 0,
                is_stale: false,
                stale_reason: None,
                verification_status: MemoryVerificationStatus::Verified,
            })
            .unwrap();
        let mut fields = MemoryStructuredFields::default();
        fields.memory_class = MemoryClass::Constraint;
        fields.verification_status = MemoryVerificationStatus::Verified;
        store
            .update_structured_fields(&constraint_id, &fields)
            .unwrap();
        std::fs::write(checkout.join("fixture.txt"), "dirty working set\n").unwrap();

        let route = HookSessionRoute::open_at(&directory).unwrap();
        let opened = route
            .handle_open(
                &hello,
                serde_json::json!({
                    "integration": "codex-hooks/v1",
                    "host_session_id": "presentation-session",
                    "presentation": {
                        "kind": "session-start",
                        "request_id": "start-1"
                    }
                }),
            )
            .unwrap();
        let start = opened["presentation"].as_object().expect("working-set hit");
        let injection_id = start["injection_id"].as_str().unwrap().to_string();
        let context = start["context"].as_str().unwrap();
        assert!(context.contains("constraint-fixture"));
        assert!(context.contains(&injection_id));
        assert!(context.chars().count() <= SESSION_PRESENTATION_BUDGET_TOKENS * 4);

        let resume = |request_id: &str, mut presentation: Value| {
            presentation["request_id"] = Value::String(request_id.to_string());
            serde_json::json!({
                "integration": "codex-hooks/v1",
                "host_session_id": "presentation-session",
                "resume": {
                    "binding_id": opened["binding_id"],
                    "capability": opened["capability"]
                },
                "presentation": presentation
            })
        };
        let replay = route
            .handle_open(
                &hello,
                resume("start-1", serde_json::json!({"kind": "session-start"})),
            )
            .unwrap();
        assert_eq!(replay["presentation"]["injection_id"], injection_id);

        let prompt = route
            .handle_open(
                &hello,
                resume(
                    "prompt-1",
                    serde_json::json!({
                        "kind": "user-prompt-submit",
                        "prompt": "How should fixture writes avoid corrupt recovery?"
                    }),
                ),
            )
            .unwrap();
        assert!(prompt["presentation"]["context"]
            .as_str()
            .unwrap()
            .contains("constraint-fixture"));

        let unrelated = route
            .handle_open(
                &hello,
                resume(
                    "edit-unrelated",
                    serde_json::json!({"kind": "post-tool-use", "path": "unrelated.rs"}),
                ),
            )
            .unwrap();
        assert!(unrelated.get("presentation").is_none());

        let edit = route
            .handle_open(
                &hello,
                resume(
                    "edit-fixture",
                    serde_json::json!({"kind": "post-tool-use", "path": "fixture.txt"}),
                ),
            )
            .unwrap();
        let warning = edit["presentation"]["context"].as_str().unwrap();
        assert!(!warning.contains('\n'));
        assert!(warning.contains("constraint-fixture"));
        assert!(warning.chars().count() <= POST_TOOL_PRESENTATION_BUDGET_TOKENS * 4);

        // An unrelated authenticated request causes no inferred action. The
        // exact prior id must be supplied explicitly on a later request.
        let before_action = AdoptionMetricsStore::new(&identity.repository_root)
            .read_json()
            .expect("read telemetry");
        let before_actions = telemetry_memory_counter(&before_action, "memory_injection_actions");
        assert_eq!(before_actions, 0);
        route
            .handle_open(
                &hello,
                resume(
                    "action-1",
                    serde_json::json!({
                        "kind": "user-prompt-submit",
                        "prompt": "completely unrelated phrase",
                        "acted_on_injection_id": injection_id
                    }),
                ),
            )
            .unwrap();
        let after_action = AdoptionMetricsStore::new(&identity.repository_root)
            .read_json()
            .expect("read telemetry");
        let action_count = telemetry_memory_counter(&after_action, "memory_injection_actions");
        assert_eq!(action_count, 1);
        route
            .handle_open(
                &hello,
                resume(
                    "action-1",
                    serde_json::json!({
                        "kind": "user-prompt-submit",
                        "prompt": "completely unrelated phrase",
                        "acted_on_injection_id": injection_id
                    }),
                ),
            )
            .unwrap();
        let replayed_metrics = AdoptionMetricsStore::new(&identity.repository_root)
            .read_json()
            .expect("read replayed telemetry");
        assert_eq!(
            telemetry_memory_counter(&replayed_metrics, "memory_injection_actions"),
            1
        );
        assert_eq!(
            telemetry_memory_counter(&replayed_metrics, "memory_injections"),
            3,
            "start replay must not duplicate its metric; only start, prompt, and edit present"
        );

        drop(route);
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
        std::fs::remove_dir_all(checkout).unwrap();
    }

    #[test]
    fn presentation_remains_valid_when_telemetry_storage_is_unavailable() {
        use lattice_core::memory::{
            Memory, MemoryScope, MemoryStructuredFields, MemoryType, MemoryVerificationStatus,
        };

        let directory = test_directory("presentation-telemetry-unavailable");
        let checkout = committed_repository("presentation-telemetry-unavailable-checkout");
        let identity = WorkspaceIdentity::resolve(&checkout).unwrap();
        let hello = ProxyRequest {
            workspace_roots: vec![identity.checkout_root.to_string_lossy().to_string()],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };
        std::fs::create_dir_all(&identity.repository_lattice_dir).unwrap();
        let store = MemoryStore::open(&identity.memories_path()).unwrap();
        let memory_id = store
            .store(Memory {
                id: "telemetry-unavailable-constraint".to_string(),
                session_id: "prior-session".to_string(),
                content: "Keep fixture writes atomic; partial writes corrupt recovery.".to_string(),
                memory_type: MemoryType::Observation,
                scope: MemoryScope::Repo,
                confidence: 0.92,
                linked_symbols: Vec::new(),
                linked_files: vec!["fixture.txt".to_string()],
                workspace_id: Some(identity.repository_id.clone()),
                branch: Some("other-branch".to_string()),
                scope_organization_id: None,
                refresh_key: None,
                source_query: None,
                created_at: 1,
                last_accessed: 1,
                access_count: 0,
                is_stale: false,
                stale_reason: None,
                verification_status: MemoryVerificationStatus::Unverified,
            })
            .unwrap();
        let mut fields = MemoryStructuredFields::default();
        fields.memory_class = MemoryClass::Constraint;
        store.update_structured_fields(&memory_id, &fields).unwrap();
        std::fs::write(checkout.join("fixture.txt"), "dirty working set\n").unwrap();
        // SQLite cannot open a directory as the telemetry database. This only
        // affects observation; the authenticated presentation must still work.
        std::fs::create_dir(
            identity
                .repository_lattice_dir
                .join("adoption_metrics.sqlite3"),
        )
        .unwrap();

        let route = HookSessionRoute::open_at(&directory).unwrap();
        let opened = route
            .handle_open(
                &hello,
                serde_json::json!({
                    "integration": "codex-hooks/v1",
                    "host_session_id": "telemetry-unavailable-session",
                    "presentation": {
                        "kind": "session-start",
                        "request_id": "start-1"
                    }
                }),
            )
            .expect("telemetry failure must not suppress a valid briefing");
        let presentation = opened["presentation"].as_object().expect("presentation");
        assert!(presentation["context"]
            .as_str()
            .expect("presentation context")
            .contains("telemetry-unavailable-constraint"));
        assert!(presentation["injection_id"].as_str().is_some());

        drop(route);
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
        std::fs::remove_dir_all(checkout).unwrap();
    }

    #[test]
    fn unavailable_capture_store_records_content_free_failure() {
        let directory = test_directory("capture-store-unavailable-state");
        let checkout = committed_repository("capture-store-unavailable-checkout");
        let identity = WorkspaceIdentity::resolve(&checkout).unwrap();
        let hello = ProxyRequest {
            workspace_roots: vec![identity.checkout_root.to_string_lossy().to_string()],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };
        let route = HookSessionRoute::open_at(&directory).unwrap();
        let opened = route
            .handle_open(
                &hello,
                serde_json::json!({
                    "integration": "codex/v1",
                    "host_session_id": "store-unavailable-host-session",
                }),
            )
            .unwrap();
        let delivery = |sequence: u64, delivery_id: &str, event: Value| {
            serde_json::json!({
                "binding_id": opened["binding_id"],
                "capability": opened["capability"],
                "integration": "codex/v1",
                "delivery_id": delivery_id,
                "sequence": sequence,
                "event": event,
            })
        };
        route
            .handle_event(
                &hello,
                delivery(
                    1,
                    "11111111111111111111111111111111",
                    serde_json::json!({
                        "schema_version": 1,
                        "kind": "edited_path",
                        "path": "fixture.txt",
                    }),
                ),
            )
            .unwrap();
        std::fs::create_dir(identity.memories_path()).unwrap();
        let close = route.handle_close(
            &hello,
            delivery(
                2,
                "22222222222222222222222222222222",
                serde_json::json!({"schema_version": 1}),
            ),
        );
        assert!(matches!(close, Err(HookSessionRouteError::Unavailable)));

        let capture_health =
            crate::adoption_metrics::capture_health_for_workspace(&identity.repository_root)
                .expect("read capture counters");
        assert_eq!(capture_health.total_attempts, 2);
        assert_eq!(capture_health.outcomes.get("captured"), Some(&1));
        assert_eq!(capture_health.outcomes.get("store_unavailable"), Some(&1));

        drop(route);
        std::fs::remove_dir_all(directory).unwrap();
        std::fs::remove_dir_all(checkout).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn edited_path_symlink_escape_is_rejected() {
        let directory = test_directory("path-state");
        let checkout = committed_repository("path-checkout");
        let outside = test_directory("path-outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, checkout.join("escape")).unwrap();
        let identity = WorkspaceIdentity::resolve(&checkout).unwrap();
        let hello = ProxyRequest {
            workspace_roots: vec![identity.checkout_root.to_string_lossy().to_string()],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };
        let route = HookSessionRoute::open_at(&directory).unwrap();
        let opened = route
            .handle_open(
                &hello,
                serde_json::json!({
                    "integration": "codex/v1",
                    "host_session_id": "path-host-session",
                }),
            )
            .unwrap();
        let result = route.handle_event(
            &hello,
            serde_json::json!({
                "binding_id": opened["binding_id"],
                "capability": opened["capability"],
                "integration": "codex/v1",
                "delivery_id": "11111111111111111111111111111111",
                "sequence": 1,
                "event": {"schema_version":1,"kind":"edited_path","path":"escape/file.rs"},
            }),
        );
        assert!(matches!(
            result,
            Err(HookSessionRouteError::AuthorityRejected)
        ));

        drop(route);
        std::fs::remove_dir_all(directory).unwrap();
        std::fs::remove_dir_all(checkout).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn normalized_error_without_summary_remains_reparseable() {
        let event = parse_session_capture_event(
            &serde_json::json!({
                "schema_version": SESSION_CAPTURE_SCHEMA_VERSION,
                "kind": "error",
                "category": "test",
                "fingerprint": format!("sha256:{}", "5".repeat(64)),
                "status": "observed",
            })
            .to_string(),
        )
        .unwrap();
        let normalized = normalized_event_value(&event).unwrap();
        assert!(normalized.get("summary").is_none());
        assert!(parse_session_capture_event(&normalized.to_string()).is_ok());
    }

    struct EnforcementFixture {
        directory: PathBuf,
        checkout: PathBuf,
        hello: ProxyRequest,
        route: HookSessionRoute,
        resume: Option<Value>,
    }

    impl EnforcementFixture {
        fn new(label: &str) -> Self {
            let directory = test_directory(&format!("{label}-state"));
            let checkout = committed_repository(&format!("{label}-checkout"));
            let identity = WorkspaceIdentity::resolve(&checkout).unwrap();
            let hello = ProxyRequest {
                workspace_roots: vec![identity.checkout_root.to_string_lossy().to_string()],
                focus_files: Vec::new(),
                focus_dirs: Vec::new(),
            };
            let route = HookSessionRoute::open_at(&directory).unwrap();
            Self {
                directory,
                checkout,
                hello,
                route,
                resume: None,
            }
        }

        /// One authenticated hook request for the same host session.
        fn ask(&mut self, enforcement: Value, index_state: IndexState) -> Value {
            let mut params = serde_json::json!({
                "integration": "claude-code-hooks/v1",
                "host_session_id": "enforced-host-session",
                "enforcement": enforcement,
            });
            if let Some(resume) = &self.resume {
                params["resume"] = resume.clone();
            }
            let opened = self
                .route
                .handle_open_in(&self.hello, params, index_state)
                .unwrap();
            self.resume = Some(serde_json::json!({
                "binding_id": opened["binding_id"],
                "capability": opened["capability"],
            }));
            opened["enforcement"].clone()
        }

        fn edit(&mut self) -> Value {
            self.ask(
                serde_json::json!({"event": "pre-tool-use"}),
                IndexState::Ready,
            )
        }
    }

    impl Drop for EnforcementFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
            let _ = std::fs::remove_dir_all(&self.checkout);
        }
    }

    #[test]
    fn product_edit_is_denied_until_a_plan_is_served_and_one_plan_covers_many_edits() {
        let mut fixture = EnforcementFixture::new("gate");
        let denied = fixture.edit();
        assert_eq!(denied["decision"], "deny");
        assert_eq!(denied["plan_state"], "missing");

        // Another checkout's plan never satisfies this checkout's gate.
        let other = committed_repository("gate-other-checkout");
        let other_identity = WorkspaceIdentity::resolve(&other).unwrap();
        fixture.route.record_workflow_step(
            &ProxyRequest {
                workspace_roots: vec![other_identity.checkout_root.to_string_lossy().to_string()],
                focus_files: Vec::new(),
                focus_dirs: Vec::new(),
            },
            WorkflowStep::PrepareChange,
        );
        assert_eq!(fixture.edit()["decision"], "deny");
        // Neither does a different workflow step.
        let hello = fixture.hello.clone();
        fixture
            .route
            .record_workflow_step(&hello, WorkflowStep::Remember);
        assert_eq!(fixture.edit()["decision"], "deny");

        fixture
            .route
            .record_workflow_step(&hello, WorkflowStep::PrepareChange);
        for _ in 0..5 {
            let allowed = fixture.edit();
            assert_eq!(allowed["decision"], "allow");
            assert_eq!(allowed["plan_state"], "current");
        }
        std::fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn a_fresh_model_context_needs_a_new_plan_but_a_resume_does_not() {
        let mut fixture = EnforcementFixture::new("context-reset");
        let hello = fixture.hello.clone();
        fixture.ask(
            serde_json::json!({"event": "session-start", "session_source": "startup"}),
            IndexState::Ready,
        );
        fixture
            .route
            .record_workflow_step(&hello, WorkflowStep::PrepareChange);
        assert_eq!(fixture.edit()["decision"], "allow");

        fixture.ask(
            serde_json::json!({"event": "session-start", "session_source": "resume"}),
            IndexState::Ready,
        );
        assert_eq!(fixture.edit()["decision"], "allow");

        // Compaction replaces the context that held the plan.
        std::thread::sleep(std::time::Duration::from_millis(5));
        fixture.ask(
            serde_json::json!({"event": "session-start", "session_source": "compact"}),
            IndexState::Ready,
        );
        let after_compact = fixture.edit();
        assert_eq!(after_compact["decision"], "deny");
        assert_eq!(after_compact["plan_state"], "missing");
    }

    #[test]
    fn a_plan_survives_binding_regeneration_within_one_host_session() {
        let mut fixture = EnforcementFixture::new("regeneration");
        let hello = fixture.hello.clone();
        fixture.edit();
        fixture
            .route
            .record_workflow_step(&hello, WorkflowStep::PrepareChange);
        assert_eq!(fixture.edit()["decision"], "allow");

        // An idle binding expires after thirty minutes and the next hook mints
        // a new one. Revoking reproduces that without waiting.
        let binding_hex = fixture.resume.as_ref().unwrap()["binding_id"]
            .as_str()
            .unwrap()
            .to_string();
        let binding_id =
            RegistryId::from_bytes(decode_hex::<16>(&binding_hex).unwrap().to_vec()).unwrap();
        fixture
            .route
            .registry
            .lock()
            .unwrap()
            .revoke(&binding_id)
            .unwrap();
        fixture.resume = None;
        let regenerated = fixture.edit();
        assert_ne!(
            fixture.resume.as_ref().unwrap()["binding_id"]
                .as_str()
                .unwrap(),
            binding_hex
        );
        assert_eq!(regenerated["decision"], "allow");
        assert_eq!(regenerated["plan_state"], "current");
    }

    #[test]
    fn a_degraded_index_never_blocks_and_reports_its_state() {
        for (state, wire) in [
            (IndexState::Indexing, "indexing"),
            (IndexState::Deferred, "deferred"),
            (IndexState::Failed, "failed"),
        ] {
            let mut fixture = EnforcementFixture::new(&format!("degraded-{wire}"));
            let answer = fixture.ask(serde_json::json!({"event": "pre-tool-use"}), state);
            assert_eq!(answer["decision"], "allow", "{wire}");
            assert_eq!(answer["plan_state"], "missing");
            assert_eq!(answer["index_state"], wire);
        }
        // A workspace that simply is not loaded yet still requires the plan:
        // asking for one is what loads it.
        let mut fixture = EnforcementFixture::new("not-loaded");
        let answer = fixture.ask(
            serde_json::json!({"event": "pre-tool-use"}),
            IndexState::NotLoaded,
        );
        assert_eq!(answer["decision"], "deny");
        assert_eq!(answer["index_state"], "not-loaded");
    }

    #[test]
    fn shell_edits_are_never_denied_and_the_stop_reminder_is_claimed_once() {
        let mut fixture = EnforcementFixture::new("shell-stop");
        let hello = fixture.hello.clone();
        let stop = serde_json::json!({"event": "stop"});

        // No product edit yet: nothing to remind about.
        assert!(fixture.ask(stop.clone(), IndexState::Ready)["followup"].is_null());

        let shell = fixture.ask(
            serde_json::json!({"event": "shell-edit", "product_paths": 3}),
            IndexState::Ready,
        );
        assert_eq!(shell["decision"], "allow");
        assert_eq!(shell["plan_state"], "missing");

        fixture
            .route
            .record_workflow_step(&hello, WorkflowStep::StaleDocs);
        let reminded = fixture.ask(stop.clone(), IndexState::Ready);
        assert_eq!(
            reminded["followup"],
            serde_json::json!({"stale_docs": false, "remember": true})
        );
        assert_eq!(reminded["decision"], "allow");
        // Exactly once per session, even though the gap remains.
        assert!(fixture.ask(stop, IndexState::Ready)["followup"].is_null());
    }

    #[test]
    fn enforcement_params_are_strict_and_absent_by_default() {
        let mut fixture = EnforcementFixture::new("strict");
        for invalid in [
            serde_json::json!({"event": "pre-tool-use", "command": "rm -rf /"}),
            serde_json::json!({"event": "pre-tool-use", "path": "src/lib.rs"}),
            serde_json::json!({"event": "unknown"}),
            serde_json::json!({"event": "session-start", "session_source": "other"}),
            serde_json::json!({"event": "shell-edit", "product_paths": -1}),
        ] {
            let result = fixture.route.handle_open_in(
                &fixture.hello,
                serde_json::json!({
                    "integration": "claude-code-hooks/v1",
                    "host_session_id": "strict-host-session",
                    "enforcement": invalid,
                }),
                IndexState::Ready,
            );
            assert!(matches!(result, Err(HookSessionRouteError::InvalidRequest)));
        }
        let plain = fixture
            .route
            .handle_open(
                &fixture.hello,
                serde_json::json!({
                    "integration": "claude-code-hooks/v1",
                    "host_session_id": "plain-host-session",
                }),
            )
            .unwrap();
        assert!(plain.get("enforcement").is_none());
        // The workflow database holds categories and times only.
        let bytes = std::fs::read(fixture.directory.join("workflow.db")).unwrap();
        for forbidden in ["rm -rf", "src/lib.rs", "strict-host-session"] {
            assert!(!bytes
                .windows(forbidden.len())
                .any(|window| window == forbidden.as_bytes()));
        }
        fixture.edit();
    }

    fn committed_repository(label: &str) -> PathBuf {
        let root = test_directory(label);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init"]);
        git(&root, &["config", "user.email", "lattice@example.test"]);
        git(&root, &["config", "user.name", "Lattice Test"]);
        std::fs::write(root.join("fixture.txt"), "fixture\n").unwrap();
        git(&root, &["add", "fixture.txt"]);
        git(&root, &["commit", "-m", "fixture"]);
        root
    }

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn test_directory(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lattice-hook-route-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn telemetry_memory_counter(ledger: &Value, field: &str) -> u64 {
        ledger["days"]
            .as_object()
            .into_iter()
            .flat_map(|days| days.values())
            .filter_map(Value::as_object)
            .flat_map(|clients| clients.values())
            .filter_map(Value::as_object)
            .flat_map(|channels| channels.values())
            .filter_map(Value::as_object)
            .filter_map(|tools| tools.get("memory"))
            .filter_map(|memory| memory.get(field))
            .filter_map(Value::as_u64)
            .sum()
    }
}
