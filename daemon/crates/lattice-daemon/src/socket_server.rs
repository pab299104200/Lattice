use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::hook_session_route::{
    HookSessionRoute, HOOK_EVENT_METHOD, HOOK_SESSION_CLOSE_METHOD, HOOK_SESSION_OPEN_METHOD,
    HOOK_TURN_SUMMARY_METHOD,
};
use crate::lifecycle_log;
use crate::proxy::daemon_addr;
use crate::rpc::mcp::McpHandler;
use crate::rpc::protocol::{format_response, parse_request, JsonRpcResponse};
use crate::rpc::server::RequestHandler;
use crate::transport::{ClientKind, ConnectionMetadata, ProxyRequest, ServerTransport};

fn lock_owned<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct ActiveRequest {
    generation: u64,
    handle: JoinHandle<()>,
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

struct PendingResponse {
    request_key: String,
    generation: u64,
    response: JsonRpcResponse,
}

struct ShardEntry {
    root: PathBuf,
    runtime: StdMutex<Option<crate::WorkspaceRuntime>>,
    bootstrap: StdMutex<Option<JoinHandle<()>>>,
    bootstrap_error: StdMutex<Option<String>>,
    bootstrapping: AtomicBool,
    index_work: Arc<crate::index_work::IndexWorkCoordinator>,
    active_connections: AtomicUsize,
    last_used_epoch_secs: AtomicU64,
    /// Set when the shard has been taken out of the map to be shut down. A
    /// lease that still holds it re-acquires a fresh shard on its next
    /// request, which is what makes unloading a connected shard transparent.
    retired: AtomicBool,
    /// Growth of the daemon's footprint while this shard loaded. Zero when it
    /// could not be attributed, because another shard was loading too.
    load_cost_bytes: AtomicU64,
    _materialization_reservation: crate::resource_budget::ResourceReservation,
}

impl ShardEntry {
    fn pending(
        root: PathBuf,
        index_work: Arc<crate::index_work::IndexWorkCoordinator>,
        materialization_reservation: crate::resource_budget::ResourceReservation,
    ) -> Self {
        Self {
            root,
            runtime: StdMutex::new(None),
            bootstrap: StdMutex::new(None),
            bootstrap_error: StdMutex::new(None),
            bootstrapping: AtomicBool::new(true),
            index_work,
            active_connections: AtomicUsize::new(0),
            last_used_epoch_secs: AtomicU64::new(now_epoch_secs()),
            retired: AtomicBool::new(false),
            load_cost_bytes: AtomicU64::new(0),
            _materialization_reservation: materialization_reservation,
        }
    }

    fn published_handler(&self) -> Result<Option<Arc<dyn RequestHandler>>> {
        let guard = self
            .runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("workspace shard runtime lock poisoned"))?;
        Ok(guard
            .as_ref()
            .map(|runtime| Arc::clone(&runtime.handler) as Arc<dyn RequestHandler>))
    }

    fn start_bootstrap(
        self: &Arc<Self>,
        focus_files: Vec<String>,
        focus_dirs: Vec<String>,
        cost: LoadCostProbe,
    ) {
        let entry = Arc::clone(self);
        let root = entry.root.clone();
        let index_work = Arc::clone(&entry.index_work);
        let task = tokio::spawn(async move {
            let alone = cost.loading.fetch_add(1, Ordering::SeqCst) == 0;
            let before = (cost.footprint)();
            let built =
                crate::build_workspace_runtime(vec![root], focus_files, focus_dirs, index_work)
                    .await;
            let still_alone = cost.loading.fetch_sub(1, Ordering::SeqCst) == 1;
            match built {
                Ok(runtime) => {
                    // The footprint is process-wide, so growth can be
                    // attributed to this shard only if it loaded alone.
                    if let (true, true, Some(before), Some(after)) =
                        (alone, still_alone, before, (cost.footprint)())
                    {
                        entry
                            .load_cost_bytes
                            .store(after.saturating_sub(before), Ordering::Release);
                    }
                    let mut slot = lock_owned(&entry.runtime);
                    *slot = Some(runtime);
                }
                Err(error) => {
                    let message = error.to_string();
                    tracing::error!(workspace = %entry.root.display(), %message, "workspace shard bootstrap failed");
                    *lock_owned(&entry.bootstrap_error) = Some(message);
                }
            }
            entry.bootstrapping.store(false, Ordering::Release);
        });
        let mut bootstrap = lock_owned(&self.bootstrap);
        *bootstrap = Some(task);
    }

    fn bootstrap_error(&self) -> Option<String> {
        lock_owned(&self.bootstrap_error).clone()
    }

    fn is_bootstrapping(&self) -> bool {
        self.bootstrapping.load(Ordering::Acquire)
    }

    fn retain(self: &Arc<Self>) {
        self.active_connections.fetch_add(1, Ordering::AcqRel);
        self.last_used_epoch_secs
            .store(now_epoch_secs(), Ordering::Release);
    }

    fn release(&self) {
        self.active_connections.fetch_sub(1, Ordering::AcqRel);
        self.last_used_epoch_secs
            .store(now_epoch_secs(), Ordering::Release);
    }

    /// The single definition of "this shard can be dropped without destroying
    /// work in flight".  Cold start is two sequential phases of one operation:
    /// `build_workspace_runtime` registers the workspace's index job before it
    /// returns, and `bootstrapping` only clears after that.  A shard is
    /// therefore safe to evict only when it serves no request, has published
    /// its runtime, and has no queued or running index work.  Both the
    /// capacity-admission path and the idle sweeper must ask this same
    /// question, or one of them will evict a shard the other considers busy.
    fn is_evictable(&self) -> bool {
        self.active_connections.load(Ordering::Acquire) == 0
            && !self.is_bootstrapping()
            && !self.index_work.workspace_is_busy(&shard_key(&self.root))
    }

    fn touch(&self) {
        self.last_used_epoch_secs
            .store(now_epoch_secs(), Ordering::Release);
    }

    fn is_retired(&self) -> bool {
        self.retired.load(Ordering::SeqCst)
    }

    fn connections(&self) -> usize {
        self.active_connections.load(Ordering::Acquire)
    }

    fn is_loaded(&self) -> bool {
        self.is_bootstrapping() || lock_owned(&self.runtime).is_some()
    }

    fn has_work_in_flight(&self) -> bool {
        lock_owned(&self.runtime)
            .as_ref()
            .is_some_and(|runtime| runtime.active_work() > 0)
    }

    fn idle_secs(&self, now_epoch_secs: u64) -> u64 {
        now_epoch_secs.saturating_sub(self.last_used_epoch_secs.load(Ordering::Acquire))
    }

    /// Agents are connected but nothing has asked this shard for anything for
    /// `threshold_secs`, and nothing is running against it. Such a shard may
    /// be unloaded when another connected workspace needs the memory; it
    /// reloads on its next request. It is never unloaded merely for being
    /// idle, only to make room.
    fn is_idle_connected(&self, now_epoch_secs: u64, threshold_secs: u64) -> bool {
        self.connections() > 0
            && !self.is_bootstrapping()
            && !self.index_work.workspace_is_busy(&shard_key(&self.root))
            && !self.has_work_in_flight()
            && self.idle_secs(now_epoch_secs) >= threshold_secs
    }

    /// Mark the shard retired unless a request got in first. A request takes
    /// its work guard and then re-checks `retired` (see
    /// `RuntimeLease::begin_work`), so exactly one side backs off.
    fn try_retire(&self) -> bool {
        self.retired.store(true, Ordering::SeqCst);
        if self.has_work_in_flight() {
            self.retired.store(false, Ordering::SeqCst);
            return false;
        }
        true
    }

    fn is_idle(&self, now_epoch_secs: u64, idle_ttl_secs: u64) -> bool {
        if !self.is_evictable() {
            return false;
        }
        let last_used = self.last_used_epoch_secs.load(Ordering::Acquire);
        now_epoch_secs.saturating_sub(last_used) >= idle_ttl_secs
    }

    async fn shutdown(&self) {
        let bootstrap = lock_owned(&self.bootstrap).take();
        if let Some(task) = bootstrap {
            // Runtime construction is finite and begins storage workers before
            // publishing the completed runtime. Aborting here would detach
            // those workers and release the checkout lease prematurely. Join
            // construction, then shut down the published runtime below.
            let _ = task.await;
        }
        let runtime = lock_owned(&self.runtime).take();
        if let Some(runtime) = runtime {
            runtime.shutdown().await;
        }
    }
}

#[async_trait::async_trait]
impl RequestHandler for ShardEntry {
    async fn handle(&self, method: &str, params: Value) -> Result<Value, (i32, String)> {
        if let Some(handler) = self
            .published_handler()
            .map_err(|error| (-32603, error.to_string()))?
        {
            return handler.handle(method, params).await;
        }
        cold_start_response(
            method,
            &params,
            &self.root,
            &self.index_work,
            self.bootstrap_error(),
            None,
        )
    }
}

/// An index status request, by either route the CLI, doctor and MCP use.
fn is_index_status_request(method: &str, params: &Value) -> bool {
    match method {
        "lattice/status" => true,
        "tools/call" => {
            params.get("name").and_then(Value::as_str) == Some("status")
                && matches!(
                    params.pointer("/arguments/scope").and_then(Value::as_str),
                    None | Some("index")
                )
        }
        _ => false,
    }
}

/// Add the daemon-wide facts to a status answer. A workspace shard cannot
/// know them: the cap and its source belong to the process, and whether this
/// workspace is starved depends on what else is loaded.
fn attach_daemon_report(result: &mut Value, report: Value) {
    let text = result
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    match text {
        Some(Value::Object(mut payload)) => {
            payload.insert("shard_capacity".into(), shard_capacity_line(&report));
            payload.insert("daemon".into(), report);
            if let Some(slot) = result.pointer_mut("/content/0/text") {
                *slot = Value::String(Value::Object(payload).to_string());
            }
        }
        Some(_) => {}
        None => {
            if let Some(payload) = result.as_object_mut().filter(|payload| {
                payload.contains_key("status") && !payload.contains_key("content")
            }) {
                payload.insert("shard_capacity".into(), shard_capacity_line(&report));
                payload.insert("daemon".into(), report);
            }
        }
    }
}

/// One line for the plain-text status view, which collapses nested objects.
fn shard_capacity_line(report: &Value) -> Value {
    let mib = |key: &str| report[key].as_u64().map(|bytes| bytes / (1024 * 1024));
    let memory = match (mib("memory_footprint_bytes"), mib("memory_budget_bytes")) {
        (Some(used), Some(budget)) => format!(
            "memory {used} MiB of {budget} MiB budget ({}) after {} h up",
            report["memory_budget_source"].as_str().unwrap_or("unknown"),
            report["uptime_secs"].as_u64().unwrap_or_default() / 3600
        ),
        _ => "memory unknown".to_string(),
    };
    let ceiling = match report["max_loaded_shards"].as_u64() {
        Some(ceiling) => format!(
            "shard ceiling {ceiling} from {}",
            report["max_loaded_shards_source"]
                .as_str()
                .unwrap_or("unknown")
        ),
        None => "no shard ceiling".to_string(),
    };
    let deferred = match report["deferred"].as_str() {
        Some("ceiling") => "; the next workspace would be DEFERRED by the shard ceiling",
        Some("memory") => "; the next workspace would be DEFERRED by the memory budget",
        _ => "",
    };
    Value::String(format!(
        "{} connected workspace(s) with {} agent(s); {} shard(s) loaded, {} idle, {} connected but idle and unloadable; {memory}; {ceiling}{deferred}",
        report["connected_workspaces"],
        report["agents"],
        report["loaded_shards"],
        report["idle_shards"],
        report["connected_idle_shards"],
    ))
}

/// The workflow step a request satisfies if it succeeds. Only the tool name
/// and the `status` scope are inspected; no argument is retained.
fn served_workflow_step(
    method: &str,
    params: &Value,
) -> Option<crate::hook_workflow_state::WorkflowStep> {
    if method != "tools/call" {
        return None;
    }
    let tool = params.get("name").and_then(Value::as_str)?;
    let scope = params.pointer("/arguments/scope").and_then(Value::as_str);
    crate::hook_workflow_state::WorkflowStep::from_tool_call(tool, scope)
}

fn tool_result_is_error(result: &Value) -> bool {
    result.get("isError").and_then(Value::as_bool) == Some(true)
}

/// A bounded response for a workspace whose durable runtime has not published
/// yet.  This deliberately reports unknown graph counts rather than an empty
/// graph: zero is a claim about the repository, while bootstrap has no graph
/// authority at all.  Once the runtime is published the same shard entry
/// delegates every request to the normal MCP handler.
fn cold_start_response(
    method: &str,
    params: &Value,
    workspace: &Path,
    index_work: &Arc<crate::index_work::IndexWorkCoordinator>,
    bootstrap_error: Option<String>,
    deferred_reason: Option<&str>,
) -> Result<Value, (i32, String)> {
    match method {
        "initialize" => Ok(McpHandler::protocol_initialize_response()),
        "tools/list" => Ok(McpHandler::agent_tools_list_response()),
        "ping"
        | "notifications/initialized"
        | "notifications/cancelled"
        | "notifications/progress"
        | "notifications/roots/list_changed" => Ok(serde_json::json!({})),
        "lattice/status" => Ok(cold_index_status_payload(
            workspace,
            index_work,
            bootstrap_error,
            deferred_reason,
        )),
        "tools/call" => {
            let tool_name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or((-32602, "Missing tool name".to_string()))?;
            if !is_public_agent_tool(tool_name) {
                return Err((-32602, format!("Unknown tool: {tool_name}")));
            }
            if tool_name == "status"
                && params.pointer("/arguments/scope").and_then(Value::as_str) == Some("storage")
            {
                return Ok(wrap_json_text(crate::storage_operator::status_payload(
                    workspace,
                )));
            }
            if tool_name == "status" {
                return Ok(wrap_json_text(cold_index_status_payload(
                    workspace,
                    index_work,
                    bootstrap_error,
                    deferred_reason,
                )));
            }
            let query = params
                .get("arguments")
                .and_then(|arguments| {
                    arguments
                        .get("query")
                        .or_else(|| arguments.get("task"))
                        .and_then(Value::as_str)
                })
                .unwrap_or_default();
            Ok(wrap_json_text(cold_indexing_tool_payload(
                tool_name,
                query,
                workspace,
                bootstrap_error,
                deferred_reason,
            )))
        }
        _ => Err((-32601, format!("Method not found: {method}"))),
    }
}

fn is_public_agent_tool(name: &str) -> bool {
    matches!(
        name,
        "context"
            | "prepare_change"
            | "impact"
            | "diagnose"
            | "search"
            | "remember"
            | "recall"
            | "status"
    )
}

fn cold_index_status_payload(
    workspace: &Path,
    index_work: &Arc<crate::index_work::IndexWorkCoordinator>,
    bootstrap_error: Option<String>,
    deferred_reason: Option<&str>,
) -> Value {
    let bootstrap_state = if bootstrap_error.is_some() {
        "failed"
    } else if deferred_reason.is_some() {
        "deferred"
    } else {
        "starting"
    };
    let summary = if bootstrap_error.is_some() {
        "Failed to load this workspace. Lattice answers for it are unavailable. Run `lattice doctor`."
            .to_string()
    } else if let Some(reason) = deferred_reason {
        // Say the thing an operator needs in words. `indexing` and
        // `not_loaded` both read as "wait", and waiting never ends here.
        format!(
            "Deferred, not loading: {reason} Nothing is indexing this workspace, and Lattice answers for it are partial until it can be loaded."
        )
    } else {
        "Loading this workspace for the first time. Answers are partial until indexing finishes."
            .to_string()
    };
    serde_json::json!({
        "summary": summary,
        "status": if bootstrap_error.is_some() {
            "degraded"
        } else if deferred_reason.is_some() {
            "deferred"
        } else {
            "indexing"
        },
        // A deferred workspace has no index job at all. Reporting it as
        // indexing sends the operator to wait for work that was never queued.
        "indexing": bootstrap_error.is_none() && deferred_reason.is_none(),
        "version": env!("CARGO_PKG_VERSION"),
        "workspace": workspace.to_string_lossy(),
        "nodes": Value::Null,
        "edges": Value::Null,
        "files": Value::Null,
        "languages": Value::Null,
        "graph_snapshot_state": "not_loaded",
        "graph_counts_state": "unknown",
        "bootstrap": {
            "state": bootstrap_state,
            "retryable": bootstrap_error.is_none(),
            "error": bootstrap_error,
            "deferred_reason": deferred_reason,
        },
        "index_work": index_work.snapshot(),
        "resource_admission": index_work.resource_snapshot(),
        "semantic_retrieval": {
            "status": "initializing",
            "reason": "workspace runtime bootstrap has not published a graph yet; lexical retrieval will remain available after publication"
        }
    })
}

fn cold_indexing_tool_payload(
    tool_name: &str,
    query: &str,
    workspace: &Path,
    bootstrap_error: Option<String>,
    deferred_reason: Option<&str>,
) -> Value {
    let failed = bootstrap_error.is_some();
    serde_json::json!({
        "query": query,
        "overview": if failed {
            "The workspace runtime failed during bootstrap; no graph or memory operation was attempted. Inspect status for the actionable bootstrap error."
        } else if deferred_reason.is_some() {
            "The workspace runtime is queued behind active shard capacity; no graph or memory operation was attempted. Check status and retry when a shard becomes available."
        } else {
            "The workspace runtime is starting and has not published a graph yet; no graph or memory operation was attempted. Check status and retry after indexing completes."
        },
        "indexing": !failed,
        "partial": true,
        "partial_reason": if failed { "runtime_bootstrap_failed" } else if deferred_reason.is_some() { "runtime_bootstrap_deferred" } else { "runtime_bootstrap_pending" },
        "result_set_state": "not_evaluated",
        "operation_performed": false,
        "workspace": workspace.to_string_lossy(),
        "completed_stages": [],
        "last_completed_stage": Value::Null,
        "omitted_stages": ["anchors", "lexical_structural", "graph_expansion", "semantic", "repository_memory", "shared_memory"],
        "freshness": {
            "state": "unavailable",
            "served_snapshot": false,
            "reason": if failed { "runtime_bootstrap_failed" } else if deferred_reason.is_some() { "runtime_bootstrap_deferred" } else { "runtime_bootstrap_pending" },
        },
        "primary_files": [],
        "symbols": [],
        "tests": [],
        "rationale": [format!("{tool_name} returned a bounded bootstrap response instead of waiting for a cold workspace runtime.")],
        "bootstrap_error": bootstrap_error,
        "bootstrap_deferred_reason": deferred_reason,
        "suggested_expand": {
            "focus": "index_status",
            "reason": if failed { "Resolve the reported bootstrap error before retrying." } else { "Check index_status, then retry once the workspace runtime is ready." },
        }
    })
}

struct RuntimeLease {
    retained_shards: Arc<StdMutex<HashMap<String, Arc<ShardEntry>>>>,
    handler: Arc<dyn RequestHandler>,
    _connection: ConnectionRegistration,
}

