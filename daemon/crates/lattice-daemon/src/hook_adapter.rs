//! Private adapter for installed host hook packages.
//!
//! Host envelopes terminate here. Only an opaque host session identifier and
//! an edit hook's dedicated path field are retained. Repository authority is
//! always derived from the process working directory.

use anyhow::{anyhow, Result};
use lattice_core::memory::{
    parse_session_capture_close, parse_session_capture_event, SessionCaptureClose,
    SessionCaptureEvent, SessionCaptureFact, SESSION_CAPTURE_SCHEMA_VERSION,
};
use lattice_core::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;
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
};
use crate::proxy::daemon_addr;
use crate::transport::{self, ClientKind, ProxyRequest};
use crate::workspace_identity::WorkspaceIdentity;

const MAX_HOST_ENVELOPE_BYTES: usize = 64 * 1024;
const ADAPTER_DEADLINE: Duration = Duration::from_millis(2_000);

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
    PostToolUse,
    Stop,
}

impl HookKind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "session-start" => Some(Self::SessionStart),
            "post-tool-use" => Some(Self::PostToolUse),
            "stop" => Some(Self::Stop),
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
}

pub(crate) fn is_hook_adapter_command() -> bool {
    std::env::args().nth(1).as_deref() == Some("__hook-adapter")
}

/// Hook invocations are deliberately best-effort and silent. All internal
/// failures are content-free and never render rejected host input.
pub(crate) async fn run_from_env() {
    let Some(invocation) = Invocation::from_env() else {
        return;
    };
    let _ = tokio::time::timeout(ADAPTER_DEADLINE, run(invocation)).await;
}

async fn run(invocation: Invocation) -> Result<()> {
    let input = read_bounded_stdin(std::io::stdin().lock())?;
    let fact = extract_host_fact(invocation.kind, &input)?;
    let identity = checkout_identity_from_cwd()?;
    let key = HookClientBindingKey::new(
        invocation.integration.binding_id(),
        fact.host_session_id.clone(),
        identity.repository_id.clone(),
        identity.checkout_root.to_string_lossy().to_string(),
    )?;
    let client = HookSessionClient::open_default()?;
    let now_ms = unix_time_ms()?;

    match invocation.kind {
        HookKind::SessionStart => {
            open_or_resume(
                &client,
                &key,
                invocation.integration,
                &fact.host_session_id,
                &identity,
                now_ms,
            )
            .await?;
        }
        HookKind::PostToolUse | HookKind::Stop => {
            let Some(payload) = fact.payload else {
                return Ok(());
            };
            let binding = client.load_binding(&key, now_ms)?;
            match client.enqueue(&key, payload, now_ms) {
                Ok(_) => {}
                Err(HookSessionClientError::BindingClosed) if invocation.kind == HookKind::Stop => {
                }
                Err(error) => return Err(error.into()),
            }
            let mut wire = HookWire::connect(&identity).await?;
            flush_pending(&client, &key, &binding, &mut wire, now_ms).await?;
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
) -> Result<()> {
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
    let mut wire = HookWire::connect(identity).await?;
    let result = wire.call(HOOK_SESSION_OPEN_METHOD, params).await?;
    let opened = decode_open_result(integration, identity, result)?;
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
    Ok(())
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
}

fn decode_open_result(
    integration: Integration,
    identity: &WorkspaceIdentity,
    value: Value,
) -> Result<HookClientBinding> {
    let result: OpenResult = serde_json::from_value(value)?;
    let _ = (result.generation, result.resumed);
    if result.repository_id != identity.repository_id
        || Path::new(&result.checkout_id) != identity.checkout_root
    {
        return Err(anyhow!("hook binding authority mismatch"));
    }
    Ok(HookClientBinding::new(
        HookClientOpaqueId::new(result.binding_id)?,
        HookClientOpaqueId::new(result.capability)?,
        integration.binding_id(),
        result.repository_id,
        result.checkout_id,
        result.idle_deadline_ms,
        result.absolute_deadline_ms,
    )?)
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
        let mut stream = TcpStream::connect(&address).await?;
        let request = ProxyRequest {
            workspace_roots: vec![identity.checkout_root.to_string_lossy().to_string()],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };
        transport::client_handshake(&mut stream, &address, ClientKind::HookAdapter, &request)
            .await?;
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

    let payload = match kind {
        HookKind::SessionStart => None,
        HookKind::PostToolUse => extract_edit_event(object)?,
        HookKind::Stop => Some(HookClientCapturePayload::Close(
            parse_session_capture_close(
                &json!({"schema_version": SESSION_CAPTURE_SCHEMA_VERSION}).to_string(),
                DateTime::<Utc>::from_unix_seconds(unix_time_ms()? / 1_000),
            )
            .map_err(|_| anyhow!("invalid close marker"))?,
        )),
    };
    Ok(HostFact {
        host_session_id,
        payload,
    })
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
    let path = object.get("file_path").and_then(Value::as_str).or_else(|| {
        object
            .get("tool_input")
            .and_then(Value::as_object)
            .and_then(|input| input.get("file_path"))
            .and_then(Value::as_str)
    });
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
    fn close_has_no_host_summary_or_envelope_fields() {
        let input = br#"{
            "session_id":"opaque-session",
            "transcript_path":"/tmp/private",
            "final_summary":"host text is not an admitted dedicated field yet",
            "files":["src/private.rs"]
        }"#;
        let extracted = extract_host_fact(HookKind::Stop, input).unwrap();
        let HookClientCapturePayload::Close(close) = extracted.payload.unwrap() else {
            panic!("expected a close marker");
        };
        assert_eq!(encode_close(&close), json!({"schema_version": 1}));
    }
}
