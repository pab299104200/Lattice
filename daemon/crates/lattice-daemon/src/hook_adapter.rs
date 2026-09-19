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

use crate::hook_enforcement::{
    self, checkout_relative_path, classify_edit_path, classify_relative_path, deny_reason,
    followup_reminder, load_policy, notice_text, FollowupGaps, IndexState, NoticeCondition,
    PathClass, PlanState, PolicyState, BEST_EFFORT_DAEMON_NOTICE, MAX_SHELL_CHANGED_PATHS,
};
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
const NOTICE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
/// Share of the two-second invocation deadline that `git status` may spend.
/// The rest is kept for the daemon round trip that follows it.
const SHELL_DETECTION_BUDGET: Duration = Duration::from_millis(700);
/// Claude Code spills hook context above 10,000 characters to a file.
const MAX_CONTEXT_CHARS: usize = 9_000;
const MAX_LISTED_SHELL_PATHS: usize = 5;

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
    PreToolUse,
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
            "pre-tool-use" => Some(Self::PreToolUse),
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
    tool: ToolFact,
    session_source: Option<&'static str>,
    stop_hook_active: bool,
}

/// What a tool event is, reduced to the category and, for an edit tool, its
/// dedicated path field. A shell tool's input is never read.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ToolFact {
    NotATool,
    /// An edit tool that names its target.
    Edit { path: String, scratch: Option<PathBuf> },
    /// An edit tool whose host exposes no target (Codex `apply_patch` carries
    /// only patch text, which is not read).
    EditWithoutPath,
    Shell,
    Other,
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

/// The daemon answered and refused. Distinct from a transport failure so an
/// enforcing workspace can say which one happened.
#[derive(Debug, thiserror::Error)]
#[error("hook request rejected")]
struct HookDaemonRejected;

pub(crate) fn is_hook_adapter_command() -> bool {
    std::env::args().nth(1).as_deref() == Some("__hook-adapter")
}

/// Everything decided before the daemon is contacted. It is resolved outside
/// the invocation deadline so that a timeout can still be reported.
struct Prepared {
    fact: HostFact,
    identity: WorkspaceIdentity,
    policy: PolicyState,
}

/// What one invocation tells the host. Rendering is host-specific; building
/// it is not.
#[derive(Debug, Default, Eq, PartialEq)]
struct HookOutput {
    contexts: Vec<String>,
    deny: Option<String>,
    system_message: Option<String>,
}

impl HookOutput {
    fn context(text: String) -> Self {
        Self {
            contexts: vec![text],
            ..Self::default()
        }
    }

    fn is_empty(&self) -> bool {
        self.contexts.is_empty() && self.deny.is_none() && self.system_message.is_none()
    }
}

/// In a workspace that has not opted in to enforcement, hooks stay
/// best-effort: only authenticated, bounded daemon presentations reach
/// stdout and failures are silent, apart from the one SessionStart recovery
/// notice. In an enforcing workspace every failure still allows the tool
/// call, but is reported once per session and condition. Rejected host input
/// is never rendered in either mode.
pub(crate) async fn run_from_env() {
    let Some(invocation) = Invocation::from_env() else {
        return;
    };
    let Ok(prepared) = prepare(&invocation) else {
        return;
    };
    let outcome = tokio::time::timeout(ADAPTER_DEADLINE, run(&invocation, &prepared)).await;
    let output = match outcome {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => failure_output(&invocation, &prepared, failure_condition(&error)),
        Err(_) => failure_output(&invocation, &prepared, NoticeCondition::AdapterTimeout),
    };
    if let Some(rendered) = render_output(invocation.integration, invocation.kind, output) {
        println!("{rendered}");
    }
}

fn prepare(invocation: &Invocation) -> Result<Prepared> {
    let input = read_bounded_stdin(std::io::stdin().lock())?;
    let identity = checkout_identity_from_cwd()?;
    let fact = extract_host_fact(invocation.kind, &input, &identity.checkout_root)?;
    let policy = load_policy(&identity.checkout_root);
    Ok(Prepared {
        fact,
        identity,
        policy,
    })
}

fn failure_condition(error: &anyhow::Error) -> NoticeCondition {
    if error.downcast_ref::<HookDaemonUnavailable>().is_some() {
        NoticeCondition::DaemonUnreachable
    } else if error.downcast_ref::<HookDaemonRejected>().is_some() {
        NoticeCondition::DaemonRejected
    } else {
        NoticeCondition::AdapterFailed
    }
}