impl RuntimeLease {
    fn begin_work(&self) -> Vec<crate::index_work::RuntimeWorkGuard> {
        lock_owned(&self.retained_shards)
            .values()
            .filter_map(|shard| {
                let guard = lock_owned(&shard.runtime)
                    .as_ref()
                    .map(|runtime| runtime.begin_work())?;
                // A guard on a retired runtime would make its shutdown wait
                // for a request that is about to go and wait for that very
                // shutdown. Re-check after taking it; see `try_retire`.
                (!shard.is_retired()).then_some(guard)
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VictimClass {
    /// No agent connected and no work: costs nobody anything.
    Unconnected,
    /// Agents connected but silent past the threshold. It reloads on its
    /// next request.
    ConnectedIdle,
}

impl VictimClass {
    fn label(self) -> &'static str {
        match self {
            Self::Unconnected => "unconnected",
            Self::ConnectedIdle => "connected_idle",
        }
    }
}

/// Choose and remove the shard to unload under pressure, least recently used
/// first: unconnected shards before connected-but-idle ones, and never a
/// shard that is loading, indexing, or answering a request.
fn take_victim(
    shards: &mut HashMap<String, Arc<ShardEntry>>,
    now_epoch_secs: u64,
    connected_idle_secs: u64,
) -> Option<(String, Arc<ShardEntry>, VictimClass)> {
    for class in [VictimClass::Unconnected, VictimClass::ConnectedIdle] {
        let mut candidates = shards
            .iter()
            .filter(|(_, entry)| match class {
                VictimClass::Unconnected => entry.is_evictable(),
                VictimClass::ConnectedIdle => {
                    entry.is_idle_connected(now_epoch_secs, connected_idle_secs)
                }
            })
            .map(|(key, entry)| {
                (
                    entry.last_used_epoch_secs.load(Ordering::Acquire),
                    key.clone(),
                )
            })
            .collect::<Vec<_>>();
        candidates.sort();
        for (_, key) in candidates {
            let retired = shards.get(&key).is_some_and(|entry| entry.try_retire());
            if retired {
                return shards.remove(&key).map(|entry| (key, entry, class));
            }
        }
    }
    None
}

impl DeferralKind {
    fn label(&self) -> &'static str {
        match self {
            Self::Ceiling => "ceiling",
            Self::Memory => "memory",
        }
    }
}

/// Why a connected workspace could not be given a shard right now.
#[derive(Clone, Debug, Eq, PartialEq)]
enum DeferralKind {
    /// The operator's `max_loaded_shards` ceiling is reached and every
    /// loaded workspace is busy.
    Ceiling,
    /// The daemon's real memory is at its budget and no loaded workspace is
    /// idle enough to unload.
    Memory,
}

#[derive(Debug)]
struct ShardDeferred {
    kind: DeferralKind,
    message: String,
}

impl std::fmt::Display for ShardDeferred {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ShardDeferred {}

/// Registers one connection against its workspace for as long as the lease
/// lives, whether or not the workspace could be loaded. This, not the shard
/// map, is what "connected" means: a deferred workspace has no shard, and
/// its agents must still be counted and reported.
struct ConnectionRegistration {
    registry: Arc<StdMutex<HashMap<String, ConnectedWorkspace>>>,
    key: String,
    agent: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ConnectedWorkspace {
    /// Live agent sessions: stdio proxies held open by Claude Code or Codex.
    agents: usize,
    /// Short-lived clients such as the CLI and doctor.
    other: usize,
}

impl ConnectionRegistration {
    fn new(
        registry: &Arc<StdMutex<HashMap<String, ConnectedWorkspace>>>,
        key: String,
        agent: bool,
    ) -> Self {
        let mut connected = lock_owned(registry);
        let entry = connected.entry(key.clone()).or_default();
        if agent {
            entry.agents += 1;
        } else {
            entry.other += 1;
        }
        drop(connected);
        Self {
            registry: Arc::clone(registry),
            key,
            agent,
        }
    }
}

impl Drop for ConnectionRegistration {
    fn drop(&mut self) {
        let mut connected = lock_owned(&self.registry);
        if let Some(entry) = connected.get_mut(&self.key) {
            if self.agent {
                entry.agents = entry.agents.saturating_sub(1);
            } else {
                entry.other = entry.other.saturating_sub(1);
            }
            if *entry == ConnectedWorkspace::default() {
                connected.remove(&self.key);
            }
        }
    }
}

/// A logical shard that has not been admitted because every resident shard is
/// active or indexing.  It retries admission on each request, but never turns
/// that transient resource boundary into a generic RPC availability failure.
struct DeferredShardHandler {
    daemon: Arc<GlobalDaemon>,
    root: PathBuf,
    focus_files: Vec<String>,
    focus_dirs: Vec<String>,
}

#[async_trait::async_trait]
impl RequestHandler for DeferredShardHandler {
    async fn handle(&self, method: &str, params: Value) -> Result<Value, (i32, String)> {
        match self
            .daemon
            .shard_for(
                self.root.clone(),
                self.focus_files.clone(),
                self.focus_dirs.clone(),
                true,
            )
            .await
        {
            Ok(shard) => {
                let retained = RetainedShard::from_retained(shard);
                retained.handler().handle(method, params).await
            }
            Err(error) if is_shard_capacity_error(&error) => {
                let reason = error.to_string();
                cold_start_response(
                    method,
                    &params,
                    &self.root,
                    &self.daemon.index_work,
                    None,
                    Some(reason.as_str()),
                )
            }
            Err(error) => Err((-32603, format!("workspace bootstrap failed: {error}"))),
        }
    }
}

fn is_shard_capacity_error(error: &anyhow::Error) -> bool {
    if error.downcast_ref::<ShardDeferred>().is_some() {
        return true;
    }
    error.to_string().contains("resource-limited:")
}

/// The handler every lease uses for its primary workspace.
///
/// A lease used to hold its shard directly. A shard that was then evicted
/// answered "indexing" to that connection for the rest of its life, so a
/// connected shard could never be unloaded, and a fixed count of shards
/// starved whichever workspace connected last. This handler resolves the
/// shard on each request instead: a retired shard is replaced by a fresh one,
/// and a workspace that could not be admitted is retried, so both unloading
/// and deferral heal on the next request without the client noticing more
/// than a cold answer while the index reloads from disk.
struct LeaseShardHandler {
    daemon: Arc<GlobalDaemon>,
    root: PathBuf,
    focus_files: Vec<String>,
    focus_dirs: Vec<String>,
    retained_shards: Arc<StdMutex<HashMap<String, Arc<ShardEntry>>>>,
}

impl LeaseShardHandler {
    fn current(&self) -> Option<Arc<ShardEntry>> {
        lock_owned(&self.retained_shards)
            .get(&shard_key(&self.root))
            .filter(|entry| !entry.is_retired())
            .cloned()
    }

    /// Swap a freshly retained shard into the lease, releasing the retired
    /// one it replaces.
    fn adopt(&self, entry: &Arc<ShardEntry>) {
        let replaced =
            lock_owned(&self.retained_shards).insert(shard_key(&self.root), Arc::clone(entry));
        if let Some(replaced) = replaced {
            replaced.release();
        }
    }
}

#[async_trait::async_trait]
impl RequestHandler for LeaseShardHandler {
    async fn handle(&self, method: &str, params: Value) -> Result<Value, (i32, String)> {
        let entry = match self.current() {
            Some(entry) => entry,
            None => match self
                .daemon
                .shard_for(
                    self.root.clone(),
                    self.focus_files.clone(),
                    self.focus_dirs.clone(),
                    true,
                )
                .await
            {
                Ok(entry) => {
                    self.adopt(&entry);
                    entry
                }
                Err(error) if is_shard_capacity_error(&error) => {
                    let reason = error.to_string();
                    return cold_start_response(
                        method,
                        &params,
                        &self.root,
                        &self.daemon.index_work,
                        None,
                        Some(reason.as_str()),
                    );
                }
                Err(error) => return Err((-32603, format!("workspace bootstrap failed: {error}"))),
            },
        };
        entry.touch();
        entry.handle(method, params).await
    }
}

impl Drop for RuntimeLease {
    fn drop(&mut self) {
        let retained = lock_owned(&self.retained_shards)
            .drain()
            .map(|(_, shard)| shard)
            .collect::<Vec<_>>();
        for shard in retained {
            shard.release();
        }
    }
}

struct RetainedShard {
    shard: Option<Arc<ShardEntry>>,
}

impl RetainedShard {
    fn from_retained(shard: Arc<ShardEntry>) -> Self {
        Self { shard: Some(shard) }
    }

    fn handler(&self) -> Arc<dyn RequestHandler> {
        Arc::clone(
            self.shard
                .as_ref()
                .expect("retained shard is present until ownership transfer"),
        ) as Arc<dyn RequestHandler>
    }

    fn root(&self) -> &PathBuf {
        &self
            .shard
            .as_ref()
            .expect("retained shard is present until ownership transfer")
            .root
    }

    fn into_shard(mut self) -> Arc<ShardEntry> {
        self.shard
            .take()
            .expect("retained shard is transferred at most once")
    }
}

impl Drop for RetainedShard {
    fn drop(&mut self) {
        if let Some(shard) = self.shard.as_ref() {
            shard.release();
        }
    }
}

fn adopt_retained_lease_shard(
    retained_shards: &Arc<StdMutex<HashMap<String, Arc<ShardEntry>>>>,
    shard: Arc<ShardEntry>,
) -> Result<()> {
    let key = shard_key(&shard.root);
    let mut retained = match retained_shards.lock() {
        Ok(retained) => retained,
        Err(_) => {
            shard.release();
            anyhow::bail!("logical-view retained shard lock poisoned");
        }
    };
    if retained.contains_key(&key) {
        shard.release();
        return Ok(());
    }
    retained.insert(key, shard);
    Ok(())
}

fn retained_shard_count(
    retained_shards: &Arc<StdMutex<HashMap<String, Arc<ShardEntry>>>>,
) -> usize {
    retained_shards
        .lock()
        .map(|shards| shards.len())
        .unwrap_or(0)
}

struct ViewRequestHandler {
    daemon: Arc<GlobalDaemon>,
    roots: Vec<PathBuf>,
    primary_root: PathBuf,
    primary_handler: Arc<dyn RequestHandler>,
    focus_files: Vec<String>,
    focus_dirs: Vec<String>,
    handle_owners: Mutex<HashMap<String, PathBuf>>,
}

#[async_trait::async_trait]
impl RequestHandler for ViewRequestHandler {
    async fn handle(&self, method: &str, params: Value) -> Result<Value, (i32, String)> {
        if method != "tools/call" {
            return self.primary_handler.handle(method, params).await;
        }

        let tool_name = params["name"].as_str().unwrap_or_default();
        if tool_name == "index_status" {
            return self.handle_index_status(params).await;
        }
        if tool_name == "search_memory" || tool_name == "recall_memories" {
            return self.handle_cross_shard_search_memory(method, params).await;
        }
        if tool_name == "expand_context" {
            if let Some(root) = self.route_root_for_context_handle(&params).await {
                if root != self.primary_root {
                    return self.handle_with_root(root, method, params).await;
                }
            }
        }

        if is_authoritative_cross_shard_graph_tool(tool_name) {
            return self.handle_cross_shard_workflow(method, params).await;
        }

        let route_decision = self.route_decision_for_tool_call(&params);
        if let RouteDecision::Match(root) = &route_decision {
            if root != &self.primary_root {
                return self.handle_with_root(root.clone(), method, params).await;
            }
        }
        if is_cross_shard_workflow_tool(tool_name) {
            return self.handle_cross_shard_workflow(method, params).await;
        }

        if is_memory_write_tool(tool_name) {
            match &route_decision {
                RouteDecision::Ambiguous { path, candidates } => {
                    return Err((
                        -32602,
                        format!(
                            "Ambiguous workspace for memory write path {path:?}; candidates: {}. Use an absolute linked file/doc/test path or a path unique to one configured workspace.",
                            candidates.join(", ")
                        ),
                    ));
                }
                RouteDecision::None if self.roots.len() > 1 => {
                    let response = self.primary_handler.handle(method, params).await?;
                    return Ok(annotate_wrapped_tool_json(
                        response,
                        serde_json::json!({
                            "routing_diagnostics": {
                                "status": "primary_workspace_fallback",
                                "fallback_workspace": self.primary_root.to_string_lossy(),
                                "reason": "No path-bearing memory arguments matched a configured workspace; stdio MCP does not expose a per-call caller working directory. Provide linked_files, linked_docs, linked_tests, files, or an embedded absolute path to route durable memory writes to a non-primary shard."
                            }
                        }),
                    ));
                }
                RouteDecision::Match(_) | RouteDecision::None => {}
            }
        }

        let response = self.primary_handler.handle(method, params).await?;
        if let RouteDecision::Ambiguous { path, candidates } = route_decision {
            Ok(annotate_wrapped_tool_json(
                response,
                serde_json::json!({
                    "routing_diagnostics": {
                        "status": "ambiguous_relative_path",
                        "path": path,
                        "candidate_workspaces": candidates,
                        "fallback_workspace": self.primary_root.to_string_lossy(),
                        "reason": "Relative path existed under multiple configured workspace roots; Lattice refused to infer shard ownership and used the primary shard fallback."
                    }
                }),
            ))
        } else {
            Ok(response)
        }
    }
}

impl ViewRequestHandler {
    async fn handle_with_root(
        &self,
        root: PathBuf,
        method: &str,
        params: Value,
    ) -> Result<Value, (i32, String)> {
        let shard = match self
            .daemon
            .shard_for(
                root.clone(),
                self.focus_files.clone(),
                self.focus_dirs.clone(),
                true,
            )
            .await
        {
            Ok(shard) => shard,
            Err(error) if is_shard_capacity_error(&error) => {
                return DeferredShardHandler {
                    daemon: Arc::clone(&self.daemon),
                    root,
                    focus_files: self.focus_files.clone(),
                    focus_dirs: self.focus_dirs.clone(),
                }
                .handle(method, params)
                .await;
            }
            Err(error) => return Err(internal_error(error)),
        };
        let retained = RetainedShard::from_retained(shard);
        let handler = retained.handler();
        let value = handler.handle(method, params).await?;
        self.remember_handles_for_root(&value, retained.root())
            .await;
        Ok(value)
    }

    async fn handle_index_status(&self, params: Value) -> Result<Value, (i32, String)> {
        let mut statuses = Vec::new();
        for root in &self.roots {
            let value = match self
                .daemon
                .shard_for(
                    root.clone(),
                    self.focus_files.clone(),
                    self.focus_dirs.clone(),
                    true,
                )
                .await
            {
                Ok(shard) => {
                    let retained = RetainedShard::from_retained(shard);
                    retained
                        .handler()
                        .handle("tools/call", params.clone())
                        .await?
                }
                Err(error) if is_shard_capacity_error(&error) => {
                    let reason = error.to_string();
                    cold_start_response(
                        "tools/call",
                        &params,
                        root,
                        &self.daemon.index_work,
                        None,
                        Some(reason.as_str()),
                    )?
                }
                Err(error) => return Err(internal_error(error)),
            };
            statuses.push(extract_tool_json(value)?);
        }

        let primary = statuses
            .first()
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let graph_counts_unknown = statuses.iter().any(|status| {
            status
                .get("graph_counts_state")
                .and_then(Value::as_str)
                .is_some_and(|state| state == "unknown")
        });
        let total_nodes: u64 = statuses
            .iter()
            .filter_map(|status| status.get("nodes").and_then(Value::as_u64))
            .sum();
        let total_edges: u64 = statuses
            .iter()
            .filter_map(|status| status.get("edges").and_then(Value::as_u64))
            .sum();
        let total_files: u64 = statuses
            .iter()
            .filter_map(|status| status.get("files").and_then(Value::as_u64))
            .sum();
        let any_indexing = statuses.iter().any(|status| {
            status
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| status == "indexing")
        });

        Ok(wrap_json_text(serde_json::json!({
            "status": if any_indexing { "indexing" } else { "ready" },
            "version": primary.get("version").cloned().unwrap_or_else(|| serde_json::json!(env!("CARGO_PKG_VERSION"))),
            "workspace": self.primary_root.to_string_lossy(),
            "workspace_role": "primary_shard",
            "workspace_field_meaning": "daemon_primary_shard_for_this_logical_view",
            "primary_workspace": self.primary_root.to_string_lossy(),
            "request_workspace": Value::Null,
            "request_workspace_available": false,
            "request_workspace_note": "stdio MCP does not expose the caller's current working directory per tool call; use path-bearing tool arguments for request-specific routing.",
            "query_scope": "logical_view_all_shards",
            "logical_view": true,
            "workspaces": self.roots.iter().map(|root| root.to_string_lossy().to_string()).collect::<Vec<_>>(),
            "query_workspaces": self.roots.iter().map(|root| root.to_string_lossy().to_string()).collect::<Vec<_>>(),
            "nodes": if graph_counts_unknown { Value::Null } else { serde_json::json!(total_nodes) },
            "edges": if graph_counts_unknown { Value::Null } else { serde_json::json!(total_edges) },
            "files": if graph_counts_unknown { Value::Null } else { serde_json::json!(total_files) },
            "graph_counts_state": if graph_counts_unknown { "partially_unknown" } else { "known" },
            "shards": statuses,
        })))
    }

    async fn handle_cross_shard_workflow(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, (i32, String)> {
        let render = workflow_render_mode_for_tool_call(&params);
        let mut shard_results = Vec::new();
        let mut failed_shards = Vec::new();
        let mut incomplete_shards = Vec::new();
        for root in &self.roots {
            match self
                .handle_with_root(root.clone(), method, params.clone())
                .await
            {
                Ok(value) => match extract_tool_json(value.clone()) {
                    Ok(payload) if is_indexing_payload(&payload) => {
                        incomplete_shards.push(shard_incomplete_value(root, &payload));
                    }
                    Ok(payload) => shard_results.push((root.clone(), payload)),
                    Err((_, error)) => failed_shards.push(serde_json::json!({
                        "workspace": root.to_string_lossy(),
                        "error": error,
                    })),
                },
                Err((_, error)) => failed_shards.push(serde_json::json!({
                    "workspace": root.to_string_lossy(),
                    "error": error,
                })),
            }
        }

        if shard_results.is_empty() && incomplete_shards.is_empty() {
            return Err((
                -32603,
                "No shard produced a workflow response for the logical view".to_string(),
            ));
        }

        let merged = merge_workflow_payloads(
            &self.primary_root,
            self.roots.len(),
            shard_results,
            failed_shards,
            incomplete_shards,
        );
        Ok(wrap_workflow_json_for_render(merged, render))
    }

    async fn handle_cross_shard_search_memory(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, (i32, String)> {
        let mut shard_results = Vec::new();
        let mut failed_shards = Vec::new();
        for root in &self.roots {
            match self
                .handle_with_root(root.clone(), method, params.clone())
                .await
            {
                Ok(value) => match extract_tool_json(value) {
                    Ok(payload) => shard_results.push((root.clone(), payload)),
                    Err((_, error)) => failed_shards.push(serde_json::json!({
                        "workspace": root.to_string_lossy(),
                        "error": error,
                    })),
                },
                Err((_, error)) => failed_shards.push(serde_json::json!({
                    "workspace": root.to_string_lossy(),
                    "error": error,
                })),
            }
        }

        Ok(wrap_json_text(merge_search_memory_payloads(
            self.roots.len(),
            shard_results,
            failed_shards,
        )))
    }

    async fn route_root_for_context_handle(&self, params: &Value) -> Option<PathBuf> {
        let handle = params
            .get("arguments")
            .and_then(|arguments| arguments.get("handle"))
            .and_then(Value::as_str)?;
        self.handle_owners.lock().await.get(handle).cloned()
    }

    async fn remember_handles_for_root(&self, value: &Value, root: &PathBuf) {
        if let Ok(payload) = extract_tool_json(value.clone()) {
            let handles = handles_from_payload(&payload);
            if !handles.is_empty() {
                let mut owners = self.handle_owners.lock().await;
                for handle in handles {
                    owners.insert(handle, root.clone());
                }
            }
        }
    }

    #[cfg(test)]
    fn route_root_for_tool_call(&self, params: &Value) -> Option<PathBuf> {
        match self.route_decision_for_tool_call(params) {
            RouteDecision::Match(root) => Some(root),
            RouteDecision::None | RouteDecision::Ambiguous { .. } => None,
        }
    }

    fn route_decision_for_tool_call(&self, params: &Value) -> RouteDecision {
        let arguments = params.get("arguments").unwrap_or(&Value::Null);
        for value in explicit_path_values(arguments) {
            match self.root_for_path(value) {
                PathRoute::Match(root) => return RouteDecision::Match(root),
                PathRoute::Ambiguous { path, candidates } => {
                    return RouteDecision::Ambiguous { path, candidates }
                }
                PathRoute::None => {}
            }
        }
        RouteDecision::None
    }

    fn root_for_path(&self, raw: &str) -> PathRoute {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            let canonical = path.canonicalize().unwrap_or(path);
            return self
                .roots
                .iter()
                .find(|root| canonical.starts_with(root))
                .cloned()
                .map(PathRoute::Match)
                .unwrap_or(PathRoute::None);
        }

        let mut matches = self
            .roots
            .iter()
            .filter_map(|root| {
                let candidate = root.join(&path);
                candidate.exists().then(|| root.clone())
            })
            .collect::<Vec<_>>();
        matches.dedup();
        if matches.len() == 1 {
            PathRoute::Match(matches.pop().expect("one match"))
        } else if matches.len() > 1 {
            PathRoute::Ambiguous {
                path: raw.to_string(),
                candidates: matches
                    .into_iter()
                    .map(|root| root.to_string_lossy().to_string())
                    .collect(),
            }
        } else {
            PathRoute::None
        }
    }
}

enum RouteDecision {
    Match(PathBuf),
    Ambiguous {
        path: String,
        candidates: Vec<String>,
    },
    None,
}

enum PathRoute {
    Match(PathBuf),
    Ambiguous {
        path: String,
        candidates: Vec<String>,
    },
    None,
}

/// What a loading shard needs to measure its own cost.
#[derive(Clone)]
struct LoadCostProbe {
    footprint: FootprintProbe,
    loading: Arc<AtomicUsize>,
}

/// Reads the daemon's real memory footprint. Injected so tests can drive
/// memory pressure without allocating gigabytes.
type FootprintProbe = Arc<dyn Fn() -> Option<u64> + Send + Sync>;

/// Room kept free for the workspace about to load. Its cost is unknown until
/// it has loaded; measured in 2026-09 it is about 0.12 MB per indexed file,
/// so 512 MiB covers a repository of roughly 4,000 files, and the periodic
/// sweep corrects for anything larger.
const LOAD_HEADROOM_BYTES: u64 = 512 * 1024 * 1024;
/// How long a connected shard must go unused before it may be unloaded to
/// make room. Ten minutes is well past a pause between tool calls, and a
/// reload costs seconds because the index is read back from disk.
const DEFAULT_CONNECTED_IDLE_SECS: u64 = 600;

pub(crate) struct GlobalDaemon {
    shards: Mutex<HashMap<String, Arc<ShardEntry>>>,
    /// Serialises loading a new shard with shutting an old one down, so a
    /// workspace is never bootstrapped while its previous runtime still
    /// holds the checkout lease.
    lifecycle_gate: Mutex<()>,
    connected: Arc<StdMutex<HashMap<String, ConnectedWorkspace>>>,
    loading: Arc<AtomicUsize>,
    footprint: FootprintProbe,
    memory_budget_bytes: u64,
    connected_idle_secs: u64,
    max_loaded_shards: Option<usize>,
    prewarm_view_shards: bool,
    idle_ttl: Duration,
    has_loaded_runtime: AtomicBool,
    exit_when_idle: bool,
    index_work: Arc<crate::index_work::IndexWorkCoordinator>,
    resource_budget: Arc<crate::resource_budget::ResourceBudget>,
    view_reservation_bytes: u64,
    hook_session_route: Option<Arc<HookSessionRoute>>,
    shutting_down: AtomicBool,
    settings: crate::daemon_settings::DaemonSettings,
    started_at: std::time::Instant,
}

impl GlobalDaemon {
    pub(crate) fn new(settings: crate::daemon_settings::DaemonSettings) -> Self {
        let mut daemon = Self::new_with_ceiling(
            settings.max_loaded_shards,
            env_bool("LATTICE_PREWARM_VIEW_SHARDS", false),
        );
        daemon.memory_budget_bytes = settings.memory_budget_bytes;
        daemon.settings = settings;
        match HookSessionRoute::open_default() {
            Ok(route) => daemon.hook_session_route = Some(Arc::new(route)),
            Err(_) => {
                tracing::error!("hook-session service is unavailable");
                lifecycle_log::log_event("daemon", "hook_session_registry_unavailable", &[]);
            }
        }
        daemon
    }

    /// A daemon with an explicit shard ceiling, as tests and an operator's
    /// `max_loaded_shards` both express it.
    fn new_with_config(max_loaded_shards: usize, prewarm_view_shards: bool) -> Self {
        Self::new_with_ceiling(Some(max_loaded_shards), prewarm_view_shards)
    }

    fn new_with_ceiling(max_loaded_shards: Option<usize>, prewarm_view_shards: bool) -> Self {
        let resource_budget = crate::resource_budget::ResourceBudget::from_env();
        let settings = crate::daemon_settings::DaemonSettings::unconfigured(max_loaded_shards);
        Self {
            shards: Mutex::new(HashMap::new()),
            lifecycle_gate: Mutex::new(()),
            connected: Arc::new(StdMutex::new(HashMap::new())),
            loading: Arc::new(AtomicUsize::new(0)),
            footprint: Arc::new(crate::daemon_settings::process_memory_footprint_bytes),
            memory_budget_bytes: settings.memory_budget_bytes,
            connected_idle_secs: env_duration_secs(
                "LATTICE_CONNECTED_IDLE_SECS",
                Duration::from_secs(DEFAULT_CONNECTED_IDLE_SECS),
            )
            .as_secs(),
            max_loaded_shards,
            prewarm_view_shards,
            idle_ttl: env_duration_secs(
                "LATTICE_WORKSPACE_IDLE_TTL_SECS",
                Duration::from_secs(1800),
            ),
            has_loaded_runtime: AtomicBool::new(false),
            exit_when_idle: env_bool("LATTICE_DAEMON_EXIT_WHEN_IDLE", false),
            index_work: crate::index_work::IndexWorkCoordinator::from_env_with_budget(Arc::clone(
                &resource_budget,
            )),
            resource_budget,
            view_reservation_bytes:
                crate::resource_budget::ResourceBudget::default_view_reservation(),
            hook_session_route: None,
            settings,
            started_at: std::time::Instant::now(),
            shutting_down: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    fn with_hook_session_route(mut self, route: HookSessionRoute) -> Self {
        self.hook_session_route = Some(Arc::new(route));
        self
    }

    #[cfg(test)]
    async fn handler_for(self: &Arc<Self>, request: &ProxyRequest) -> Result<RuntimeLease> {
        self.handler_for_client(request, ClientKind::Cli).await
    }

    async fn handler_for_client(
        self: &Arc<Self>,
        request: &ProxyRequest,
        client_kind: ClientKind,
    ) -> Result<RuntimeLease> {
        let roots = canonical_roots(&request.workspace_roots)?;
        let view_key = workspace_key(&roots);
        self.evict_idle().await;

        let primary_root = roots
            .first()
            .ok_or_else(|| anyhow::anyhow!("workspace request resolved to zero shards"))?
            .clone();
        let primary = match self
            .shard_for(
                primary_root.clone(),
                request.focus_files.clone(),
                request.focus_dirs.clone(),
                true,
            )
            .await
        {
            Ok(shard) => Some(RetainedShard::from_retained(shard)),
            Err(error) if is_shard_capacity_error(&error) => None,
            Err(error) => return Err(error),
        };
        let deferred_primary = primary.is_none();
        // Counted as connected whether or not it could be loaded.
        let connection = ConnectionRegistration::new(
            &self.connected,
            shard_key(&primary_root),
            client_kind == ClientKind::StdioProxy,
        );
        let retained_shards = Arc::new(StdMutex::new(HashMap::new()));
        if let Some(primary) = primary {
            adopt_retained_lease_shard(&retained_shards, primary.into_shard())?;
        }
        let primary_handler: Arc<dyn RequestHandler> = Arc::new(LeaseShardHandler {
            daemon: Arc::clone(self),
            root: primary_root.clone(),
            focus_files: request.focus_files.clone(),
            focus_dirs: request.focus_dirs.clone(),
            retained_shards: Arc::clone(&retained_shards),
        });
        let handler: Arc<dyn RequestHandler> = if roots.len() > 1 {
            Arc::new(ViewRequestHandler {
                daemon: Arc::clone(self),
                roots: roots.clone(),
                primary_root: primary_root.clone(),
                primary_handler,
                focus_files: request.focus_files.clone(),
                focus_dirs: request.focus_dirs.clone(),
                handle_owners: Mutex::new(HashMap::new()),
            })
        } else {
            primary_handler
        };
        if self.prewarm_view_shards && roots.len() > 1 {
            self.spawn_view_prewarm(
                view_key.clone(),
                roots[1..].to_vec(),
                request.focus_files.clone(),
                request.focus_dirs.clone(),
            );
        }
        lifecycle_log::log_event(
            "daemon",
            "session_view_leased",
            &[
                ("workspace_key", serde_json::json!(view_key)),
                ("requested_shard_count", serde_json::json!(roots.len())),
                (
                    "active_shard_count",
                    serde_json::json!(retained_shard_count(&retained_shards)),
                ),
                (
                    "primary_shard",
                    serde_json::json!(primary_root.to_string_lossy().to_string()),
                ),
                (
                    "primary_shard_deferred",
                    serde_json::json!(deferred_primary),
                ),
            ],
        );
        Ok(RuntimeLease {
            retained_shards,
            handler,
            _connection: connection,
        })
    }

    fn spawn_view_prewarm(
        self: &Arc<Self>,
        view_key: String,
        roots: Vec<PathBuf>,
        focus_files: Vec<String>,
        focus_dirs: Vec<String>,
    ) {
        let daemon = Arc::clone(self);
        tokio::spawn(async move {
            lifecycle_log::log_event(
                "daemon",
                "view_prewarm_started",
                &[
                    ("workspace_key", serde_json::json!(view_key.clone())),
                    ("remaining_shard_count", serde_json::json!(roots.len())),
                ],
            );
            for root in roots {
                let shard_key = shard_key(&root);
                match daemon
                    .shard_for(root, focus_files.clone(), focus_dirs.clone(), false)
                    .await
                {
                    Ok(_) => {
                        let fields = vec![
                            ("workspace_key", serde_json::json!(view_key.clone())),
                            ("shard_key", serde_json::json!(shard_key)),
                        ];
                        lifecycle_log::log_event("daemon", "view_prewarm_shard_ready", &fields);
                    }
                    Err(error) => {
                        lifecycle_log::log_event(
                            "daemon",
                            "view_prewarm_shard_failed",
                            &[
                                ("workspace_key", serde_json::json!(view_key.clone())),
                                ("shard_key", serde_json::json!(shard_key)),
                                ("error", serde_json::json!(error.to_string())),
                            ],
                        );
                        break;
                    }
                }
            }
            lifecycle_log::log_event(
                "daemon",
                "view_prewarm_finished",
                &[("workspace_key", serde_json::json!(view_key))],
            );
        });
    }

    /// Daemon-wide facts for `status` and `doctor`: the effective shard cap,
    /// where it came from, and how much of it is in use.
    pub(crate) async fn report(&self) -> Value {
        const MAX_REPORTED_WORKSPACES: usize = 32;
        let mut report = self.settings.report();
        let now = now_epoch_secs();
        let connected = lock_owned(&self.connected).clone();
        let shards = self.shards.lock().await;

        // Every workspace that is connected, loaded, or both.
        let mut keys = connected
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        keys.extend(shards.keys().cloned());
        let mut workspaces = Vec::new();
        for key in &keys {
            let clients = connected.get(key).copied().unwrap_or_default();
            let entry = shards.get(key);
            let state = match entry {
                None => "not_loaded",
                Some(entry) if entry.bootstrap_error().is_some() => "failed",
                Some(entry) if entry.is_bootstrapping() => "loading",
                Some(entry) if entry.index_work.workspace_is_busy(key) => "indexing",
                Some(entry) if entry.is_evictable() => "idle",
                Some(entry) if entry.is_idle_connected(now, self.connected_idle_secs) => {
                    "connected_idle"
                }
                Some(_) => "in_use",
            };
            workspaces.push(serde_json::json!({
                "workspace": key,
                "agents": clients.agents,
                "other_clients": clients.other,
                "state": state,
                "idle_secs": entry.map(|entry| entry.idle_secs(now)),
                // Attributable only when nothing else was loading at the time.
                "load_cost_bytes": entry
                    .map(|entry| entry.load_cost_bytes.load(Ordering::Acquire))
                    .filter(|bytes| *bytes > 0),
            }));
        }
        let count = |state: &str| {
            workspaces
                .iter()
                .filter(|workspace| workspace["state"] == state)
                .count()
        };
        report["connected_workspaces"] = serde_json::json!(connected
            .values()
            .filter(|clients| clients.agents > 0)
            .count());
        report["agents"] = serde_json::json!(connected
            .values()
            .map(|clients| clients.agents)
            .sum::<usize>());
        report["loaded_shards"] = serde_json::json!(shards.len());
        report["idle_shards"] = serde_json::json!(count("idle"));
        report["connected_idle_shards"] = serde_json::json!(count("connected_idle"));
        report["evictable_shards"] = serde_json::json!(count("idle") + count("connected_idle"));
        report["connected_idle_threshold_secs"] = serde_json::json!(self.connected_idle_secs);
        report["idle_grace_secs"] = serde_json::json!(self.idle_ttl.as_secs());
        report["deferred"] = serde_json::json!(self.would_defer(&shards).map(|kind| kind.label()));
        report["memory_budget_bytes"] = serde_json::json!(self.memory_budget_bytes);
        report["memory_footprint_bytes"] = serde_json::json!((self.footprint)());
        report["uptime_secs"] = serde_json::json!(self.started_at.elapsed().as_secs());
        report["workspaces_truncated"] =
            serde_json::json!(workspaces.len() > MAX_REPORTED_WORKSPACES);
        workspaces.truncate(MAX_REPORTED_WORKSPACES);
        report["workspaces"] = Value::Array(workspaces);
        report
    }

    /// Whether one more workspace would be deferred right now, without
    /// changing anything. Mirrors the decision in `shard_for`.
    fn would_defer(&self, shards: &HashMap<String, Arc<ShardEntry>>) -> Option<DeferralKind> {
        let kind = self.pressure(shards)?;
        let now = now_epoch_secs();
        let has_victim = shards.values().any(|entry| {
            entry.is_evictable() || entry.is_idle_connected(now, self.connected_idle_secs)
        });
        if has_victim {
            return None;
        }
        if kind == DeferralKind::Memory && !shards.values().any(|entry| entry.is_loaded()) {
            return None;
        }
        Some(kind)
    }

    /// Report a workspace's index state without loading, retaining or
    /// evicting any shard. Hooks fire constantly; they must never be the
    /// reason a shard is admitted.
    pub(crate) async fn index_state_for(
        &self,
        request: &ProxyRequest,
    ) -> crate::hook_enforcement::IndexState {
        use crate::hook_enforcement::IndexState;
        let Ok(roots) = canonical_roots(&request.workspace_roots) else {
            return IndexState::NotLoaded;
        };
        let [root] = roots.as_slice() else {
            return IndexState::NotLoaded;
        };
        let shards = self.shards.lock().await;
        match shards.get(&shard_key(root)) {
            Some(entry) if entry.bootstrap_error().is_some() => IndexState::Failed,
            Some(entry) => match entry.published_handler() {
                Ok(Some(_)) if !entry.index_work.workspace_is_busy(&shard_key(root)) => {
                    IndexState::Ready
                }
                Ok(_) => IndexState::Indexing,
                Err(_) => IndexState::Failed,
            },
            None if self.would_defer(&shards).is_some() => IndexState::Deferred,
            None => IndexState::NotLoaded,
        }
    }

    async fn reuse_shard(&self, key: &str, retain: bool) -> Result<Option<Arc<ShardEntry>>> {
        let shards = self.shards.lock().await;
        if self.shutting_down.load(Ordering::Acquire) {
            anyhow::bail!("daemon is shutting down; new workspace shards are unavailable");
        }
        Ok(shards.get(key).map(|entry| {
            if retain {
                entry.retain();
            }
            Arc::clone(entry)
        }))
    }

    /// What, if anything, stands in the way of loading one more shard.
    fn pressure(&self, shards: &HashMap<String, Arc<ShardEntry>>) -> Option<DeferralKind> {
        if self
            .max_loaded_shards
            .is_some_and(|ceiling| shards.len() >= ceiling)
        {
            return Some(DeferralKind::Ceiling);
        }
        let footprint = (self.footprint)()?;
        (footprint.saturating_add(LOAD_HEADROOM_BYTES) > self.memory_budget_bytes)
            .then_some(DeferralKind::Memory)
    }

    fn deferral_message(&self, kind: &DeferralKind, key: &str, loaded: usize) -> String {
        match kind {
            DeferralKind::Ceiling => format!(
                "the shard ceiling max_loaded_shards={} (from {}) is reached: {loaded} workspaces are loaded and every one of them is in use, so {key} cannot be loaded. Raise or remove max_loaded_shards in the daemon settings file, or wait for another workspace to go idle for {} minutes.",
                self.max_loaded_shards.unwrap_or_default(),
                self.settings.max_loaded_shards_source.describe(),
                self.connected_idle_secs / 60,
            ),
            DeferralKind::Memory => format!(
                "the daemon's memory is at its budget: {} MiB used of {} MiB (from {}), with {loaded} workspaces loaded and every one of them in use, so {key} cannot be loaded. Raise memory_budget_mb in the daemon settings file, close a workspace's agent session, or wait for one to go idle for {} minutes.",
                (self.footprint)().unwrap_or_default() / (1024 * 1024),
                self.memory_budget_bytes / (1024 * 1024),
                self.settings.memory_budget_source.describe(),
                self.connected_idle_secs / 60,
            ),
        }
    }

    async fn shard_for(
        &self,
        root: PathBuf,
        focus_files: Vec<String>,
        focus_dirs: Vec<String>,
        retain: bool,
    ) -> Result<Arc<ShardEntry>> {
        let key = shard_key(&root);
        if let Some(entry) = self.reuse_shard(&key, retain).await? {
            return Ok(entry);
        }
        // Loading is serialised with shutting shards down, so this workspace
        // is never bootstrapped while a previous runtime of it still holds
        // the checkout lease. Reuse above never waits here.
        let _lifecycle = self.lifecycle_gate.lock().await;
        let (entry, victim) = {
            let mut shards = self.shards.lock().await;
            if self.shutting_down.load(Ordering::Acquire) {
                anyhow::bail!("daemon is shutting down; new workspace shards are unavailable");
            }
            if let Some(entry) = shards.get(&key) {
                if retain {
                    entry.retain();
                }
                lifecycle_log::log_event(
                    "daemon",
                    "shard_reused",
                    &[("shard_key", serde_json::json!(key.clone()))],
                );
                return Ok(Arc::clone(entry));
            }

            // Demand-driven admission: see docs/shard-capacity.md. A count
            // never defers a connected workspace by itself; only an explicit
            // ceiling or real memory can, and then only after everything
            // idle has been offered up.
            let victim = match self.pressure(&shards) {
                None => None,
                Some(kind) => {
                    match take_victim(&mut shards, now_epoch_secs(), self.connected_idle_secs) {
                        Some((victim_key, entry, class)) => Some((victim_key, entry, class, kind)),
                        // Memory that stays high with nothing loaded is not
                        // something refusing this workspace could fix.
                        None if kind == DeferralKind::Memory
                            && !shards.values().any(|entry| entry.is_loaded()) =>
                        {
                            None
                        }
                        None => {
                            return Err(anyhow::Error::new(ShardDeferred {
                                message: self.deferral_message(&kind, &key, shards.len()),
                                kind,
                            }))
                        }
                    }
                }
            };

            // Reserve the full configured logical materialization allowance
            // before runtime bootstrap can allocate a graph or ANN view.
            let reservation = self
                .resource_budget
                .try_reserve("active_checkout_view", self.view_reservation_bytes)
                .map_err(anyhow::Error::new)?;
            let entry = Arc::new(ShardEntry::pending(
                root,
                Arc::clone(&self.index_work),
                reservation,
            ));
            if retain {
                entry.retain();
            }
            shards.insert(key.clone(), Arc::clone(&entry));
            self.has_loaded_runtime.store(true, Ordering::Release);
            (entry, victim)
        };

        if let Some((victim_key, victim, class, kind)) = victim {
            lifecycle_log::log_event(
                "daemon",
                "shard_capacity_eviction",
                &[
                    ("evicted_shard", serde_json::json!(victim_key)),
                    ("requested_shard", serde_json::json!(key.clone())),
                    ("victim", serde_json::json!(class.label())),
                    ("pressure", serde_json::json!(kind.label())),
                    (
                        "victim_connections",
                        serde_json::json!(victim.connections()),
                    ),
                ],
            );
            victim.shutdown().await;
        }

        {
            let mut shards = self.shards.lock().await;
            let still_registered = shards
                .get(&key)
                .is_some_and(|registered| Arc::ptr_eq(registered, &entry));
            if self.shutting_down.load(Ordering::Acquire) || !still_registered {
                if still_registered {
                    shards.remove(&key);
                }
                anyhow::bail!("daemon is shutting down; workspace bootstrap was cancelled");
            }
            // Bootstrap ownership is installed while admission remains fenced
            // by the shard-map lock. Shutdown cannot drain this entry between
            // the final admission check and constructor-handle publication.
            entry.start_bootstrap(
                focus_files,
                focus_dirs,
                LoadCostProbe {
                    footprint: Arc::clone(&self.footprint),
                    loading: Arc::clone(&self.loading),
                },
            );
        }

        lifecycle_log::log_event(
            "daemon",
            "shard_bootstrap_started",
            &[("shard_key", serde_json::json!(key))],
        );
        Ok(entry)
    }

    /// Unload shards until the daemon is back inside its memory budget, or
    /// nothing idle is left. Admission keeps headroom for a load it cannot
    /// size in advance; this corrects for loads that turned out larger, and
    /// for growth over a long uptime.
    async fn relieve_memory_pressure(&self) {
        let _lifecycle = self.lifecycle_gate.lock().await;
        loop {
            let over_budget =
                (self.footprint)().is_some_and(|footprint| footprint > self.memory_budget_bytes);
            if !over_budget {
                return;
            }
            let victim = {
                let mut shards = self.shards.lock().await;
                take_victim(&mut shards, now_epoch_secs(), self.connected_idle_secs)
            };
            let Some((key, entry, class)) = victim else {
                return;
            };
            lifecycle_log::log_event(
                "daemon",
                "shard_memory_eviction",
                &[
                    ("shard_key", serde_json::json!(key)),
                    ("victim", serde_json::json!(class.label())),
                    (
                        "footprint_bytes",
                        serde_json::json!((self.footprint)().unwrap_or_default()),
                    ),
                    ("budget_bytes", serde_json::json!(self.memory_budget_bytes)),
                ],
            );
            entry.shutdown().await;
        }
    }

    async fn evict_idle(&self) {
        let now = now_epoch_secs();
        let ttl = self.idle_ttl.as_secs();
        let mut victims = Vec::new();
        let _lifecycle = self.lifecycle_gate.lock().await;
        {
            let mut shards = self.shards.lock().await;
            let idle_keys: Vec<String> = shards
                .iter()
                .filter(|(_, entry)| entry.is_idle(now, ttl))
                .map(|(key, _)| key.clone())
                .collect();
            for key in idle_keys {
                if shards.get(&key).is_some_and(|entry| entry.try_retire()) {
                    if let Some(entry) = shards.remove(&key) {
                        victims.push((key, entry));
                    }
                }
            }
        }

        for (key, entry) in victims {
            tracing::info!(
                "evicting idle lattice workspace shard: {}",
                key.replace('\n', ", ")
            );
            lifecycle_log::log_event(
                "daemon",
                "shard_evicted_idle",
                &[("shard_key", serde_json::json!(key.clone()))],
            );
            entry.shutdown().await;
        }
    }

    async fn should_shutdown_when_idle(&self) -> bool {
        self.exit_when_idle
            && self.has_loaded_runtime.load(Ordering::Acquire)
            && self.shards.lock().await.is_empty()
    }

    fn cleanup_interval(&self) -> Duration {
        let ttl_secs = self.idle_ttl.as_secs().max(1);
        Duration::from_secs(ttl_secs.min(60))
    }
}

pub(crate) async fn run_global_daemon() -> Result<()> {
    // Before binding: a daemon that is going to refuse its settings must not
    // first accept connections from proxies that then lose it.
    let settings = crate::daemon_settings::load()
        .inspect_err(|error| {
            lifecycle_log::log_event(
                "daemon",
                "daemon_settings_invalid",
                &[("error", serde_json::json!(format!("{error:#}")))],
            );
        })
        .context("lattice daemon did not start: its settings are invalid")?;
    lifecycle_log::log_event(
        "daemon",
        "daemon_settings_loaded",
        &[("settings", settings.report())],
    );
    let addr = daemon_addr();
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind lattice daemon listener {addr}"))?;
    let bound_address = listener.local_addr()?;
    if !bound_address.ip().is_loopback() {
        anyhow::bail!("lattice daemon listener must bind to a loopback address");
    }
    let transport =
        Arc::new(ServerTransport::issue(&addr).with_context(|| {
            format!("failed to establish protected daemon transport for {addr}")
        })?);
    tracing::info!("lattice daemon listening on {}", addr);
    lifecycle_log::log_event(
        "daemon",
        "listener_bound",
        &[("daemon_addr", serde_json::json!(addr.clone()))],
    );

    let daemon = Arc::new(GlobalDaemon::new(settings));
    let mut connections = tokio::task::JoinSet::new();
    let mut cleanup_interval = tokio::time::interval(daemon.cleanup_interval());
    cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    cleanup_interval.tick().await;

    let result = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(error.into()),
                };
                let daemon = Arc::clone(&daemon);
                let transport = Arc::clone(&transport);
                connections.spawn(async move {
                    if let Err(err) = handle_proxy_connection(daemon, transport, stream).await {
                        tracing::warn!("proxy connection failed: {}", err);
                    }
                });
            }
            _ = cleanup_interval.tick() => {
                daemon.evict_idle().await;
                daemon.relieve_memory_pressure().await;
                if daemon.should_shutdown_when_idle().await {
                    tracing::info!("lattice daemon exiting after all workspace runtimes went idle");
                    lifecycle_log::log_event("daemon", "idle_exit", &[]);
                    break Ok(());
                }
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    tracing::warn!(%error, "proxy connection task failed");
                }
            }
        }
    };
    // Close admission while holding the same mutex used by `shard_for`; a
    // request that passed the flag check must finish insertion before this
    // barrier returns, and later requests cannot repopulate after the drain.
    close_shard_admission(&daemon).await;
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    shutdown_daemon_shards(&daemon).await;
    result
}

async fn close_shard_admission(daemon: &GlobalDaemon) {
    let _shards = daemon.shards.lock().await;
    daemon.shutting_down.store(true, Ordering::Release);
}

async fn shutdown_daemon_shards(daemon: &GlobalDaemon) {
    let victims = {
        let mut shards = daemon.shards.lock().await;
        shards.drain().map(|(_, entry)| entry).collect::<Vec<_>>()
    };
    for shard in victims {
        shard.shutdown().await;
    }
}

async fn handle_proxy_connection(
    daemon: Arc<GlobalDaemon>,
    transport: Arc<ServerTransport>,
    stream: TcpStream,
) -> Result<()> {
    let peer = stream
        .peer_addr()
        .context("failed to identify transport peer")?;
    let peer_addr = peer.to_string();
    lifecycle_log::log_event(
        "daemon",
        "proxy_connection_accepted",
        &[("peer_addr", serde_json::json!(peer_addr.clone()))],
    );
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let hello_line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow::anyhow!("proxy disconnected before hello"))?;
    let authenticated = match transport.authenticate(peer, &hello_line) {
        Ok(authenticated) => authenticated,
        Err(error) => {
            lifecycle_log::log_event(
                "daemon",
                "transport_auth_failed",
                &[("peer_addr", serde_json::json!(peer_addr))],
            );
            return Err(error);
        }
    };
    let metadata = authenticated.metadata;
    let request = authenticated.request;
    write_response_line(&mut write_half, &authenticated.ack).await?;
    lifecycle_log::log_event(
        "daemon",
        "proxy_transport_authenticated",
        &[
            (
                "workspace_roots",
                serde_json::json!(request.workspace_roots.clone()),
            ),
            ("peer_addr", serde_json::json!(peer_addr.clone())),
            (
                "connection_id",
                serde_json::json!(metadata.connection_id.clone()),
            ),
            ("client_kind", serde_json::json!(metadata.client_kind)),
        ],
    );
    let workspace_key = request.workspace_roots.join("\n");
    let result = run_json_rpc_connection(daemon, request, metadata, lines, write_half).await;
    match &result {
        Ok(()) => lifecycle_log::log_event(
            "daemon",
            "proxy_connection_closed",
            &[
                ("peer_addr", serde_json::json!(peer_addr)),
                ("workspace_key", serde_json::json!(workspace_key)),
                ("status", serde_json::json!("ok")),
            ],
        ),
        Err(error) => lifecycle_log::log_event(
            "daemon",
            "proxy_connection_closed",
            &[
                ("peer_addr", serde_json::json!(peer_addr)),
                ("workspace_key", serde_json::json!(workspace_key)),
                ("status", serde_json::json!("error")),
                ("error", serde_json::json!(error.to_string())),
            ],
        ),
    }
    result
}

