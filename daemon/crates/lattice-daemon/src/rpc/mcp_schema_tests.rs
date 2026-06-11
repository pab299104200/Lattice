//! MCP tool-surface contract regression tests.
//!
//! Implements the contract gate from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-build/tasks/R64.md`.
//! It binds the daemon's advertised tool list, request/response schemas,
//! render modes to the canonical 8-verb tool reference.
//!
//! Cited spec heading: `## Phase 2 — Consolidate the agent-facing tool surface
//! to 8 verbs` from `docs/plans/2026-06-11-agent-adoption-overhaul.md`.

#![cfg(test)]

mod render_modes;
mod round_trip;
pub(crate) mod tool_list;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use lattice_core::events::{EventStore, EventWriter, FlushPolicy};
use lattice_core::graph::CodeGraph;
use lattice_core::indexer::Indexer;
use lattice_core::memory::MemoryStore;
use lattice_core::query::QueryEngine;
use lattice_core::storage::GraphStore;
use serde_json::Value;
use tokio::sync::Mutex;

use super::mcp::McpHandler;

/// Bundles the handler under test with the side-channel stores tests inspect.
pub(crate) struct SchemaFixture {
    pub handler: McpHandler,
    pub memory_store: Arc<Mutex<MemoryStore>>,
    pub event_store: Arc<EventStore>,
    pub workspace_root: PathBuf,
    pub context_cache_path: PathBuf,
    pub session_id: String,
}

impl SchemaFixture {
    pub fn new(suffix: &str) -> Self {
        let workspace_root = unique_test_path(&format!("lattice-mcp-schema-{suffix}"));
        std::fs::create_dir_all(&workspace_root).expect("workspace dir");
        std::fs::create_dir_all(workspace_root.join(".git")).expect("git dir");
        std::fs::write(
            workspace_root.join(".git").join("HEAD"),
            "ref: refs/heads/main\n",
        )
        .expect("git head");
        let context_cache_path = workspace_root.join("context_handles.json");
        let memory_store = Arc::new(Mutex::new(
            MemoryStore::open_in_memory().expect("memory store"),
        ));
        let event_store = Arc::new(EventStore::open_in_memory().expect("event store"));
        let event_writer = Arc::new(
            EventWriter::new(
                event_store.clone(),
                workspace_root.to_string_lossy().to_string(),
                4096,
            )
            .with_flush_policy(FlushPolicy::Sync),
        );
        let session_id = format!("session-schema-{suffix}");
        let handler = McpHandler::new(
            Arc::new(Mutex::new(QueryEngine::new(CodeGraph::new(), None, None))),
            Arc::new(Mutex::new(Indexer::new(workspace_root.clone()))),
            memory_store.clone(),
            Arc::new(Mutex::new(
                GraphStore::open_in_memory().expect("graph store"),
            )),
            Arc::new(std::sync::OnceLock::new()),
            None,
            workspace_root.clone(),
            context_cache_path.clone(),
            session_id.clone(),
            None,
            vec![workspace_root.clone()],
            Arc::new(AtomicBool::new(false)),
            Some(event_writer),
            Vec::new(),
            Vec::new(),
        );
        SchemaFixture {
            handler,
            memory_store,
            event_store,
            workspace_root,
            context_cache_path,
            session_id,
        }
    }
}

impl Drop for SchemaFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.context_cache_path);
        let _ = std::fs::remove_dir_all(&self.workspace_root);
    }
}

/// Returns a uniquely named temp path so parallel tests do not collide.
fn unique_test_path(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{nanos}"))
}

/// Performs a serde round-trip and panics if the restored value differs.
pub(crate) fn assert_round_trip<T>(value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).expect("serialize");
    let restored: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(&restored, value);
}

/// Parses the tool payload returned by `handle_tools_call` into its JSON form.
pub(crate) fn parse_tool_payload(response: &Value) -> Value {
    let text = response["content"][0]["text"]
        .as_str()
        .expect("tool returned a text payload");
    serde_json::from_str(text).expect("tool payload is valid JSON")
}

/// Builds a `tools/call` parameter object with a name and an arguments value.
pub(crate) fn call_args(name: &str, arguments: Value) -> Value {
    serde_json::json!({"name": name, "arguments": arguments})
}
