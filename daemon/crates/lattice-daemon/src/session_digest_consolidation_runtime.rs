//! Daemon-owned scheduling and provider transport for opt-in session-digest
//! consolidation.
//!
//! The core consolidator owns source filtering and proposal construction. This
//! layer owns secrets, bounded HTTP, durable single-flight scheduling, and
//! restart recovery. No runtime is created unless all opt-in configuration is
//! present and valid.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lattice_core::consolidation::llm::{
    LlmDriver, LlmDriverError, LlmJobServices, LlmRequest, LlmResponse,
    SessionDigestConsolidationConfig, SessionDigestConsolidationOutcome,
    SessionDigestLlmConsolidator, SessionDigestLlmProvider,
};
use lattice_core::consolidation::{ConsolidationConfig, ConsolidationJobRuntime};
use lattice_core::events::EventWriter;
use lattice_core::memory::session_capture::SessionCaptureRetentionPolicy;
use lattice_core::memory::{MemoryQueryAuthority, MemoryStore, MemoryStoreRouter};
use lattice_core::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(5 * 60);
const MAX_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const MAX_RETENTION: Duration = Duration::from_secs(365 * 24 * 60 * 60);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_MAX_SOURCE_FACTS: usize = 32;
const MAX_SOURCE_FACTS: usize = 128;
const DEFAULT_MAX_PROPOSALS: usize = 4;
const MAX_PROPOSALS: usize = 16;
const DEFAULT_MAX_PENDING_PROPOSALS: usize = 128;
const MAX_PENDING_PROPOSALS: usize = 1024;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_REQUEST_BYTES: usize = 256 * 1024;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_MODEL_BYTES: usize = 256;
const MAX_ENDPOINT_BYTES: usize = 2048;
const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/responses";
const ANTHROPIC_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

// Automatic capture retention is deliberately independent from opt-in LLM
// consolidation. These are repository-local bounds: every daemon workspace
// gets a scheduler unless its retention configuration is invalid.
const DEFAULT_CAPTURE_RETENTION_INTERVAL: Duration = Duration::from_secs(60 * 60);
const MAX_CAPTURE_RETENTION_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_CAPTURE_RETENTION_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const MAX_CAPTURE_RETENTION_MAX_AGE: Duration = Duration::from_secs(365 * 24 * 60 * 60);
const DEFAULT_CAPTURE_RETENTION_MAX_CAPTURES: usize = 1_024;
const MAX_CAPTURE_RETENTION_MAX_CAPTURES: usize = 100_000;
const CAPTURE_RETENTION_LEASE: Duration = Duration::from_secs(5 * 60);

pub(crate) const ENABLE_ENV: &str = "LATTICE_ENABLE_SESSION_DIGEST_CONSOLIDATION";
pub(crate) const PROVIDER_ENV: &str = "LATTICE_SESSION_DIGEST_CONSOLIDATION_PROVIDER";
pub(crate) const MODEL_ENV: &str = "LATTICE_SESSION_DIGEST_CONSOLIDATION_MODEL";
pub(crate) const INTERVAL_ENV: &str = "LATTICE_SESSION_DIGEST_CONSOLIDATION_INTERVAL_SECS";
pub(crate) const RETENTION_ENV: &str = "LATTICE_SESSION_DIGEST_CONSOLIDATION_RETENTION_SECS";
pub(crate) const TIMEOUT_ENV: &str = "LATTICE_SESSION_DIGEST_CONSOLIDATION_TIMEOUT_SECS";
pub(crate) const MAX_SOURCE_FACTS_ENV: &str =
    "LATTICE_SESSION_DIGEST_CONSOLIDATION_MAX_SOURCE_FACTS";
pub(crate) const MAX_PROPOSALS_ENV: &str =
    "LATTICE_SESSION_DIGEST_CONSOLIDATION_MAX_PROPOSALS_PER_RUN";
pub(crate) const MAX_PENDING_ENV: &str =
    "LATTICE_SESSION_DIGEST_CONSOLIDATION_MAX_PENDING_PROPOSALS";
pub(crate) const MAX_RESPONSE_ENV: &str = "LATTICE_SESSION_DIGEST_CONSOLIDATION_MAX_RESPONSE_BYTES";
pub(crate) const CAPTURE_RETENTION_INTERVAL_ENV: &str =
    "LATTICE_SESSION_CAPTURE_RETENTION_INTERVAL_SECS";
pub(crate) const CAPTURE_RETENTION_MAX_AGE_ENV: &str =
    "LATTICE_SESSION_CAPTURE_RETENTION_MAX_AGE_SECS";
pub(crate) const CAPTURE_RETENTION_MAX_CAPTURES_ENV: &str =
    "LATTICE_SESSION_CAPTURE_RETENTION_MAX_CAPTURES";