async fn run_json_rpc_connection(
    daemon: Arc<GlobalDaemon>,
    proxy_request: ProxyRequest,
    connection: ConnectionMetadata,
    mut lines: tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    mut writer: tokio::net::tcp::OwnedWriteHalf,
) -> Result<()> {
    let mut lease: Option<RuntimeLease> = None;
    let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel::<PendingResponse>();
    let mut active_requests: HashMap<String, ActiveRequest> = HashMap::new();
    let mut cancelled_requests = HashSet::new();
    let mut shutting_down = false;
    let mut next_generation = 0u64;

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(message) = line? else {
                    abort_all_requests(&mut active_requests);
                    break;
                };
                let request = match parse_request(&message) {
                    Ok(request) => request,
                    Err(err) => {
                        write_response(&mut writer, &JsonRpcResponse::error(
                            Value::Null,
                            -32700,
                            format!("Parse error: {}", err),
                        )).await?;
                        continue;
                    }
                };
                let is_notification = request.id.is_null();
                let request_id = request_id_key(&request.id);

                if matches!(
                    request.method.as_str(),
                    HOOK_SESSION_OPEN_METHOD
                        | HOOK_EVENT_METHOD
                        | HOOK_TURN_SUMMARY_METHOD
                        | HOOK_SESSION_CLOSE_METHOD
                ) {
                    if is_notification {
                        continue;
                    }
                    let response = if connection.client_kind != ClientKind::HookAdapter {
                        JsonRpcResponse::error(
                            request.id,
                            -32601,
                            "Method not found".to_string(),
                        )
                    } else { match daemon.hook_session_route.as_ref() {
                        Some(route) => {
                            let route = Arc::clone(route);
                            let hook_request = proxy_request.clone();
                            let method = request.method;
                            let index_state = if method == HOOK_SESSION_OPEN_METHOD {
                                daemon.index_state_for(&proxy_request).await
                            } else {
                                crate::hook_enforcement::IndexState::NotLoaded
                            };
                            match tokio::task::spawn_blocking(move || {
                                match method.as_str() {
                                    HOOK_SESSION_OPEN_METHOD => {
                                        route.handle_open_in(
                                            &hook_request,
                                            request.params,
                                            index_state,
                                        )
                                    }
                                    HOOK_EVENT_METHOD => {
                                        route.handle_event(&hook_request, request.params)
                                    }
                                    HOOK_TURN_SUMMARY_METHOD => {
                                        route.handle_turn_summary(&hook_request, request.params)
                                    }
                                    HOOK_SESSION_CLOSE_METHOD => {
                                        route.handle_close(&hook_request, request.params)
                                    }
                                    _ => unreachable!("hook route was matched above"),
                                }
                            }).await {
                                Ok(Ok(result)) => JsonRpcResponse::success(request.id, result),
                                Ok(Err(error)) => {
                                    let (code, message) = error.json_rpc_error();
                                    JsonRpcResponse::error(request.id, code, message)
                                }
                                Err(_) => JsonRpcResponse::error(
                                    request.id,
                                    -32603,
                                    "hook session service is unavailable".to_string(),
                                ),
                            }
                        }
                        None => JsonRpcResponse::error(
                            request.id,
                            -32603,
                            "hook session service is unavailable".to_string(),
                        ),
                    }};
                    write_response(&mut writer, &response).await?;
                    continue;
                }

                if connection.client_kind == ClientKind::HookAdapter {
                    if !is_notification {
                        write_response(&mut writer, &JsonRpcResponse::error(
                            request.id,
                            -32601,
                            "Method not found".to_string(),
                        )).await?;
                    }
                    continue;
                }

                match request.method.as_str() {
                    "notifications/cancelled" => {
                        cancel_active_request(
                            &mut active_requests,
                            &mut cancelled_requests,
                            &request.params,
                        );
                        continue;
                    }
                    "shutdown" => {
                        shutting_down = true;
                        if !is_notification {
                            write_response(&mut writer, &JsonRpcResponse::success(
                                request.id,
                                serde_json::json!({}),
                            )).await?;
                        }
                        continue;
                    }
                    "exit" => {
                        abort_all_requests(&mut active_requests);
                        break;
                    }
                    _ => {}
                }

                if shutting_down {
                    if !is_notification {
                        write_response(&mut writer, &JsonRpcResponse::error(
                            request.id,
                            -32000,
                            "Server is shutting down".to_string(),
                        )).await?;
                    }
                    continue;
                }

                if lease.is_none() {
                    match daemon
                        .handler_for_client(&proxy_request, connection.client_kind)
                        .await
                    {
                        Ok(loaded) => lease = Some(loaded),
                        Err(_) => {
                            if !is_notification {
                                write_response(&mut writer, &JsonRpcResponse::error(
                                    request.id,
                                    -32603,
                                    "workspace runtime is unavailable".to_string(),
                                )).await?;
                            }
                            continue;
                        }
                    }
                }
                let handler = Arc::clone(
                    &lease
                        .as_ref()
                        .expect("runtime lease was loaded for ordinary RPC")
                        .handler,
                );

                if let Some(key) = request_id {
                    cancelled_requests.remove(&key);
                    let response_tx = response_tx.clone();
                    let response_key = key.clone();
                    let generation = next_generation;
                    next_generation = next_generation.wrapping_add(1);
                    let handler = Arc::clone(&handler);
                    let method = request.method;
                    let params = request.params;
                    let id = request.id;
                    let request_work = lease
                        .as_ref()
                        .expect("runtime lease was loaded for ordinary RPC")
                        .begin_work();
                    let workflow_step = served_workflow_step(&method, &params)
                        .zip(daemon.hook_session_route.clone());
                    let workflow_hello = proxy_request.clone();
                    let status_daemon =
                        is_index_status_request(&method, &params).then(|| Arc::clone(&daemon));
                    let task = tokio::spawn(async move {
                        let _request_work = request_work;
                        let mut outcome = handler.handle(&method, params).await;
                        if let (Ok(result), Some(daemon)) = (&mut outcome, status_daemon) {
                            attach_daemon_report(result, daemon.report().await);
                        }
                        if let (Ok(result), Some((step, route))) = (&outcome, workflow_step) {
                            if !tool_result_is_error(result) {
                                // Recorded before the response is written, so an
                                // edit retried straight after a plan sees it.
                                let _ = tokio::task::spawn_blocking(move || {
                                    route.record_workflow_step(&workflow_hello, step);
                                })
                                .await;
                            }
                        }
                        let response = match outcome {
                            Ok(result) => JsonRpcResponse::success(id, result),
                            Err((code, message)) => JsonRpcResponse::error(id, code, message),
                        };
                        let _ = response_tx.send(PendingResponse {
                            request_key: response_key,
                            generation,
                            response,
                        });
                    });
                    if let Some(previous) = active_requests.insert(
                        key,
                        ActiveRequest { generation, handle: task },
                    ) {
                        previous.handle.abort();
                    }
                } else {
                    let handler = Arc::clone(&handler);
                    let request_work = lease
                        .as_ref()
                        .expect("runtime lease was loaded for ordinary RPC")
                        .begin_work();
                    tokio::spawn(async move {
                        let _request_work = request_work;
                        let _ = handler.handle(&request.method, request.params).await;
                    });
                }
            }
            response = response_rx.recv() => {
                let Some(response) = response else { continue; };
                if should_write_tracked_response(
                    &mut active_requests,
                    &mut cancelled_requests,
                    &response,
                ) {
                    write_response(&mut writer, &response.response).await?;
                }
            }
        }
    }
    Ok(())
}

