//! Private adapter for installed host hook packages.
//!
//! Host envelopes terminate here. Only an opaque host session identifier and
//! an edit hook's dedicated path field are retained. Repository authority is
//! always derived from the process working directory.

use anyhow::{anyhow, Result};
use lattice_core::memory::{
    parse_session_capture_close, parse_session_capture_event,
    session_capture_turn_summary_from_host, SessionCaptureClose, SessionCaptureEvent,
    SessionCaptureFact, SESSION_CAPTURE_SCHEMA_VERSION,
};
use lattice_core::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use crate::hook_session_client::{
    HookClientBinding, HookClientBindingKey, HookClientCapturePayload, HookClientOpaqueId,
    HookSessionClient, HookSessionClientError, PendingHookDelivery,
};
use crate::hook_session_route::{
    HOOK_EVENT_METHOD, HOOK_SESSION_CLOSE_METHOD, HOOK_SESSION_OPEN_METHOD,
    HOOK_TURN_SUMMARY_METHOD,
};
use crate::proxy::daemon_addr;
use crate::transport::{self, ClientKind, ProxyRequest};
use crate::workspace_identity::WorkspaceIdentity;

const MAX_HOST_ENVELOPE_BYTES: usize = 64 * 1024;
const ADAPTER_DEADLINE: Duration = Duration::from_millis(2_000);
const SESSION_START_NOTICE: &str = "lattice: daemon unreachable — run 'lattice doctor'";
const SESSION_START_NOTICE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Integration {
    Codex,
    ClaudeCode,
}