/// Turn a failure into at most one notice. Nothing here can block a tool.
fn failure_output(
    invocation: &Invocation,
    prepared: &Prepared,
    condition: NoticeCondition,
) -> HookOutput {
    if !prepared.policy.enforcing() {
        // Pre-enforcement contract, unchanged.
        if invocation.kind == HookKind::SessionStart
            && condition == NoticeCondition::DaemonUnreachable
            && claim_notice(invocation.integration, prepared, condition)
        {
            return HookOutput::context(BEST_EFFORT_DAEMON_NOTICE.to_string());
        }
        return HookOutput::default();
    }
    let condition = if invocation.kind == HookKind::Stop {
        // Stop has no passive channel to the model; tell the operator that
        // the turn summary was not captured.
        NoticeCondition::CaptureUnavailable
    } else {
        condition
    };
    notice_output(invocation, prepared, condition)
}

/// A claimed notice, routed to the only channel the event offers that does
/// not alter control flow.
fn notice_output(
    invocation: &Invocation,
    prepared: &Prepared,
    condition: NoticeCondition,
) -> HookOutput {
    if invocation.kind == HookKind::SessionEnd
        || !claim_notice(invocation.integration, prepared, condition)
    {
        return HookOutput::default();
    }
    let text = notice_text(condition);
    if invocation.kind == HookKind::Stop {
        HookOutput {
            system_message: Some(text),
            ..HookOutput::default()
        }
    } else {
        HookOutput::context(text)
    }
}

async fn run(invocation: &Invocation, prepared: &Prepared) -> Result<HookOutput> {
    let fact = &prepared.fact;
    let identity = &prepared.identity;
    let enforcing = prepared.policy.enforcing();
    let mut output = HookOutput::default();
    if matches!(prepared.policy, PolicyState::Unreadable(_))
        && invocation.kind != HookKind::Stop
    {
        output = notice_output(invocation, prepared, NoticeCondition::PolicyUnreadable);
    }

    if invocation.kind == HookKind::StructuredFact && fact.payload.is_none() {
        return Ok(output);
    }
    if invocation.kind == HookKind::Stop && fact.payload.is_none() && !enforcing {
        return Ok(output);
    }
    let key = HookClientBindingKey::new(
        invocation.integration.binding_id(),
        fact.host_session_id.clone(),
        identity.repository_id.clone(),
        identity.checkout_root.to_string_lossy().to_string(),
    )?;
    let client = HookSessionClient::open_default()?;
    let now_ms = unix_time_ms()?;
    let session = SessionContext {
        client: &client,
        key: &key,
        integration: invocation.integration,
        host_session_id: &fact.host_session_id,
        identity,
        now_ms,
    };

    match invocation.kind {
        HookKind::SessionStart => {
            client.retire_acknowledged_close(&key)?;
            let enforcement = enforcing.then(|| {
                let mut request = json!({"event": "session-start"});
                if let Some(source) = fact.session_source {
                    request["session_source"] = json!(source);
                }
                request
            });
            let answer = session
                .open_or_resume(fact.presentation.as_ref(), enforcement)
                .await?;
            push_presentation(&mut output, answer.presentation);
            push_index_notice(&mut output, invocation, prepared, answer.enforcement.as_ref());
        }
        HookKind::UserPromptSubmit => {
            let answer = session
                .open_or_resume(fact.presentation.as_ref(), None)
                .await?;
            push_presentation(&mut output, answer.presentation);
        }
        HookKind::PreToolUse => {
            if !enforcing {
                return Ok(output);
            }
            let path_known = match &fact.tool {
                ToolFact::Edit { path, scratch } => {
                    if !classify_edit_path(&identity.checkout_root, path, scratch.as_deref())
                        .gated()
                    {
                        return Ok(output);
                    }
                    true
                }
                ToolFact::EditWithoutPath => false,
                ToolFact::NotATool | ToolFact::Shell | ToolFact::Other => return Ok(output),
            };
            let answer = session
                .open_or_resume(None, Some(json!({"event": "pre-tool-use"})))
                .await?;
            let enforcement = answer
                .enforcement
                .ok_or_else(|| anyhow::Error::new(HookDaemonRejected))?;
            if enforcement.deny {
                output.deny = Some(deny_reason(enforcement.plan_state, path_known));
            } else {
                push_index_notice(&mut output, invocation, prepared, Some(&enforcement));
            }
        }
        HookKind::PostToolUse => {
            run_post_tool_use(invocation, prepared, &session, &mut output).await?;
        }
        HookKind::Stop => {
            let enforcement =
                (enforcing && !fact.stop_hook_active).then(|| json!({"event": "stop"}));
            let answer = session.open_or_resume(None, enforcement).await?;
            if let Some(payload) = fact.payload.clone() {
                session.deliver(payload).await?;
            }
            if let Some(reminder) = answer
                .enforcement
                .and_then(|enforcement| enforcement.followup)
                .and_then(followup_reminder)
            {
                match invocation.integration {
                    // One continuation, claimed once per session by the daemon
                    // and never requested while a stop hook is already active.
                    Integration::ClaudeCode => output.contexts.push(reminder),
                    // Codex can only block-and-continue; tell the operator.
                    Integration::Codex => output.system_message = Some(reminder),
                }
            }
        }
        HookKind::StructuredFact | HookKind::SessionEnd => {
            let Some(payload) = fact.payload.clone() else {
                return Ok(output);
            };
            // These never create a binding: a fact or a close for a session
            // Lattice never opened has nothing to attach to.
            let binding = client.load_binding(&key, now_ms)?;
            match client.enqueue(&key, payload, now_ms) {
                Ok(_) => {}
                Err(HookSessionClientError::BindingClosed) => {
                    if invocation.kind == HookKind::StructuredFact {
                        return Ok(output);
                    }
                }
                Err(error) => return Err(error.into()),
            }
            let mut wire = HookWire::connect(identity).await?;
            flush_pending(&client, &key, &binding, &mut wire, now_ms).await?;
        }
    }
    Ok(output)
}

