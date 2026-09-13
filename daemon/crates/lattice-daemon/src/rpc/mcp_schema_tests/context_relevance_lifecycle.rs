//! Public relevance-diagnostic expansion lifecycle regressions.
//!
//! The production contract keeps numeric ranking diagnostics separate from
//! cached memory references and never treats a diagnostic expansion as memory
//! delivery.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use lattice_core::events::{EventStore, EventWriter, FlushPolicy};
use lattice_core::graph::CodeGraph;
use lattice_core::indexer::Indexer;
use lattice_core::memory::MemoryStore;
use lattice_core::query::QueryEngine;
use lattice_core::storage::GraphStore;
use lattice_core::symbols::{Language, SymbolId, SymbolKind};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::super::mcp::McpHandler;
use super::{call_args, parse_tool_payload};
use crate::rpc::server::RequestHandler;

struct RelevanceFixture {
    handler: McpHandler,
    memory_store: Arc<Mutex<MemoryStore>>,
    workspace_root: std::path::PathBuf,
    context_cache_path: std::path::PathBuf,
}

impl RelevanceFixture {
    fn new(suffix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let workspace_root =
            std::env::temp_dir().join(format!("lattice-relevance-{suffix}-{nanos}"));
        std::fs::create_dir_all(workspace_root.join(".git")).expect("git directory");
        std::fs::write(workspace_root.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("git head");
        let mut graph = CodeGraph::new();
        graph.add_node(
            SymbolId {
                file: "src/lib.rs".into(),
                name: "login".into(),
                byte_offset: 0,
            },
            SymbolKind::Function,
            "login".into(),
            "pub fn login()",
            "pub fn login() {}",
            "src/lib.rs".into(),
            1,
            1,
            true,
            Language::Rust,
        );
        let memory_store = Arc::new(Mutex::new(
            MemoryStore::open_in_memory().expect("memory store"),
        ));
        let event_store = Arc::new(EventStore::open_in_memory().expect("events"));
        let event_writer = Arc::new(
            EventWriter::new(
                event_store,
                workspace_root.to_string_lossy().to_string(),
                4096,
            )
            .with_flush_policy(FlushPolicy::Sync),
        );
        let context_cache_path = workspace_root.join("context_handles.json");
        let handler = McpHandler::new(
            Arc::new(Mutex::new(QueryEngine::new(graph, None))),
            Arc::new(Mutex::new(Indexer::new(workspace_root.clone()))),
            memory_store.clone(),
            Arc::new(Mutex::new(
                GraphStore::open_in_memory().expect("graph store"),
            )),
            Arc::new(std::sync::OnceLock::new()),
            None,
            workspace_root.clone(),
            context_cache_path.clone(),
            format!("session-{suffix}"),
            None,
            vec![workspace_root.clone()],
            Arc::new(AtomicBool::new(false)),
            Some(event_writer),
            Vec::new(),
            Vec::new(),
        );
        Self {
            handler,
            memory_store,
            workspace_root,
            context_cache_path,
        }
    }
}

impl Drop for RelevanceFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.workspace_root);
    }
}

async fn call(fixture: &RelevanceFixture, name: &str, arguments: Value) -> Value {
    let response = fixture
        .handler
        .handle("tools/call", call_args(name, arguments))
        .await
        .unwrap_or_else(|error| panic!("{name} failed: {error:?}"));
    parse_tool_payload(&response)
}