#[derive(Clone)]
struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SessionDigestRuntimeConfig {
    provider: SessionDigestLlmProvider,
    provider_key: Secret,
    model: String,
    endpoint: String,
    interval: Duration,
    retention: Duration,
    timeout: Duration,
    max_source_facts: usize,
    max_proposals_per_run: usize,
    max_pending_proposals: usize,
    max_response_bytes: usize,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum RuntimeConfigError {
    #[error("unsupported session-digest consolidation provider '{0}'")]
    UnsupportedProvider(String),
    #[error("session-digest consolidation requires {0}")]
    Missing(&'static str),
    #[error("session-digest consolidation {0} must be a positive integer")]
    InvalidPositive(&'static str),
    #[error("session-digest consolidation {0} exceeds its safety bound")]
    ExceedsBound(&'static str),
}

impl SessionDigestRuntimeConfig {
    pub(crate) fn from_environment() -> Result<Option<Self>, RuntimeConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<Option<Self>, RuntimeConfigError> {
        if !value_enabled(lookup(ENABLE_ENV).as_deref()) {
            return Ok(None);
        }
        let provider_raw = required_value(PROVIDER_ENV, lookup(PROVIDER_ENV))?;
        let provider = match provider_raw.to_ascii_lowercase().as_str() {
            "openai" => SessionDigestLlmProvider::OpenAi,
            "anthropic" => SessionDigestLlmProvider::Anthropic,
            _ => return Err(RuntimeConfigError::UnsupportedProvider(provider_raw)),
        };
        let key_env = match provider {
            SessionDigestLlmProvider::OpenAi => "OPENAI_API_KEY",
            SessionDigestLlmProvider::Anthropic => "ANTHROPIC_API_KEY",
        };
        let provider_key = required_value(key_env, lookup(key_env))?;
        if !usable_secret(&provider_key) {
            return Err(RuntimeConfigError::Missing(key_env));
        }
        let model = required_value(MODEL_ENV, lookup(MODEL_ENV))?;
        if model.len() > MAX_MODEL_BYTES || model.chars().any(char::is_control) {
            return Err(RuntimeConfigError::ExceedsBound("model"));
        }
        let timeout = value_duration(TIMEOUT_ENV, lookup(TIMEOUT_ENV), DEFAULT_TIMEOUT)?;
        if timeout > MAX_TIMEOUT {
            return Err(RuntimeConfigError::ExceedsBound("timeout"));
        }
        let max_response_bytes = value_usize(
            MAX_RESPONSE_ENV,
            lookup(MAX_RESPONSE_ENV),
            DEFAULT_MAX_RESPONSE_BYTES,
        )?;
        if max_response_bytes > MAX_RESPONSE_BYTES {
            return Err(RuntimeConfigError::ExceedsBound("response limit"));
        }
        let interval = value_duration(INTERVAL_ENV, lookup(INTERVAL_ENV), DEFAULT_INTERVAL)?;
        if interval > MAX_INTERVAL {
            return Err(RuntimeConfigError::ExceedsBound("interval"));
        }
        let retention = value_duration(RETENTION_ENV, lookup(RETENTION_ENV), DEFAULT_RETENTION)?;
        if retention > MAX_RETENTION {
            return Err(RuntimeConfigError::ExceedsBound("retention"));
        }
        let max_source_facts = value_usize(
            MAX_SOURCE_FACTS_ENV,
            lookup(MAX_SOURCE_FACTS_ENV),
            DEFAULT_MAX_SOURCE_FACTS,
        )?;
        if max_source_facts > MAX_SOURCE_FACTS {
            return Err(RuntimeConfigError::ExceedsBound("source fact limit"));
        }
        let max_proposals_per_run = value_usize(
            MAX_PROPOSALS_ENV,
            lookup(MAX_PROPOSALS_ENV),
            DEFAULT_MAX_PROPOSALS,
        )?;
        if max_proposals_per_run > MAX_PROPOSALS {
            return Err(RuntimeConfigError::ExceedsBound("proposal limit"));
        }
        let max_pending_proposals = value_usize(
            MAX_PENDING_ENV,
            lookup(MAX_PENDING_ENV),
            DEFAULT_MAX_PENDING_PROPOSALS,
        )?;
        if max_pending_proposals > MAX_PENDING_PROPOSALS {
            return Err(RuntimeConfigError::ExceedsBound("pending proposal limit"));
        }
        Ok(Some(Self {
            provider,
            provider_key: Secret(provider_key),
            model,
            endpoint: match provider {
                SessionDigestLlmProvider::OpenAi => OPENAI_ENDPOINT,
                SessionDigestLlmProvider::Anthropic => ANTHROPIC_ENDPOINT,
            }
            .to_string(),
            interval,
            retention,
            timeout,
            max_source_facts,
            max_proposals_per_run,
            max_pending_proposals,
            max_response_bytes,
        }))
    }

    fn core_config(&self) -> Result<SessionDigestConsolidationConfig, String> {
        SessionDigestConsolidationConfig::from_daemon_config(
            true,
            Some(self.provider),
            Some(&self.provider_key.0),
            self.retention,
        )
        .and_then(|config| {
            config.with_bounds(
                self.max_source_facts,
                self.max_proposals_per_run,
                self.max_pending_proposals,
            )
        })
        .map_err(|error| error.to_string())
    }

    #[cfg(test)]
    fn test(provider: SessionDigestLlmProvider, endpoint: String) -> Self {
        Self {
            provider,
            provider_key: Secret("mock-provider-key".to_string()),
            model: "mock-model".to_string(),
            endpoint,
            interval: Duration::from_millis(20),
            retention: DEFAULT_RETENTION,
            timeout: Duration::from_secs(2),
            max_source_facts: 16,
            max_proposals_per_run: 2,
            max_pending_proposals: 8,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }
}

fn required_value(name: &'static str, value: Option<String>) -> Result<String, RuntimeConfigError> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or(RuntimeConfigError::Missing(name))
}

fn value_enabled(value: Option<&str>) -> bool {
    matches!(
        value,
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

fn value_duration(
    name: &'static str,
    value: Option<String>,
    fallback: Duration,
) -> Result<Duration, RuntimeConfigError> {
    value
        .map(|raw| {
            raw.parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .map(Duration::from_secs)
                .ok_or(RuntimeConfigError::InvalidPositive(name))
        })
        .transpose()
        .map(|value| value.unwrap_or(fallback))
}

fn value_usize(
    name: &'static str,
    value: Option<String>,
    fallback: usize,
) -> Result<usize, RuntimeConfigError> {
    value
        .map(|raw| {
            raw.parse::<usize>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or(RuntimeConfigError::InvalidPositive(name))
        })
        .transpose()
        .map(|value| value.unwrap_or(fallback))
}

fn usable_secret(value: &str) -> bool {
    value.len() >= 8
        && value.len() <= 16 * 1024
        && !value.chars().any(char::is_control)
        && !value.chars().any(char::is_whitespace)
}

#[derive(Debug)]
struct ProviderDriver {
    provider: SessionDigestLlmProvider,
    provider_key: Secret,
    model: String,
    endpoint: String,
    max_response_bytes: usize,
    agent: ureq::Agent,
}

impl ProviderDriver {
    fn new(config: &SessionDigestRuntimeConfig) -> Result<Self, String> {
        if config.endpoint.len() > MAX_ENDPOINT_BYTES {
            return Err("provider endpoint exceeds its safety bound".to_string());
        }
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(config.timeout))
            .timeout_connect(Some(config.timeout))
            .timeout_send_request(Some(config.timeout))
            .timeout_send_body(Some(config.timeout))
            .timeout_recv_response(Some(config.timeout))
            .timeout_recv_body(Some(config.timeout))
            .build()
            .into();
        Ok(Self {
            provider: config.provider,
            provider_key: config.provider_key.clone(),
            model: config.model.clone(),
            endpoint: config.endpoint.clone(),
            max_response_bytes: config.max_response_bytes,
            agent,
        })
    }

    fn request_body(&self, request: &LlmRequest) -> Result<Value, LlmDriverError> {
        let input = format!(
            "{}\n\nReturn only JSON matching this response contract:\n{}",
            request.prompt, request.response_schema
        );
        if input.len() > MAX_REQUEST_BYTES {
            return Err(LlmDriverError::Failed(
                "provider request exceeded the configured safety bound".to_string(),
            ));
        }
        Ok(match self.provider {
            SessionDigestLlmProvider::OpenAi => json!({
                "model": self.model,
                "input": input,
                "max_output_tokens": 2048,
                "store": false,
                "text": { "format": { "type": "json_object" } }
            }),
            SessionDigestLlmProvider::Anthropic => json!({
                "model": self.model,
                "max_tokens": 2048,
                "messages": [{ "role": "user", "content": input }]
            }),
        })
    }

    fn parse_response(&self, payload: Value) -> Result<String, LlmDriverError> {
        let text = match self.provider {
            SessionDigestLlmProvider::OpenAi => payload
                .get("output")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
                .flat_map(|item| {
                    item.get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                })
                .find(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
                .and_then(|part| part.get("text"))
                .and_then(Value::as_str),
            SessionDigestLlmProvider::Anthropic => payload
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .and_then(|part| part.get("text"))
                .and_then(Value::as_str),
        };
        text.map(str::to_string).ok_or_else(|| {
            LlmDriverError::Failed("provider response contained no text output".to_string())
        })
    }
}

impl LlmDriver for ProviderDriver {
    fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmDriverError> {
        let body = self.request_body(&request)?;
        let mut http = self
            .agent
            .post(&self.endpoint)
            .header("content-type", "application/json");
        http = match self.provider {
            SessionDigestLlmProvider::OpenAi => {
                http.header("authorization", &format!("Bearer {}", self.provider_key.0))
            }
            SessionDigestLlmProvider::Anthropic => http
                .header("x-api-key", &self.provider_key.0)
                .header("anthropic-version", ANTHROPIC_VERSION),
        };
        let response = http.send_json(body).map_err(|error| match error {
            ureq::Error::StatusCode(status) => {
                let disposition = if matches!(status, 408 | 409 | 429 | 500 | 502 | 503 | 504) {
                    "retryable"
                } else {
                    "non_retryable"
                };
                LlmDriverError::Failed(format!(
                    "provider returned {disposition} HTTP status {status}"
                ))
            }
            _ => LlmDriverError::Unavailable("provider transport unavailable".to_string()),
        })?;
        let mut reader = response
            .into_body()
            .into_with_config()
            .limit(self.max_response_bytes as u64 + 1)
            .reader();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).map_err(|_| {
            LlmDriverError::Failed(
                "provider response exceeded the configured safety bound".to_string(),
            )
        })?;
        if bytes.len() > self.max_response_bytes {
            return Err(LlmDriverError::Failed(
                "provider response exceeded the configured safety bound".to_string(),
            ));
        }
        let payload: Value = serde_json::from_slice(&bytes)
            .map_err(|_| LlmDriverError::Failed("provider returned malformed JSON".to_string()))?;
        Ok(LlmResponse {
            content: self.parse_response(payload)?,
        })
    }

    fn name(&self) -> &str {
        &self.model
    }
}

pub(crate) struct SessionDigestConsolidationHandle {
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl SessionDigestConsolidationHandle {
    pub(crate) async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionCaptureRetentionRuntimeConfig {
    interval: Duration,
    max_age: Duration,
    max_captures: usize,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
enum SessionCaptureRetentionConfigError {
    #[error("session capture retention {0} must be a positive integer")]
    InvalidPositive(&'static str),
    #[error("session capture retention {0} exceeds its safety bound")]
    ExceedsBound(&'static str),
}

impl SessionCaptureRetentionRuntimeConfig {
    fn from_environment() -> Result<Self, SessionCaptureRetentionConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, SessionCaptureRetentionConfigError> {
        let interval = capture_retention_duration(
            "interval",
            lookup(CAPTURE_RETENTION_INTERVAL_ENV),
            DEFAULT_CAPTURE_RETENTION_INTERVAL,
        )?;
        if interval > MAX_CAPTURE_RETENTION_INTERVAL {
            return Err(SessionCaptureRetentionConfigError::ExceedsBound("interval"));
        }
        let max_age = capture_retention_duration(
            "maximum age",
            lookup(CAPTURE_RETENTION_MAX_AGE_ENV),
            DEFAULT_CAPTURE_RETENTION_MAX_AGE,
        )?;
        if max_age > MAX_CAPTURE_RETENTION_MAX_AGE {
            return Err(SessionCaptureRetentionConfigError::ExceedsBound(
                "maximum age",
            ));
        }
        let max_captures = capture_retention_usize(
            "capture limit",
            lookup(CAPTURE_RETENTION_MAX_CAPTURES_ENV),
            DEFAULT_CAPTURE_RETENTION_MAX_CAPTURES,
        )?;
        if max_captures > MAX_CAPTURE_RETENTION_MAX_CAPTURES {
            return Err(SessionCaptureRetentionConfigError::ExceedsBound(
                "capture limit",
            ));
        }
        Ok(Self {
            interval,
            max_age,
            max_captures,
        })
    }

    fn policy(&self) -> Result<SessionCaptureRetentionPolicy, String> {
        SessionCaptureRetentionPolicy::new(self.max_age, self.max_captures)
            .map_err(|error| error.to_string())
    }

    #[cfg(test)]
    fn test(interval: Duration, max_age: Duration, max_captures: usize) -> Self {
        Self {
            interval,
            max_age,
            max_captures,
        }
    }
}

fn capture_retention_duration(
    name: &'static str,
    value: Option<String>,
    fallback: Duration,
) -> Result<Duration, SessionCaptureRetentionConfigError> {
    value
        .map(|raw| {
            raw.parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .map(Duration::from_secs)
                .ok_or(SessionCaptureRetentionConfigError::InvalidPositive(name))
        })
        .transpose()
        .map(|value| value.unwrap_or(fallback))
}

fn capture_retention_usize(
    name: &'static str,
    value: Option<String>,
    fallback: usize,
) -> Result<usize, SessionCaptureRetentionConfigError> {
    value
        .map(|raw| {
            raw.parse::<usize>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or(SessionCaptureRetentionConfigError::InvalidPositive(name))
        })
        .transpose()
        .map(|value| value.unwrap_or(fallback))
}

pub(crate) struct SessionCaptureRetentionHandle {
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl SessionCaptureRetentionHandle {
    pub(crate) async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}

struct SessionCaptureRetentionWorker {
    repository_id: String,
    checkout_id: String,
    memory_path: PathBuf,
    state_path: PathBuf,
    owner: String,
    config: SessionCaptureRetentionRuntimeConfig,
}

impl SessionCaptureRetentionWorker {
    fn run_once(
        &self,
    ) -> Result<lattice_core::memory::session_capture::SessionCaptureDeletionResult, String> {
        if !self.claim_lease()? {
            return Ok(Default::default());
        }
        let result = self.prune();
        let release = self.release_lease();
        match (result, release) {
            (Ok(result), Ok(())) => Ok(result),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(prune_error), Err(release_error)) => Err(format!(
                "session capture retention failed ({prune_error}) and could not release its lease ({release_error})"
            )),
        }
    }

    fn prune(
        &self,
    ) -> Result<lattice_core::memory::session_capture::SessionCaptureDeletionResult, String> {
        let store = MemoryStore::open(&self.memory_path).map_err(|error| error.to_string())?;
        // Retention has no shared-memory target. Constructing the router with
        // the daemon's repository and checkout identity makes the repository
        // qualifier a strict lease before the core deletion path is reached.
        let authority = MemoryQueryAuthority::new(
            self.repository_id.clone(),
            self.checkout_id.clone(),
            None,
            "session-capture-retention".to_string(),
            None,
        )
        .map_err(|error| error.to_string())?;
        let router =
            MemoryStoreRouter::new(&store, None, authority).map_err(|error| error.to_string())?;
        router
            .prune_session_captures(
                self.config.policy()?,
                DateTime::<Utc>::from_unix_seconds(now_seconds()),
            )
            .map_err(|error| error.to_string())
    }

    fn open_state(&self) -> Result<Connection, String> {
        let connection = Connection::open(&self.state_path).map_err(|error| error.to_string())?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|error| error.to_string())?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE IF NOT EXISTS retention_state (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    lease_owner TEXT,
                    lease_expires_at INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT OR IGNORE INTO retention_state(singleton) VALUES (1);",
            )
            .map_err(|error| error.to_string())?;
        Ok(connection)
    }

    fn claim_lease(&self) -> Result<bool, String> {
        let mut connection = self.open_state()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let now = now_seconds();
        let lease_expires_at = transaction
            .query_row(
                "SELECT lease_expires_at FROM retention_state WHERE singleton = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| error.to_string())?;
        if lease_expires_at > now {
            return Ok(false);
        }
        let lease_seconds = CAPTURE_RETENTION_LEASE.as_secs().min(i64::MAX as u64) as i64;
        let changed = transaction
            .execute(
                "UPDATE retention_state
                 SET lease_owner = ?1, lease_expires_at = ?2
                 WHERE singleton = 1 AND lease_expires_at <= ?3",
                params![self.owner, now.saturating_add(lease_seconds), now],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(changed == 1)
    }

    fn release_lease(&self) -> Result<(), String> {
        let connection = self.open_state()?;
        let now = now_seconds();
        let changed = connection
            .execute(
                "UPDATE retention_state
                 SET lease_owner = NULL, lease_expires_at = 0
                 WHERE singleton = 1 AND lease_owner = ?1 AND lease_expires_at > ?2",
                params![self.owner, now],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err("session capture retention lease is no longer held".to_string());
        }
        Ok(())
    }
}

struct RuntimeWorker {
    repository_id: String,
    memory_path: PathBuf,
    state_path: PathBuf,
    owner: String,
    config: SessionDigestRuntimeConfig,
    driver: Arc<dyn LlmDriver + Send + Sync>,
    event_writer: Arc<EventWriter>,
}

impl RuntimeWorker {
    fn run_once(&self) -> Result<RunOutcome, String> {
        let lease = self.claim_due_captures()?;
        let Some(watermark) = lease else {
            return Ok(RunOutcome::Idle);
        };
        let result = self.run_consolidator();
        match result {
            Ok(outcome) => {
                self.finish_success(&watermark)?;
                Ok(match outcome {
                    SessionDigestConsolidationOutcome::Proposed { proposal_ids, .. } => {
                        RunOutcome::Proposed(proposal_ids.len())
                    }
                    SessionDigestConsolidationOutcome::Skipped { .. } => RunOutcome::Skipped,
                })
            }
            Err(WorkerRunError::Terminal(error)) => {
                // A content-free, non-retryable provider rejection is consumed
                // for this capture generation. Retrying an invalid credential
                // or request every interval would generate cost/noise without
                // improving recovery; a newly committed capture can trigger a
                // later run after configuration changes.
                self.finish_success(&watermark)?;
                tracing::warn!(%error, "session-digest consolidation provider rejected the bounded request");
                Ok(RunOutcome::Skipped)
            }
            Err(WorkerRunError::Retryable(error)) => {
                self.finish_failure()?;
                Err(error)
            }
        }
    }

    fn run_consolidator(&self) -> Result<SessionDigestConsolidationOutcome, WorkerRunError> {
        let memory_store =
            MemoryStore::open(&self.memory_path).map_err(|error| WorkerRunError::retry(error))?;
        let connection =
            Connection::open(&self.memory_path).map_err(|error| WorkerRunError::retry(error))?;
        let mut runtime = ConsolidationJobRuntime::new(
            connection,
            ConsolidationConfig {
                max_queue_depth: self.config.max_pending_proposals,
                llm_budget_catalog: None,
            },
        )
        .map_err(WorkerRunError::retry)?;
        let mut services = LlmJobServices {
            driver: self.driver.as_ref(),
            runtime: &mut runtime,
            memory_store: &memory_store,
            event_writer: self.event_writer.as_ref(),
        };
        SessionDigestLlmConsolidator::run(
            &self
                .config
                .core_config()
                .map_err(WorkerRunError::Retryable)?,
            &self.repository_id,
            &mut services,
        )
        .map_err(|error| match &error {
            lattice_core::consolidation::llm::LlmJobError::DriverError(LlmDriverError::Failed(
                message,
            )) if message.contains("non_retryable HTTP status") => {
                WorkerRunError::Terminal(error.to_string())
            }
            _ => WorkerRunError::Retryable(error.to_string()),
        })
    }

    fn open_state(&self) -> Result<Connection, String> {
        let connection = Connection::open(&self.state_path).map_err(|error| error.to_string())?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|error| error.to_string())?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE IF NOT EXISTS runtime_state (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    lease_owner TEXT,
                    lease_expires_at INTEGER NOT NULL DEFAULT 0,
                    consecutive_failures INTEGER NOT NULL DEFAULT 0,
                    next_eligible_at INTEGER NOT NULL DEFAULT 0,
                    watermark_created_at INTEGER NOT NULL DEFAULT -1,
                    watermark_delivery_key TEXT NOT NULL DEFAULT ''
                 );
                 INSERT OR IGNORE INTO runtime_state(singleton) VALUES (1);
                 CREATE TABLE IF NOT EXISTS watermark_capture_keys (
                    delivery_key TEXT PRIMARY KEY,
                    created_at INTEGER NOT NULL
                 );",
            )
            .map_err(|error| error.to_string())?;
        let memory_path = self
            .memory_path
            .to_str()
            .ok_or_else(|| "repository memory path is not valid UTF-8".to_string())?;
        connection
            .execute("ATTACH DATABASE ?1 AS memory", params![memory_path])
            .map_err(|error| error.to_string())?;
        Ok(connection)
    }

    fn claim_due_captures(&self) -> Result<Option<CaptureWatermark>, String> {
        let mut connection = self.open_state()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let now = now_seconds();
        let state = transaction
            .query_row(
                "SELECT lease_owner, lease_expires_at, next_eligible_at FROM runtime_state WHERE singleton = 1",
                [],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
            )
            .map_err(|error| error.to_string())?;
        if state.1 > now {
            return Ok(None);
        }
        if state.2 > now {
            return Ok(None);
        }
        let oldest = transaction
            .query_row(
                "SELECT d.created_at, d.delivery_key
                 FROM memory.session_digest_deliveries d
                 CROSS JOIN runtime_state s
                 WHERE d.repository_id = ?1
                   AND d.committed_count = d.candidate_count
                   AND (
                       d.created_at > s.watermark_created_at
                       OR (d.created_at = s.watermark_created_at
                           AND NOT EXISTS (
                               SELECT 1 FROM watermark_capture_keys w
                               WHERE w.delivery_key = d.delivery_key
                           ))
                   )
                 ORDER BY d.created_at ASC, d.delivery_key ASC
                 LIMIT 1",
                params![self.repository_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let Some((created_at, delivery_key)) = oldest else {
            return Ok(None);
        };
        let watermark = CaptureWatermark {
            created_at,
            delivery_keys: vec![delivery_key],
        };
        let lease_seconds = self
            .config
            .timeout
            .as_secs()
            .saturating_add(30)
            .min(i64::MAX as u64) as i64;
        transaction
            .execute(
                "UPDATE runtime_state SET lease_owner = ?1, lease_expires_at = ?2 WHERE singleton = 1",
                params![self.owner, now.saturating_add(lease_seconds)],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(Some(watermark))
    }

    fn finish_success(&self, watermark: &CaptureWatermark) -> Result<(), String> {
        let mut connection = self.open_state()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let current_watermark = transaction
            .query_row(
                "SELECT lease_owner, lease_expires_at, watermark_created_at
                 FROM runtime_state WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .map_err(|error| error.to_string())?;
        if current_watermark.0.as_deref() != Some(self.owner.as_str())
            || current_watermark.1 <= now_seconds()
        {
            return Err("session-digest consolidation lease is no longer held".to_string());
        }
        if watermark.created_at > current_watermark.2 {
            transaction
                .execute("DELETE FROM watermark_capture_keys", [])
                .map_err(|error| error.to_string())?;
        }
        for delivery_key in &watermark.delivery_keys {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO watermark_capture_keys(delivery_key, created_at)
                     VALUES (?1, ?2)",
                    params![delivery_key, watermark.created_at],
                )
                .map_err(|error| error.to_string())?;
        }
        transaction
            .execute(
                "UPDATE runtime_state
                 SET lease_owner = NULL, lease_expires_at = 0,
                     consecutive_failures = 0, next_eligible_at = 0,
                     watermark_created_at = ?1, watermark_delivery_key = ?2
                 WHERE singleton = 1 AND lease_owner = ?3",
                params![
                    watermark.created_at,
                    watermark.delivery_keys.last().cloned().unwrap_or_default(),
                    self.owner
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())
    }

    fn finish_failure(&self) -> Result<(), String> {
        let connection = self.open_state()?;
        let now = now_seconds();
        let failures = connection
            .query_row(
                "SELECT consecutive_failures FROM runtime_state WHERE singleton = 1",
                [],
                |row| row.get::<_, u32>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .unwrap_or(0)
            .saturating_add(1);
        let exponent = failures.saturating_sub(1).min(6);
        let backoff = self
            .config
            .interval
            .as_secs()
            .saturating_mul(1_u64 << exponent)
            .min(60 * 60)
            .max(1);
        connection
            .execute(
                "UPDATE runtime_state
                 SET lease_owner = NULL, lease_expires_at = 0,
                     consecutive_failures = ?1, next_eligible_at = ?2
                 WHERE singleton = 1 AND lease_owner = ?3",
                params![failures, now.saturating_add(backoff as i64), self.owner],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunOutcome {
    Idle,
    Skipped,
    Proposed(usize),
}

#[derive(Debug, PartialEq, Eq)]
enum WorkerRunError {
    Retryable(String),
    Terminal(String),
}

impl WorkerRunError {
    fn retry(error: impl ToString) -> Self {
        Self::Retryable(error.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CaptureWatermark {
    created_at: i64,
    delivery_keys: Vec<String>,
}

/// Start repository-local automatic capture retention. This intentionally has
/// no relationship to opt-in session-digest consolidation or LLM credentials.
pub(crate) fn start_capture_retention(
    repository_id: String,
    checkout_id: String,
    memory_path: PathBuf,
) -> Option<SessionCaptureRetentionHandle> {
    let config = match SessionCaptureRetentionRuntimeConfig::from_environment() {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(%error, "session capture retention runtime disabled due to invalid configuration");
            return None;
        }
    };
    Some(spawn_capture_retention_worker(
        repository_id,
        checkout_id,
        memory_path,
        config,
    ))
}

fn spawn_capture_retention_worker(
    repository_id: String,
    checkout_id: String,
    memory_path: PathBuf,
    config: SessionCaptureRetentionRuntimeConfig,
) -> SessionCaptureRetentionHandle {
    let state_path = memory_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("session_capture_retention_runtime.db");
    let worker = Arc::new(SessionCaptureRetentionWorker {
        repository_id,
        checkout_id,
        memory_path,
        state_path,
        owner: format!("{}-{}", std::process::id(), now_micros()),
        config: config.clone(),
    });
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(config.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                _ = interval.tick() => {
                    let run_worker = Arc::clone(&worker);
                    match tokio::task::spawn_blocking(move || run_worker.run_once()).await {
                        Ok(Ok(result)) if result.deleted_capture_ids.is_empty() => {}
                        Ok(Ok(result)) => {
                            tracing::info!(
                                deleted_capture_count = result.deleted_capture_ids.len(),
                                deleted_memory_count = result.deleted_memory_count,
                                retained_derived_memory_count = result.retained_derived_memory_count,
                                retained_proposal_count = result.retained_proposal_count,
                                "session capture retention pruned repository-local captures"
                            );
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(%error, "session capture retention run failed; scheduler remains active");
                        }
                        Err(error) => {
                            tracing::warn!(%error, "session capture retention worker failed; scheduler remains active");
                        }
                    }
                }
            }
        }
    });
    SessionCaptureRetentionHandle {
        shutdown: Some(shutdown_tx),
        task,
    }
}

pub(crate) fn start(
    repository_id: String,
    memory_path: PathBuf,
    event_writer: Arc<EventWriter>,
) -> Option<SessionDigestConsolidationHandle> {
    let config = match SessionDigestRuntimeConfig::from_environment() {
        Ok(Some(config)) => config,
        Ok(None) => {
            tracing::debug!("session-digest consolidation runtime disabled");
            return None;
        }
        Err(error) => {
            tracing::warn!(%error, "session-digest consolidation runtime disabled due to invalid configuration");
            return None;
        }
    };
    let driver = match ProviderDriver::new(&config) {
        Ok(driver) => Arc::new(driver) as Arc<dyn LlmDriver + Send + Sync>,
        Err(error) => {
            tracing::warn!(%error, "session-digest consolidation runtime disabled");
            return None;
        }
    };
    Some(spawn_worker(
        repository_id,
        memory_path,
        event_writer,
        config,
        driver,
    ))
}

fn spawn_worker(
    repository_id: String,
    memory_path: PathBuf,
    event_writer: Arc<EventWriter>,
    config: SessionDigestRuntimeConfig,
    driver: Arc<dyn LlmDriver + Send + Sync>,
) -> SessionDigestConsolidationHandle {
    let state_path = memory_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("session_digest_consolidation_runtime.db");
    let worker = Arc::new(RuntimeWorker {
        repository_id,
        memory_path,
        state_path,
        owner: format!("{}-{}", std::process::id(), now_micros()),
        config: config.clone(),
        driver,
        event_writer,
    });
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(config.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Do not turn daemon startup into an implicit provider call.
        interval.tick().await;
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                _ = interval.tick() => {
                    let run_worker = Arc::clone(&worker);
                    match tokio::task::spawn_blocking(move || run_worker.run_once()).await {
                        Ok(Ok(RunOutcome::Idle | RunOutcome::Skipped)) => {}
                        Ok(Ok(RunOutcome::Proposed(count))) => {
                            tracing::info!(proposal_count = count, "session-digest consolidation proposals queued for review");
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(%error, "session-digest consolidation run failed; retry remains pending");
                        }
                        Err(error) => {
                            tracing::warn!(%error, "session-digest consolidation worker failed; scheduler remains active");
                        }
                    }
                }
            }
        }
    });
    SessionDigestConsolidationHandle {
        shutdown: Some(shutdown_tx),
        task,
    }
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

fn now_micros() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use lattice_core::consolidation::{ProposalDecision, ReviewQueue, ReviewQueueFilter};
    use lattice_core::events::{EventStore, FlushPolicy};
    use lattice_core::memory::{
        extract_default_session_digest_candidates, CheckOutcome, MemoryQueryAuthority,
        MemoryStoreRouter, SessionDigest, SessionDigestObservation,
    };
    use lattice_core::{DateTime, Utc};
    use tempfile::tempdir;

    const REPOSITORY: &str = "repo-runtime-test";
    const RESPONSE: &str = r#"{"proposals":[{"memory_class":"decision","content":"Review-gated consolidation is required.","evidence_summary":"A committed capture supports this proposal.","uncertainty":"One session was observed.","confidence":0.8}]}"#;

    #[derive(Default)]
    struct MockDriver {
        calls: AtomicUsize,
        responses: Mutex<VecDeque<Result<String, LlmDriverError>>>,
    }

    impl MockDriver {
        fn push(&self, response: Result<&str, LlmDriverError>) {
            self.responses
                .lock()
                .unwrap()
                .push_back(response.map(str::to_string));
        }
    }

    impl LlmDriver for MockDriver {
        fn complete(&self, _: LlmRequest) -> Result<LlmResponse, LlmDriverError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(LlmDriverError::Failed("no response".to_string())))
                .map(|content| LlmResponse { content })
        }

        fn name(&self) -> &str {
            "mock-model"
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        memory_path: PathBuf,
        event_writer: Arc<EventWriter>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempdir().unwrap();
            let memory_path = dir.path().join("memories.db");
            MemoryStore::open(&memory_path).unwrap();
            let events = Arc::new(EventStore::open_in_memory().unwrap());
            let event_writer = Arc::new(
                EventWriter::new(events, REPOSITORY.to_string(), 64)
                    .with_flush_policy(FlushPolicy::Sync),
            );
            Self {
                _dir: dir,
                memory_path,
                event_writer,
            }
        }

        fn capture(&self, session: &str) {
            let store = MemoryStore::open(&self.memory_path).unwrap();
            let authority = MemoryQueryAuthority::new(
                REPOSITORY.to_string(),
                "checkout-main".to_string(),
                Some("main".to_string()),
                session.to_string(),
                None,
            )
            .unwrap();
            let router = MemoryStoreRouter::new(&store, None, authority).unwrap();
            let now = DateTime::<Utc>::from_unix_seconds(now_seconds());
            let digest = SessionDigest {
                schema_version: lattice_core::memory::SESSION_DIGEST_SCHEMA_VERSION,
                session_id: session.to_string(),
                repository_id: REPOSITORY.to_string(),
                checkout_id: Some("checkout-main".to_string()),
                branch: Some("main".to_string()),
                revision: format!("revision-{session}"),
                segment: 0,
                ended_at: now,
                received_at: now,
                edited_paths: vec!["daemon/src/main.rs".to_string()],
                final_summary: Some("Kept consolidation review gated".to_string()),
                observations: vec![SessionDigestObservation::Check {
                    label: "runtime test".to_string(),
                    outcome: CheckOutcome::Passed,
                }],
                payload_hash:
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        .to_string(),
                dropped_observation_count: 0,
            };
            let candidates = extract_default_session_digest_candidates(&digest);
            router
                .capture_session_digest_candidate_batch(&digest, &candidates)
                .unwrap();
        }

        fn capture_at(&self, session: &str, created_at: i64) {
            self.capture(session);
            let connection = Connection::open(&self.memory_path).unwrap();
            connection
                .execute(
                    "UPDATE session_digest_deliveries SET created_at = ?1 WHERE session_id = ?2",
                    params![created_at, session],
                )
                .unwrap();
        }

        fn worker(&self, driver: Arc<dyn LlmDriver + Send + Sync>) -> RuntimeWorker {
            let config = SessionDigestRuntimeConfig::test(
                SessionDigestLlmProvider::OpenAi,
                "http://127.0.0.1:1".to_string(),
            );
            RuntimeWorker {
                repository_id: REPOSITORY.to_string(),
                state_path: self._dir.path().join("runtime.db"),
                memory_path: self.memory_path.clone(),
                owner: format!("test-{}", now_micros()),
                config,
                driver,
                event_writer: Arc::clone(&self.event_writer),
            }
        }

        fn retention_worker(
            &self,
            config: SessionCaptureRetentionRuntimeConfig,
        ) -> SessionCaptureRetentionWorker {
            SessionCaptureRetentionWorker {
                repository_id: REPOSITORY.to_string(),
                checkout_id: "checkout-main".to_string(),
                memory_path: self.memory_path.clone(),
                state_path: self._dir.path().join("capture-retention.db"),
                owner: format!("retention-test-{}", now_micros()),
                config,
            }
        }
    }

    fn table_count(path: &Path, table: &str) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    async fn wait_for_table_count(path: &Path, table: &str, expected: i64) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if table_count(path, table) == expected {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {table} to reach {expected} rows"));
    }

    #[test]
    fn capture_retention_defaults_are_bounded_without_provider_configuration() {
        let config = SessionCaptureRetentionRuntimeConfig::from_lookup(|_| None).unwrap();

        assert_eq!(config.interval, DEFAULT_CAPTURE_RETENTION_INTERVAL);
        assert_eq!(config.max_age, DEFAULT_CAPTURE_RETENTION_MAX_AGE);
        assert_eq!(config.max_captures, DEFAULT_CAPTURE_RETENTION_MAX_CAPTURES);
        assert!(config.policy().is_ok());
    }

    #[test]
    fn capture_retention_rejects_unbounded_configuration() {
        let environment = std::collections::HashMap::from([
            (CAPTURE_RETENTION_INTERVAL_ENV, "86401"),
            (CAPTURE_RETENTION_MAX_AGE_ENV, "1"),
            (CAPTURE_RETENTION_MAX_CAPTURES_ENV, "1"),
        ]);
        assert_eq!(
            SessionCaptureRetentionRuntimeConfig::from_lookup(|name| {
                environment.get(name).map(|value| (*value).to_string())
            })
            .unwrap_err(),
            SessionCaptureRetentionConfigError::ExceedsBound("interval")
        );
    }

    #[test]
    fn retention_worker_prunes_by_repository_router_and_records_tombstones() {
        let fixture = Fixture::new();
        fixture.capture_at("expired-capture", now_seconds().saturating_sub(10));
        fixture.capture_at("newest-capture", now_seconds());
        let worker = fixture.retention_worker(SessionCaptureRetentionRuntimeConfig::test(
            Duration::from_secs(1),
            Duration::from_secs(1),
            1,
        ));
        let memory_count_before = table_count(&fixture.memory_path, "memories");

        let result = worker.run_once().unwrap();

        assert_eq!(result.deleted_capture_ids.len(), 1);
        assert!(result.deleted_memory_count > 0);
        assert_eq!(
            table_count(&fixture.memory_path, "session_digest_deliveries"),
            1
        );
        assert_eq!(
            table_count(&fixture.memory_path, "memories"),
            memory_count_before - result.deleted_memory_count as i64
        );
        assert_eq!(
            table_count(&fixture.memory_path, "session_capture_tombstones"),
            1
        );
        let tombstone: (String, String) = Connection::open(&fixture.memory_path)
            .unwrap()
            .query_row(
                "SELECT repository_id, deletion_reason FROM session_capture_tombstones",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(tombstone, (REPOSITORY.to_string(), "retention".to_string()));
    }

    #[test]
    fn retention_lease_prevents_a_second_worker_from_pruning_the_same_repository() {
        let fixture = Fixture::new();
        fixture.capture_at("leased-capture", now_seconds().saturating_sub(10));
        let config = SessionCaptureRetentionRuntimeConfig::test(
            Duration::from_secs(1),
            Duration::from_secs(1),
            1,
        );
        let first = fixture.retention_worker(config.clone());
        let mut second = fixture.retention_worker(config);
        second.state_path = first.state_path.clone();

        assert!(first.claim_lease().unwrap());
        assert!(second.run_once().unwrap().deleted_capture_ids.is_empty());
        assert_eq!(
            table_count(&fixture.memory_path, "session_capture_tombstones"),
            0
        );
        first.release_lease().unwrap();
    }

    #[tokio::test]
    async fn periodic_retention_runs_without_consolidation_or_a_provider_and_stays_repo_local() {
        let fixture = Fixture::new();
        fixture.capture_at("repo-a-expired", now_seconds().saturating_sub(10));

        let other_directory = tempdir().unwrap();
        let other_path = other_directory.path().join("memories.db");
        let other_store = MemoryStore::open(&other_path).unwrap();
        let other_authority =
            MemoryQueryAuthority::new("repo-other", "checkout-other", None, "other-session", None)
                .unwrap();
        let other_router = MemoryStoreRouter::new(&other_store, None, other_authority).unwrap();
        let now = DateTime::<Utc>::from_unix_seconds(now_seconds());
        let other_digest = SessionDigest {
            schema_version: lattice_core::memory::SESSION_DIGEST_SCHEMA_VERSION,
            session_id: "other-session".to_string(),
            repository_id: "repo-other".to_string(),
            checkout_id: Some("checkout-other".to_string()),
            branch: None,
            revision: "other-revision".to_string(),
            segment: 0,
            ended_at: now,
            received_at: now,
            edited_paths: vec!["src/other.rs".to_string()],
            final_summary: Some("Other repository capture".to_string()),
            observations: Vec::new(),
            payload_hash: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .to_string(),
            dropped_observation_count: 0,
        };
        let other_candidates = extract_default_session_digest_candidates(&other_digest);
        other_router
            .capture_session_digest_candidate_batch(&other_digest, &other_candidates)
            .unwrap();

        let handle = spawn_capture_retention_worker(
            REPOSITORY.to_string(),
            "checkout-main".to_string(),
            fixture.memory_path.clone(),
            SessionCaptureRetentionRuntimeConfig::test(
                Duration::from_millis(10),
                Duration::from_secs(1),
                1,
            ),
        );
        wait_for_table_count(&fixture.memory_path, "session_capture_tombstones", 1).await;
        handle.shutdown().await;

        assert_eq!(
            table_count(&fixture.memory_path, "session_digest_deliveries"),
            0
        );
        assert_eq!(table_count(&other_path, "session_digest_deliveries"), 1);
        assert_eq!(table_count(&other_path, "session_capture_tombstones"), 0);
    }

    #[test]
    fn disabled_and_missing_key_configuration_never_construct_a_runtime() {
        let mut environment = std::collections::HashMap::<&str, &str>::new();
        assert!(SessionDigestRuntimeConfig::from_lookup(|name| {
            environment.get(name).map(|value| (*value).to_string())
        })
        .unwrap()
        .is_none());
        environment.insert(ENABLE_ENV, "1");
        environment.insert(PROVIDER_ENV, "openai");
        environment.insert(MODEL_ENV, "mock-model");
        assert_eq!(
            SessionDigestRuntimeConfig::from_lookup(|name| {
                environment.get(name).map(|value| (*value).to_string())
            })
            .unwrap_err(),
            RuntimeConfigError::Missing("OPENAI_API_KEY")
        );
    }

    #[test]
    fn enabled_configuration_rejects_unbounded_recurrence() {
        let environment = std::collections::HashMap::from([
            (ENABLE_ENV, "1"),
            (PROVIDER_ENV, "openai"),
            (MODEL_ENV, "mock-model"),
            ("OPENAI_API_KEY", "mock-provider-key"),
            (INTERVAL_ENV, "86401"),
        ]);
        assert_eq!(
            SessionDigestRuntimeConfig::from_lookup(|name| {
                environment.get(name).map(|value| (*value).to_string())
            })
            .unwrap_err(),
            RuntimeConfigError::ExceedsBound("interval")
        );
    }

    #[test]
    fn enabled_worker_runs_committed_capture_once_and_creates_only_pending_review_proposals() {
        let fixture = Fixture::new();
        fixture.capture("session-one");
        let driver = Arc::new(MockDriver::default());
        driver.push(Ok(RESPONSE));
        let worker = fixture.worker(driver.clone());
        let memory_count_before = MemoryStore::open(&fixture.memory_path)
            .unwrap()
            .list_all()
            .unwrap()
            .len();

        assert_eq!(worker.run_once().unwrap(), RunOutcome::Proposed(1));
        assert_eq!(worker.run_once().unwrap(), RunOutcome::Idle);
        assert_eq!(driver.calls.load(Ordering::SeqCst), 1);

        let memory = MemoryStore::open(&fixture.memory_path).unwrap();
        assert_eq!(memory.list_all().unwrap().len(), memory_count_before);
        let connection = Connection::open(&fixture.memory_path).unwrap();
        let queue = ReviewQueue::new(&connection, &memory, fixture.event_writer.as_ref());
        let items = queue
            .list_pending(REPOSITORY, &ReviewQueueFilter::default())
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].decision, ProposalDecision::Pending);
        assert!(items[0].target_memory_id.is_none());
    }

    #[test]
    fn failed_run_releases_lease_and_retries_without_losing_capture() {
        let fixture = Fixture::new();
        fixture.capture("session-recovery");
        let driver = Arc::new(MockDriver::default());
        driver.push(Err(LlmDriverError::Unavailable("offline".to_string())));
        driver.push(Ok(RESPONSE));
        let mut worker = fixture.worker(driver.clone());

        assert!(worker.run_once().is_err());
        assert_eq!(driver.calls.load(Ordering::SeqCst), 1);
        // Use a zero backoff only in this direct recovery test.
        worker.config.interval = Duration::from_nanos(1);
        let connection = worker.open_state().unwrap();
        connection
            .execute("UPDATE runtime_state SET next_eligible_at = 0", [])
            .unwrap();
        assert_eq!(worker.run_once().unwrap(), RunOutcome::Proposed(1));
        assert_eq!(driver.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn non_retryable_provider_rejection_is_consumed_without_exposing_content() {
        let fixture = Fixture::new();
        fixture.capture("session-terminal");
        let driver = Arc::new(MockDriver::default());
        driver.push(Err(LlmDriverError::Failed(
            "provider returned non_retryable HTTP status 401".to_string(),
        )));
        let worker = fixture.worker(driver.clone());

        assert_eq!(worker.run_once().unwrap(), RunOutcome::Skipped);
        assert_eq!(worker.run_once().unwrap(), RunOutcome::Idle);
        assert_eq!(driver.calls.load(Ordering::SeqCst), 1);
        let connection = Connection::open(&fixture.memory_path).unwrap();
        let proposal_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM consolidation_proposals", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(proposal_count, 0);
    }

    #[test]
    fn exclusive_lease_allows_only_one_worker_to_call_provider() {
        let fixture = Fixture::new();
        fixture.capture("session-lease");
        let driver = Arc::new(MockDriver::default());
        driver.push(Ok(RESPONSE));
        let first = fixture.worker(driver.clone());
        let mut second = fixture.worker(driver.clone());
        second.state_path = first.state_path.clone();
        let claimed = first.claim_due_captures().unwrap().unwrap();

        assert!(second.claim_due_captures().unwrap().is_none());
        assert_eq!(driver.calls.load(Ordering::SeqCst), 0);
        first.finish_success(&claimed).unwrap();
    }

    #[test]
    fn expired_lease_cannot_advance_the_checkpoint() {
        let fixture = Fixture::new();
        fixture.capture("session-expired-lease");
        let worker = fixture.worker(Arc::new(MockDriver::default()));
        let claimed = worker.claim_due_captures().unwrap().unwrap();
        let connection = worker.open_state().unwrap();
        connection
            .execute("UPDATE runtime_state SET lease_expires_at = 0", [])
            .unwrap();

        assert_eq!(
            worker.finish_success(&claimed).unwrap_err(),
            "session-digest consolidation lease is no longer held"
        );
        let connection = worker.open_state().unwrap();
        let checkpoint: (i64, String) = connection
            .query_row(
                "SELECT watermark_created_at, watermark_delivery_key
                 FROM runtime_state WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(checkpoint, (-1, String::new()));
    }

    #[test]
    fn checkpoint_never_consumes_an_incomplete_same_second_capture() {
        let fixture = Fixture::new();
        fixture.capture("session-committed");
        fixture.capture("session-incomplete");
        let connection = Connection::open(&fixture.memory_path).unwrap();
        connection
            .execute("UPDATE session_digest_deliveries SET created_at = 1234", [])
            .unwrap();
        connection
            .execute(
                "UPDATE session_digest_deliveries SET committed_count = 0 WHERE session_id = ?1",
                params!["session-incomplete"],
            )
            .unwrap();
        let driver = Arc::new(MockDriver::default());
        let worker = fixture.worker(driver.clone());

        let first = worker.claim_due_captures().unwrap().unwrap();
        assert_eq!(first.delivery_keys.len(), 1);
        worker.finish_success(&first).unwrap();
        connection
            .execute(
                "UPDATE session_digest_deliveries
                 SET committed_count = candidate_count WHERE session_id = ?1",
                params!["session-incomplete"],
            )
            .unwrap();
        let second = worker.claim_due_captures().unwrap().unwrap();
        assert_eq!(second.delivery_keys.len(), 1);
        assert_ne!(first.delivery_keys, second.delivery_keys);
        assert_eq!(driver.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn oldest_pending_checkpoint_drains_backlog_beyond_source_limit_without_skips() {
        let fixture = Fixture::new();
        for index in 0..5 {
            fixture.capture_at(&format!("session-backlog-{index}"), 10_000 + index);
        }
        let driver = Arc::new(MockDriver::default());
        let mut worker = fixture.worker(driver);
        worker.config.max_source_facts = 2;

        let connection = Connection::open(&fixture.memory_path).unwrap();
        let expected = {
            let mut statement = connection
                .prepare(
                    "SELECT delivery_key
                     FROM session_digest_deliveries
                     WHERE repository_id = ?1 AND committed_count = candidate_count
                     ORDER BY created_at ASC, delivery_key ASC",
                )
                .unwrap();
            statement
                .query_map(params![REPOSITORY], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };

        let mut claimed = Vec::new();
        for expected_key in &expected {
            let watermark = worker.claim_due_captures().unwrap().unwrap();
            assert_eq!(watermark.delivery_keys, vec![expected_key.clone()]);
            claimed.extend(watermark.delivery_keys.iter().cloned());
            worker.finish_success(&watermark).unwrap();
        }

        assert_eq!(claimed, expected);
        assert!(worker.claim_due_captures().unwrap().is_none());
    }

    #[test]
    fn provider_drivers_use_bounded_documented_wire_shapes() {
        for provider in [
            SessionDigestLlmProvider::OpenAi,
            SessionDigestLlmProvider::Anthropic,
        ] {
            let (endpoint, request_rx) = one_shot_server(provider);
            let config = SessionDigestRuntimeConfig::test(provider, endpoint);
            let driver = ProviderDriver::new(&config).unwrap();
            let response = driver
                .complete(LlmRequest {
                    workspace_id: REPOSITORY.to_string(),
                    job_kind: lattice_core::consolidation::llm::ConsolidationJobKind::SessionDigestConsolidation,
                    prompt: "persisted sanitized facts".to_string(),
                    response_schema: "strict JSON".to_string(),
                })
                .unwrap();
            assert_eq!(response.content, RESPONSE);
            let request = request_rx.recv().unwrap();
            assert!(request.contains("mock-model"));
            assert!(request.contains("persisted sanitized facts"));
            assert!(!request.contains("RAW_TRANSCRIPT"));
            match provider {
                SessionDigestLlmProvider::OpenAi => {
                    assert!(request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer mock-provider-key"));
                    assert!(request.contains("/v1/responses"));
                }
                SessionDigestLlmProvider::Anthropic => {
                    assert!(request
                        .to_ascii_lowercase()
                        .contains("x-api-key: mock-provider-key"));
                    assert!(request.contains("anthropic-version: 2023-06-01"));
                    assert!(request.contains("/v1/messages"));
                }
            }
        }
    }

    #[test]
    fn provider_driver_rejects_oversized_response_without_parsing_it() {
        let (endpoint, _request_rx) = one_shot_server(SessionDigestLlmProvider::OpenAi);
        let mut config =
            SessionDigestRuntimeConfig::test(SessionDigestLlmProvider::OpenAi, endpoint);
        config.max_response_bytes = 32;
        let driver = ProviderDriver::new(&config).unwrap();

        let error = driver
            .complete(LlmRequest {
                workspace_id: REPOSITORY.to_string(),
                job_kind: lattice_core::consolidation::llm::ConsolidationJobKind::SessionDigestConsolidation,
                prompt: "persisted sanitized facts".to_string(),
                response_schema: "strict JSON".to_string(),
            })
            .unwrap_err();
        assert_eq!(
            error,
            LlmDriverError::Failed(
                "provider response exceeded the configured safety bound".to_string()
            )
        );
    }

    fn one_shot_server(
        provider: SessionDigestLlmProvider,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    request.push_str(&line);
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().unwrap();
                }
                request.push_str(&line);
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            request.push_str(&String::from_utf8(body).unwrap());
            tx.send(request).unwrap();
            let payload = match provider {
                SessionDigestLlmProvider::OpenAi => json!({
                    "output": [{"type":"message","content":[{"type":"output_text","text":RESPONSE}]}]
                }),
                SessionDigestLlmProvider::Anthropic => json!({
                    "content": [{"type":"text","text":RESPONSE}]
                }),
            }
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                payload.len(),
                payload
            )
            .unwrap();
        });
        (
            format!(
                "http://{address}/v1/{}",
                match provider {
                    SessionDigestLlmProvider::OpenAi => "responses",
                    SessionDigestLlmProvider::Anthropic => "messages",
                }
            ),
            rx,
        )
    }
}