async fn run_post_tool_use(
    invocation: &Invocation,
    prepared: &Prepared,
    session: &SessionContext<'_>,
    output: &mut HookOutput,
) -> Result<()> {
    let fact = &prepared.fact;
    let identity = &prepared.identity;
    let enforcing = prepared.policy.enforcing();
    let marker = session_marker(
        invocation.integration,
        &fact.host_session_id,
        &identity.repository_id,
        &identity.checkout_root,
    );

    let inspect_repository = enforcing
        && matches!(fact.tool, ToolFact::Shell | ToolFact::EditWithoutPath);
    if !inspect_repository {
        let Some(payload) = fact.payload.clone() else {
            return Ok(());
        };
        if enforcing {
            if let Some(path) = fact.presentation.as_ref().and_then(|request| request.path.as_deref())
            {
                // Best effort: a stale snapshot only costs one extra mention.
                let _ = crate::hook_shell_changes::absorb_tool_edit(
                    &identity.checkout_root,
                    &default_state_directory("hook-shell-snapshots")?,
                    &marker,
                    path,
                );
            }
        }
        let answer = session
            .open_or_resume(fact.presentation.as_ref(), None)
            .await?;
        session.deliver(payload).await?;
        push_presentation(output, answer.presentation);
        return Ok(());
    }

    let detection = crate::hook_shell_changes::detect_shell_changes(
        &identity.checkout_root,
        &default_state_directory("hook-shell-snapshots")?,
        &marker,
        SHELL_DETECTION_BUDGET,
    )
    .await;
    let changed = match detection {
        Ok(crate::hook_shell_changes::ShellDetection::Changed(paths)) => paths,
        Ok(crate::hook_shell_changes::ShellDetection::Baseline) => Vec::new(),
        Ok(crate::hook_shell_changes::ShellDetection::Degraded) | Err(_) => {
            merge_output(
                output,
                notice_output(invocation, prepared, NoticeCondition::ShellDetectionDegraded),
            );
            Vec::new()
        }
    };
    let product = changed
        .into_iter()
        .filter(|path| classify_relative_path(Path::new(path)) == PathClass::Product)
        .collect::<Vec<_>>();
    if product.is_empty() {
        return Ok(());
    }
    let total = product.len();
    let forwarded = &product[..total.min(MAX_SHELL_CHANGED_PATHS)];

    let presentation = HostPresentationRequest {
        kind: HookKind::PostToolUse,
        request_id: fact
            .presentation
            .as_ref()
            .map_or_else(|| format!("evt_{marker}_{}", session.now_ms), |request| {
                request.request_id.clone()
            }),
        prompt: None,
        path: Some(forwarded[0].clone()),
        acted_on_injection_id: None,
    };
    let answer = session
        .open_or_resume(
            Some(&presentation),
            Some(json!({"event": "shell-edit", "product_paths": forwarded.len()})),
        )
        .await?;
    for path in forwarded {
        // A path the capture schema refuses is skipped, never sent raw.
        if let Ok(event) = edited_path_event(path) {
            session
                .deliver(HookClientCapturePayload::Event(event))
                .await?;
        }
    }
    output.contexts.push(shell_change_summary(forwarded, total));
    push_presentation(output, answer.presentation);
    let plan_current = answer
        .enforcement
        .as_ref()
        .is_some_and(|enforcement| enforcement.plan_state == PlanState::Current);
    if !plan_current {
        merge_output(
            output,
            notice_output(invocation, prepared, NoticeCondition::ShellEditWithoutPlan),
        );
    }
    push_index_notice(output, invocation, prepared, answer.enforcement.as_ref());
    Ok(())
}

