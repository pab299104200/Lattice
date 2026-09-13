//! Public context-expansion lifecycle regression tests.
//!
//! A handle caches memory references, never memory payloads.  Expansion must
//! resolve the selected reference against current canonical memory state and
//! issue a delivery receipt only for the resolved record.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use lattice_core::events::{EventStore, EventWriter, FlushPolicy};
use lattice_core::graph::CodeGraph;
use lattice_core::indexer::Indexer;
use lattice_core::memory::retention::{sweep, RetentionPolicy, MAX_REPLAY_AGE_SECS};
use lattice_core::memory::MemoryStore;
use lattice_core::query::QueryEngine;
use lattice_core::storage::GraphStore;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::super::mcp::McpHandler;
use super::{call_args, parse_tool_payload, SchemaFixture};
use crate::rpc::server::RequestHandler;

async fn call(fixture: &SchemaFixture, name: &str, arguments: Value) -> Value {
    call_handler(&fixture.handler, name, arguments).await
}

async fn call_handler(handler: &McpHandler, name: &str, arguments: Value) -> Value {
    let response = handler
        .handle("tools/call", call_args(name, arguments))
        .await
        .unwrap_or_else(|error| panic!("{name} failed: {error:?}"));
    parse_tool_payload(&response)
}

fn reopened_handler(fixture: &SchemaFixture, memory_path: &std::path::Path) -> McpHandler {
    let memory_store = Arc::new(Mutex::new(
        MemoryStore::open(memory_path).expect("reopened memory store"),
    ));
    let event_store = Arc::new(EventStore::open_in_memory().expect("reopened events"));
    let event_writer = Arc::new(
        EventWriter::new(
            event_store,
            fixture.workspace_root.to_string_lossy().to_string(),
            4096,
        )
        .with_flush_policy(FlushPolicy::Sync),
    );
    McpHandler::new(
        Arc::new(Mutex::new(QueryEngine::new(CodeGraph::new(), None))),
        Arc::new(Mutex::new(Indexer::new(fixture.workspace_root.clone()))),
        memory_store,
        Arc::new(Mutex::new(
            GraphStore::open_in_memory().expect("reopened graph store"),
        )),
        Arc::new(std::sync::OnceLock::new()),
        None,
        fixture.workspace_root.clone(),
        fixture.context_cache_path.clone(),
        fixture.session_id.clone(),
        None,
        vec![fixture.workspace_root.clone()],
        Arc::new(AtomicBool::new(false)),
        Some(event_writer),
        Vec::new(),
        Vec::new(),
    )
}

async fn seed_memory_handle(fixture: &SchemaFixture, content: &str) -> (String, String) {
    let saved = call(
        fixture,
        "remember",
        json!({
            "kind":"quick", "content":content, "scope":"repo",
            "memory_class":"constraint", "assertion_type":"constraint",
            "confidence":1.0,
            "confidence_reason":"context lifecycle public fixture",
            "freshness_policy":"repo_scoped"
        }),
    )
    .await;
    let memory_id = saved["memory_id"].as_str().expect("memory id").to_owned();
    let prepared = call(
        fixture,
        "prepare_change",
        json!({
            "task":"resolve the context lifecycle lesson", "render":"json", "max_tokens":4000
        }),
    )
    .await;
    let handle = prepared["context_handle"]
        .as_str()
        .expect("context handle")
        .to_owned();
    (memory_id, format!("{handle}\nmemory:0"))
}

fn split_handle_focus(encoded: &str) -> (&str, &str) {
    encoded.split_once('\n').expect("handle/focus encoding")
}

#[tokio::test]
async fn memory_expand_resolves_current_canonical_record_and_renews_only_after_ack() {
    let fixture = SchemaFixture::new("context-memory-current");
    let content = "canonical context lifecycle lesson payload-sentinel-current";
    let (memory_id, encoded) = seed_memory_handle(&fixture, content).await;
    let (handle, _) = split_handle_focus(&encoded);
    // The fully-qualified public focus must identify this reference exactly;
    // it must never be classified as a symbol and fall back to slot zero.
    let focus = format!(
        "memory:repository:{}:{}",
        fixture.workspace_root.display(),
        memory_id
    );
    let before: Option<i64> = fixture
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
        .expect("read pre-ack clock");
    assert_eq!(before, None);
    let expanded = call(
        &fixture,
        "context",
        json!({"mode":"expand","handle":handle,"focus":focus,"max_tokens":4000}),
    )
    .await;
    assert!(expanded.to_string().contains(content));
    let receipt = expanded["memory_deliveries"]
        .as_array()
        .and_then(|items| items.first())
        .expect("expand delivery receipt");
    assert_eq!(
        fixture
            .memory_store
            .lock()
            .await
            .with_connection(|connection| {
                connection
                    .query_row(
                        "SELECT last_recalled_at FROM memories WHERE id=?1",
                        [&memory_id],
                        |row| row.get::<_, Option<i64>>(0),
                    )
                    .map_err(|error| lattice_core::LatticeError::Storage(error.to_string()))
            })
            .unwrap(),
        None,
        "expansion alone must not renew retention"
    );
    let acknowledged = call(
        &fixture,
        "recall",
        json!({
            "mode":"acknowledge_delivery", "authority":receipt["authority"],
            "delivery_id":receipt["delivery_id"], "payload_hash":receipt["payload_hash"]
        }),
    )
    .await;
    assert_eq!(acknowledged["acknowledged_count"].as_u64(), Some(1));
    assert!(
        fixture
            .memory_store
            .lock()
            .await
            .with_connection(|connection| {
                connection
                    .query_row(
                        "SELECT last_recalled_at FROM memories WHERE id=?1",
                        [&memory_id],
                        |row| row.get::<_, Option<i64>>(0),
                    )
                    .map_err(|error| lattice_core::LatticeError::Storage(error.to_string()))
            })
            .expect("read post-ack clock")
            .is_some(),
        "only an acknowledgement renews retention"
    );
}