async fn write_response_line<T: serde::Serialize>(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    value: &T,
) -> Result<()> {
    let mut encoded = serde_json::to_vec(value)?;
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    writer.flush().await?;
    Ok(())
}

async fn write_response(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    response: &JsonRpcResponse,
) -> Result<()> {
    writer
        .write_all(format_response(response).as_bytes())
        .await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

fn canonical_roots(raw_roots: &[String]) -> Result<Vec<PathBuf>> {
    let roots: Vec<PathBuf> = if raw_roots.is_empty() {
        vec![std::env::current_dir()?]
    } else {
        raw_roots.iter().map(PathBuf::from).collect()
    };
    let mut canonical = Vec::new();
    for root in roots {
        if !root.is_dir() {
            anyhow::bail!("workspace path {} is not a directory", root.display());
        }
        canonical.push(root.canonicalize().unwrap_or(root));
    }
    Ok(crate::deduplicate_roots(canonical))
}

fn workspace_key(roots: &[PathBuf]) -> String {
    roots.iter().map(shard_key).collect::<Vec<_>>().join("\n")
}

fn shard_key(root: &PathBuf) -> String {
    root.to_string_lossy().to_string()
}

fn request_id_key(id: &Value) -> Option<String> {
    match id {
        Value::Null => None,
        Value::String(value) => Some(format!("string:{value}")),
        Value::Number(value) => Some(format!("number:{value}")),
        Value::Bool(value) => Some(format!("bool:{value}")),
        other => serde_json::to_string(other)
            .ok()
            .map(|encoded| format!("json:{encoded}")),
    }
}

#[cfg(test)]
mod tests {
    include!("socket_shutdown_acceptance_tests.rs");
    use super::*;
    use crate::transport::{client_handshake, ClientKind};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::AsyncWriteExt;

    #[test]
    fn deferred_workspace_status_does_not_claim_to_be_indexing() {
        let root = unique_test_root("deferred-status");
        let index_work = crate::index_work::IndexWorkCoordinator::new(1);
        let deferred = cold_index_status_payload(
            &root,
            &index_work,
            None,
            Some("3 loaded workspace shards and all are active"),
        );
        assert_eq!(deferred["status"], "deferred");
        assert_eq!(deferred["indexing"], false);
        let summary = deferred["summary"].as_str().unwrap();
        assert!(summary.starts_with("Deferred, not loading: 3 loaded workspace shards"));
        assert!(summary.contains("Nothing is indexing this workspace"));
        assert_eq!(deferred["bootstrap"]["state"], "deferred");
        assert_eq!(deferred["graph_snapshot_state"], "not_loaded");
        // A merged view must still treat the deferred shard as incomplete.
        assert!(is_indexing_payload(&deferred));

        let starting = cold_index_status_payload(&root, &index_work, None, None);
        assert_eq!(starting["status"], "indexing");
        assert_eq!(starting["indexing"], true);

        let failed =
            cold_index_status_payload(&root, &index_work, Some("bootstrap failed".into()), None);
        assert_eq!(failed["status"], "degraded");
        assert_eq!(failed["indexing"], false);
    }

    const TEST_MIB: u64 = 1024 * 1024;

    /// A daemon whose memory reading the test controls.
    fn daemon_with_memory(
        ceiling: Option<usize>,
        budget_mib: u64,
    ) -> (Arc<GlobalDaemon>, Arc<AtomicU64>) {
        let used = Arc::new(AtomicU64::new(64 * TEST_MIB));
        let mut daemon = GlobalDaemon::new_with_ceiling(ceiling, false);
        let probe = Arc::clone(&used);
        daemon.footprint = Arc::new(move || Some(probe.load(Ordering::Acquire)));
        daemon.memory_budget_bytes = budget_mib * TEST_MIB;
        daemon.connected_idle_secs = 600;
        (Arc::new(daemon), used)
    }

    fn capacity_roots(label: &str, count: usize) -> Vec<PathBuf> {
        (0..count)
            .map(|index| {
                let root = unique_test_root(&format!("lattice-demand-{label}-{index}"));
                std::fs::create_dir_all(&root).expect("create capacity root");
                root.canonicalize().unwrap_or(root)
            })
            .collect()
    }

    fn request_for(root: &Path) -> ProxyRequest {
        ProxyRequest {
            workspace_roots: vec![root.to_string_lossy().to_string()],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        }
    }

    /// Make a shard look as if nothing has asked it for anything for a while.
    fn age(entry: &ShardEntry, secs: u64) {
        entry
            .last_used_epoch_secs
            .store(now_epoch_secs().saturating_sub(secs), Ordering::Release);
    }

    async fn loaded_keys(daemon: &GlobalDaemon) -> Vec<String> {
        let mut keys = daemon
            .shards
            .lock()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }

    async fn cleanup(daemon: &Arc<GlobalDaemon>, roots: Vec<PathBuf>) {
        shutdown_all_shards(daemon).await;
        for root in roots {
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn every_connected_workspace_loads_with_no_count_based_deferral() {
        // Far more than the old fixed count of three, and more than the eight
        // the fixed 256 MiB view reservation used to allow.
        let (daemon, _) = daemon_with_memory(None, 8 * 1024);
        let roots = capacity_roots("all-load", 12);
        let mut leases = Vec::new();
        for root in &roots {
            let lease = daemon
                .handler_for_client(&request_for(root), ClientKind::StdioProxy)
                .await
                .expect("a connected workspace always gets a lease");
            assert_eq!(
                retained_shard_count(&lease.retained_shards),
                1,
                "workspace {} was deferred",
                root.display()
            );
            leases.push(lease);
        }
        assert_eq!(loaded_keys(&daemon).await.len(), 12);
        let report = daemon.report().await;
        assert_eq!(report["connected_workspaces"], 12);
        assert_eq!(report["agents"], 12);
        assert_eq!(report["loaded_shards"], 12);
        assert!(report["deferred"].is_null());
        assert!(report["max_loaded_shards"].is_null());
        drop(leases);
        cleanup(&daemon, roots).await;
    }

    #[tokio::test]
    async fn two_agents_on_one_checkout_share_a_shard_and_are_counted_separately() {
        let (daemon, _) = daemon_with_memory(None, 8 * 1024);
        let roots = capacity_roots("shared", 1);
        let first = daemon
            .handler_for_client(&request_for(&roots[0]), ClientKind::StdioProxy)
            .await
            .unwrap();
        let second = daemon
            .handler_for_client(&request_for(&roots[0]), ClientKind::StdioProxy)
            .await
            .unwrap();
        let cli = daemon
            .handler_for_client(&request_for(&roots[0]), ClientKind::Cli)
            .await
            .unwrap();
        assert_eq!(
            loaded_keys(&daemon).await.len(),
            1,
            "one checkout, one shard"
        );
        let shard = Arc::clone(daemon.shards.lock().await.values().next().unwrap());
        assert_eq!(shard.connections(), 3);

        let report = daemon.report().await;
        assert_eq!(report["connected_workspaces"], 1);
        assert_eq!(report["agents"], 2);
        assert_eq!(report["workspaces"][0]["agents"], 2);
        assert_eq!(report["workspaces"][0]["other_clients"], 1);

        drop(first);
        drop(cli);
        let report = daemon.report().await;
        assert_eq!(report["agents"], 1);
        assert_eq!(shard.connections(), 1);
        drop(second);
        let report = daemon.report().await;
        assert_eq!(report["connected_workspaces"], 0);
        assert_eq!(report["agents"], 0);
        assert_eq!(shard.connections(), 0);
        cleanup(&daemon, roots).await;
    }

    #[tokio::test]
    async fn an_unconnected_shard_is_evicted_after_its_grace_and_not_before() {
        let (mut_daemon, _) = daemon_with_memory(None, 8 * 1024);
        let daemon = mut_daemon;
        let roots = capacity_roots("grace", 2);
        let connected = daemon
            .handler_for_client(&request_for(&roots[0]), ClientKind::StdioProxy)
            .await
            .unwrap();
        let gone = daemon
            .handler_for_client(&request_for(&roots[1]), ClientKind::StdioProxy)
            .await
            .unwrap();
        drop(gone);
        wait_for_shard_evictable(&daemon, &roots[1]).await;
        let grace = daemon.idle_ttl.as_secs();

        // Inside the grace period nothing goes, so a reconnect is instant.
        daemon.evict_idle().await;
        assert_eq!(loaded_keys(&daemon).await.len(), 2);

        // Past it, only the unconnected shard goes, however old the other is.
        for entry in daemon.shards.lock().await.values() {
            age(entry, grace + 10);
        }
        daemon.evict_idle().await;
        assert_eq!(loaded_keys(&daemon).await, vec![shard_key(&roots[0])]);
        drop(connected);
        cleanup(&daemon, roots).await;
    }

    #[tokio::test]
    async fn a_reconnect_after_eviction_loads_the_workspace_again() {
        let (daemon, _) = daemon_with_memory(None, 8 * 1024);
        let roots = capacity_roots("reconnect", 1);
        let lease = daemon
            .handler_for_client(&request_for(&roots[0]), ClientKind::StdioProxy)
            .await
            .unwrap();
        let first = Arc::clone(daemon.shards.lock().await.values().next().unwrap());
        drop(lease);
        wait_for_shard_evictable(&daemon, &roots[0]).await;
        age(&first, daemon.idle_ttl.as_secs() + 10);
        daemon.evict_idle().await;
        assert!(loaded_keys(&daemon).await.is_empty());
        assert!(first.is_retired());

        let lease = daemon
            .handler_for_client(&request_for(&roots[0]), ClientKind::StdioProxy)
            .await
            .unwrap();
        assert_eq!(retained_shard_count(&lease.retained_shards), 1);
        let second = Arc::clone(daemon.shards.lock().await.values().next().unwrap());
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(!second.is_retired());
        drop(lease);
        cleanup(&daemon, roots).await;
    }

    #[tokio::test]
    async fn memory_pressure_evicts_unconnected_then_idle_connected_and_never_a_busy_shard() {
        let (daemon, used) = daemon_with_memory(None, 2 * 1024);
        let roots = capacity_roots("memory", 4);
        let busy = daemon
            .handler_for_client(&request_for(&roots[0]), ClientKind::StdioProxy)
            .await
            .unwrap();
        let silent = daemon
            .handler_for_client(&request_for(&roots[1]), ClientKind::StdioProxy)
            .await
            .unwrap();
        let gone = daemon
            .handler_for_client(&request_for(&roots[2]), ClientKind::StdioProxy)
            .await
            .unwrap();
        drop(gone);
        for root in &roots[..3] {
            wait_for_shard_published(&daemon, root).await;
        }
        wait_for_shard_evictable(&daemon, &roots[2]).await;
        {
            let shards = daemon.shards.lock().await;
            age(&shards[&shard_key(&roots[1])], 3_600);
            age(&shards[&shard_key(&roots[2])], 30);
        }

        // At the budget. The newcomer still loads: the unconnected shard goes
        // first, although the connected one has been idle for far longer.
        used.store(2 * 1024 * TEST_MIB, Ordering::Release);
        let newcomer = daemon
            .handler_for_client(&request_for(&roots[3]), ClientKind::StdioProxy)
            .await
            .unwrap();
        assert_eq!(retained_shard_count(&newcomer.retained_shards), 1);
        let keys = loaded_keys(&daemon).await;
        assert!(
            !keys.contains(&shard_key(&roots[2])),
            "unconnected shard must go first"
        );
        assert!(keys.contains(&shard_key(&roots[1])));

        // Still over budget at the next sweep: now the connected-but-silent
        // shard is unloaded. The busy one and the newcomer are untouched.
        let silent_shard = Arc::clone(&daemon.shards.lock().await[&shard_key(&roots[1])]);
        wait_for_shard_published(&daemon, &roots[3]).await;
        // Exactly at the budget the sweep leaves everything alone.
        daemon.relieve_memory_pressure().await;
        assert!(loaded_keys(&daemon).await.contains(&shard_key(&roots[1])));
        used.store(2 * 1024 * TEST_MIB + 1, Ordering::Release);
        daemon.relieve_memory_pressure().await;
        let keys = loaded_keys(&daemon).await;
        assert!(!keys.contains(&shard_key(&roots[1])));
        assert!(keys.contains(&shard_key(&roots[0])));
        assert!(keys.contains(&shard_key(&roots[3])));
        assert!(silent_shard.is_retired());

        // Its agent asks again: it reloads, invisibly to the client.
        used.store(256 * TEST_MIB, Ordering::Release);
        let status = silent
            .handler
            .handle(
                "tools/call",
                serde_json::json!({"name": "status", "arguments": {"scope": "index"}}),
            )
            .await
            .expect("an unloaded shard answers its next request");
        assert!(extract_tool_json(status).is_ok());
        let reloaded = Arc::clone(&daemon.shards.lock().await[&shard_key(&roots[1])]);
        assert!(!Arc::ptr_eq(&reloaded, &silent_shard));
        assert_eq!(reloaded.connections(), 1, "the lease retains its new shard");
        assert_eq!(
            silent_shard.connections(),
            0,
            "and released the retired one"
        );

        drop((busy, silent, newcomer));
        cleanup(&daemon, roots).await;
    }

    #[tokio::test]
    async fn deferral_is_honest_and_happens_only_when_nothing_can_be_unloaded() {
        let (daemon, used) = daemon_with_memory(None, 2 * 1024);
        let roots = capacity_roots("honest", 3);
        let mut leases = Vec::new();
        for root in &roots[..2] {
            leases.push(
                daemon
                    .handler_for_client(&request_for(root), ClientKind::StdioProxy)
                    .await
                    .unwrap(),
            );
            wait_for_shard_published(&daemon, root).await;
        }
        // Both in use moments ago, and memory is at its budget.
        used.store(2 * 1024 * TEST_MIB, Ordering::Release);
        assert_eq!(
            daemon.index_state_for(&request_for(&roots[2])).await,
            crate::hook_enforcement::IndexState::Deferred
        );
        let starved = daemon
            .handler_for_client(&request_for(&roots[2]), ClientKind::StdioProxy)
            .await
            .expect("a deferred workspace still gets a lease that can heal");
        assert_eq!(retained_shard_count(&starved.retained_shards), 0);
        let status = extract_tool_json(
            starved
                .handler
                .handle(
                    "tools/call",
                    serde_json::json!({"name": "status", "arguments": {"scope": "index"}}),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(status["status"], "deferred");
        assert_eq!(status["indexing"], false);
        let summary = status["summary"].as_str().unwrap();
        assert!(
            summary.contains("memory is at its budget: 2048 MiB used of 2048 MiB"),
            "{summary}"
        );
        assert!(summary.contains("memory_budget_mb"), "{summary}");
        assert_eq!(
            loaded_keys(&daemon).await.len(),
            2,
            "nothing busy was evicted"
        );

        // It is counted as connected even though it has no shard.
        let report = daemon.report().await;
        assert_eq!(report["connected_workspaces"], 3);
        assert_eq!(report["loaded_shards"], 2);
        assert_eq!(report["deferred"], "memory");
        assert!(shard_capacity_line(&report)
            .as_str()
            .unwrap()
            .contains("would be DEFERRED by the memory budget"));

        // One of the others falls silent. The starved lease heals by itself.
        age(&daemon.shards.lock().await[&shard_key(&roots[0])], 3_600);
        let status = extract_tool_json(
            starved
                .handler
                .handle(
                    "tools/call",
                    serde_json::json!({"name": "status", "arguments": {"scope": "index"}}),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_ne!(status["status"], "deferred");
        assert_eq!(retained_shard_count(&starved.retained_shards), 1);
        drop(leases);
        drop(starved);
        cleanup(&daemon, roots).await;
    }

    #[tokio::test]
    async fn a_configured_ceiling_is_honoured_reported_and_never_starves_silently() {
        // The settings file rolled out on 2026-09-19 says six. Two keeps the
        // test small; the rule is the same.
        let (daemon, _) = daemon_with_memory(Some(2), 8 * 1024);
        let roots = capacity_roots("ceiling", 3);
        let mut leases = Vec::new();
        for root in &roots[..2] {
            leases.push(
                daemon
                    .handler_for_client(&request_for(root), ClientKind::StdioProxy)
                    .await
                    .unwrap(),
            );
            wait_for_shard_published(&daemon, root).await;
        }
        let third = daemon
            .handler_for_client(&request_for(&roots[2]), ClientKind::StdioProxy)
            .await
            .unwrap();
        assert_eq!(retained_shard_count(&third.retained_shards), 0);
        let status = extract_tool_json(
            third
                .handler
                .handle(
                    "tools/call",
                    serde_json::json!({"name": "status", "arguments": {"scope": "index"}}),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(status["status"], "deferred");
        let summary = status["summary"].as_str().unwrap();
        assert!(
            summary.contains("shard ceiling max_loaded_shards=2"),
            "{summary}"
        );
        assert!(
            summary.contains("Raise or remove max_loaded_shards"),
            "{summary}"
        );
        assert_eq!(
            daemon.index_state_for(&request_for(&roots[2])).await,
            crate::hook_enforcement::IndexState::Deferred
        );
        assert_eq!(daemon.report().await["deferred"], "ceiling");

        // Unlike the old fixed count, a ceiling does not let a silent session
        // hold a slot against a workspace that needs it.
        age(&daemon.shards.lock().await[&shard_key(&roots[1])], 3_600);
        assert_eq!(
            daemon.index_state_for(&request_for(&roots[2])).await,
            crate::hook_enforcement::IndexState::NotLoaded
        );
        third
            .handler
            .handle(
                "tools/call",
                serde_json::json!({"name": "status", "arguments": {"scope": "index"}}),
            )
            .await
            .unwrap();
        let keys = loaded_keys(&daemon).await;
        assert_eq!(keys.len(), 2, "the ceiling still holds");
        assert!(keys.contains(&shard_key(&roots[2])));
        assert!(!keys.contains(&shard_key(&roots[1])));
        drop(leases);
        drop(third);
        cleanup(&daemon, roots).await;
    }

    #[tokio::test]
    async fn a_shard_with_a_request_in_flight_is_never_chosen() {
        let (daemon, _) = daemon_with_memory(Some(1), 8 * 1024);
        let roots = capacity_roots("in-flight", 2);
        let lease = daemon
            .handler_for_client(&request_for(&roots[0]), ClientKind::StdioProxy)
            .await
            .unwrap();
        wait_for_shard_published(&daemon, &roots[0]).await;
        let shard = Arc::clone(&daemon.shards.lock().await[&shard_key(&roots[0])]);
        age(&shard, 3_600);
        assert!(shard.is_idle_connected(now_epoch_secs(), 600));

        let in_flight = lease.begin_work();
        assert_eq!(in_flight.len(), 1);
        assert!(!shard.is_idle_connected(now_epoch_secs(), 600));
        assert!(!shard.try_retire(), "a request in flight wins the race");
        assert!(!shard.is_retired());
        let error = match daemon
            .shard_for(roots[1].clone(), Vec::new(), Vec::new(), false)
            .await
        {
            Ok(_) => panic!("a shard answering a request must not be unloaded"),
            Err(error) => error,
        };
        assert!(is_shard_capacity_error(&error));

        drop(in_flight);
        assert!(shard.try_retire());
        // A request that arrives after retirement takes no guard on the old
        // runtime, so its shutdown cannot wait on it.
        assert!(lease.begin_work().is_empty());
        shard.retired.store(false, Ordering::SeqCst);
        drop(lease);
        cleanup(&daemon, roots).await;
    }

    #[tokio::test]
    async fn high_memory_with_nothing_loaded_does_not_refuse_the_first_workspace() {
        // Whatever is using the memory, refusing this workspace cannot free it.
        let (daemon, used) = daemon_with_memory(None, 2 * 1024);
        used.store(3 * 1024 * TEST_MIB, Ordering::Release);
        let roots = capacity_roots("nothing-loaded", 1);
        let lease = daemon
            .handler_for_client(&request_for(&roots[0]), ClientKind::StdioProxy)
            .await
            .unwrap();
        assert_eq!(retained_shard_count(&lease.retained_shards), 1);
        drop(lease);
        cleanup(&daemon, roots).await;
    }

    #[tokio::test]
    async fn a_workspace_starved_of_a_shard_slot_reports_deferred_to_the_hook_gate() {
        use crate::hook_enforcement::IndexState;
        let daemon = Arc::new(GlobalDaemon::new_with_config(1, false));
        let resident = unique_test_root("index-state-resident");
        let starved = unique_test_root("index-state-starved");
        for root in [&resident, &starved] {
            std::fs::create_dir_all(root).unwrap();
        }
        let request = |root: &Path| ProxyRequest {
            workspace_roots: vec![root.to_string_lossy().to_string()],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };
        // Nothing loaded and a free slot: the first Lattice call will load it.
        assert_eq!(
            daemon.index_state_for(&request(&starved)).await,
            IndexState::NotLoaded
        );

        // One slot, held by a workspace that is still bootstrapping and so is
        // not evictable. The probe must not admit, retain or evict anything.
        let resident_root = resident.canonicalize().unwrap();
        let reservation = daemon
            .resource_budget
            .try_reserve("test_view", 1)
            .expect("reserve test view");
        daemon.shards.lock().await.insert(
            shard_key(&resident_root),
            Arc::new(ShardEntry::pending(
                resident_root,
                Arc::clone(&daemon.index_work),
                reservation,
            )),
        );
        assert_eq!(
            daemon.index_state_for(&request(&resident)).await,
            IndexState::Indexing
        );
        assert_eq!(
            daemon.index_state_for(&request(&starved)).await,
            IndexState::Deferred
        );
        assert_eq!(daemon.shards.lock().await.len(), 1);
        // More than one root is a merged view, which has no single index.
        let mut view = request(&resident);
        view.workspace_roots
            .push(starved.to_string_lossy().to_string());
        assert_eq!(daemon.index_state_for(&view).await, IndexState::NotLoaded);

        daemon.shards.lock().await.clear();
        for root in [resident, starved] {
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn status_answers_carry_capacity_memory_the_ceiling_and_its_source() {
        const MIB: u64 = 1024 * 1024;
        let mut daemon = GlobalDaemon::new_with_config(6, false);
        daemon.footprint = Arc::new(|| Some(3072 * MIB));
        daemon.memory_budget_bytes = 5461 * MIB;
        daemon.settings = crate::daemon_settings::DaemonSettings {
            max_loaded_shards: Some(6),
            max_loaded_shards_source: crate::daemon_settings::SettingSource::SettingsFile(
                PathBuf::from("/home/op/.config/lattice/daemon.toml"),
            ),
            memory_budget_bytes: 5461 * MIB,
            memory_budget_source: crate::daemon_settings::SettingSource::Default,
            settings_file: Some(PathBuf::from("/home/op/.config/lattice/daemon.toml")),
            settings_file_present: true,
        };
        let report = daemon.report().await;
        assert_eq!(report["max_loaded_shards"], 6);
        assert_eq!(
            report["max_loaded_shards_source"],
            "settings file /home/op/.config/lattice/daemon.toml"
        );
        assert_eq!(report["loaded_shards"], 0);
        assert_eq!(report["connected_workspaces"], 0);
        assert_eq!(report["agents"], 0);
        assert_eq!(report["memory_footprint_bytes"], 3072 * MIB);
        assert_eq!(report["memory_budget_bytes"], 5461 * MIB);
        assert!(report["deferred"].is_null());
        assert!(report["uptime_secs"].is_u64());
        assert_eq!(report["connected_idle_threshold_secs"], 600);

        // The MCP tool envelope: JSON inside a text block.
        let mut tool = serde_json::json!({"content": [{"type": "text",
            "text": "{\"status\":\"ready\",\"files\":12}"}]});
        attach_daemon_report(&mut tool, report.clone());
        let inner: Value =
            serde_json::from_str(tool["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(inner["files"], 12);
        assert_eq!(inner["daemon"]["max_loaded_shards"], 6);
        assert_eq!(
            inner["shard_capacity"],
            "0 connected workspace(s) with 0 agent(s); 0 shard(s) loaded, 0 idle, 0 connected but idle and unloadable; memory 3072 MiB of 5461 MiB budget (built-in default) after 0 h up; shard ceiling 6 from settings file /home/op/.config/lattice/daemon.toml"
        );
        // The bare `lattice/status` payload.
        let mut bare = serde_json::json!({"status": "deferred"});
        attach_daemon_report(&mut bare, report.clone());
        assert_eq!(bare["daemon"]["loaded_shards"], 0);
        // Anything else is left exactly as it was.
        for untouched in [
            serde_json::json!({"content": [{"type": "text", "text": "## markdown status"}]}),
            serde_json::json!({"content": [{"type": "text", "text": "[1,2]"}]}),
            serde_json::json!({"tools": []}),
        ] {
            let mut value = untouched.clone();
            attach_daemon_report(&mut value, report.clone());
            assert_eq!(value, untouched);
        }

        let call = |name: &str, arguments: Value| serde_json::json!({"name": name, "arguments": arguments});
        assert!(is_index_status_request("lattice/status", &Value::Null));
        assert!(is_index_status_request(
            "tools/call",
            &call("status", serde_json::json!({}))
        ));
        assert!(is_index_status_request(
            "tools/call",
            &call("status", serde_json::json!({"scope": "index"}))
        ));
        for (method, params) in [
            (
                "tools/call",
                call("status", serde_json::json!({"scope": "docs"})),
            ),
            (
                "tools/call",
                call("status", serde_json::json!({"scope": "storage"})),
            ),
            ("tools/call", call("context", serde_json::json!({}))),
            ("tools/list", Value::Null),
        ] {
            assert!(!is_index_status_request(method, &params));
        }
    }

    #[test]
    fn workflow_steps_are_read_from_the_tool_name_and_status_scope_only() {
        use crate::hook_workflow_state::WorkflowStep;
        let call = |name: &str, arguments: Value| serde_json::json!({"name": name, "arguments": arguments});
        assert_eq!(
            served_workflow_step(
                "tools/call",
                &call(
                    "prepare_change",
                    serde_json::json!({"task": "private task text"})
                )
            ),
            Some(WorkflowStep::PrepareChange)
        );
        assert_eq!(
            served_workflow_step(
                "tools/call",
                &call("status", serde_json::json!({"scope": "docs"}))
            ),
            Some(WorkflowStep::StaleDocs)
        );
        assert_eq!(
            served_workflow_step("tools/call", &call("status", serde_json::json!({}))),
            None
        );
        assert_eq!(
            served_workflow_step("lattice/status", &call("prepare_change", Value::Null)),
            None
        );
        assert!(tool_result_is_error(&serde_json::json!({"isError": true})));
        assert!(!tool_result_is_error(&serde_json::json!({"content": []})));
    }

    #[test]
    fn poisoned_ownership_mutex_preserves_value_for_shutdown() {
        let owned = Arc::new(StdMutex::new(Some("runtime-or-bootstrap")));
        let poisoned = Arc::clone(&owned);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("poison ownership mutex");
        })
        .join();

        assert_eq!(lock_owned(&owned).take(), Some("runtime-or-bootstrap"));
    }

    #[tokio::test]
    async fn closed_admission_rejects_late_shards_and_drain_joins_bootstrap() {
        let daemon = Arc::new(GlobalDaemon::new_with_config(2, false));
        let root = unique_test_root("shutdown-admission");
        let reservation = daemon
            .resource_budget
            .try_reserve("test_view", 1)
            .expect("reserve test view");
        let entry = Arc::new(ShardEntry::pending(
            root.clone(),
            Arc::clone(&daemon.index_work),
            reservation,
        ));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        *lock_owned(&entry.bootstrap) = Some(tokio::spawn(async move {
            let _ = release_rx.await;
        }));
        daemon.shards.lock().await.insert(shard_key(&root), entry);

        close_shard_admission(&daemon).await;
        let cleanup_daemon = Arc::clone(&daemon);
        let cleanup = tokio::spawn(async move {
            shutdown_daemon_shards(&cleanup_daemon).await;
        });
        tokio::task::yield_now().await;
        assert!(
            !cleanup.is_finished(),
            "drain must join constructor ownership"
        );
        let late_error = daemon
            .shard_for(root.clone(), Vec::new(), Vec::new(), false)
            .await
            .err()
            .expect("closed admission rejects late shard");
        assert!(late_error.to_string().contains("shutting down"));

        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), cleanup)
            .await
            .expect("shutdown completed")
            .expect("cleanup task joined");
        assert!(daemon.shards.lock().await.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn authenticated_hook_open_does_not_load_a_workspace_shard() {
        let root = committed_test_repository("hook-open-no-shard");
        let state = unique_test_root("hook-open-state");
        let route = HookSessionRoute::open_at(&state).expect("open test hook route");
        let daemon =
            Arc::new(GlobalDaemon::new_with_config(8, false).with_hook_session_route(route));
        let proxy_request = ProxyRequest {
            workspace_roots: vec![root.to_string_lossy().to_string()],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };
        let (mut reader, mut writer, server) = authenticated_test_connection_kind(
            Arc::clone(&daemon),
            &proxy_request,
            ClientKind::HookAdapter,
        )
        .await;

        assert!(daemon.shards.lock().await.is_empty());
        let params = serde_json::json!({
            "integration": "codex/v1",
            "host_session_id": "host-session-1",
        });
        let mut unknown = params.clone();
        unknown["client_name"] = serde_json::json!("trusted-hook");
        write_json_rpc_request(&mut writer, 0, HOOK_SESSION_OPEN_METHOD, unknown).await;
        let rejected_label = read_json_line(&mut reader).await;
        assert_eq!(rejected_label["error"]["code"], -32602);
        assert_eq!(
            rejected_label["error"]["message"],
            "hook request is invalid"
        );
        assert!(daemon.shards.lock().await.is_empty());

        write_json_rpc_request(&mut writer, 1, HOOK_SESSION_OPEN_METHOD, params.clone()).await;
        let opened = read_json_line(&mut reader).await;
        assert!(opened.get("error").is_none(), "open failed: {opened}");
        assert_eq!(opened["result"]["resumed"], false);
        assert!(daemon.shards.lock().await.is_empty());

        write_json_rpc_request(&mut writer, 2, HOOK_SESSION_OPEN_METHOD, params.clone()).await;
        let exclusive = read_json_line(&mut reader).await;
        assert_eq!(exclusive["error"]["code"], -32001);
        assert_eq!(exclusive["error"]["message"], "hook request rejected");

        let mut resumed_params = params;
        resumed_params["resume"] = serde_json::json!({
            "binding_id": opened["result"]["binding_id"],
            "capability": opened["result"]["capability"],
        });
        let mut wrong_tuple = resumed_params.clone();
        wrong_tuple["host_session_id"] = serde_json::json!("different-host-session");
        write_json_rpc_request(&mut writer, 3, HOOK_SESSION_OPEN_METHOD, wrong_tuple).await;
        let rejected_tuple = read_json_line(&mut reader).await;
        assert_eq!(rejected_tuple["error"]["code"], -32001);

        let mut wrong_capability = resumed_params.clone();
        wrong_capability["resume"]["capability"] = serde_json::json!("00".repeat(32));
        write_json_rpc_request(&mut writer, 3, HOOK_SESSION_OPEN_METHOD, wrong_capability).await;
        let rejected_capability = read_json_line(&mut reader).await;
        assert_eq!(rejected_capability["error"]["code"], -32001);

        tokio::time::sleep(Duration::from_millis(2)).await;
        write_json_rpc_request(&mut writer, 3, HOOK_SESSION_OPEN_METHOD, resumed_params).await;
        let resumed = read_json_line(&mut reader).await;
        assert_eq!(resumed["result"]["resumed"], true);
        assert_eq!(
            resumed["result"]["binding_id"],
            opened["result"]["binding_id"]
        );
        assert!(
            resumed["result"]["idle_deadline_ms"].as_i64()
                > opened["result"]["idle_deadline_ms"].as_i64()
        );
        assert!(daemon.shards.lock().await.is_empty());

        drop(writer);
        drop(reader);
        server.await.unwrap().unwrap();
        drop(daemon);
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(state).unwrap();
    }

    #[tokio::test]
    async fn authenticated_hook_open_rejects_multi_root_without_loading_shards() {
        let root = committed_test_repository("hook-open-multi-root");
        let other = unique_test_root("hook-open-other-root");
        let state = unique_test_root("hook-open-multi-state");
        std::fs::create_dir_all(&other).unwrap();
        let route = HookSessionRoute::open_at(&state).expect("open test hook route");
        let daemon =
            Arc::new(GlobalDaemon::new_with_config(8, false).with_hook_session_route(route));
        let proxy_request = ProxyRequest {
            workspace_roots: vec![
                root.to_string_lossy().to_string(),
                other.to_string_lossy().to_string(),
            ],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };
        let (mut reader, mut writer, server) = authenticated_test_connection_kind(
            Arc::clone(&daemon),
            &proxy_request,
            ClientKind::HookAdapter,
        )
        .await;
        write_json_rpc_request(
            &mut writer,
            1,
            HOOK_SESSION_OPEN_METHOD,
            serde_json::json!({
                "integration": "codex/v1",
                "host_session_id": "host-session-1",
            }),
        )
        .await;
        let rejected = read_json_line(&mut reader).await;
        assert_eq!(rejected["error"]["code"], -32001);
        assert_eq!(rejected["error"]["message"], "hook request rejected");
        assert!(daemon.shards.lock().await.is_empty());

        drop(writer);
        drop(reader);
        server.await.unwrap().unwrap();
        drop(daemon);
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(other).unwrap();
        std::fs::remove_dir_all(state).unwrap();
    }

    #[tokio::test]
    async fn non_hook_clients_cannot_call_hook_routes() {
        let root = committed_test_repository("hook-client-kind");
        let state = unique_test_root("hook-client-kind-state");
        let route = HookSessionRoute::open_at(&state).expect("open test hook route");
        let daemon =
            Arc::new(GlobalDaemon::new_with_config(8, false).with_hook_session_route(route));
        let proxy_request = ProxyRequest {
            workspace_roots: vec![root.to_string_lossy().to_string()],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };
        for kind in [ClientKind::StdioProxy, ClientKind::Cli, ClientKind::Doctor] {
            let (mut reader, mut writer, server) =
                authenticated_test_connection_kind(Arc::clone(&daemon), &proxy_request, kind).await;
            write_json_rpc_request(
                &mut writer,
                1,
                HOOK_SESSION_OPEN_METHOD,
                serde_json::json!({
                    "integration": "codex/v1",
                    "host_session_id": "not-authorized",
                }),
            )
            .await;
            let rejected = read_json_line(&mut reader).await;
            assert_eq!(rejected["error"]["code"], -32601);
            assert_eq!(rejected["error"]["message"], "Method not found");
            drop(writer);
            drop(reader);
            server.await.unwrap().unwrap();
        }
        assert!(daemon.shards.lock().await.is_empty());

        drop(daemon);
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(state).unwrap();
    }

    #[tokio::test]
    async fn authenticated_ordinary_rpc_loads_its_shard_only_after_first_request() {
        let root = unique_test_root("ordinary-lazy-shard");
        std::fs::create_dir_all(&root).expect("create ordinary workspace");
        let daemon = Arc::new(GlobalDaemon::new_with_config(8, false));
        let proxy_request = ProxyRequest {
            workspace_roots: vec![root.to_string_lossy().to_string()],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };
        let (mut reader, mut writer, server) =
            authenticated_test_connection(Arc::clone(&daemon), &proxy_request).await;

        assert!(daemon.shards.lock().await.is_empty());
        write_json_rpc_request(&mut writer, 1, "initialize", serde_json::json!({})).await;
        let initialized = read_json_line(&mut reader).await;
        assert!(
            initialized.get("error").is_none(),
            "initialize failed: {initialized}"
        );
        assert_eq!(daemon.shards.lock().await.len(), 1);

        drop(writer);
        drop(reader);
        server.await.unwrap().unwrap();
        shutdown_all_shards(&daemon).await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn concurrent_cold_workspace_statuses_are_bounded_and_never_claim_an_empty_graph() {
        let roots = (0..4)
            .map(|index| {
                let root = unique_test_root(&format!("cold-status-{index}"));
                std::fs::create_dir_all(&root).expect("create cold workspace");
                root.canonicalize().unwrap_or(root)
            })
            .collect::<Vec<_>>();
        // Hold the only index-work permit so every bootstrap remains cold for
        // the duration of the concurrent requests. Two roots are admitted as
        // pending shards; the other two exercise the capacity-deferred path.
        let daemon = Arc::new(GlobalDaemon::new_with_config(2, false));
        let bootstrap_gate = daemon
            .index_work
            .acquire("cold-status-test-gate", "test")
            .await
            .expect("test index permit");

        let mut calls = tokio::task::JoinSet::new();
        for root in roots.iter().cloned() {
            let daemon = Arc::clone(&daemon);
            calls.spawn(async move {
                let request = ProxyRequest {
                    workspace_roots: vec![root.to_string_lossy().to_string()],
                    focus_files: Vec::new(),
                    focus_dirs: Vec::new(),
                };
                let lease =
                    tokio::time::timeout(Duration::from_millis(250), daemon.handler_for(&request))
                        .await
                        .expect("cold shard admission must not wait for bootstrap")
                        .expect("cold shard admission must not fail at capacity");

                let initialized = tokio::time::timeout(
                    Duration::from_millis(250),
                    lease.handler.handle("initialize", serde_json::json!({})),
                )
                .await
                .expect("initialize must remain bounded")
                .expect("initialize must preserve the MCP contract");
                assert_eq!(
                    initialized,
                    McpHandler::protocol_initialize_response(),
                    "cold startup must use the normal initialize response"
                );

                let tools = tokio::time::timeout(
                    Duration::from_millis(250),
                    lease.handler.handle("tools/list", serde_json::json!({})),
                )
                .await
                .expect("tools/list must remain bounded")
                .expect("tools/list must preserve the MCP contract");
                assert_eq!(
                    tools,
                    McpHandler::agent_tools_list_response(),
                    "cold startup must expose the normal public tool contract"
                );

                let status = tokio::time::timeout(
                    Duration::from_millis(250),
                    lease.handler.handle(
                        "tools/call",
                        serde_json::json!({"name": "status", "arguments": {"scope": "index"}}),
                    ),
                )
                .await
                .expect("cold status must remain bounded")
                .expect("cold status must not become a runtime-unavailable error");
                extract_tool_json(status).expect("status must use the MCP tool-result envelope")
            });
        }

        let mut responses = Vec::new();
        while let Some(result) = calls.join_next().await {
            responses.push(result.expect("cold status task must not panic"));
        }
        assert_eq!(responses.len(), roots.len());
        let mut deferred = 0;
        for status in responses {
            // A workspace that was never admitted must not claim to be
            // indexing: nothing is indexing it.
            if status["bootstrap"]["state"].as_str() == Some("deferred") {
                deferred += 1;
                assert_eq!(status["status"].as_str(), Some("deferred"));
                assert_eq!(status["indexing"].as_bool(), Some(false));
            } else {
                assert_eq!(status["status"].as_str(), Some("indexing"));
                assert_eq!(status["indexing"].as_bool(), Some(true));
            }
            assert_eq!(status["graph_snapshot_state"].as_str(), Some("not_loaded"));
            assert!(
                status["nodes"].is_null(),
                "cold status must not claim zero nodes"
            );
            assert!(
                status["edges"].is_null(),
                "cold status must not claim zero edges"
            );
            assert!(
                status["files"].is_null(),
                "cold status must not claim zero files"
            );
        }
        assert!(
            deferred > 0,
            "this fixture opens more workspaces than shard slots, so some must be deferred"
        );

        drop(bootstrap_gate);
        shutdown_all_shards(&daemon).await;
        for root in roots {
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn overlapping_workspace_views_reuse_primary_shard_without_warming_the_full_view() {
        let root_a = unique_test_root("lattice-shard-a");
        let root_b = unique_test_root("lattice-shard-b");
        std::fs::create_dir_all(&root_a).expect("create root a");
        std::fs::create_dir_all(&root_b).expect("create root b");

        let daemon = Arc::new(GlobalDaemon::new_with_config(8, false));
        let hello_a = ProxyRequest {
            workspace_roots: vec![root_a.to_string_lossy().to_string()],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };
        let hello_ab = ProxyRequest {
            workspace_roots: vec![
                root_a.to_string_lossy().to_string(),
                root_b.to_string_lossy().to_string(),
            ],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };

        let lease_a = daemon
            .handler_for(&hello_a)
            .await
            .expect("single shard lease");
        let lease_ab = daemon
            .handler_for(&hello_ab)
            .await
            .expect("overlapping logical view lease");

        {
            let shards = daemon.shards.lock().await;
            assert_eq!(
                shards.len(),
                1,
                "combined workspace requests must not eagerly warm every logical-view shard"
            );
            assert!(
                shards.contains_key(&shard_key(&root_a.canonicalize().unwrap_or(root_a.clone()))),
                "root a shard should be reused instead of replaced"
            );
            assert!(
                !shards.contains_key(&shard_key(&root_b.canonicalize().unwrap_or(root_b.clone()))),
                "root b should remain a logical-view root until a tool needs that shard"
            );
        }

        drop(lease_ab);
        drop(lease_a);
        shutdown_all_shards(&daemon).await;
        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    #[tokio::test]
    async fn combined_workspace_view_prewarms_remaining_shards_in_background() {
        let root_a = unique_test_root("lattice-shard-prewarm-a");
        let root_b = unique_test_root("lattice-shard-prewarm-b");
        std::fs::create_dir_all(&root_a).expect("create root a");
        std::fs::create_dir_all(&root_b).expect("create root b");

        let daemon = Arc::new(GlobalDaemon::new_with_config(8, true));
        let hello_ab = ProxyRequest {
            workspace_roots: vec![
                root_a.to_string_lossy().to_string(),
                root_b.to_string_lossy().to_string(),
            ],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };

        let lease = daemon
            .handler_for(&hello_ab)
            .await
            .expect("combined logical view lease");

        let root_b_key = shard_key(&root_b.canonicalize().unwrap_or(root_b.clone()));
        let mut warmed = false;
        for _ in 0..50 {
            {
                let shards = daemon.shards.lock().await;
                if shards.contains_key(&root_b_key) {
                    warmed = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(
            warmed,
            "combined workspace views should prewarm non-primary shards without blocking the initial lease"
        );
        {
            let shards = daemon.shards.lock().await;
            let root_b_entry = shards
                .get(&root_b_key)
                .expect("prewarmed secondary shard should be loaded");
            assert_eq!(
                root_b_entry.active_connections.load(Ordering::Acquire),
                0,
                "prewarmed secondary shards must stay evictable when the logical view is idle"
            );
        }

        drop(lease);
        {
            let shards = daemon.shards.lock().await;
            let root_b_entry = shards
                .get(&root_b_key)
                .expect("prewarmed secondary shard should remain loaded until shutdown");
            assert_eq!(
                root_b_entry.active_connections.load(Ordering::Acquire),
                0,
                "dropping the logical-view lease must leave secondary shards evictable"
            );
        }
        shutdown_all_shards(&daemon).await;
        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    #[tokio::test]
    async fn released_shard_is_not_evictable_until_bootstrap_publishes_its_runtime() {
        let root = unique_test_root("lattice-bootstrap-evictable");
        std::fs::create_dir_all(&root).expect("create bootstrap root");
        let root = root.canonicalize().unwrap_or(root);
        let daemon = Arc::new(GlobalDaemon::new_with_config(2, false));

        let lease = daemon
            .handler_for(&ProxyRequest {
                workspace_roots: vec![root.to_string_lossy().to_string()],
                focus_files: Vec::new(),
                focus_dirs: Vec::new(),
            })
            .await
            .expect("load shard");
        drop(lease);

        {
            let shards = daemon.shards.lock().await;
            let entry = shards.get(&shard_key(&root)).expect("shard is resident");
            assert_eq!(
                entry.active_connections.load(Ordering::Acquire),
                0,
                "dropping the lease must release the shard's request retention"
            );
            assert!(
                entry.is_bootstrapping(),
                "a just-leased shard is still bootstrapping its runtime"
            );
            assert!(
                !entry.is_evictable(),
                "a shard whose runtime bootstrap is still in flight must not be evictable, \
                 even though no index work has been enqueued yet"
            );
        }

        wait_for_shard_evictable(&daemon, &root).await;
        {
            let shards = daemon.shards.lock().await;
            let entry = shards.get(&shard_key(&root)).expect("shard is resident");
            assert!(
                !entry.is_bootstrapping()
                    && !daemon.index_work.workspace_is_busy(&shard_key(&root)),
                "an evictable shard has finished both bootstrap and index work"
            );
            assert!(entry.is_evictable());
            assert!(
                entry.is_idle(now_epoch_secs() + 10_000, 1),
                "an evictable shard past its idle TTL is also idle-sweepable"
            );
        }

        shutdown_all_shards(&daemon).await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn shard_capacity_evicts_inactive_runtime_before_loading_another() {
        let roots = (0..3)
            .map(|index| {
                let root = unique_test_root(&format!("lattice-capacity-{index}"));
                std::fs::create_dir_all(&root).expect("create capacity root");
                root.canonicalize().unwrap_or(root)
            })
            .collect::<Vec<_>>();
        let daemon = Arc::new(GlobalDaemon::new_with_config(2, false));

        for root in &roots {
            let lease = daemon
                .handler_for(&ProxyRequest {
                    workspace_roots: vec![root.to_string_lossy().to_string()],
                    focus_files: Vec::new(),
                    focus_dirs: Vec::new(),
                })
                .await
                .expect("load shard within bounded capacity");
            drop(lease);
            wait_for_shard_evictable(&daemon, root).await;
        }

        let shards = daemon.shards.lock().await;
        assert_eq!(shards.len(), 2);
        assert!(shards.contains_key(&shard_key(&roots[2])));
        assert!(
            roots[..2]
                .iter()
                .any(|root| !shards.contains_key(&shard_key(root))),
            "one inactive shard should be evicted instead of exceeding capacity"
        );
        drop(shards);
        shutdown_all_shards(&daemon).await;
        for root in roots {
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn active_view_byte_limit_returns_explicit_resource_limited_admission() {
        let budget = Arc::new(crate::resource_budget::ResourceBudget::new(10));
        let mut daemon = GlobalDaemon::new_with_config(20, false);
        daemon.resource_budget = Arc::clone(&budget);
        daemon.index_work = crate::index_work::IndexWorkCoordinator::with_resource_budget(
            1,
            Arc::clone(&budget),
            1,
        );
        daemon.view_reservation_bytes = 6;
        let daemon = Arc::new(daemon);
        let first_root = unique_test_root("lattice-byte-admission-a");
        let second_root = unique_test_root("lattice-byte-admission-b");
        std::fs::create_dir_all(&first_root).unwrap();
        std::fs::create_dir_all(&second_root).unwrap();
        let first = daemon
            .shard_for(first_root, Vec::new(), Vec::new(), true)
            .await
            .unwrap();
        let error = match daemon
            .shard_for(second_root, Vec::new(), Vec::new(), true)
            .await
        {
            Ok(_) => panic!("second active view exceeded its byte allowance"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("resource-limited:"));
        assert_eq!(budget.snapshot().reserved_bytes, 6);
        first.release();
        first.shutdown().await;
    }

    #[tokio::test]
    async fn shard_lookup_retention_prevents_capacity_eviction_before_request_use() {
        let root_a = unique_test_root("lattice-retained-capacity-a");
        let root_b = unique_test_root("lattice-retained-capacity-b");
        std::fs::create_dir_all(&root_a).expect("create retained root a");
        std::fs::create_dir_all(&root_b).expect("create retained root b");
        let root_a = root_a.canonicalize().unwrap_or(root_a);
        let root_b = root_b.canonicalize().unwrap_or(root_b);
        let daemon = Arc::new(GlobalDaemon::new_with_config(1, false));

        let retained = RetainedShard::from_retained(
            daemon
                .shard_for(root_a.clone(), Vec::new(), Vec::new(), true)
                .await
                .expect("load and retain first shard"),
        );
        let error = match daemon
            .shard_for(root_b.clone(), Vec::new(), Vec::new(), false)
            .await
        {
            Ok(_) => panic!("capacity must not evict a shard retained by a pending request"),
            Err(error) => error,
        };
        assert!(is_shard_capacity_error(&error));
        assert_eq!(
            error
                .downcast_ref::<ShardDeferred>()
                .map(|deferred| &deferred.kind),
            Some(&DeferralKind::Ceiling)
        );
        assert!(
            error
                .to_string()
                .contains("shard ceiling max_loaded_shards=1"),
            "{error}"
        );

        drop(retained);
        wait_for_shard_evictable(&daemon, &root_a).await;
        daemon
            .shard_for(root_b.clone(), Vec::new(), Vec::new(), false)
            .await
            .expect("released shard should become capacity-evictable");

        shutdown_all_shards(&daemon).await;
        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    #[test]
    fn workspace_key_is_a_logical_view_over_shard_keys() {
        let roots = vec![PathBuf::from("/repo/a"), PathBuf::from("/repo/b")];
        assert_eq!(workspace_key(&roots), "/repo/a\n/repo/b");
        assert_eq!(shard_key(&roots[0]), "/repo/a");
        assert_eq!(shard_key(&roots[1]), "/repo/b");
    }

    #[test]
    fn explicit_path_values_reads_scalar_and_array_tool_arguments() {
        let args = serde_json::json!({
            "file": "/repo/a/src/lib.rs",
            "entry_files": ["/repo/b/src/main.rs", "relative/path.rs"],
            "query": "auth",
            "task_statement": "Check /repo/c/docs/audit/current-state.md for the remediation state."
        });

        let paths = explicit_path_values(&args);

        assert_eq!(
            paths,
            vec![
                "/repo/a/src/lib.rs",
                "/repo/b/src/main.rs",
                "relative/path.rs",
                "/repo/c/docs/audit/current-state.md",
            ]
        );
    }

    #[test]
    fn unmatched_absolute_paths_do_not_route_to_non_primary_shards() {
        let root_a = PathBuf::from("/repo/a");
        let root_b = PathBuf::from("/repo/b");
        let handler = ViewRequestHandler {
            daemon: Arc::new(GlobalDaemon::new_with_config(8, false)),
            roots: vec![root_a.clone(), root_b],
            primary_root: root_a,
            primary_handler: Arc::new(NoopRequestHandler),
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
            handle_owners: Mutex::new(HashMap::new()),
        };
        let params = serde_json::json!({
            "name": "get_skeleton",
            "arguments": {
                "file": "/outside/repo/src/lib.rs"
            }
        });

        assert!(
            handler.route_root_for_tool_call(&params).is_none(),
            "absolute paths outside all logical-view shards intentionally fall back to primary handling"
        );
    }

    #[test]
    fn embedded_task_statement_paths_route_memory_calls_to_matching_shard() {
        let root_a = PathBuf::from("/repo/a");
        let root_b = PathBuf::from("/repo/b");
        let handler = ViewRequestHandler {
            daemon: Arc::new(GlobalDaemon::new_with_config(8, false)),
            roots: vec![root_a.clone(), root_b.clone()],
            primary_root: root_a,
            primary_handler: Arc::new(NoopRequestHandler),
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
            handle_owners: Mutex::new(HashMap::new()),
        };
        let params = serde_json::json!({
            "name": "get_task_memory",
            "arguments": {
                "task_id": "task-1",
                "task_statement": "Review /repo/b/docs/audit/2026-05-17-remediation-run before recalling memories."
            }
        });

        assert_eq!(handler.route_root_for_tool_call(&params), Some(root_b));
    }

    #[test]
    fn relative_file_paths_route_to_unique_matching_shard() {
        let root_a = unique_test_root("lattice-route-relative-a");
        let root_b = unique_test_root("lattice-route-relative-b");
        std::fs::create_dir_all(root_b.join("docs/audit")).expect("create shard b docs");
        std::fs::write(
            root_b.join("docs/audit/remediation.md"),
            "IU-0031 PX-0040 governance contract",
        )
        .expect("write shard b file");

        let handler = ViewRequestHandler {
            daemon: Arc::new(GlobalDaemon::new_with_config(8, false)),
            roots: vec![root_a.clone(), root_b.clone()],
            primary_root: root_a.clone(),
            primary_handler: Arc::new(NoopRequestHandler),
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
            handle_owners: Mutex::new(HashMap::new()),
        };
        let params = serde_json::json!({
            "name": "record_workflow_outcome",
            "arguments": {
                "task": "Portal remediation IU-0031 PX-0040",
                "files": ["docs/audit/remediation.md"]
            }
        });

        assert_eq!(
            handler.route_root_for_tool_call(&params),
            Some(root_b.clone())
        );

        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    #[test]
    fn save_memory_linked_files_route_to_unique_matching_shard() {
        let root_a = unique_test_root("lattice-route-save-memory-a");
        let root_b = unique_test_root("lattice-route-save-memory-b");
        std::fs::create_dir_all(root_b.join("docs/audit")).expect("create shard b docs");
        std::fs::write(
            root_b.join("docs/audit/meridian-memory.md"),
            "Cadres Meridian memory evidence",
        )
        .expect("write shard b linked file");

        let handler = ViewRequestHandler {
            daemon: Arc::new(GlobalDaemon::new_with_config(8, false)),
            roots: vec![root_a.clone(), root_b.clone()],
            primary_root: root_a.clone(),
            primary_handler: Arc::new(NoopRequestHandler),
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
            handle_owners: Mutex::new(HashMap::new()),
        };
        let params = serde_json::json!({
            "name": "save_memory",
            "arguments": {
                "content": "Cadres Meridian workflow contract",
                "linked_files": ["docs/audit/meridian-memory.md"]
            }
        });

        assert_eq!(
            handler.route_root_for_tool_call(&params),
            Some(root_b.clone())
        );

        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    #[test]
    fn relative_file_paths_do_not_route_when_ambiguous_across_shards() {
        let root_a = unique_test_root("lattice-route-ambiguous-a");
        let root_b = unique_test_root("lattice-route-ambiguous-b");
        for root in [&root_a, &root_b] {
            std::fs::create_dir_all(root.join("docs/audit")).expect("create docs");
            std::fs::write(root.join("docs/audit/remediation.md"), "same relative file")
                .expect("write relative file");
        }

        let handler = ViewRequestHandler {
            daemon: Arc::new(GlobalDaemon::new_with_config(8, false)),
            roots: vec![root_a.clone(), root_b.clone()],
            primary_root: root_a.clone(),
            primary_handler: Arc::new(NoopRequestHandler),
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
            handle_owners: Mutex::new(HashMap::new()),
        };
        let params = serde_json::json!({
            "name": "record_workflow_outcome",
            "arguments": {
                "task": "remediation",
                "files": ["docs/audit/remediation.md"]
            }
        });

        assert!(
            handler.route_root_for_tool_call(&params).is_none(),
            "ambiguous relative paths must not silently pick a non-primary shard"
        );

        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    #[tokio::test]
    async fn ambiguous_memory_write_path_returns_error_instead_of_primary_fallback() {
        let root_a = unique_test_root("lattice-write-ambiguous-a");
        let root_b = unique_test_root("lattice-write-ambiguous-b");
        for root in [&root_a, &root_b] {
            std::fs::create_dir_all(root.join("docs/audit")).expect("create docs");
            std::fs::write(root.join("docs/audit/remediation.md"), "same relative file")
                .expect("write relative file");
        }

        let handler = ViewRequestHandler {
            daemon: Arc::new(GlobalDaemon::new_with_config(8, false)),
            roots: vec![root_a.clone(), root_b.clone()],
            primary_root: root_a.clone(),
            primary_handler: Arc::new(NoopRequestHandler),
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
            handle_owners: Mutex::new(HashMap::new()),
        };
        let params = serde_json::json!({
            "name": "save_memory",
            "arguments": {
                "content": "Ambiguous durable memory",
                "linked_files": ["docs/audit/remediation.md"]
            }
        });

        let error = handler
            .handle("tools/call", params)
            .await
            .expect_err("ambiguous memory write should fail");
        assert_eq!(error.0, -32602);
        assert!(error
            .1
            .contains("Ambiguous workspace for memory write path"));

        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    #[test]
    fn task_and_summary_paths_participate_in_shard_routing() {
        let root_a = PathBuf::from("/repo/a");
        let root_b = PathBuf::from("/repo/b");
        let handler = ViewRequestHandler {
            daemon: Arc::new(GlobalDaemon::new_with_config(8, false)),
            roots: vec![root_a.clone(), root_b.clone()],
            primary_root: root_a,
            primary_handler: Arc::new(NoopRequestHandler),
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
            handle_owners: Mutex::new(HashMap::new()),
        };
        let params = serde_json::json!({
            "name": "record_workflow_outcome",
            "arguments": {
                "task": "Record outcome for /repo/b/docs/audit/remediation.md",
                "summary": "Fixed /repo/b/src/governance_contract.py"
            }
        });

        assert_eq!(handler.route_root_for_tool_call(&params), Some(root_b));
    }

    #[test]
    fn extract_tool_json_reads_hybrid_structured_payload() {
        let wrapped = serde_json::json!({
            "content": [{
                "type": "text",
                "text": "### Summary\nready\n\n### Structured Payload\n```json\n{\"context_handle\":\"ctx-1\"}\n```"
            }]
        });

        let payload = extract_tool_json(wrapped).expect("extract json payload");

        assert_eq!(payload["context_handle"], "ctx-1");
    }

    #[test]
    fn merge_workflow_payloads_combines_ranked_results_and_handles() {
        let root_a = PathBuf::from("/repo/a");
        let root_b = PathBuf::from("/repo/b");
        let payload_a = serde_json::json!({
            "overview": "A",
            "ranked_pivots": [{"label": "a", "score": 0.4}],
            "relevant_context": [],
            "memory_highlights": [],
            "event_episodes": [],
            "risks": [],
            "verification_commands": [],
            "context_handle": "ctx-a",
            "stable_handles": ["ctx-a"]
        });
        let payload_b = serde_json::json!({
            "overview": "B",
            "ranked_pivots": [{"label": "b", "score": 0.9}],
            "relevant_context": [],
            "memory_highlights": [],
            "event_episodes": [],
            "risks": [],
            "verification_commands": [],
            "context_handle": "ctx-b",
            "stable_handles": ["ctx-b"]
        });

        let merged = merge_workflow_payloads(
            &root_a,
            2,
            vec![(root_a.clone(), payload_a), (root_b, payload_b)],
            vec![],
            vec![],
        );

        assert_eq!(merged["logical_view"], true);
        assert_eq!(merged["ranked_pivots"][0]["label"], "b");
        assert_eq!(merged["ranked_pivots"][0]["source_workspace"], "/repo/b");
        assert_eq!(
            merged["stable_handles"],
            serde_json::json!(["ctx-a", "ctx-b"])
        );
    }

    #[test]
    fn merge_workflow_payloads_separates_incomplete_and_failed_shards() {
        let root_a = PathBuf::from("/repo/a");
        let root_b = PathBuf::from("/repo/b");
        let root_c = PathBuf::from("/repo/c");
        let payload_a = serde_json::json!({
            "overview": "A",
            "dependencies": [{"s": "dep", "f": "src/dep.rs"}],
        });
        let incomplete = shard_incomplete_value(
            &root_b,
            &serde_json::json!({
                "indexing": true,
                "reason": "branch_switch",
                "overview": "refreshing"
            }),
        );
        let failed = serde_json::json!({
            "workspace": root_c.to_string_lossy(),
            "error": "boom",
        });

        let merged = merge_workflow_payloads(
            &root_a,
            3,
            vec![(root_a.clone(), payload_a)],
            vec![failed],
            vec![incomplete],
        );

        assert_eq!(merged["partial"], true);
        assert_eq!(merged["partial_failures"], true);
        assert_eq!(merged["failed_shards"][0]["workspace"], "/repo/c");
        assert_eq!(merged["incomplete_shards"][0]["workspace"], "/repo/b");
        assert_eq!(merged["incomplete_shards"][0]["reason"], "branch_switch");
        assert_eq!(merged["dependencies"][0]["source_workspace"], "/repo/a");
        assert_eq!(merged["count"], 1);
    }

    #[test]
    fn merge_search_memory_payloads_reranks_exact_matches_across_shards() {
        let merged = merge_search_memory_payloads(
            2,
            vec![
                (
                    PathBuf::from("/repo/rmm"),
                    serde_json::json!({
                        "query": "IU-0031 PX-0040 governance contract Portal remediation",
                        "count": 1,
                        "memories": [{
                            "id": "rmm-memory",
                            "content": "RMM governance notes",
                            "workspace_id": "/repo/rmm",
                            "created_at": 2
                        }],
                        "diagnostics": {
                            "query_exact_terms": ["iu-0031", "px-0040"],
                            "matched_exact_terms": [],
                            "durable_exact_term_counts": {"iu-0031": 0, "px-0040": 0},
                            "matches": [{
                                "memory_id": "rmm-memory",
                                "workspace": "/repo/rmm",
                                "matched_terms": ["governance"],
                                "score": 100
                            }]
                        }
                    }),
                ),
                (
                    PathBuf::from("/repo/portal"),
                    serde_json::json!({
                        "query": "IU-0031 PX-0040 governance contract Portal remediation",
                        "count": 1,
                        "memories": [{
                            "id": "portal-memory",
                            "content": "Portal IU-0031 PX-0040 remediation",
                            "workspace_id": "/repo/portal",
                            "created_at": 1
                        }],
                        "diagnostics": {
                            "query_exact_terms": ["iu-0031", "px-0040"],
                            "matched_exact_terms": ["iu-0031", "px-0040"],
                            "durable_exact_term_counts": {"iu-0031": 1, "px-0040": 1},
                            "matches": [{
                                "memory_id": "portal-memory",
                                "workspace": "/repo/portal",
                                "matched_terms": ["iu-0031", "px-0040"],
                                "score": 10000
                            }]
                        }
                    }),
                ),
            ],
            Vec::new(),
        );

        assert_eq!(merged["count"].as_u64(), Some(2));
        assert_eq!(merged["memories"][0]["id"].as_str(), Some("portal-memory"));
        assert_eq!(
            merged["memories"][0]["source_workspace"].as_str(),
            Some("/repo/portal")
        );
        assert_eq!(
            merged["diagnostics"]["matches"][0]["memory_id"].as_str(),
            Some("portal-memory")
        );
        assert_eq!(
            merged["diagnostics"]["unmatched_exact_terms"],
            serde_json::json!([])
        );
        assert_eq!(
            merged["diagnostics"]["exact_term_status"].as_str(),
            Some("matched")
        );
    }

    #[test]
    fn merge_search_memory_payloads_uses_context_to_break_exact_id_collisions() {
        let merged = merge_search_memory_payloads(
            2,
            vec![
                (
                    PathBuf::from("/repo/meridian"),
                    serde_json::json!({
                        "query": "PX-0033 product-profile launch claims Portal remediation",
                        "count": 1,
                        "memories": [{
                            "id": "meridian-memory",
                            "content": "Meridian PX-0033 launch remediation",
                            "workspace_id": "/repo/meridian",
                            "created_at": 10
                        }],
                        "diagnostics": {
                            "query_exact_terms": ["px-0033"],
                            "matched_exact_terms": ["px-0033"],
                            "durable_exact_term_counts": {"px-0033": 1},
                            "matches": [{
                                "memory_id": "meridian-memory",
                                "workspace": "/repo/meridian",
                                "matched_terms": ["px-0033"],
                                "score": 10000
                            }]
                        }
                    }),
                ),
                (
                    PathBuf::from("/repo/portal"),
                    serde_json::json!({
                        "query": "PX-0033 product-profile launch claims Portal remediation",
                        "count": 1,
                        "memories": [{
                            "id": "portal-memory",
                            "content": "Portal PX-0033 product-profile launch claims remediation",
                            "workspace_id": "/repo/portal",
                            "linked_files": ["docs/audit/product-profile.md"],
                            "created_at": 1
                        }],
                        "diagnostics": {
                            "query_exact_terms": ["px-0033"],
                            "matched_exact_terms": ["px-0033"],
                            "durable_exact_term_counts": {"px-0033": 1},
                            "matches": [{
                                "memory_id": "portal-memory",
                                "workspace": "/repo/portal",
                                "matched_terms": ["px-0033", "product", "profile", "portal"],
                                "score": 9000
                            }]
                        }
                    }),
                ),
            ],
            Vec::new(),
        );

        assert_eq!(merged["memories"][0]["id"].as_str(), Some("portal-memory"));
        assert_eq!(
            merged["diagnostics"]["matches"][0]["memory_id"].as_str(),
            Some("portal-memory")
        );
    }

    async fn shutdown_all_shards(daemon: &Arc<GlobalDaemon>) {
        let victims = {
            let mut shards = daemon.shards.lock().await;
            shards.drain().map(|(_, entry)| entry).collect::<Vec<_>>()
        };
        for shard in victims {
            shard.shutdown().await;
        }
    }

    fn unique_test_root(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nanos}-{}", std::process::id()))
    }

    fn committed_test_repository(prefix: &str) -> PathBuf {
        let root = unique_test_root(prefix);
        std::fs::create_dir_all(&root).expect("create Git fixture");
        run_git(&root, &["init"]);
        run_git(&root, &["config", "user.email", "lattice@example.test"]);
        run_git(&root, &["config", "user.name", "Lattice Test"]);
        std::fs::write(root.join("fixture.txt"), "fixture\n").expect("write Git fixture");
        run_git(&root, &["add", "fixture.txt"]);
        run_git(&root, &["commit", "-m", "fixture"]);
        root
    }

    fn run_git(root: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("run Git fixture command");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    async fn authenticated_test_connection(
        daemon: Arc<GlobalDaemon>,
        request: &ProxyRequest,
    ) -> (
        tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
        tokio::net::tcp::OwnedWriteHalf,
        JoinHandle<Result<()>>,
    ) {
        authenticated_test_connection_kind(daemon, request, ClientKind::Cli).await
    }

    async fn authenticated_test_connection_kind(
        daemon: Arc<GlobalDaemon>,
        request: &ProxyRequest,
        client_kind: ClientKind,
    ) -> (
        tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
        tokio::net::tcp::OwnedWriteHalf,
        JoinHandle<Result<()>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let transport = Arc::new(ServerTransport::issue(&address).unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            handle_proxy_connection(daemon, transport, stream).await
        });
        let mut client = TcpStream::connect(&address).await.unwrap();
        client_handshake(&mut client, &address, client_kind, request)
            .await
            .unwrap();
        let (read_half, write_half) = client.into_split();
        (BufReader::new(read_half).lines(), write_half, server)
    }

    async fn write_json_rpc_request(
        writer: &mut tokio::net::tcp::OwnedWriteHalf,
        id: u64,
        method: &str,
        params: Value,
    ) {
        let mut request = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .unwrap();
        request.push(b'\n');
        writer.write_all(&request).await.unwrap();
        writer.flush().await.unwrap();
    }

    async fn read_json_line(
        reader: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    ) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(30), reader.next_line())
            .await
            .expect("JSON-RPC response timed out")
            .expect("read JSON-RPC response")
            .expect("server closed before JSON-RPC response");
        serde_json::from_str(&line).expect("parse JSON-RPC response")
    }

    /// Wait until a shard has actually reached the state the eviction paths
    /// call evictable.  Polling index work alone is not that state: a shard
    /// that has just been leased is still bootstrapping and has not enqueued
    /// its index job yet, so an index-only poll reports "quiet" during the
    /// busiest moment of cold start.
    async fn wait_for_shard_evictable(daemon: &GlobalDaemon, root: &PathBuf) {
        let key = shard_key(root);
        for _ in 0..500 {
            {
                let shards = daemon.shards.lock().await;
                match shards.get(&key) {
                    Some(entry) if entry.is_evictable() => return,
                    None => return,
                    Some(_) => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("shard never became evictable for {}", root.display());
    }

    /// Loaded and quiet: bootstrap finished and no index job queued or
    /// running. It waits on real indexing of an empty scratch directory,
    /// which takes well under a second alone; sixty seconds is a ceiling for
    /// a stuck bootstrap under a fully parallel test run, not an expectation.
    async fn wait_for_shard_published(daemon: &GlobalDaemon, root: &PathBuf) {
        let key = shard_key(root);
        for _ in 0..6_000 {
            {
                let shards = daemon.shards.lock().await;
                if let Some(entry) = shards.get(&key) {
                    // One lock at a time: `has_work_in_flight` takes the
                    // runtime lock, which a guard held across this whole
                    // condition would still own.
                    let published = lock_owned(&entry.runtime).is_some();
                    if published
                        && !entry.is_bootstrapping()
                        && !entry.index_work.workspace_is_busy(&key)
                        && !entry.has_work_in_flight()
                    {
                        return;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let shards = daemon.shards.lock().await;
        let stuck = shards.get(&key).map(|entry| {
            let published = lock_owned(&entry.runtime).is_some();
            format!(
                "bootstrapping={} runtime={} index_busy={} work_in_flight={} error={:?}",
                entry.is_bootstrapping(),
                published,
                entry.index_work.workspace_is_busy(&key),
                entry.has_work_in_flight(),
                entry.bootstrap_error(),
            )
        });
        panic!(
            "shard never finished loading for {}: {stuck:?}",
            root.display()
        );
    }

    struct NoopRequestHandler;

    #[async_trait::async_trait]
    impl RequestHandler for NoopRequestHandler {
        async fn handle(&self, _method: &str, _params: Value) -> Result<Value, (i32, String)> {
            Ok(serde_json::json!({}))
        }
    }
}

fn cancelled_request_key(params: &Value) -> Option<String> {
    params
        .get("requestId")
        .or_else(|| params.get("request_id"))
        .or_else(|| params.get("id"))
        .and_then(request_id_key)
}

fn cancel_active_request(
    active_requests: &mut HashMap<String, ActiveRequest>,
    cancelled_requests: &mut HashSet<String>,
    params: &Value,
) -> bool {
    let Some(key) = cancelled_request_key(params) else {
        return false;
    };
    cancelled_requests.insert(key.clone());
    if let Some(handle) = active_requests.remove(&key) {
        handle.handle.abort();
        return true;
    }
    false
}

fn should_write_tracked_response(
    active_requests: &mut HashMap<String, ActiveRequest>,
    cancelled_requests: &mut HashSet<String>,
    pending: &PendingResponse,
) -> bool {
    match active_requests.get(&pending.request_key) {
        Some(active) if active.generation == pending.generation => {
            active_requests.remove(&pending.request_key);
            !cancelled_requests.remove(&pending.request_key)
        }
        Some(_) => false,
        None => {
            cancelled_requests.remove(&pending.request_key);
            false
        }
    }
}

fn abort_all_requests(active_requests: &mut HashMap<String, ActiveRequest>) {
    for (_, handle) in active_requests.drain() {
        handle.handle.abort();
    }
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn env_duration_secs(name: &str, fallback: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(fallback)
}

fn env_bool(name: &str, fallback: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.as_str(),
                "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
            )
        })
        .unwrap_or(fallback)
}

fn explicit_path_values(value: &Value) -> Vec<&str> {
    let mut paths = Vec::new();
    for key in [
        "file",
        "path",
        "target",
        "focus",
        "from_file",
        "to_file",
        "files",
        "linked_files",
        "linked_docs",
        "linked_tests",
        "entry_files",
        "focus_files",
    ] {
        collect_path_values(value.get(key), &mut paths);
    }
    for key in [
        "query",
        "task",
        "summary",
        "task_statement",
        "intent_hint",
        "content",
        "source_query",
        "refresh_key",
    ] {
        collect_embedded_path_values(value.get(key), &mut paths);
    }
    paths
}

fn collect_path_values<'a>(value: Option<&'a Value>, output: &mut Vec<&'a str>) {
    match value {
        Some(Value::String(text)) => output.push(text),
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(text) = item.as_str() {
                    output.push(text);
                }
            }
        }
        _ => {}
    }
}

fn collect_embedded_path_values<'a>(value: Option<&'a Value>, output: &mut Vec<&'a str>) {
    match value {
        Some(Value::String(text)) => {
            for candidate in embedded_absolute_path_candidates(text) {
                output.push(candidate);
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(text) = item.as_str() {
                    for candidate in embedded_absolute_path_candidates(text) {
                        output.push(candidate);
                    }
                }
            }
        }
        _ => {}
    }
}

fn embedded_absolute_path_candidates(text: &str) -> Vec<&str> {
    text.split_whitespace()
        .map(|part| {
            part.trim_matches(|ch: char| {
                matches!(
                    ch,
                    '"' | '\'' | '`' | ',' | ';' | ':' | ')' | '(' | '[' | ']' | '{' | '}'
                )
            })
        })
        .filter(|part| part.starts_with('/'))
        .collect()
}

fn extract_tool_json(value: Value) -> Result<Value, (i32, String)> {
    let text = value
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            (
                -32603,
                "Tool response did not contain text content".to_string(),
            )
        })?;
    let json_text = if let Some(start) = text.find("```json") {
        let body = &text[start + "```json".len()..];
        body.find("```")
            .map(|end| body[..end].trim())
            .unwrap_or(text)
    } else {
        text
    };
    serde_json::from_str(json_text).map_err(|error| {
        (
            -32603,
            format!("Tool response text was not structured JSON: {error}"),
        )
    })
}

fn annotate_wrapped_tool_json(value: Value, annotation: Value) -> Value {
    let Ok(mut payload) = extract_tool_json(value.clone()) else {
        return value;
    };
    if let (Some(payload), Some(annotation)) = (payload.as_object_mut(), annotation.as_object()) {
        for (key, value) in annotation {
            payload.insert(key.clone(), value.clone());
        }
        wrap_json_text(Value::Object(payload.clone()))
    } else {
        value
    }
}

fn is_cross_shard_workflow_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "get_context_capsule"
            | "query_context"
            | "prepare_change"
            | "plan_edit"
            | "trace_scenario"
            | "find_relevant_tests"
            | "impact_from_diff"
            | "get_working_set_context"
            | "summarize_subsystem"
            | "get_docs_capsule"
            | "diagnose_failure"
            | "get_dependencies"
            | "get_dependents"
            | "get_impact_graph"
            | "blast_radius"
            | "search_logic_flow"
    )
}

fn is_authoritative_cross_shard_graph_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "get_dependencies"
            | "get_dependents"
            | "get_impact_graph"
            | "blast_radius"
            | "search_logic_flow"
    )
}

fn is_memory_write_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "save_memory" | "save_quick_memory" | "record_workflow_outcome"
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewWorkflowRender {
    Json,
    Markdown,
    Hybrid,
}

fn workflow_render_mode_for_tool_call(params: &Value) -> ViewWorkflowRender {
    match params
        .get("arguments")
        .and_then(|arguments| arguments.get("render"))
        .and_then(Value::as_str)
    {
        Some("json") => ViewWorkflowRender::Json,
        Some("markdown") => ViewWorkflowRender::Markdown,
        _ => ViewWorkflowRender::Hybrid,
    }
}

fn merge_search_memory_payloads(
    requested_shard_count: usize,
    shard_results: Vec<(PathBuf, Value)>,
    failed_shards: Vec<Value>,
) -> Value {
    let query = shard_results
        .first()
        .and_then(|(_, value)| value.get("query"))
        .cloned()
        .unwrap_or(Value::Null);
    let mut memories = Vec::new();
    let mut diagnostics = Vec::new();
    let mut shards = Vec::new();
    let mut query_exact_terms = std::collections::BTreeSet::new();
    let mut matched_exact_terms = std::collections::BTreeSet::new();
    let mut durable_exact_term_counts = serde_json::Map::new();
    let mut per_shard_exact_term_counts = serde_json::Map::new();

    for (root, value) in &shard_results {
        let workspace = root.to_string_lossy().to_string();
        append_search_items_with_workspace(&mut memories, value.get("memories"), &workspace);
        if let Some(exact_terms) = value
            .get("diagnostics")
            .and_then(|diagnostics| diagnostics.get("query_exact_terms"))
            .and_then(Value::as_array)
        {
            for term in exact_terms.iter().filter_map(Value::as_str) {
                query_exact_terms.insert(term.to_string());
            }
        }
        if let Some(exact_terms) = value
            .get("diagnostics")
            .and_then(|diagnostics| diagnostics.get("matched_exact_terms"))
            .and_then(Value::as_array)
        {
            for term in exact_terms.iter().filter_map(Value::as_str) {
                matched_exact_terms.insert(term.to_string());
            }
        }
        if let Some(counts) = value
            .get("diagnostics")
            .and_then(|diagnostics| diagnostics.get("durable_exact_term_counts"))
            .and_then(Value::as_object)
        {
            per_shard_exact_term_counts.insert(workspace.clone(), Value::Object(counts.clone()));
            for (term, count) in counts {
                let existing = durable_exact_term_counts
                    .get(term)
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let next = existing + count.as_u64().unwrap_or(0);
                durable_exact_term_counts.insert(term.clone(), serde_json::json!(next));
            }
        }
        if let Some(matches) = value
            .get("diagnostics")
            .and_then(|diagnostics| diagnostics.get("matches"))
        {
            append_search_items_with_workspace(&mut diagnostics, Some(matches), &workspace);
        }
        shards.push(serde_json::json!({
            "workspace": workspace,
            "status": "included",
            "count": value.get("count").and_then(Value::as_u64).unwrap_or(0),
            "durable_exact_term_counts": value
                .get("diagnostics")
                .and_then(|diagnostics| diagnostics.get("durable_exact_term_counts"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
        }));
    }
    for shard in &failed_shards {
        shards.push(shard.clone());
    }

    let query_text = query.as_str().unwrap_or_default();
    sort_search_items_by_diagnostic_score(&mut memories, &diagnostics, query_text);
    sort_diagnostics_by_score(&mut diagnostics, query_text);
    dedupe_search_memories(&mut memories);
    truncate_array(&mut memories, 10);
    truncate_array(&mut diagnostics, 32);
    let partial = !failed_shards.is_empty();
    let unmatched_exact_terms: Vec<String> = query_exact_terms
        .difference(&matched_exact_terms)
        .cloned()
        .collect();
    let exact_term_status = if query_exact_terms.is_empty() {
        "not_requested"
    } else if unmatched_exact_terms.is_empty() {
        "matched"
    } else if matched_exact_terms.is_empty()
        && durable_exact_term_counts
            .values()
            .all(|count| count.as_u64().unwrap_or(0) == 0)
    {
        "absent_from_durable_memory"
    } else {
        "partially_matched"
    };

    serde_json::json!({
        "query": query,
        "memories": memories,
        "count": memories.len(),
        "logical_view": true,
        "partial": partial,
        "requested_shard_count": requested_shard_count,
        "included_shard_count": shard_results.len(),
        "failed_shards": failed_shards,
        "shards": shards,
        "diagnostics": {
            "logical_view": true,
            "exact_term_rerank": true,
            "query_exact_terms": query_exact_terms.into_iter().collect::<Vec<_>>(),
            "matched_exact_terms": matched_exact_terms.into_iter().collect::<Vec<_>>(),
            "unmatched_exact_terms": unmatched_exact_terms,
            "durable_exact_term_counts": durable_exact_term_counts,
            "per_shard_exact_term_counts": per_shard_exact_term_counts,
            "exact_term_status": exact_term_status,
            "matches": diagnostics,
        }
    })
}

fn append_search_items_with_workspace(
    output: &mut Vec<Value>,
    value: Option<&Value>,
    workspace: &str,
) {
    let Some(Value::Array(items)) = value else {
        return;
    };
    for item in items {
        let mut item = item.clone();
        if let Value::Object(object) = &mut item {
            object
                .entry("source_workspace".to_string())
                .or_insert_with(|| serde_json::json!(workspace));
        }
        output.push(item);
    }
}

fn sort_search_items_by_diagnostic_score(
    memories: &mut [Value],
    diagnostics: &[Value],
    query: &str,
) {
    memories.sort_by(|left, right| {
        let left_score = search_memory_score(left, diagnostics, query);
        let right_score = search_memory_score(right, diagnostics, query);
        right_score
            .cmp(&left_score)
            .then_with(|| search_memory_created_at(right).cmp(&search_memory_created_at(left)))
    });
}

fn search_memory_score(memory: &Value, diagnostics: &[Value], query: &str) -> i64 {
    let memory_id = memory.get("id").and_then(Value::as_str);
    let workspace = memory
        .get("source_workspace")
        .or_else(|| memory.get("workspace_id"))
        .and_then(Value::as_str);
    let base = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.get("memory_id").and_then(Value::as_str) == memory_id
                && diagnostic
                    .get("source_workspace")
                    .or_else(|| diagnostic.get("workspace"))
                    .and_then(Value::as_str)
                    == workspace
        })
        .and_then(|diagnostic| diagnostic.get("score").and_then(Value::as_i64))
        .unwrap_or(0);
    base + cross_shard_context_score(memory, query)
}

fn search_memory_created_at(memory: &Value) -> u64 {
    memory
        .get("created_at")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn sort_diagnostics_by_score(diagnostics: &mut [Value], query: &str) {
    diagnostics.sort_by(|left, right| {
        let left_score = left.get("score").and_then(Value::as_i64).unwrap_or(0)
            + cross_shard_context_score(left, query);
        let right_score = right.get("score").and_then(Value::as_i64).unwrap_or(0)
            + cross_shard_context_score(right, query);
        right_score.cmp(&left_score)
    });
}

fn cross_shard_context_score(value: &Value, query: &str) -> i64 {
    let terms = cross_shard_context_terms(query);
    if terms.is_empty() {
        return 0;
    }
    let haystack = search_item_context_text(value);
    let mut score = 0;
    for term in &terms {
        if haystack.contains(term) {
            score += 900;
        }
    }
    if let Some(workspace) = value
        .get("source_workspace")
        .or_else(|| value.get("workspace_id"))
        .or_else(|| value.get("workspace"))
        .and_then(Value::as_str)
    {
        let workspace = workspace.to_ascii_lowercase();
        for term in &terms {
            if workspace
                .split(|ch: char| !ch.is_ascii_alphanumeric())
                .any(|part| part == term)
            {
                score += 1800;
            }
        }
    }
    score
}

fn search_item_context_text(value: &Value) -> String {
    let mut parts = Vec::new();
    collect_search_item_text(value, &mut parts);
    parts.join(" ").to_ascii_lowercase()
}

fn collect_search_item_text(value: &Value, parts: &mut Vec<String>) {
    match value {
        Value::String(text) => parts.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                collect_search_item_text(item, parts);
            }
        }
        Value::Object(object) => {
            for key in [
                "content",
                "source_query",
                "refresh_key",
                "linked_files",
                "linked_docs",
                "linked_tests",
                "matched_terms",
                "workspace_id",
                "source_workspace",
                "workspace",
            ] {
                if let Some(value) = object.get(key) {
                    collect_search_item_text(value, parts);
                }
            }
        }
        _ => {}
    }
}

fn cross_shard_context_terms(query: &str) -> Vec<String> {
    let stopwords = [
        "remediation",
        "workflow",
        "outcome",
        "memory",
        "search",
        "current",
        "state",
        "fix",
        "fixed",
    ];
    let mut terms = Vec::new();
    for part in query.split(|ch: char| !ch.is_ascii_alphanumeric()) {
        if part.len() < 3 {
            continue;
        }
        let term = part.to_ascii_lowercase();
        if stopwords.contains(&term.as_str()) {
            continue;
        }
        if is_structured_id_fragment(&term) {
            continue;
        }
        if !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms
}

fn is_structured_id_fragment(term: &str) -> bool {
    matches!(term, "iu" | "px" | "im") || term.chars().all(|ch| ch.is_ascii_digit())
}

fn dedupe_search_memories(memories: &mut Vec<Value>) {
    let mut seen = std::collections::HashSet::new();
    memories.retain(|memory| {
        let key = format!(
            "{}::{}",
            memory
                .get("source_workspace")
                .or_else(|| memory.get("workspace_id"))
                .and_then(Value::as_str)
                .unwrap_or_default(),
            memory.get("id").and_then(Value::as_str).unwrap_or_default()
        );
        seen.insert(key)
    });
}

fn merge_workflow_payloads(
    primary_root: &PathBuf,
    requested_shard_count: usize,
    shard_results: Vec<(PathBuf, Value)>,
    failed_shards: Vec<Value>,
    incomplete_shards: Vec<Value>,
) -> Value {
    let mut merged = shard_results
        .first()
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| serde_json::json!({}));
    let included_shard_count = shard_results.len();
    let mut overviews = Vec::new();
    let mut pivots = Vec::new();
    let mut context = Vec::new();
    let mut memories = Vec::new();
    let mut events = Vec::new();
    let mut risks = Vec::new();
    let mut handles = Vec::new();
    let mut verification = Vec::new();
    let mut dependencies = Vec::new();
    let mut dependents = Vec::new();
    let mut affected = Vec::new();
    let mut files = Vec::new();
    let mut paths = Vec::new();
    let mut shard_summaries = Vec::new();

    for (root, value) in &shard_results {
        let workspace = root.to_string_lossy().to_string();
        let no_match = value.get("error").and_then(Value::as_str).is_some();
        if let Some(overview) = value.get("overview").and_then(Value::as_str) {
            overviews.push(format!("{workspace}: {overview}"));
        }
        append_array_with_workspace(&mut pivots, value.get("ranked_pivots"), &workspace);
        append_array_with_workspace(&mut context, value.get("relevant_context"), &workspace);
        append_array_with_workspace(&mut memories, value.get("memory_highlights"), &workspace);
        append_array_with_workspace(&mut events, value.get("event_episodes"), &workspace);
        append_array_with_workspace(&mut risks, value.get("risks"), &workspace);
        append_array_with_workspace(
            &mut verification,
            value.get("verification_commands"),
            &workspace,
        );
        append_array_with_workspace(&mut dependencies, value.get("dependencies"), &workspace);
        append_array_with_workspace(&mut dependents, value.get("dependents"), &workspace);
        append_array_with_workspace(&mut affected, value.get("affected"), &workspace);
        append_array_with_workspace(&mut files, value.get("files"), &workspace);
        append_array_with_workspace(&mut paths, value.get("paths"), &workspace);
        for handle in handles_from_payload(value) {
            push_unique_string(&mut handles, handle);
        }
        shard_summaries.push(serde_json::json!({
            "workspace": workspace,
            "status": if no_match { "no_match" } else { "included" },
            "ranked_pivots": value.get("ranked_pivots").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            "relevant_context": value.get("relevant_context").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            "dependencies": value.get("dependencies").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            "dependents": value.get("dependents").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            "affected": value.get("affected").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            "paths": value.get("paths").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
        }));
    }

    sort_by_score_desc(&mut pivots);
    truncate_array(&mut pivots, 12);
    truncate_array(&mut context, 16);
    truncate_array(&mut memories, 8);
    truncate_array(&mut events, 8);
    truncate_array(&mut risks, 12);
    truncate_array(&mut verification, 12);
    truncate_array(&mut dependencies, 32);
    truncate_array(&mut dependents, 32);
    truncate_array(&mut affected, 64);
    truncate_array(&mut files, 64);
    truncate_array(&mut paths, 16);

    for shard in &failed_shards {
        shard_summaries.push(shard.clone());
    }
    for shard in &incomplete_shards {
        shard_summaries.push(shard.clone());
    }

    let partial_failures = !failed_shards.is_empty();
    let partial = partial_failures || !incomplete_shards.is_empty();
    if let Some(object) = merged.as_object_mut() {
        object.remove("error");
        object.insert(
            "overview".to_string(),
            Value::String(format!(
                "Merged logical-view workflow result across {included_shard_count}/{requested_shard_count} included shard(s). {}",
                overviews.join(" ")
            )),
        );
        object.insert("ranked_pivots".to_string(), Value::Array(pivots));
        object.insert("relevant_context".to_string(), Value::Array(context));
        object.insert("memory_highlights".to_string(), Value::Array(memories));
        object.insert("event_episodes".to_string(), Value::Array(events));
        object.insert("risks".to_string(), Value::Array(risks));
        object.insert(
            "verification_commands".to_string(),
            Value::Array(verification),
        );
        object.insert("dependencies".to_string(), Value::Array(dependencies));
        object.insert("dependents".to_string(), Value::Array(dependents));
        object.insert("affected".to_string(), Value::Array(affected));
        object.insert("files".to_string(), Value::Array(files));
        object.insert("paths".to_string(), Value::Array(paths));
        object.insert("count".to_string(), serde_json::json!(merged_count(object)));
        object.insert("stable_handles".to_string(), serde_json::json!(handles));
        object.insert("logical_view".to_string(), Value::Bool(true));
        object.insert(
            "workspace".to_string(),
            Value::String(primary_root.to_string_lossy().to_string()),
        );
        object.insert("partial".to_string(), Value::Bool(partial));
        object.insert("shards".to_string(), Value::Array(shard_summaries));
        object.insert("failed_shards".to_string(), Value::Array(failed_shards));
        object.insert(
            "incomplete_shards".to_string(),
            Value::Array(incomplete_shards),
        );
        object.insert(
            "partial_failures".to_string(),
            Value::Bool(partial_failures),
        );
    }
    merged
}

fn append_array_with_workspace(output: &mut Vec<Value>, value: Option<&Value>, workspace: &str) {
    let Some(items) = value.and_then(Value::as_array) else {
        return;
    };
    for item in items {
        let mut item = item.clone();
        if let Some(object) = item.as_object_mut() {
            object.insert(
                "source_workspace".to_string(),
                Value::String(workspace.to_string()),
            );
        }
        output.push(item);
    }
}

fn is_indexing_payload(value: &Value) -> bool {
    value
        .get("indexing")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value
            .get("status")
            .and_then(Value::as_str)
            // A deferred shard has no answer either: it is incomplete for a
            // merged view even though nothing is indexing it.
            .is_some_and(|status| status == "indexing" || status == "deferred")
}

fn shard_incomplete_value(root: &PathBuf, payload: &Value) -> Value {
    serde_json::json!({
        "workspace": root.to_string_lossy(),
        "status": "incomplete",
        "indexing": true,
        "reason": payload.get("reason").and_then(Value::as_str).unwrap_or("indexing"),
        "retry": "wait_and_retry",
        "overview": payload.get("overview").and_then(Value::as_str),
    })
}

fn merged_count(object: &serde_json::Map<String, Value>) -> usize {
    [
        "dependencies",
        "dependents",
        "affected",
        "paths",
        "ranked_pivots",
    ]
    .iter()
    .find_map(|key| object.get(*key).and_then(Value::as_array).map(Vec::len))
    .unwrap_or(0)
}

fn sort_by_score_desc(items: &mut [Value]) {
    items.sort_by(|left, right| {
        let left_score = left.get("score").and_then(Value::as_f64).unwrap_or(0.0);
        let right_score = right.get("score").and_then(Value::as_f64).unwrap_or(0.0);
        right_score
            .partial_cmp(&left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn truncate_array(items: &mut Vec<Value>, max_len: usize) {
    if items.len() > max_len {
        items.truncate(max_len);
    }
}

fn handles_from_payload(value: &Value) -> Vec<String> {
    let mut handles = Vec::new();
    collect_handles_recursive(value, &mut handles);
    handles
}

fn collect_handles_recursive(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if (key == "context_handle"
                    || key == "suggested_expand"
                    || key == "suggested_next_expansion"
                    || key.ends_with("_handle"))
                    && value.is_string()
                {
                    if let Some(handle) = value.as_str() {
                        push_unique_string(output, handle.to_string());
                    }
                }
                collect_handles_recursive(value, output);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_handles_recursive(item, output);
            }
        }
        _ => {}
    }
}

fn push_unique_string(items: &mut Vec<String>, value: String) {
    if !items.iter().any(|existing| existing == &value) {
        items.push(value);
    }
}

fn wrap_workflow_json_for_render(value: Value, render: ViewWorkflowRender) -> Value {
    let serialized = serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
    let summary = value
        .get("overview")
        .and_then(Value::as_str)
        .unwrap_or("Merged logical-view workflow result ready.");
    match render {
        ViewWorkflowRender::Json => wrap_text(serialized),
        ViewWorkflowRender::Markdown => wrap_text(format!("### Summary\n{summary}")),
        ViewWorkflowRender::Hybrid => wrap_text(format!(
            "### Summary\n{summary}\n\n### Structured Payload\n```json\n{serialized}\n```"
        )),
    }
}

fn wrap_json_text(value: Value) -> Value {
    wrap_text(serde_json::to_string(&value).unwrap_or_else(|_| value.to_string()))
}

fn wrap_text(text: String) -> Value {
    serde_json::json!({
        "content": [{
            "type": "text",
            "text": text,
        }]
    })
}

fn internal_error(error: anyhow::Error) -> (i32, String) {
    (-32603, error.to_string())
}