/// Name the files a shell command changed. Paths come from `git status`, not
/// from the command, and the list is bounded.
fn shell_change_summary(forwarded: &[String], total: usize) -> String {
    let listed = forwarded
        .iter()
        .take(MAX_LISTED_SHELL_PATHS)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let more = total.saturating_sub(MAX_LISTED_SHELL_PATHS.min(forwarded.len()));
    let suffix = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    format!(
        "lattice: the shell changed {total} product file(s): {listed}{suffix}. Impact notes \
         below cover the first; run `lattice impact <file>` for the others you have not checked."
    )
}

fn push_presentation(output: &mut HookOutput, presentation: Option<HostPresentationResult>) {
    if let Some(presentation) = presentation {
        output.contexts.push(presentation.context);
    }
}

fn push_index_notice(
    output: &mut HookOutput,
    invocation: &Invocation,
    prepared: &Prepared,
    enforcement: Option<&EnforcementAnswer>,
) {
    if let Some(condition) =
        enforcement.and_then(|enforcement| NoticeCondition::for_index(&enforcement.index_state))
    {
        merge_output(output, notice_output(invocation, prepared, condition));
    }
}

fn merge_output(output: &mut HookOutput, other: HookOutput) {
    output.contexts.extend(other.contexts);
    output.deny = output.deny.take().or(other.deny);
    output.system_message = output.system_message.take().or(other.system_message);
}

/// Render for the host. Both hosts accept the same JSON for tool events;
/// Codex ignores plain text there, and takes plain text for session events.
fn render_output(integration: Integration, kind: HookKind, output: HookOutput) -> Option<String> {
    if output.is_empty() {
        return None;
    }
    let context = (!output.contexts.is_empty()).then(|| {
        let joined = output.contexts.join("\n\n");
        match joined.char_indices().nth(MAX_CONTEXT_CHARS) {
            Some((end, _)) => joined[..end].to_string(),
            None => joined,
        }
    });
    let event = match kind {
        HookKind::SessionStart => "SessionStart",
        HookKind::UserPromptSubmit => "UserPromptSubmit",
        HookKind::PreToolUse => "PreToolUse",
        HookKind::PostToolUse => "PostToolUse",
        HookKind::Stop => "Stop",
        HookKind::StructuredFact | HookKind::SessionEnd => return None,
    };
    let plain_text_event = integration == Integration::Codex
        && matches!(kind, HookKind::SessionStart | HookKind::UserPromptSubmit);
    if plain_text_event && output.deny.is_none() && output.system_message.is_none() {
        return context;
    }
    let mut rendered = serde_json::Map::new();
    let mut specific = serde_json::Map::new();
    if let Some(reason) = output.deny {
        specific.insert("permissionDecision".into(), json!("deny"));
        specific.insert("permissionDecisionReason".into(), json!(reason));
    }
    if let Some(context) = context {
        specific.insert("additionalContext".into(), json!(context));
    }
    if !specific.is_empty() {
        specific.insert("hookEventName".into(), json!(event));
        rendered.insert("hookSpecificOutput".into(), Value::Object(specific));
    }
    if let Some(message) = output.system_message {
        rendered.insert("systemMessage".into(), json!(message));
    }
    Some(Value::Object(rendered).to_string())
}

/// Claim the single notice for a condition in a locally authenticated host
/// session. The marker is non-authoritative and stores no host data: it only
/// prevents a repeated bounded presentation. A claim that cannot be recorded
/// is treated as already made, so a broken state directory cannot turn one
/// notice into one per tool call.
fn claim_notice(integration: Integration, prepared: &Prepared, condition: NoticeCondition) -> bool {
    let marker = session_marker(
        integration,
        &prepared.fact.host_session_id,
        &prepared.identity.repository_id,
        &prepared.identity.checkout_root,
    );
    default_state_directory("hook-notices")
        .and_then(|root| claim_notice_at(&root, condition, &marker))
        .unwrap_or(false)
}