#[tokio::test]
async fn invalidated_selected_memory_never_leaks_cached_payload_or_receipt() {
    let fixture = SchemaFixture::new("context-memory-invalidated");
    let content = "context lifecycle lesson payload-sentinel-invalidated";
    let (memory_id, encoded) = seed_memory_handle(&fixture, content).await;
    let (handle, focus) = split_handle_focus(&encoded);
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
        .expect_err("ineligible selected memory rejects expansion");
    assert!(error.1.contains("no longer available") || error.1.contains("retry"));
    let persisted = std::fs::read_to_string(&fixture.context_cache_path).expect("cache persists");
    assert!(
        !persisted.contains("payload-sentinel-invalidated"),
        "persisted cache stores references, not lesson payloads: {persisted}"
    );
}

#[tokio::test]
async fn superseded_selected_memory_is_rejected_without_substitution() {
    let fixture = SchemaFixture::new("context-memory-superseded");
    let content = "context lifecycle lesson payload-sentinel-superseded";
    let (memory_id, encoded) = seed_memory_handle(&fixture, content).await;
    let (handle, focus) = split_handle_focus(&encoded);
    fixture
        .memory_store
        .lock()
        .await
        .mark_memory_superseded(&memory_id, "replacement-memory-id")
        .expect("canonical supersession");

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
        .expect_err("superseded selected memory rejects expansion");
    assert!(
        error.1.contains("no longer available") || error.1.contains("retry"),
        "{error:?}"
    );
    assert!(
        !error.1.contains(content),
        "must not expose the cached lesson"
    );
}

#[tokio::test]
async fn selected_memory_for_a_different_branch_is_rejected_under_current_authority() {
    let fixture = SchemaFixture::new("context-memory-wrong-branch");
    let content = "context lifecycle lesson payload-sentinel-wrong-branch";
    let (memory_id, encoded) = seed_memory_handle(&fixture, content).await;
    let (handle, focus) = split_handle_focus(&encoded);
    fixture
        .memory_store
        .lock()
        .await
        .with_connection(|connection| {
            connection
                .execute(
                    "UPDATE memories SET scope='branch',branch='release/foreign' WHERE id=?1",
                    [&memory_id],
                )
                .map_err(|error| lattice_core::LatticeError::Storage(error.to_string()))?;
            Ok(())
        })
        .expect("trusted fixture moves canonical memory to another branch");
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
        .expect_err("branch-scoped memory must not cross into main");
    assert!(
        error.1.contains("no longer available") || error.1.contains("retry"),
        "{error:?}"
    );
    assert!(
        !error.1.contains(content),
        "foreign branch payload must not leak"
    );
}