impl Integration {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "codex" => Some(Self::Codex),
            "claude-code" => Some(Self::ClaudeCode),
            _ => None,
        }
    }

    fn binding_id(self) -> &'static str {
        match self {
            Self::Codex => "codex-hooks/v1",
            Self::ClaudeCode => "claude-code-hooks/v1",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HookKind {
    SessionStart,
    UserPromptSubmit,
    PostToolUse,
    Stop,
    StructuredFact,
    SessionEnd,
}

impl HookKind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "session-start" => Some(Self::SessionStart),
            "user-prompt-submit" => Some(Self::UserPromptSubmit),
            "post-tool-use" => Some(Self::PostToolUse),
            "stop" => Some(Self::Stop),
            "structured-fact" => Some(Self::StructuredFact),
            "session-end" => Some(Self::SessionEnd),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct Invocation {
    integration: Integration,
    kind: HookKind,
}

impl Invocation {
    fn from_env() -> Option<Self> {
        let mut args = std::env::args();
        let _program = args.next()?;
        if args.next()?.as_str() != "__hook-adapter" {
            return None;
        }
        let integration = Integration::parse(&args.next()?)?;
        let kind = HookKind::parse(&args.next()?)?;
        if args.next().is_some() {
            return None;
        }
        Some(Self { integration, kind })
    }
}

#[derive(Debug)]
struct HostFact {
    host_session_id: String,
    payload: Option<HookClientCapturePayload>,
    presentation: Option<HostPresentationRequest>,
}

#[derive(Debug)]
struct HostPresentationRequest {
    kind: HookKind,
    request_id: String,
    prompt: Option<String>,
    path: Option<String>,
    acted_on_injection_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HostPresentationResult {
    injection_id: String,
    context: String,
}

#[derive(Debug, thiserror::Error)]
#[error("hook daemon transport unavailable")]
struct HookDaemonUnavailable(#[source] anyhow::Error);

pub(crate) fn is_hook_adapter_command() -> bool {
    std::env::args().nth(1).as_deref() == Some("__hook-adapter")
}

/// Hook invocations are deliberately best-effort. Only authenticated, bounded
/// daemon presentations reach stdout; failures remain silent and never render
/// rejected host input.
pub(crate) async fn run_from_env() {
    let Some(invocation) = Invocation::from_env() else {
        return;
    };
    if let Ok(Ok(Some(output))) = tokio::time::timeout(ADAPTER_DEADLINE, run(invocation)).await {
        println!("{output}");
    }
}

async fn run(invocation: Invocation) -> Result<Option<String>> {
    let input = read_bounded_stdin(std::io::stdin().lock())?;
    let fact = extract_host_fact(invocation.kind, &input)?;
    if matches!(invocation.kind, HookKind::Stop | HookKind::StructuredFact)
        && fact.payload.is_none()
    {
        return Ok(None);
    }
    let identity = checkout_identity_from_cwd()?;
    let key = HookClientBindingKey::new(
        invocation.integration.binding_id(),
        fact.host_session_id.clone(),
        identity.repository_id.clone(),
        identity.checkout_root.to_string_lossy().to_string(),
    )?;
    let client = HookSessionClient::open_default()?;
    let now_ms = unix_time_ms()?;

    if invocation.kind == HookKind::SessionStart {
        client.retire_acknowledged_close(&key)?;
    }

    match invocation.kind {
        HookKind::SessionStart | HookKind::UserPromptSubmit => {
            let presentation = match open_or_resume(
                &client,
                &key,
                invocation.integration,
                &fact.host_session_id,
                &identity,
                now_ms,
                fact.presentation.as_ref(),
            )
            .await
            {
                Ok(presentation) => presentation,
                Err(error)
                    if invocation.kind == HookKind::SessionStart
                        && session_start_notice_eligible(&error) =>
                {
                    // A raw host session identifier is not authority.  Only
                    // the validated host envelope and checkout identity scope
                    // this non-authoritative recovery hint. The marker is
                    // privacy-preserving and claimed atomically, including
                    // before the daemon has minted the session's first binding.
                    if claim_session_start_notice(
                        invocation.integration,
                        &fact.host_session_id,
                        &identity,
                    )? {
                        return Ok(Some(render_session_start_notice(invocation.integration)));
                    }
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            Ok(presentation.map(|result| {
                render_host_presentation(invocation.integration, invocation.kind, result)
            }))
        }
        HookKind::PostToolUse => {
            let Some(payload) = fact.payload else {
                return Ok(None);
            };
            let presentation = open_or_resume(
                &client,
                &key,
                invocation.integration,
                &fact.host_session_id,
                &identity,
                now_ms,
                fact.presentation.as_ref(),
            )
            .await?;
            let binding = client.load_binding(&key, now_ms)?;
            match client.enqueue(&key, payload, now_ms) {
                Ok(_) => {}
                Err(error) => return Err(error.into()),
            }
            let mut wire = HookWire::connect(&identity).await?;
            flush_pending(&client, &key, &binding, &mut wire, now_ms).await?;
            Ok(presentation.map(|result| {
                render_host_presentation(invocation.integration, invocation.kind, result)
            }))
        }
        HookKind::Stop => {
            let Some(payload) = fact.payload else {
                return Ok(None);
            };
            let binding = client.load_binding(&key, now_ms)?;
            match client.enqueue(&key, payload, now_ms) {
                Ok(_) | Err(HookSessionClientError::BindingClosed) => {}
                Err(error) => return Err(error.into()),
            }
            let mut wire = HookWire::connect(&identity).await?;
            flush_pending(&client, &key, &binding, &mut wire, now_ms).await?;
            Ok(None)
        }
        HookKind::StructuredFact => {
            let Some(payload) = fact.payload else {
                return Ok(None);
            };
            let binding = client.load_binding(&key, now_ms)?;
            match client.enqueue(&key, payload, now_ms) {
                Ok(_) => {}
                Err(HookSessionClientError::BindingClosed) => return Ok(None),
                Err(error) => return Err(error.into()),
            }
            let mut wire = HookWire::connect(&identity).await?;
            flush_pending(&client, &key, &binding, &mut wire, now_ms).await?;
            Ok(None)
        }
        HookKind::SessionEnd => {
            let Some(payload) = fact.payload else {
                return Ok(None);
            };
            let binding = client.load_binding(&key, now_ms)?;
            match client.enqueue(&key, payload, now_ms) {
                Ok(_) | Err(HookSessionClientError::BindingClosed) => {}
                Err(error) => return Err(error.into()),
            }
            let mut wire = HookWire::connect(&identity).await?;
            flush_pending(&client, &key, &binding, &mut wire, now_ms).await?;
            Ok(None)
        }
    }
}

/// Claim the single recovery notice for a locally authenticated host session.
/// The marker is non-authoritative and stores no host data: it only prevents
/// a repeated bounded presentation after transport/configuration failure.
fn claim_session_start_notice(
    integration: Integration,
    host_session_id: &str,
    identity: &WorkspaceIdentity,
) -> Result<bool> {
    let marker = session_start_notice_marker(
        integration,
        host_session_id,
        &identity.repository_id,
        &identity.checkout_root,
    );
    claim_session_start_notice_at(&default_notice_root()?, &marker)
}

fn session_start_notice_eligible(error: &anyhow::Error) -> bool {
    error.downcast_ref::<HookDaemonUnavailable>().is_some()
}

fn render_session_start_notice(integration: Integration) -> String {
    match integration {
        Integration::Codex => SESSION_START_NOTICE.to_string(),
        Integration::ClaudeCode => json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": SESSION_START_NOTICE,
            }
        })
        .to_string(),
    }
}

fn session_start_notice_marker(
    integration: Integration,
    host_session_id: &str,
    repository_id: &str,
    checkout_root: &Path,
) -> String {
    stable_digest_hex(
        format!(
            "{}\0{}\0{}\0{}",
            integration.binding_id(),
            host_session_id,
            repository_id,
            checkout_root.to_string_lossy()
        )
        .as_bytes(),
    )
}

fn default_notice_root() -> Result<PathBuf> {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| anyhow!("hook notice state location is unavailable"))?;
    Ok(state_home.join("lattice").join("hook-notices"))
}

fn claim_session_start_notice_at(root: &Path, marker: &str) -> Result<bool> {
    fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(root)?.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(root, permissions)?;
    }
    prune_session_start_notices(root)?;
    let path = root.join(format!("session-start-{marker}"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    match options.open(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn prune_session_start_notices(root: &Path) -> Result<()> {
    let now = SystemTime::now();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file()
            || !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("session-start-"))
        {
            continue;
        }
        let expired = entry
            .metadata()?
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > SESSION_START_NOTICE_RETENTION);
        if expired {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

async fn open_or_resume(
    client: &HookSessionClient,
    key: &HookClientBindingKey,
    integration: Integration,
    host_session_id: &str,
    identity: &WorkspaceIdentity,
    now_ms: i64,
    presentation: Option<&HostPresentationRequest>,
) -> Result<Option<HostPresentationResult>> {
    let existing = match client.load_binding(key, now_ms) {
        Ok(binding) => Some(binding),
        Err(HookSessionClientError::BindingMissing) => None,
        Err(HookSessionClientError::BindingExpired) => {
            client.prune(key, now_ms)?;
            None
        }
        Err(error) => return Err(error.into()),
    };
    let mut params = json!({
        "integration": integration.binding_id(),
        "host_session_id": host_session_id,
    });
    if let Some(binding) = &existing {
        params["resume"] = json!({
            "binding_id": binding.binding_id().as_str(),
            "capability": binding.capability().as_str(),
        });
    }
    if let Some(presentation) = presentation {
        params["presentation"] = presentation_params(presentation);
    }
    let mut wire = HookWire::connect(identity).await?;
    let result = wire.call(HOOK_SESSION_OPEN_METHOD, params).await?;
    let (opened, presentation) = decode_open_result(integration, identity, result)?;
    if let Some(existing) = existing {
        if existing.binding_id() != opened.binding_id()
            || existing.capability() != opened.capability()
            || existing.repository_id() != opened.repository_id()
            || existing.checkout_id() != opened.checkout_id()
        {
            return Err(anyhow!("hook binding resume was rejected"));
        }
        client.refresh_binding(key, &opened)?;
        flush_pending(client, key, &opened, &mut wire, now_ms).await?;
    } else {
        client.store_binding(key, &opened)?;
        flush_pending(client, key, &opened, &mut wire, now_ms).await?;
    }
    Ok(presentation)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenResult {
    binding_id: String,
    capability: String,
    generation: u64,
    resumed: bool,
    idle_deadline_ms: i64,
    absolute_deadline_ms: i64,
    repository_id: String,
    checkout_id: String,
    #[serde(default)]
    presentation: Option<HostPresentationResult>,
}

fn decode_open_result(
    integration: Integration,
    identity: &WorkspaceIdentity,
    value: Value,
) -> Result<(HookClientBinding, Option<HostPresentationResult>)> {
    let result: OpenResult = serde_json::from_value(value)?;
    let _ = (result.generation, result.resumed);
    if result.repository_id != identity.repository_id
        || Path::new(&result.checkout_id) != identity.checkout_root
    {
        return Err(anyhow!("hook binding authority mismatch"));
    }
    let presentation = result.presentation;
    if let Some(presentation) = &presentation {
        if !valid_injection_id(&presentation.injection_id)
            || !presentation
                .context
                .contains(&format!("injection_id={}", presentation.injection_id))
        {
            return Err(anyhow!("hook presentation is invalid"));
        }
    }
    Ok((
        HookClientBinding::new(
            HookClientOpaqueId::new(result.binding_id)?,
            HookClientOpaqueId::new(result.capability)?,
            integration.binding_id(),
            result.repository_id,
            result.checkout_id,
            result.idle_deadline_ms,
            result.absolute_deadline_ms,
        )?,
        presentation,
    ))
}

fn valid_injection_id(value: &str) -> bool {
    value.len() == 37
        && value.starts_with("hinj_")
        && value[5..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

async fn flush_pending(
    client: &HookSessionClient,
    key: &HookClientBindingKey,
    binding: &HookClientBinding,
    wire: &mut HookWire,
    now_ms: i64,
) -> Result<()> {
    for delivery in client.pending(key)? {
        let method = match delivery.payload {
            HookClientCapturePayload::Event(_) => HOOK_EVENT_METHOD,
            HookClientCapturePayload::TurnSummary(_) => HOOK_TURN_SUMMARY_METHOD,
            HookClientCapturePayload::Close(_) => HOOK_SESSION_CLOSE_METHOD,
        };
        let params = delivery_params(binding, &delivery)?;
        wire.call(method, params).await?;
        client.acknowledge(key, &delivery.delivery_id, now_ms)?;
    }
    let _ = client.prune(key, now_ms);
    Ok(())
}

fn delivery_params(binding: &HookClientBinding, delivery: &PendingHookDelivery) -> Result<Value> {
    let event = match &delivery.payload {
        HookClientCapturePayload::Event(event) => encode_event(event),
        HookClientCapturePayload::TurnSummary(event) => encode_event(event),
        HookClientCapturePayload::Close(close) => encode_close(close),
    };
    Ok(json!({
        "binding_id": binding.binding_id().as_str(),
        "capability": binding.capability().as_str(),
        "integration": binding.integration(),
        "delivery_id": delivery.delivery_id.as_str(),
        "sequence": delivery.sequence,
        "event": event,
    }))
}

fn encode_event(event: &SessionCaptureEvent) -> Value {
    match &event.fact {
        SessionCaptureFact::EditedPath { path } => json!({
            "schema_version": event.schema_version,
            "kind": "edited_path",
            "path": path,
        }),
        SessionCaptureFact::Check { label, outcome } => json!({
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
            let mut value = json!({
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
        SessionCaptureFact::TurnSummary { summary } => json!({
            "schema_version": event.schema_version,
            "kind": "turn_summary",
            "summary": summary,
        }),
    }
}

fn encode_close(close: &SessionCaptureClose) -> Value {
    let mut value = json!({"schema_version": close.schema_version});
    if let Some(summary) = &close.final_summary {
        value["final_summary"] = Value::String(summary.clone());
    }
    value
}

struct HookWire {
    reader: tokio::io::Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl HookWire {
    async fn connect(identity: &WorkspaceIdentity) -> Result<Self> {
        let address = daemon_addr();
        let mut stream = TcpStream::connect(&address)
            .await
            .map_err(anyhow::Error::from)
            .map_err(HookDaemonUnavailable)?;
        let request = ProxyRequest {
            workspace_roots: vec![identity.checkout_root.to_string_lossy().to_string()],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };
        transport::client_handshake(&mut stream, &address, ClientKind::HookAdapter, &request)
            .await
            .map_err(HookDaemonUnavailable)?;
        let (read, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(read).lines(),
            writer,
            next_id: 1,
        })
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("hook request limit"))?;
        let mut bytes = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        bytes.push(b'\n');
        self.writer.write_all(&bytes).await?;
        self.writer.flush().await?;
        while let Some(line) = self.reader.next_line().await? {
            let response: Value = serde_json::from_str(&line)?;
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if response.get("error").is_some() {
                return Err(anyhow!("hook request rejected"));
            }
            return response
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("hook response is invalid"));
        }
        Err(anyhow!("hook transport closed"))
    }
}

fn read_bounded_stdin(mut reader: impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(MAX_HOST_ENVELOPE_BYTES.min(8 * 1024));
    reader
        .by_ref()
        .take((MAX_HOST_ENVELOPE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_HOST_ENVELOPE_BYTES {
        return Err(anyhow!("hook envelope exceeds the size limit"));
    }
    Ok(bytes)
}

fn extract_host_fact(kind: HookKind, bytes: &[u8]) -> Result<HostFact> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| anyhow!("invalid hook envelope"))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("invalid hook envelope"))?;
    let host_session_id = object
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("hook session identity is missing"))?
        .to_string();

    let edit_path = extract_edit_path(object);
    let payload = match kind {
        HookKind::SessionStart | HookKind::UserPromptSubmit => None,
        HookKind::PostToolUse => extract_edit_event(object)?,
        HookKind::Stop => session_capture_turn_summary_from_host(
            object.get("last_assistant_message").and_then(Value::as_str),
        )
        .map(HookClientCapturePayload::TurnSummary),
        HookKind::StructuredFact => extract_structured_fact(object)?,
        HookKind::SessionEnd => Some(HookClientCapturePayload::Close(
            parse_session_capture_close(
                &json!({"schema_version": SESSION_CAPTURE_SCHEMA_VERSION}).to_string(),
                DateTime::<Utc>::from_unix_seconds(unix_time_ms()? / 1_000),
            )
            .map_err(|_| anyhow!("invalid close marker"))?,
        )),
    };
    let request_id = host_request_id(object, bytes);
    let acted_on_injection_id = object
        .get("lattice_injection_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let presentation = match kind {
        HookKind::SessionStart => Some(HostPresentationRequest {
            kind,
            request_id,
            prompt: None,
            path: None,
            acted_on_injection_id,
        }),
        HookKind::UserPromptSubmit => object
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|prompt| !prompt.trim().is_empty())
            .map(|prompt| HostPresentationRequest {
                kind,
                request_id,
                prompt: Some(prompt.to_string()),
                path: None,
                acted_on_injection_id,
            }),
        HookKind::PostToolUse => edit_path.map(|path| HostPresentationRequest {
            kind,
            request_id,
            prompt: None,
            path: Some(path.to_string()),
            acted_on_injection_id,
        }),
        HookKind::Stop => None,
        HookKind::StructuredFact => None,
        HookKind::SessionEnd => None,
    };
    Ok(HostFact {
        host_session_id,
        payload,
        presentation,
    })
}

fn extract_structured_fact(
    object: &serde_json::Map<String, Value>,
) -> Result<Option<HookClientCapturePayload>> {
    let allowed: &[&str] = match object.get("kind").and_then(Value::as_str) {
        Some("check") => &["session_id", "schema_version", "kind", "label", "outcome"],
        Some("error") => &[
            "session_id",
            "schema_version",
            "kind",
            "category",
            "fingerprint",
            "status",
        ],
        _ => return Ok(None),
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Ok(None);
    }
    let mut event = object.clone();
    event.remove("session_id");
    let normalized = parse_session_capture_event(&Value::Object(event).to_string())
        .map_err(|_| anyhow!("invalid structured fact"))?;
    if !matches!(
        normalized.fact,
        SessionCaptureFact::Check { .. } | SessionCaptureFact::Error { .. }
    ) {
        return Ok(None);
    }
    Ok(Some(HookClientCapturePayload::Event(normalized)))
}

fn extract_edit_event(
    object: &serde_json::Map<String, Value>,
) -> Result<Option<HookClientCapturePayload>> {
    let tool_name = object.get("tool_name").and_then(Value::as_str);
    if !matches!(
        tool_name,
        Some("apply_patch" | "Edit" | "Write" | "NotebookEdit")
    ) {
        return Ok(None);
    }
    let path = extract_edit_path(object);
    let Some(path) = path else {
        return Ok(None);
    };
    let normalized = parse_session_capture_event(
        &json!({
            "schema_version": SESSION_CAPTURE_SCHEMA_VERSION,
            "kind": "edited_path",
            "path": path,
        })
        .to_string(),
    )
    .map_err(|_| anyhow!("invalid edited path"))?;
    Ok(Some(HookClientCapturePayload::Event(normalized)))
}

fn extract_edit_path(object: &serde_json::Map<String, Value>) -> Option<&str> {
    let tool_name = object.get("tool_name").and_then(Value::as_str);
    if !matches!(
        tool_name,
        Some("apply_patch" | "Edit" | "Write" | "NotebookEdit")
    ) {
        return None;
    }
    object.get("file_path").and_then(Value::as_str).or_else(|| {
        object
            .get("tool_input")
            .and_then(Value::as_object)
            .and_then(|input| input.get("file_path"))
            .and_then(Value::as_str)
    })
}

fn host_request_id(object: &serde_json::Map<String, Value>, bytes: &[u8]) -> String {
    for key in ["hook_event_id", "event_id", "tool_use_id", "request_id"] {
        if let Some(value) = object.get(key).and_then(Value::as_str).filter(|value| {
            !value.is_empty()
                && value.len() <= 96
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        }) {
            return value.to_string();
        }
    }
    format!("evt_{}", stable_digest_hex(bytes))
}

fn presentation_params(request: &HostPresentationRequest) -> Value {
    let kind = match request.kind {
        HookKind::SessionStart => "session-start",
        HookKind::UserPromptSubmit => "user-prompt-submit",
        HookKind::PostToolUse => "post-tool-use",
        HookKind::Stop => unreachable!("stop has no presentation request"),
        HookKind::StructuredFact => {
            unreachable!("structured facts have no presentation request")
        }
        HookKind::SessionEnd => unreachable!("session end has no presentation request"),
    };
    let mut value = json!({
        "kind": kind,
        "request_id": request.request_id,
    });
    if let Some(prompt) = &request.prompt {
        value["prompt"] = Value::String(prompt.clone());
    }
    if let Some(path) = &request.path {
        value["path"] = Value::String(path.clone());
    }
    if let Some(injection_id) = &request.acted_on_injection_id {
        value["acted_on_injection_id"] = Value::String(injection_id.clone());
    }
    value
}

fn render_host_presentation(
    integration: Integration,
    kind: HookKind,
    result: HostPresentationResult,
) -> String {
    let _ = result.injection_id;
    match integration {
        Integration::Codex => result.context,
        Integration::ClaudeCode => json!({
            "hookSpecificOutput": {
                "hookEventName": match kind {
                    HookKind::SessionStart => "SessionStart",
                    HookKind::UserPromptSubmit => "UserPromptSubmit",
                    HookKind::PostToolUse => "PostToolUse",
                    HookKind::Stop => "Stop",
                    HookKind::StructuredFact => {
                        unreachable!("structured facts have no presentation")
                    }
                    HookKind::SessionEnd => "SessionEnd",
                },
                "additionalContext": result.context,
            }
        })
        .to_string(),
    }
}

fn stable_digest_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut left = 0xcbf29ce484222325_u64;
    let mut right = 0x84222325cbf29ce4_u64;
    for byte in bytes {
        left ^= u64::from(*byte);
        left = left.wrapping_mul(0x100000001b3);
        right ^= u64::from(*byte).rotate_left(1);
        right = right.wrapping_mul(0x100000001b3);
    }
    let digest = [left.to_be_bytes(), right.to_be_bytes()].concat();
    let mut value = String::with_capacity(digest.len() * 2);
    for byte in digest {
        value.push(HEX[(byte >> 4) as usize] as char);
        value.push(HEX[(byte & 0x0f) as usize] as char);
    }
    value
}

fn checkout_identity_from_cwd() -> Result<WorkspaceIdentity> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    resolve_checkout_from(&cwd)
}

fn resolve_checkout_from(cwd: &Path) -> Result<WorkspaceIdentity> {
    let root = cwd
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .or_else(|| {
            cwd.ancestors()
                .find(|candidate| candidate.join(".lattice").is_dir())
        })
        .ok_or_else(|| anyhow!("current directory is outside a checkout"))?;
    let identity = WorkspaceIdentity::resolve(root)?;
    if identity.checkout_root != root.canonicalize()? {
        return Err(anyhow!("checkout identity is ambiguous"));
    }
    Ok(identity)
}

fn unix_time_ms() -> Result<i64> {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    i64::try_from(millis).map_err(|_| anyhow!("system time is out of range"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn bounded_reader_rejects_oversized_envelopes() {
        let input = vec![b'x'; MAX_HOST_ENVELOPE_BYTES + 1];
        assert!(read_bounded_stdin(input.as_slice()).is_err());
    }

    #[test]
    fn edit_extraction_emits_only_the_allowlisted_normalized_fact() {
        let input = br#"{
            "session_id":"opaque-session",
            "cwd":"/forged/root",
            "repository_id":"forged",
            "scope":"organization",
            "transcript_path":"/tmp/never-open",
            "tool_name":"Edit",
            "tool_input":{"file_path":"src/lib.rs","command":"secret"},
            "tool_response":{"output":"secret"}
        }"#;
        let extracted = extract_host_fact(HookKind::PostToolUse, input).unwrap();
        let HookClientCapturePayload::Event(event) = extracted.payload.unwrap() else {
            panic!("expected an edit event");
        };
        assert_eq!(
            encode_event(&event),
            json!({"schema_version": 1, "kind": "edited_path", "path": "src/lib.rs"})
        );
        let wire = encode_event(&event).to_string();
        for forbidden in ["forged", "organization", "transcript", "command", "secret"] {
            assert!(!wire.contains(forbidden));
        }
    }

    #[test]
    fn delivery_wire_contains_no_raw_host_envelope() {
        let input = br#"{
            "session_id":"opaque-session",
            "cwd":"/forged/root",
            "transcript_path":"/tmp/private-transcript",
            "tool_name":"Write",
            "tool_input":{"file_path":"src/lib.rs","content":"sentinel-secret"},
            "tool_response":{"output":"sentinel-secret"}
        }"#;
        let extracted = extract_host_fact(HookKind::PostToolUse, input).unwrap();
        let binding = HookClientBinding::new(
            HookClientOpaqueId::new("11".repeat(16)).unwrap(),
            HookClientOpaqueId::new("22".repeat(32)).unwrap(),
            Integration::Codex.binding_id(),
            "/repository/.git",
            "/repository/checkout",
            10_000,
            20_000,
        )
        .unwrap();
        let delivery = PendingHookDelivery {
            delivery_id: HookClientOpaqueId::new("33".repeat(16)).unwrap(),
            sequence: 1,
            payload: extracted.payload.unwrap(),
        };
        let wire = delivery_params(&binding, &delivery).unwrap().to_string();
        assert!(wire.contains("src/lib.rs"));
        for forbidden in [
            "opaque-session",
            "forged/root",
            "private-transcript",
            "tool_input",
            "tool_response",
            "sentinel-secret",
        ] {
            assert!(!wire.contains(forbidden), "wire retained {forbidden}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn transcript_fifo_is_never_opened() {
        let root = std::env::temp_dir().join(format!(
            "lattice-hook-fifo-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let fifo = root.join("sentinel-transcript");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        let input = json!({
            "session_id": "opaque-session",
            "transcript_path": fifo,
            "tool_name": "Write",
            "tool_input": {"file_path": "src/main.rs"},
        });
        let extracted = extract_host_fact(HookKind::PostToolUse, input.to_string().as_bytes());
        assert!(extracted.is_ok());
        fs::remove_file(fifo).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn session_end_close_has_no_host_summary_or_envelope_fields() {
        let input = br#"{
            "session_id":"opaque-session",
            "transcript_path":"/tmp/private",
            "reason":"private shutdown reason",
            "cwd":"/forged/root",
            "final_summary":"host text is not an admitted dedicated field yet",
            "files":["src/private.rs"]
        }"#;
        let extracted = extract_host_fact(HookKind::SessionEnd, input).unwrap();
        let HookClientCapturePayload::Close(close) = extracted.payload.unwrap() else {
            panic!("expected a close marker");
        };
        assert_eq!(encode_close(&close), json!({"schema_version": 1}));
    }

    #[test]
    fn stop_extracts_only_the_bounded_top_level_assistant_message() {
        let input = br#"{
            "session_id":"opaque-session",
            "last_assistant_message":" implemented the durable fix ",
            "summary":"alias must be ignored",
            "transcript_path":"/tmp/private",
            "cwd":"/forged/root",
            "nested":{"last_assistant_message":"nested must be ignored"}
        }"#;
        let extracted = extract_host_fact(HookKind::Stop, input).unwrap();
        assert!(extracted.presentation.is_none());
        let HookClientCapturePayload::TurnSummary(event) = extracted.payload.unwrap() else {
            panic!("expected a turn-summary delivery");
        };
        assert_eq!(
            encode_event(&event),
            json!({
                "schema_version": 1,
                "kind": "turn_summary",
                "summary": "implemented the durable fix",
            })
        );
        let wire = encode_event(&event).to_string();
        for forbidden in ["alias", "nested", "transcript", "forged", "opaque-session"] {
            assert!(!wire.contains(forbidden), "wire retained {forbidden}");
        }
    }

    #[test]
    fn stop_drops_missing_non_string_and_oversized_summary_fields() {
        for value in [
            json!({"session_id":"opaque-session"}),
            json!({"session_id":"opaque-session","last_assistant_message":42}),
            json!({"session_id":"opaque-session","summary":"alias"}),
            json!({"session_id":"opaque-session","nested":{"last_assistant_message":"nested"}}),
            json!({"session_id":"opaque-session","last_assistant_message":"a".repeat(2_001)}),
        ] {
            let extracted =
                extract_host_fact(HookKind::Stop, value.to_string().as_bytes()).unwrap();
            assert!(extracted.payload.is_none());
            assert!(extracted.presentation.is_none());
        }
    }

    #[test]
    fn prompt_presentation_retains_only_bounded_dedicated_fields() {
        let input = br#"{
            "session_id":"opaque-session",
            "hook_event_id":"prompt-17",
            "prompt":"explain atomic fixture recovery",
            "transcript_path":"/tmp/private-transcript",
            "cwd":"/forged/root",
            "lattice_injection_id":"hinj_11111111111111111111111111111111",
            "extra_secret":"must-not-cross"
        }"#;
        let extracted = extract_host_fact(HookKind::UserPromptSubmit, input).unwrap();
        assert!(extracted.payload.is_none());
        let presentation = extracted.presentation.unwrap();
        let wire = presentation_params(&presentation).to_string();
        assert!(wire.contains("explain atomic fixture recovery"));
        assert!(wire.contains("prompt-17"));
        assert!(wire.contains("hinj_11111111111111111111111111111111"));
        for forbidden in [
            "opaque-session",
            "private-transcript",
            "forged",
            "must-not-cross",
        ] {
            assert!(!wire.contains(forbidden), "wire retained {forbidden}");
        }
    }

    #[test]
    fn presentation_rendering_is_host_specific_and_keeps_injection_id() {
        let codex = render_host_presentation(
            Integration::Codex,
            HookKind::SessionStart,
            HostPresentationResult {
                injection_id: "hinj_11111111111111111111111111111111".to_string(),
                context: "Lattice memory [injection_id=hinj_11111111111111111111111111111111]"
                    .to_string(),
            },
        );
        assert!(codex.starts_with("Lattice memory"));
        let claude = render_host_presentation(
            Integration::ClaudeCode,
            HookKind::PostToolUse,
            HostPresentationResult {
                injection_id: "hinj_22222222222222222222222222222222".to_string(),
                context: "one-line warning [injection_id=hinj_22222222222222222222222222222222]"
                    .to_string(),
            },
        );
        let claude: Value = serde_json::from_str(&claude).unwrap();
        assert_eq!(claude["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert!(claude["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("hinj_22222222222222222222222222222222"));
    }

    #[test]
    fn session_start_transport_notice_is_claimed_once_and_host_formatted() {
        let root = std::env::temp_dir().join(format!(
            "lattice-hook-notice-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let host_session_id = "authenticated-host-session";
        let repository_id = "repository-id";
        let checkout_root = Path::new("/private/checkout");
        let marker = session_start_notice_marker(
            Integration::Codex,
            host_session_id,
            repository_id,
            checkout_root,
        );

        assert!(claim_session_start_notice_at(&root, &marker).unwrap());
        assert!(!claim_session_start_notice_at(&root, &marker).unwrap());
        let entries = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![format!("session-start-{marker}")]);
        let filename = &entries[0];
        for private_value in [host_session_id, repository_id, "/private", "checkout"] {
            assert!(
                !filename.contains(private_value),
                "notice filename leaked {private_value}"
            );
        }
        assert_eq!(
            render_session_start_notice(Integration::Codex),
            "lattice: daemon unreachable — run 'lattice doctor'"
        );
        let claude: Value =
            serde_json::from_str(&render_session_start_notice(Integration::ClaudeCode)).unwrap();
        assert_eq!(
            claude["hookSpecificOutput"]["hookEventName"],
            "SessionStart"
        );
        assert_eq!(
            claude["hookSpecificOutput"]["additionalContext"],
            "lattice: daemon unreachable — run 'lattice doctor'"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ordinary_empty_memory_result_has_no_transport_notice() {
        let presentation: Option<HostPresentationResult> = None;
        let output = presentation.map(|result| {
            render_host_presentation(Integration::Codex, HookKind::SessionStart, result)
        });

        assert_eq!(output, None);
    }

    #[test]
    fn session_start_notice_rejects_non_transport_failures() {
        let transport =
            anyhow::Error::new(HookDaemonUnavailable(anyhow!("private transport detail")));
        assert!(session_start_notice_eligible(&transport));
        assert!(!session_start_notice_eligible(&anyhow!(
            "invalid daemon presentation"
        )));
        assert!(!session_start_notice_eligible(&anyhow::Error::new(
            HookSessionClientError::BindingMissing
        )));
    }

    #[test]
    fn stop_is_supported_but_session_end_remains_the_terminal_hook() {
        assert_eq!(HookKind::parse("stop"), Some(HookKind::Stop));
        assert_eq!(HookKind::parse("session-end"), Some(HookKind::SessionEnd));
    }

    #[test]
    fn structured_fact_accepts_only_exact_typed_check_and_error_envelopes() {
        let check = br#"{
            "session_id":"opaque-session",
            "schema_version":1,
            "kind":"check",
            "label":"lattice core tests",
            "outcome":"passed"
        }"#;
        let extracted = extract_host_fact(HookKind::StructuredFact, check).unwrap();
        let HookClientCapturePayload::Event(event) = extracted.payload.unwrap() else {
            panic!("expected typed event");
        };
        assert_eq!(
            encode_event(&event),
            json!({
                "schema_version":1,
                "kind":"check",
                "label":"lattice core tests",
                "outcome":"passed"
            })
        );

        for rejected in [
            json!({"session_id":"s","schema_version":1,"kind":"check","label":"safe","outcome":"passed","command":"private"}),
            json!({"session_id":"s","schema_version":1,"kind":"check","label":"safe","outcome":"unknown"}),
            json!({"session_id":"s","schema_version":1,"kind":"edited_path","path":"src/lib.rs"}),
            json!({"session_id":"s","schema_version":1,"kind":"error","category":"test","fingerprint":format!("sha256:{}", "3".repeat(64)),"status":"resolved","summary":"not producer-declared"}),
        ] {
            let result =
                extract_host_fact(HookKind::StructuredFact, rejected.to_string().as_bytes());
            assert!(result.is_err() || result.unwrap().payload.is_none());
        }
    }
}