/// A content-free name for a host session in one checkout.
fn session_marker(
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

fn default_state_directory(name: &str) -> Result<PathBuf> {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| anyhow!("hook state location is unavailable"))?;
    Ok(state_home.join("lattice").join(name))
}

fn claim_notice_at(root: &Path, condition: NoticeCondition, marker: &str) -> Result<bool> {
    fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(root)?.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(root, permissions)?;
    }
    prune_notices(root)?;
    let path = root.join(format!("{}-{marker}", condition.slug()));
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

fn prune_notices(root: &Path) -> Result<()> {
    let now = SystemTime::now();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let expired = entry
            .metadata()?
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > NOTICE_RETENTION);
        if expired {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

struct SessionContext<'a> {
    client: &'a HookSessionClient,
    key: &'a HookClientBindingKey,
    integration: Integration,
    host_session_id: &'a str,
    identity: &'a WorkspaceIdentity,
    now_ms: i64,
}

struct OpenAnswer {
    presentation: Option<HostPresentationResult>,
    enforcement: Option<EnforcementAnswer>,
}

#[derive(Debug)]
struct EnforcementAnswer {
    deny: bool,
    plan_state: PlanState,
    index_state: IndexState,
    followup: Option<FollowupGaps>,
}

impl SessionContext<'_> {
    async fn open_or_resume(
        &self,
        presentation: Option<&HostPresentationRequest>,
        enforcement: Option<Value>,
    ) -> Result<OpenAnswer> {
        open_or_resume(self, presentation, enforcement).await
    }

    /// Queue one capture payload under the current binding and flush the
    /// queue. Must follow `open_or_resume` in the same invocation.
    async fn deliver(&self, payload: HookClientCapturePayload) -> Result<()> {
        let binding = self.client.load_binding(self.key, self.now_ms)?;
        match self.client.enqueue(self.key, payload, self.now_ms) {
            Ok(_) | Err(HookSessionClientError::BindingClosed) => {}
            Err(error) => return Err(error.into()),
        }
        let mut wire = HookWire::connect(self.identity).await?;
        flush_pending(self.client, self.key, &binding, &mut wire, self.now_ms).await
    }
}