fn relevance_targets(value: &Value, output: &mut Vec<(String, String)>) {
    match value {
        Value::Object(object) => {
            if let (Some(handle), Some(focus)) = (
                object
                    .get("relevance_detail_handle")
                    .and_then(Value::as_str),
                object.get("relevance_detail_focus").and_then(Value::as_str),
            ) {
                output.push((handle.to_string(), focus.to_string()));
            }
            for child in object.values() {
                relevance_targets(child, output);
            }
        }
        Value::Array(values) => {
            for child in values {
                relevance_targets(child, output);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn public_relevance_expansion_returns_numeric_snapshot_without_delivery() {
    let fixture = RelevanceFixture::new("context-relevance-pivot");
    let prepared = call(
        &fixture,
        "prepare_change",
        json!({"task":"investigate login behavior", "render":"json", "max_tokens":4000}),
    )
    .await;
    let mut targets = Vec::new();
    relevance_targets(&prepared, &mut targets);
    let (handle, focus) = targets
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("emitted relevance expansion target: {prepared}"));
    assert!(focus.starts_with("relevance:"), "{focus}");
    let expanded = call(
        &fixture,
        "context",
        json!({"mode":"expand","handle":handle,"focus":focus,"max_tokens":4000}),
    )
    .await;
    assert_eq!(expanded["focus_type"].as_str(), Some("relevance"));
    assert!(
        expanded["relevance"]["total_score"].is_number(),
        "{expanded}"
    );
    assert!(
        expanded["relevance"]["ranking_signals"].is_object(),
        "{expanded}"
    );
    assert!(
        expanded.get("memories").is_none(),
        "diagnostic must not return lesson content"
    );
    assert!(
        expanded.get("memory_deliveries").is_none(),
        "diagnostic must not issue a receipt"
    );
    let persisted = std::fs::read_to_string(&fixture.context_cache_path).expect("persisted cache");
    assert!(
        !persisted.contains("inclusion_reason"),
        "cache stores numeric diagnostics only: {persisted}"
    );
}

#[tokio::test]
async fn relevance_focus_is_exact_and_legacy_memory_pivot_fails_closed() {
    let fixture = RelevanceFixture::new("context-relevance-exact");
    let prepared = call(
        &fixture,
        "prepare_change",
        json!({"task":"investigate login behavior", "render":"json", "max_tokens":4000}),
    )
    .await;
    let mut targets = Vec::new();
    relevance_targets(&prepared, &mut targets);
    let (handle, _) = targets
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("emitted relevance expansion target: {prepared}"));
    for focus in ["relevance:unknown", "memory:pivot:0"] {
        let error = fixture
            .handler
            .handle(
                "tools/call",
                call_args(
                    "context",
                    json!({"mode":"expand","handle":handle,"focus":focus,"max_tokens":4000}),
                ),
            )
            .await
            .expect_err("wrong or legacy diagnostic focus must fail closed");
        assert_eq!(error.0, -32602, "{error:?}");
    }
}

#[tokio::test]
async fn memory_relevance_revalidates_canonical_state_without_delivery_or_recall_renewal() {
    let fixture = RelevanceFixture::new("context-relevance-memory");
    let lesson = "login relevance lesson payload-sentinel-relevance-memory";
    let saved = call(
        &fixture,
        "remember",
        json!({
            "kind":"quick", "content":lesson, "scope":"repo",
            "memory_class":"constraint", "assertion_type":"constraint",
            "confidence":1.0, "confidence_reason":"relevance fixture",
            "freshness_policy":"repo_scoped"
        }),
    )
    .await;
    let memory_id = saved["memory_id"].as_str().expect("memory id").to_owned();
    let prepared = call(
        &fixture,
        "prepare_change",
        json!({"task":"resolve login relevance lesson", "render":"json", "max_tokens":4000}),
    )
    .await;
    let mut targets = Vec::new();
    relevance_targets(&prepared, &mut targets);
    let (handle, focus) = targets
        .into_iter()
        .find(|(_, focus)| focus.contains(&memory_id))
        .unwrap_or_else(|| panic!("emitted memory relevance target for {memory_id}: {prepared}"));
    let expanded = call(
        &fixture,
        "context",
        json!({"mode":"expand","handle":handle,"focus":focus,"max_tokens":4000}),
    )
    .await;
    assert_eq!(
        expanded["relevance"]["kind"].as_str(),
        Some("memory"),
        "{expanded}"
    );
    assert!(expanded.get("memories").is_none());
    assert!(expanded.get("memory_deliveries").is_none());
    let recalled: Option<i64> = fixture
        .memory_store
        .lock()
        .await
        .with_connection(|connection| {
            connection
                .query_row(
                    "SELECT last_recalled_at FROM memories WHERE id=?1",
                    [&memory_id],
                    |row| row.get(0),
                )
                .map_err(|error| lattice_core::LatticeError::Storage(error.to_string()))
        })
        .expect("memory recall clock");
    assert_eq!(recalled, None, "a diagnostic is not a memory delivery");

    fixture
        .memory_store
        .lock()
        .await
        .invalidate(&memory_id)
        .expect("canonical invalidation");
    let error = fixture
        .handler
        .handle(
            "tools/call",
            call_args(
                "context",
                json!({"mode":"expand","handle":handle,"focus":focus,"max_tokens":4000}),
            ),
        )
        .await
        .expect_err("invalidated memory diagnostic must reject");
    assert_eq!(error.0, -32001, "{error:?}");
    assert!(
        !error.1.contains(lesson),
        "invalidated lesson must not leak"
    );
    let persisted = std::fs::read_to_string(&fixture.context_cache_path).expect("persisted cache");
    assert!(
        !persisted.contains("payload-sentinel-relevance-memory"),
        "{persisted}"
    );
}
