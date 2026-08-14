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

struct ActiveRequest {
    generation: u64,
    handle: JoinHandle<()>,
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
}

impl ShardEntry {
    fn pending(root: PathBuf, index_work: Arc<crate::index_work::IndexWorkCoordinator>) -> Self {
        Self {
            root,
            runtime: StdMutex::new(None),
            bootstrap: StdMutex::new(None),
            bootstrap_error: StdMutex::new(None),
            bootstrapping: AtomicBool::new(true),
            index_work,
            active_connections: AtomicUsize::new(0),
            last_used_epoch_secs: AtomicU64::new(now_epoch_secs()),
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
    ) {
        let entry = Arc::clone(self);
        let root = entry.root.clone();
        let index_work = Arc::clone(&entry.index_work);
        let task = tokio::spawn(async move {
            match crate::build_workspace_runtime(vec![root], focus_files, focus_dirs, index_work)
                .await
            {
                Ok(runtime) => {
                    if let Ok(mut slot) = entry.runtime.lock() {
                        *slot = Some(runtime);
                    } else {
                        tracing::error!(workspace = %entry.root.display(), "workspace shard runtime lock poisoned while publishing bootstrap");
                    }
                }
                Err(error) => {
                    let message = error.to_string();
                    tracing::error!(workspace = %entry.root.display(), %message, "workspace shard bootstrap failed");
                    if let Ok(mut bootstrap_error) = entry.bootstrap_error.lock() {
                        *bootstrap_error = Some(message);
                    }
                }
            }
            entry.bootstrapping.store(false, Ordering::Release);
        });
        if let Ok(mut bootstrap) = self.bootstrap.lock() {
            *bootstrap = Some(task);
        }
    }

    fn bootstrap_error(&self) -> Option<String> {
        self.bootstrap_error
            .lock()
            .ok()
            .and_then(|error| error.clone())
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

    fn is_idle(&self, now_epoch_secs: u64, idle_ttl_secs: u64) -> bool {
        if self.active_connections.load(Ordering::Acquire) != 0 || self.is_bootstrapping() {
            return false;
        }
        let last_used = self.last_used_epoch_secs.load(Ordering::Acquire);
        now_epoch_secs.saturating_sub(last_used) >= idle_ttl_secs
    }

    async fn shutdown(&self) {
        let bootstrap = self.bootstrap.lock().ok().and_then(|mut guard| guard.take());
        if let Some(task) = bootstrap {
            task.abort();
            let _ = task.await;
        }
        let runtime = self.runtime.lock().ok().and_then(|mut guard| guard.take());
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
        "context" | "prepare_change" | "impact" | "diagnose" | "search" | "remember" | "recall" | "status"
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
    serde_json::json!({
        "status": if bootstrap_error.is_some() { "degraded" } else { "indexing" },
        "indexing": bootstrap_error.is_none(),
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
    error.to_string().contains("loaded workspace shards")
}

impl Drop for RuntimeLease {
    fn drop(&mut self) {
        let retained = self
            .retained_shards
            .lock()
            .map(|mut shards| shards.drain().map(|(_, shard)| shard).collect::<Vec<_>>())
            .unwrap_or_default();
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
                    retained.handler().handle("tools/call", params.clone()).await?
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

pub(crate) struct GlobalDaemon {
    shards: Mutex<HashMap<String, Arc<ShardEntry>>>,
    max_loaded_shards: usize,
    prewarm_view_shards: bool,
    idle_ttl: Duration,
    has_loaded_runtime: AtomicBool,
    exit_when_idle: bool,
    index_work: Arc<crate::index_work::IndexWorkCoordinator>,
    hook_session_route: Option<Arc<HookSessionRoute>>,
}

impl GlobalDaemon {
    pub(crate) fn new() -> Self {
        let mut daemon = Self::new_with_config(
            env_usize(
                "LATTICE_MAX_LOADED_SHARDS",
                env_usize("LATTICE_MAX_LOADED_WORKSPACES", 3),
            ),
            env_bool("LATTICE_PREWARM_VIEW_SHARDS", false),
        );
        match HookSessionRoute::open_default() {
            Ok(route) => daemon.hook_session_route = Some(Arc::new(route)),
            Err(_) => {
                tracing::error!("hook-session service is unavailable");
                lifecycle_log::log_event("daemon", "hook_session_registry_unavailable", &[]);
            }
        }
        daemon
    }

    fn new_with_config(max_loaded_shards: usize, prewarm_view_shards: bool) -> Self {
        Self {
            shards: Mutex::new(HashMap::new()),
            max_loaded_shards,
            prewarm_view_shards,
            idle_ttl: env_duration_secs(
                "LATTICE_WORKSPACE_IDLE_TTL_SECS",
                Duration::from_secs(1800),
            ),
            has_loaded_runtime: AtomicBool::new(false),
            exit_when_idle: env_bool("LATTICE_DAEMON_EXIT_WHEN_IDLE", false),
            index_work: crate::index_work::IndexWorkCoordinator::from_env(),
            hook_session_route: None,
        }
    }

    #[cfg(test)]
    fn with_hook_session_route(mut self, route: HookSessionRoute) -> Self {
        self.hook_session_route = Some(Arc::new(route));
        self
    }

    async fn handler_for(self: &Arc<Self>, request: &ProxyRequest) -> Result<RuntimeLease> {
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
        let primary_handler: Arc<dyn RequestHandler> = if let Some(primary) = primary.as_ref() {
            primary.handler()
        } else {
            Arc::new(DeferredShardHandler {
                daemon: Arc::clone(self),
                root: primary_root.clone(),
                focus_files: request.focus_files.clone(),
                focus_dirs: request.focus_dirs.clone(),
            })
        };
        let retained_shards = Arc::new(StdMutex::new(HashMap::new()));
        if let Some(primary) = primary {
            adopt_retained_lease_shard(&retained_shards, primary.into_shard())?;
        }
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
                ("primary_shard_deferred", serde_json::json!(deferred_primary)),
            ],
        );
        Ok(RuntimeLease {
            retained_shards,
            handler,
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

    async fn shard_for(
        &self,
        root: PathBuf,
        focus_files: Vec<String>,
        focus_dirs: Vec<String>,
        retain: bool,
    ) -> Result<Arc<ShardEntry>> {
        let key = shard_key(&root);
        let (entry, victim) = {
            let mut shards = self.shards.lock().await;
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

            let victim = if shards.len() < self.max_loaded_shards {
                None
            } else {
                let victim_key = shards
                    .iter()
                    .filter(|(_, entry)| {
                        entry.active_connections.load(Ordering::Acquire) == 0
                            && !entry.is_bootstrapping()
                            && !self
                                .index_work
                                .workspace_is_busy(&shard_key(&entry.root))
                    })
                    .min_by_key(|(_, entry)| entry.last_used_epoch_secs.load(Ordering::Acquire))
                    .map(|(candidate, _)| candidate.clone());
                let Some(victim_key) = victim_key else {
                    anyhow::bail!(
                        "lattice daemon has {} loaded workspace shards and all are active or indexing; defer {} until a shard becomes evictable or LATTICE_MAX_LOADED_SHARDS is raised.",
                        self.max_loaded_shards,
                        key
                    );
                };
                shards.remove(&victim_key).map(|entry| (victim_key, entry))
            };

            let entry = Arc::new(ShardEntry::pending(root, Arc::clone(&self.index_work)));
            if retain {
                entry.retain();
            }
            shards.insert(key.clone(), Arc::clone(&entry));
            self.has_loaded_runtime.store(true, Ordering::Release);
            (entry, victim)
        };

        if let Some((victim_key, victim)) = victim {
            lifecycle_log::log_event(
                "daemon",
                "shard_capacity_eviction",
                &[
                    ("evicted_shard", serde_json::json!(victim_key)),
                    ("requested_shard", serde_json::json!(key.clone())),
                ],
            );
            victim.shutdown().await;
        }

        entry.start_bootstrap(focus_files, focus_dirs);
        lifecycle_log::log_event(
            "daemon",
            "shard_bootstrap_started",
            &[("shard_key", serde_json::json!(key))],
        );
        Ok(entry)
    }

    async fn evict_idle(&self) {
        let now = now_epoch_secs();
        let ttl = self.idle_ttl.as_secs();
        let mut victims = Vec::new();
        {
            let mut shards = self.shards.lock().await;
            let idle_keys: Vec<String> = shards
                .iter()
                .filter(|(_, entry)| entry.is_idle(now, ttl))
                .map(|(key, _)| key.clone())
                .collect();
            for key in idle_keys {
                if let Some(entry) = shards.remove(&key) {
                    victims.push((key, entry));
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

    let daemon = Arc::new(GlobalDaemon::new());
    let mut cleanup_interval = tokio::time::interval(daemon.cleanup_interval());
    cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    cleanup_interval.tick().await;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let daemon = Arc::clone(&daemon);
                let transport = Arc::clone(&transport);
                tokio::spawn(async move {
                    if let Err(err) = handle_proxy_connection(daemon, transport, stream).await {
                        tracing::warn!("proxy connection failed: {}", err);
                    }
                });
            }
            _ = cleanup_interval.tick() => {
                daemon.evict_idle().await;
                if daemon.should_shutdown_when_idle().await {
                    tracing::info!("lattice daemon exiting after all workspace runtimes went idle");
                    lifecycle_log::log_event("daemon", "idle_exit", &[]);
                    break;
                }
            }
        }
    }
    Ok(())
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
                            match tokio::task::spawn_blocking(move || {
                                match method.as_str() {
                                    HOOK_SESSION_OPEN_METHOD => {
                                        route.handle_open(&hook_request, request.params)
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
                    match daemon.handler_for(&proxy_request).await {
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
                    let task = tokio::spawn(async move {
                        let response = match handler.handle(&method, params).await {
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
                    tokio::spawn(async move {
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
    use super::*;
    use crate::transport::{client_handshake, ClientKind};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::AsyncWriteExt;

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
                let lease = tokio::time::timeout(Duration::from_millis(250), daemon.handler_for(&request))
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
        for status in responses {
            assert_eq!(status["status"].as_str(), Some("indexing"));
            assert_eq!(status["graph_snapshot_state"].as_str(), Some("not_loaded"));
            assert!(status["nodes"].is_null(), "cold status must not claim zero nodes");
            assert!(status["edges"].is_null(), "cold status must not claim zero edges");
            assert!(status["files"].is_null(), "cold status must not claim zero files");
        }

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
            wait_for_index_work(&daemon, root).await;
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
        assert!(error.to_string().contains("all are active or indexing"));

        drop(retained);
        wait_for_index_work(&daemon, &root_a).await;
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

    async fn wait_for_index_work(daemon: &GlobalDaemon, root: &PathBuf) {
        let key = shard_key(root);
        for _ in 0..100 {
            if !daemon.index_work.workspace_is_busy(&key) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("index work did not finish for {}", root.display());
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

fn env_usize(name: &str, fallback: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
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
            .is_some_and(|status| status == "indexing")
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