async fn open_or_resume(
    session: &SessionContext<'_>,
    presentation: Option<&HostPresentationRequest>,
    enforcement: Option<Value>,
) -> Result<OpenAnswer> {
    let SessionContext {
        client,
        key,
        integration,
        host_session_id,
        identity,
        now_ms,
    } = *session;
    // `idle_expired` marks a binding only the daemon can adjudicate; see
    // `HookSessionClient::load_idle_expired_binding`.
    let (existing, idle_expired) = match client.load_binding(key, now_ms) {
        Ok(binding) => (Some(binding), false),
        Err(HookSessionClientError::BindingMissing) => (None, false),
        Err(HookSessionClientError::BindingExpired) => {
            match client.load_idle_expired_binding(key, now_ms) {
                Ok(binding) => (Some(binding), true),
                Err(_) => {
                    client.prune(key, now_ms)?;
                    (None, false)
                }
            }
        }
        Err(error) => return Err(error.into()),
    };
    let build_params = |resume: Option<&HookClientBinding>| {
        let mut params = json!({
            "integration": integration.binding_id(),
            "host_session_id": host_session_id,
        });
        if let Some(binding) = resume {
            params["resume"] = json!({
                "binding_id": binding.binding_id().as_str(),
                "capability": binding.capability().as_str(),
            });
        }
        if let Some(presentation) = presentation {
            params["presentation"] = presentation_params(presentation);
        }
        if let Some(enforcement) = &enforcement {
            params["enforcement"] = enforcement.clone();
        }
        params
    };
    let mut wire = HookWire::connect(identity).await?;
    let (existing, result) = match wire
        .call(HOOK_SESSION_OPEN_METHOD, build_params(existing.as_ref()))
        .await
    {
        Ok(result) => (existing, result),
        Err(error) if idle_expired && error.downcast_ref::<HookDaemonRejected>().is_some() => {
            // The daemon agrees the binding is gone. Start a new generation.
            client.discard_refused_binding(key)?;
            let result = wire
                .call(HOOK_SESSION_OPEN_METHOD, build_params(None))
                .await?;
            (None, result)
        }
        Err(error) => return Err(error),
    };
    let (opened, answer) = decode_open_result(integration, identity, result)?;
    if let Some(existing) = existing {
        if existing.binding_id() != opened.binding_id()
            || existing.capability() != opened.capability()
            || existing.repository_id() != opened.repository_id()
            || existing.checkout_id() != opened.checkout_id()
        {
            return Err(anyhow!("hook binding resume was rejected"));
        }
        client.refresh_binding(key, &opened)?;
    } else {
        client.store_binding(key, &opened)?;
    }
    flush_pending(client, key, &opened, &mut wire, now_ms).await?;
    Ok(answer)
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
    #[serde(default)]
    enforcement: Option<EnforcementWire>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnforcementWire {
    decision: String,
    plan_state: String,
    index_state: String,
    #[serde(default)]
    followup: Option<FollowupWire>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FollowupWire {
    stale_docs: bool,
    remember: bool,
}

/// An answer the adapter cannot interpret is an error, which fails open. It
/// is never read as "allow" or as "deny".
fn decode_enforcement(wire: EnforcementWire) -> Result<EnforcementAnswer> {
    let deny = match wire.decision.as_str() {
        "allow" => false,
        "deny" => true,
        _ => return Err(anyhow!("hook enforcement decision is invalid")),
    };
    Ok(EnforcementAnswer {
        deny,
        plan_state: PlanState::parse(&wire.plan_state)
            .ok_or_else(|| anyhow!("hook enforcement plan state is invalid"))?,
        index_state: IndexState::parse(&wire.index_state)
            .ok_or_else(|| anyhow!("hook enforcement index state is invalid"))?,
        followup: wire.followup.map(|followup| FollowupGaps {
            stale_docs: followup.stale_docs,
            remember: followup.remember,
        }),
    })
}

fn decode_open_result(
    integration: Integration,
    identity: &WorkspaceIdentity,
    value: Value,
) -> Result<(HookClientBinding, OpenAnswer)> {
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
        OpenAnswer {
            presentation,
            enforcement: result.enforcement.map(decode_enforcement).transpose()?,
        },
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
    let mut rejected = None;
    for delivery in client.pending(key)? {
        let method = match delivery.payload {
            HookClientCapturePayload::Event(_) => HOOK_EVENT_METHOD,
            HookClientCapturePayload::TurnSummary(_) => HOOK_TURN_SUMMARY_METHOD,
            HookClientCapturePayload::Close(_) => HOOK_SESSION_CLOSE_METHOD,
        };
        let params = delivery_params(binding, &delivery)?;
        match wire.call(method, params).await {
            Ok(_) => client.acknowledge(key, &delivery.delivery_id, now_ms)?,
            Err(error) if error.downcast_ref::<HookDaemonRejected>().is_some() => {
                // Report the refusal after draining the rest, so one refused
                // fact can neither hide nor hold back the ones behind it.
                if !client.discard_rejected_delivery(key, &delivery.delivery_id)? {
                    return Err(error);
                }
                rejected = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    let _ = client.prune(key, now_ms);
    rejected.map_or(Ok(()), Err)
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
                return Err(HookDaemonRejected.into());
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

fn extract_host_fact(kind: HookKind, bytes: &[u8], checkout_root: &Path) -> Result<HostFact> {
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

    let tool = match kind {
        HookKind::PreToolUse | HookKind::PostToolUse => extract_tool_fact(object),
        _ => ToolFact::NotATool,
    };
    // Capture facts and presentations are repository-relative. Claude Code
    // always sends an absolute path; one outside the checkout yields no fact.
    let relative_edit_path = match &tool {
        ToolFact::Edit { path, .. } => checkout_relative_path(checkout_root, path),
        _ => None,
    };
    let payload = match kind {
        HookKind::SessionStart | HookKind::UserPromptSubmit | HookKind::PreToolUse => None,
        HookKind::PostToolUse => relative_edit_path
            .as_deref()
            .and_then(|path| edited_path_event(path).ok())
            .map(HookClientCapturePayload::Event),
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
        // A shell call has no path yet, but keeps the host's request id so
        // the repository-derived presentation stays idempotent per tool call.
        HookKind::PostToolUse if payload.is_some() || tool == ToolFact::Shell
            || tool == ToolFact::EditWithoutPath =>
        {
            Some(HostPresentationRequest {
                kind,
                request_id,
                prompt: None,
                path: relative_edit_path,
                acted_on_injection_id,
            })
        }
        HookKind::PreToolUse
        | HookKind::PostToolUse
        | HookKind::Stop
        | HookKind::StructuredFact
        | HookKind::SessionEnd => None,
    };
    // A categorical value only; any other source is ignored, never forwarded.
    let session_source = match (kind, object.get("source").and_then(Value::as_str)) {
        (HookKind::SessionStart, Some("startup")) => Some("startup"),
        (HookKind::SessionStart, Some("resume")) => Some("resume"),
        (HookKind::SessionStart, Some("clear")) => Some("clear"),
        (HookKind::SessionStart, Some("compact")) => Some("compact"),
        _ => None,
    };
    let stop_hook_active = kind == HookKind::Stop
        && object.get("stop_hook_active").and_then(Value::as_bool) == Some(true);
    Ok(HostFact {
        host_session_id,
        payload,
        presentation,
        tool,
        session_source,
        stop_hook_active,
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

const EDIT_TOOLS: [&str; 5] = ["apply_patch", "Edit", "Write", "MultiEdit", "NotebookEdit"];
const SHELL_TOOLS: [&str; 2] = ["Bash", "PowerShell"];

/// Reduce a tool envelope to its category. For an edit tool the dedicated
/// path field is kept; `notebook_path` is NotebookEdit's. Nothing else in
/// `tool_input` or `tool_response` is read, and a shell tool's input is not
/// read at all.
fn extract_tool_fact(object: &serde_json::Map<String, Value>) -> ToolFact {
    let Some(tool_name) = object.get("tool_name").and_then(Value::as_str) else {
        return ToolFact::Other;
    };
    if SHELL_TOOLS.contains(&tool_name) {
        return ToolFact::Shell;
    }
    if !EDIT_TOOLS.contains(&tool_name) {
        return ToolFact::Other;
    }
    let input = object.get("tool_input").and_then(Value::as_object);
    let path = object
        .get("file_path")
        .and_then(Value::as_str)
        .or_else(|| {
            input.and_then(|input| {
                input
                    .get("file_path")
                    .or_else(|| input.get("notebook_path"))
                    .and_then(Value::as_str)
            })
        })
        .filter(|path| !path.trim().is_empty());
    match path {
        Some(path) => ToolFact::Edit {
            path: path.to_string(),
            scratch: object
                .get("scratchpad_dir")
                .and_then(Value::as_str)
                .filter(|scratch| Path::new(scratch).is_absolute())
                .map(PathBuf::from),
        },
        None => ToolFact::EditWithoutPath,
    }
}

fn edited_path_event(relative_path: &str) -> Result<SessionCaptureEvent> {
    parse_session_capture_event(
        &json!({
            "schema_version": SESSION_CAPTURE_SCHEMA_VERSION,
            "kind": "edited_path",
            "path": relative_path,
        })
        .to_string(),
    )
    .map_err(|_| anyhow!("invalid edited path"))
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
        HookKind::PreToolUse => unreachable!("pre-tool-use has no presentation request"),
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

    /// Extraction is lexical for paths that do not exist, so a fixed
    /// non-existent checkout keeps these tests off the filesystem.
    fn checkout() -> &'static Path {
        Path::new("/lattice-test-checkout")
    }

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
        let extracted = extract_host_fact(HookKind::PostToolUse, input, checkout()).unwrap();
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
        let extracted = extract_host_fact(HookKind::PostToolUse, input, checkout()).unwrap();
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
        let extracted = extract_host_fact(
            HookKind::PostToolUse,
            input.to_string().as_bytes(),
            checkout(),
        );
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
        let extracted = extract_host_fact(HookKind::SessionEnd, input, checkout()).unwrap();
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
        let extracted = extract_host_fact(HookKind::Stop, input, checkout()).unwrap();
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
                extract_host_fact(HookKind::Stop, value.to_string().as_bytes(), checkout())
                    .unwrap();
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
        let extracted = extract_host_fact(HookKind::UserPromptSubmit, input, checkout()).unwrap();
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
        let context =
            "Lattice memory [injection_id=hinj_11111111111111111111111111111111]".to_string();
        // Codex takes plain text for session events.
        assert_eq!(
            render_output(
                Integration::Codex,
                HookKind::SessionStart,
                HookOutput::context(context.clone())
            ),
            Some(context.clone())
        );
        // Codex ignores plain text on tool events, so both hosts get JSON.
        for integration in [Integration::ClaudeCode, Integration::Codex] {
            let rendered = render_output(
                integration,
                HookKind::PostToolUse,
                HookOutput::context(context.clone()),
            )
            .unwrap();
            let rendered: Value = serde_json::from_str(&rendered).unwrap();
            assert_eq!(rendered["hookSpecificOutput"]["hookEventName"], "PostToolUse");
            assert_eq!(rendered["hookSpecificOutput"]["additionalContext"], context);
            assert!(rendered["hookSpecificOutput"]
                .get("permissionDecision")
                .is_none());
        }
        assert_eq!(
            render_output(
                Integration::ClaudeCode,
                HookKind::PostToolUse,
                HookOutput::default()
            ),
            None
        );
    }

    fn notice_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lattice-hook-notice-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn a_notice_is_claimed_once_per_session_and_condition_under_a_content_free_name() {
        let root = notice_root("claim");
        let host_session_id = "authenticated-host-session";
        let repository_id = "repository-id";
        let checkout_root = Path::new("/private/checkout");
        let marker = session_marker(
            Integration::Codex,
            host_session_id,
            repository_id,
            checkout_root,
        );

        let unreachable = NoticeCondition::DaemonUnreachable;
        assert!(claim_notice_at(&root, unreachable, &marker).unwrap());
        assert!(!claim_notice_at(&root, unreachable, &marker).unwrap());
        // De-duplication is per condition: a second condition still speaks.
        let timeout = NoticeCondition::AdapterTimeout;
        assert!(claim_notice_at(&root, timeout, &marker).unwrap());
        assert!(!claim_notice_at(&root, timeout, &marker).unwrap());
        // And per session: another session claims its own.
        let other = session_marker(
            Integration::Codex,
            "another-host-session",
            repository_id,
            checkout_root,
        );
        assert!(claim_notice_at(&root, unreachable, &other).unwrap());

        let mut entries = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        entries.sort();
        let mut expected = vec![
            format!("adapter-timeout-{marker}"),
            format!("session-start-{marker}"),
            format!("session-start-{other}"),
        ];
        expected.sort();
        assert_eq!(entries, expected);
        for filename in &entries {
            for private_value in [host_session_id, repository_id, "/private", "checkout"] {
                assert!(
                    !filename.contains(private_value),
                    "notice filename leaked {private_value}"
                );
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn best_effort_session_start_notice_keeps_its_wording_and_host_format() {
        let output = HookOutput::context(BEST_EFFORT_DAEMON_NOTICE.to_string());
        assert_eq!(
            render_output(Integration::Codex, HookKind::SessionStart, output),
            Some("lattice: daemon unreachable — run 'lattice doctor'".to_string())
        );
        let claude: Value = serde_json::from_str(
            &render_output(
                Integration::ClaudeCode,
                HookKind::SessionStart,
                HookOutput::context(BEST_EFFORT_DAEMON_NOTICE.to_string()),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            claude,
            json!({"hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": "lattice: daemon unreachable — run 'lattice doctor'",
            }})
        );
    }

    #[test]
    fn ordinary_empty_memory_result_renders_nothing() {
        let mut output = HookOutput::default();
        push_presentation(&mut output, None);
        assert_eq!(
            render_output(Integration::Codex, HookKind::SessionStart, output),
            None
        );
    }

    #[test]
    fn failures_map_to_the_condition_that_actually_happened() {
        let transport =
            anyhow::Error::new(HookDaemonUnavailable(anyhow!("private transport detail")));
        assert_eq!(
            failure_condition(&transport),
            NoticeCondition::DaemonUnreachable
        );
        assert_eq!(
            failure_condition(&anyhow::Error::new(HookDaemonRejected)),
            NoticeCondition::DaemonRejected
        );
        for local in [
            anyhow!("invalid daemon presentation"),
            anyhow::Error::new(HookSessionClientError::BindingMissing),
        ] {
            assert_eq!(failure_condition(&local), NoticeCondition::AdapterFailed);
        }
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
        let extracted = extract_host_fact(HookKind::StructuredFact, check, checkout()).unwrap();
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
                extract_host_fact(
                HookKind::StructuredFact,
                rejected.to_string().as_bytes(),
                checkout(),
            );
            assert!(result.is_err() || result.unwrap().payload.is_none());
        }
    }
}