#[tokio::test]
async fn graph_navigation_expands_when_a_cached_memory_reference_is_unavailable() {
    let fixture = SchemaFixture::new("context-memory-navigation");
    let (memory_id, encoded) = seed_memory_handle(
        &fixture,
        "context lifecycle lesson payload-sentinel-navigation",
    )
    .await;
    let (handle, _) = split_handle_focus(&encoded);
    fixture
        .memory_store
        .lock()
        .await
        .invalidate(&memory_id)
        .expect("canonical invalidation");

    let expanded = call(
        &fixture,
        "context",
        json!({"mode":"expand","handle":handle,"focus":"file:src/navigation.rs","max_tokens":4000}),
    )
    .await;
    assert_eq!(expanded["focus_type"].as_str(), Some("file"), "{expanded}");
    assert!(
        expanded["memory_deliveries"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "graph navigation must not produce a memory receipt: {expanded}"
    );
}

#[tokio::test]
async fn oversized_current_lesson_rejects_before_creating_a_delivery_receipt() {
    let fixture = SchemaFixture::new("context-memory-overbudget");
    let content = format!(
        "context lifecycle lesson {} payload-sentinel-overbudget",
        "complete-canonical-content ".repeat(180)
    );
    let (_memory_id, encoded) = seed_memory_handle(&fixture, &content).await;
    let (handle, focus) = split_handle_focus(&encoded);
    let before: i64 = fixture
        .memory_store
        .lock()
        .await
        .with_connection(|connection| {
            connection
                .query_row("SELECT COUNT(*) FROM memory_deliveries", [], |row| {
                    row.get(0)
                })
                .map_err(|error| lattice_core::LatticeError::Storage(error.to_string()))
        })
        .expect("delivery count before rejection");
    let error = fixture
        .handler
        .handle(
            "tools/call",
            call_args(
                "context",
                json!({"mode":"expand","handle":handle,"focus":focus,"max_tokens":200}),
            ),
        )
        .await
        .expect_err("an incomplete memory payload must not be delivered under a small budget");
    assert!(
        error.1.contains("cannot fit") || error.1.contains("larger max_tokens"),
        "{error:?}"
    );
    let after: i64 = fixture
        .memory_store
        .lock()
        .await
        .with_connection(|connection| {
            connection
                .query_row("SELECT COUNT(*) FROM memory_deliveries", [], |row| {
                    row.get(0)
                })
                .map_err(|error| lattice_core::LatticeError::Storage(error.to_string()))
        })
        .expect("delivery count after rejection");
    assert_eq!(
        after, before,
        "a failed fit check must not attempt a delivery"
    );
}

#[tokio::test]
async fn purged_memory_reference_stays_unavailable_after_cache_and_store_reopen() {
    let (fixture, memory_path) = SchemaFixture::new_disk_backed("context-memory-purge-reopen");
    let content = "context lifecycle lesson payload-sentinel-purge-reopen";
    let (memory_id, encoded) = seed_memory_handle(&fixture, content).await;
    let (handle, focus) = split_handle_focus(&encoded);
    {
        let store = fixture.memory_store.lock().await;
        store.with_connection(|connection| {
            connection.execute(
                "UPDATE memories SET created_at=1,last_recalled_at=NULL,retention_grace_until=0 WHERE id=?1",
                [&memory_id],
            ).map_err(|error| lattice_core::LatticeError::Storage(error.to_string()))?;
            let policy = RetentionPolicy {
                stale_after_secs: 1,
                purge_after_secs: 2,
                sweep_interval_secs: 1,
                receipt_retention_secs: MAX_REPLAY_AGE_SECS,
                max_receipts: 100,
                batch_size: 8,
            };
            let report = sweep(connection, 10_000, &policy)?;
            assert_eq!(report.purged, 1, "controlled sweep must fully purge the selected memory");
            Ok(())
        }).expect("controlled canonical retention sweep");
    }
    let reopened = reopened_handler(&fixture, &memory_path);
    let error = reopened
        .handle(
            "tools/call",
            call_args(
                "context",
                json!({"mode":"expand","handle":handle,"focus":focus,"max_tokens":4000}),
            ),
        )
        .await
        .expect_err("purged memory reference must remain fenced after restart");
    assert!(
        error.1.contains("no longer available") || error.1.contains("retry"),
        "{error:?}"
    );
    let cache = std::fs::read_to_string(&fixture.context_cache_path).expect("persisted cache");
    assert!(
        !cache.contains("payload-sentinel-purge-reopen"),
        "cache must never resurrect payload"
    );
    let reopened_store = MemoryStore::open(&memory_path).expect("reopen canonical store");
    let (missing, receipt): (bool, bool) = reopened_store
        .with_connection(|connection| {
            let missing = connection
                .query_row(
                    "SELECT NOT EXISTS(SELECT 1 FROM memories WHERE id=?1)",
                    [&memory_id],
                    |row| row.get(0),
                )
                .map_err(|error| lattice_core::LatticeError::Storage(error.to_string()))?;
            let receipt = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM memory_deletion_receipts WHERE memory_id=?1)",
                    [&memory_id],
                    |row| row.get(0),
                )
                .map_err(|error| lattice_core::LatticeError::Storage(error.to_string()))?;
            Ok((missing, receipt))
        })
        .expect("inspect purged canonical state");
    assert!(missing && receipt, "purge must preserve a deletion fence");
}

#[tokio::test]
async fn invalid_memory_slot_is_rejected_without_falling_back_to_another_lesson() {
    let fixture = SchemaFixture::new("context-memory-invalid-slot");
    let (_memory_id, encoded) = seed_memory_handle(&fixture, "only eligible lesson").await;
    let (handle, _) = split_handle_focus(&encoded);
    let error = fixture
        .handler
        .handle(
            "tools/call",
            call_args(
                "context",
                json!({"mode":"expand","handle":handle,"focus":"memory:999","max_tokens":4000}),
            ),
        )
        .await
        .expect_err("invalid memory index must not substitute another cached lesson");
    assert_eq!(error.0, -32602);
    assert!(!error.1.contains("only eligible lesson"));
}
