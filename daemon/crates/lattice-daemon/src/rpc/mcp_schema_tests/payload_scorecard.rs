//! Public workflow payload-budget regressions.
//!
//! Size is measured from the text delivered by `tools/call`, after workflow
//! shaping, pruning, wire-format selection, and rendering. Core ranking-report
//! size is intentionally outside this contract.

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
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::super::mcp::McpHandler;
use super::super::server::RequestHandler;
use super::{call_args, parse_tool_payload};

const PUBLIC_ULTRA_COMPACT_BYTES: usize = 2_000;

#[tokio::test]
async fn public_workflow_scorecard_measures_delivered_markdown_and_budgeted_json() {
    let fixture = PayloadFixture::new("public-payload-scorecard");
    let cases = [
        (
            "prepare_change",
            "prepare_change",
            json!({
                "task": "fix memory recall for a new session",
                "entry_files": ["daemon/crates/lattice-core/src/memory/store.rs"],
                "entry_symbols": ["search_across_sessions"]
            }),
        ),
        (
            "impact",
            "impact_from_diff",
            json!({
                "direction": "diff",
                "target": "src/session.rs",
                "diff": "diff --git a/src/session.rs b/src/session.rs\n--- a/src/session.rs\n+++ b/src/session.rs\n@@ -1 +1 @@\n-old\n+new\n",
                "files": ["src/session.rs"],
                "symbols": ["restore_session"]
            }),
        ),
        (
            "context",
            "get_working_set_context",
            json!({
                "mode": "working_set",
                "query": "memory recall regression",
                "files": ["daemon/crates/lattice-core/src/memory/store.rs"],
                "symbols": ["search_across_sessions"]
            }),
        ),
        (
            "diagnose",
            "diagnose_failure",
            json!({
                "failure_text": "src/session.rs:44:9 error: restore_session failed during recall",
                "kind": "runtime"
            }),
        ),
    ];

    for (tool, expected_origin, arguments) in cases {
        let markdown = delivered_text(&fixture, tool, arguments.clone()).await;
        assert!(markdown.starts_with("### Summary\n"), "{tool}: {markdown}");
        assert!(
            markdown.len() <= PUBLIC_ULTRA_COMPACT_BYTES,
            "{tool} default Markdown delivered {} bytes",
            markdown.len()
        );
        assert!(!markdown.contains("### Structured Payload"));
        assert!(!markdown.contains("```json"));
        assert!(!markdown.contains("lattice-metrics"));
        assert!(markdown.matches("- Next action:").count() <= 1);

        for (budget, max_tokens) in [("tiny", 260_u64), ("compact", 500_u64)] {
            let mut json_arguments = arguments.clone();
            let object = json_arguments.as_object_mut().expect("workflow arguments");
            object.insert("render".into(), json!("json"));
            object.insert("budget".into(), json!(budget));
            object.insert("max_tokens".into(), json!(max_tokens));
            object.insert("wire_format".into(), json!("standard"));

            let text = delivered_text(&fixture, tool, json_arguments).await;
            let payload: Value = serde_json::from_str(&text)
                .unwrap_or_else(|error| panic!("{tool} {budget} JSON: {error}: {text}"));
            assert_eq!(
                field(&payload, "budget_max_tokens", "bmt").and_then(Value::as_u64),
                Some(max_tokens),
                "{tool} {budget}: {payload}"
            );
            assert!(
                field(&payload, "approx_tokens", "apt")
                    .and_then(Value::as_u64)
                    .is_some_and(|tokens| tokens <= max_tokens),
                "{tool} {budget} exceeded its reported cap: {payload}"
            );
            assert!(
                text.len().div_ceil(4) <= max_tokens as usize,
                "{tool} {budget} delivered {} bytes ({} estimated tokens)",
                text.len(),
                text.len().div_ceil(4)
            );
            assert!(nonempty_string(&payload, "context_handle", "h"));
            assert_eq!(
                field(&payload, "context_origin", "o").and_then(Value::as_str),
                Some(expected_origin)
            );
            assert!(field(&payload, "truncated", "tr").is_some_and(Value::is_boolean));
            assert!(!text.contains("### Structured Payload"));
            assert!(!text.contains("lattice-metrics"));
        }
    }
}

#[tokio::test]
async fn compact_json_keeps_a_matching_lesson_and_delivery_receipt() {
    let fixture = PayloadFixture::new("public-payload-memory");
    save_matching_memory(&fixture).await;

    let text = delivered_text(
        &fixture,
        "prepare_change",
        json!({
            "query": "preserve the quasar receipt invariant",
            "render": "json",
            "budget": "compact",
            "max_tokens": 850,
            "wire_format": "standard"
        }),
    )
    .await;
    let payload: Value = serde_json::from_str(&text).expect("workflow JSON");
    assert!(
        text.len().div_ceil(4) <= 850,
        "compact JSON exceeded 850 tokens"
    );
    let highlights = field(&payload, "memory_highlights", "mh")
        .unwrap_or(&Value::Null)
        .as_array()
        .unwrap_or_else(|| panic!("memory highlights: {payload}"));
    let delivered = highlights
        .iter()
        .find(|item| {
            item["content"]
                .as_str()
                .is_some_and(|content| content.contains("quasar receipt invari"))
        })
        .unwrap_or_else(|| panic!("matching lesson survives compact projection: {payload}"));
    let receipts = payload["memory_deliveries"]
        .as_array()
        .filter(|receipts| !receipts.is_empty())
        .expect("delivery receipt");
    let id = delivered["memory_id"]
        .as_str()
        .or_else(|| delivered["id"].as_str())
        .or_else(|| delivered["memory_id"]["ulid"].as_str())
        .or_else(|| delivered["id"]["ulid"].as_str())
        .expect("memory id");
    let projection = json!([{"id": id, "content": delivered["content"]}]);
    let expected_hash = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&projection).expect("projection"))
    );
    assert_eq!(
        receipts[0]["payload_hash"].as_str(),
        Some(expected_hash.as_str())
    );

    let full_text = delivered_text(
        &fixture,
        "prepare_change",
        json!({
            "task": "preserve the quasar receipt invariant",
            "render": "json",
            "budget": "full",
            "max_tokens": 4_000,
            "wire_format": "standard"
        }),
    )
    .await;
    let full: Value = serde_json::from_str(&full_text).expect("full workflow JSON");
    assert!(full_text.len().div_ceil(4) <= 4_000);
    assert!(full.get("structured_payload").is_some(), "{full}");
    assert_eq!(full["truncated"], false, "{full}");
    assert!(full["memory_deliveries"]
        .as_array()
        .is_some_and(|v| !v.is_empty()));

    let tiny_text = delivered_text(
        &fixture,
        "prepare_change",
        json!({
            "task": "preserve the quasar receipt invariant",
            "render": "json",
            "budget": "tiny",
            "max_tokens": 260,
            "wire_format": "standard"
        }),
    )
    .await;
    assert!(tiny_text.len().div_ceil(4) <= 260);
    let tiny: Value = serde_json::from_str(&tiny_text).expect("tiny workflow JSON");
    let tiny_highlights = field(&tiny, "memory_highlights", "mh")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let tiny_receipts = tiny["memory_deliveries"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    assert!(
        !tiny_highlights.is_empty() || tiny_receipts.is_empty(),
        "a dropped lesson must not leave a delivery receipt: {tiny}"
    );
}

#[tokio::test]
async fn full_json_delivers_the_complete_fixture_lesson_or_no_lesson_at_all() {
    const LESSON: &str = "Revision v2 replaces the earlier export rule: exclude archived rows while preserving remaining order and caller input.";
    let fixture = PayloadFixture::new("public-payload-complete-lesson");
    let memory_id = save_memory(&fixture, LESSON).await;

    let text = delivered_text(
        &fixture,
        "prepare_change",
        json!({
            "task": "exclude archived rows while preserving remaining order and caller input",
            "render": "json",
            "budget": "full",
            "wire_format": "standard"
        }),
    )
    .await;
    let payload: Value = serde_json::from_str(&text).expect("full workflow JSON");
    assert!(text.len().div_ceil(4) <= 2_600, "{payload}");
    let delivered = rendered_memory(&payload, &memory_id)
        .unwrap_or_else(|| panic!("complete matching lesson must fit: {payload}"));
    assert_eq!(delivered["content"], LESSON, "{payload}");
    assert_receipt_binds(&payload, &memory_id, LESSON, &fixture);

    let markdown = delivered_text(
        &fixture,
        "prepare_change",
        json!({
            "task": "exclude archived rows while preserving remaining order and caller input",
            "render": "markdown",
            "budget": "full"
        }),
    )
    .await;
    assert!(markdown.contains(LESSON), "{markdown}");
    assert!(
        markdown.contains("### Memory delivery receipts"),
        "{markdown}"
    );
    assert!(!markdown.contains("earlier export r..."), "{markdown}");
    let markdown_receipts: Value = serde_json::from_str(
        markdown
            .split("### Memory delivery receipts\n```json\n")
            .nth(1)
            .and_then(|tail| tail.split("\n```").next())
            .expect("markdown receipt JSON"),
    )
    .expect("valid markdown receipt JSON");
    assert_receipt_binds(
        &json!({"memory_deliveries": markdown_receipts}),
        &memory_id,
        LESSON,
        &fixture,
    );

    let dense_text = delivered_text(
        &fixture,
        "prepare_change",
        json!({
            "task": "exclude archived rows while preserving remaining order and caller input",
            "render": "json",
            "budget": "full",
            "max_tokens": 2_600,
            "wire_format": "dense"
        }),
    )
    .await;
    let dense: Value = serde_json::from_str(&dense_text).expect("dense workflow JSON");
    let dense_delivered = rendered_memory(&dense, &memory_id)
        .unwrap_or_else(|| panic!("complete dense lesson must fit: {dense}"));
    assert_eq!(
        field(dense_delivered, "content", "ct"),
        Some(&json!(LESSON))
    );
    assert_receipt_binds(&dense, &memory_id, LESSON, &fixture);

    let tiny_text = delivered_text(
        &fixture,
        "prepare_change",
        json!({
            "task": "exclude archived rows while preserving remaining order and caller input",
            "render": "json",
            "budget": "tiny",
            "max_tokens": 260,
            "wire_format": "standard"
        }),
    )
    .await;
    let tiny: Value = serde_json::from_str(&tiny_text).expect("tiny workflow JSON");
    if let Some(delivered) = rendered_memory(&tiny, &memory_id) {
        assert_eq!(delivered["content"], LESSON, "{tiny}");
        assert_receipt_binds(&tiny, &memory_id, LESSON, &fixture);
    } else {
        assert!(
            tiny["memory_deliveries"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "dropped lesson left a ghost receipt: {tiny}"
        );
        assert!(!tiny_text.contains("Revision v2 replaces"), "{tiny}");
    }
}

#[tokio::test]
async fn canonical_reload_preserves_long_unicode_and_legitimate_ellipsis() {
    let fixture = PayloadFixture::new("public-payload-long-complete-lesson");
    let lesson = format!(
        "{} Keep the complete revision rule, including every Unicode boundary and its intentional ending...",
        "🧭資料境界".repeat(24)
    );
    assert!(lesson.chars().count() > 120);
    let memory_id = save_memory(&fixture, &lesson).await;
    let text = delivered_text(
        &fixture,
        "prepare_change",
        json!({
            "task": "Keep the complete revision rule Unicode boundary intentional ending",
            "render": "json",
            "budget": "full",
            "max_tokens": 2_600,
            "wire_format": "standard"
        }),
    )
    .await;
    let payload: Value = serde_json::from_str(&text).expect("workflow JSON");
    let delivered = rendered_memory(&payload, &memory_id)
        .unwrap_or_else(|| panic!("long complete lesson should fit: {payload}"));
    assert_eq!(delivered["content"], lesson, "{payload}");
    assert_receipt_binds(&payload, &memory_id, &lesson, &fixture);
}

#[tokio::test]
async fn accepted_token_boundaries_handle_long_unicode_without_panicking() {
    let fixture = PayloadFixture::new("public-payload-boundaries");
    let task = format!(
        "{} {}",
        "🧭資料".repeat(200),
        "src/非常に長い/経路/記憶.rs".repeat(40)
    );
    for max_tokens in [80_u64, 81, 219, 220, 221, 259, 260, 500, 850, 4_000] {
        let result = fixture
            .handler
            .handle(
                "tools/call",
                call_args(
                    "prepare_change",
                    json!({
                        "task": task,
                        "entry_files": ["src/非常に長い/経路/記憶.rs"],
                        "render": "json",
                        "budget": "tiny",
                        "max_tokens": max_tokens,
                        "wire_format": "standard"
                    }),
                ),
            )
            .await;
        match result {
            Ok(response) => {
                let text = response["content"][0]["text"].as_str().expect("text");
                assert!(text.len().div_ceil(4) <= max_tokens as usize);
                let payload: Value = serde_json::from_str(text).expect("valid JSON");
                assert!(nonempty_string(&payload, "context_handle", "h"));
            }
            Err((code, message)) => {
                assert_eq!(code, -32603);
                assert!(message.contains("cannot fit the requested"), "{message}");
            }
        }
    }
}

async fn delivered_text(fixture: &PayloadFixture, tool: &str, arguments: Value) -> String {
    let response = fixture
        .handler
        .handle("tools/call", call_args(tool, arguments))
        .await
        .unwrap_or_else(|error| panic!("{tool} tools/call failed: {error:?}"));
    response["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{tool} returned no text: {response}"))
        .to_string()
}

async fn save_matching_memory(fixture: &PayloadFixture) {
    save_memory(
        fixture,
        "Always preserve the quasar receipt invariant during recall changes.",
    )
    .await;
}

async fn save_memory(fixture: &PayloadFixture, content: &str) -> String {
    let response = fixture
        .handler
        .handle(
            "tools/call",
            call_args(
                "remember",
                json!({
                    "kind": "quick",
                    "content": content,
                    "memory_class": "constraint",
                    "assertion_type": "constraint",
                    "type": "constraint",
                    "scope": "repo",
                    "confidence": 0.92,
                    "confidence_reason": "payload scorecard fixture",
                    "freshness_policy": "manual_review"
                }),
            ),
        )
        .await
        .expect("save matching memory");
    let payload = parse_tool_payload(&response);
    payload["memory_id"]
        .as_str()
        .unwrap_or_else(|| panic!("memory id: {payload}"))
        .to_string()
}

fn rendered_memory<'a>(value: &'a Value, memory_id: &str) -> Option<&'a Value> {
    fn visit<'a>(value: &'a Value, memory_id: &str) -> Option<&'a Value> {
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    if matches!(key.as_str(), "memory_highlights" | "memories" | "mh" | "mm") {
                        if let Some(found) = child.as_array().and_then(|items| {
                            items.iter().find(|item| {
                                field(item, "memory_id", "id")
                                    .and_then(|identity| {
                                        identity.as_str().or_else(|| identity["ulid"].as_str())
                                    })
                                    .is_some_and(|id| id == memory_id || id.ends_with(memory_id))
                            })
                        }) {
                            return Some(found);
                        }
                    }
                    if let Some(found) = visit(child, memory_id) {
                        return Some(found);
                    }
                }
                None
            }
            Value::Array(items) => items.iter().find_map(|item| visit(item, memory_id)),
            _ => None,
        }
    }
    visit(value, memory_id)
}

fn assert_receipt_binds(value: &Value, memory_id: &str, content: &str, fixture: &PayloadFixture) {
    let receipts = field(value, "memory_deliveries", "md")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .unwrap_or_else(|| panic!("delivery receipt: {value}"));
    let projection = json!([{"id": memory_id, "content": content}]);
    let expected_hash = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&projection).expect("projection"))
    );
    let expected_authority = format!("repository:{}", fixture.workspace_root.to_string_lossy());
    assert!(
        receipts.iter().any(|receipt| {
            field(receipt, "payload_hash", "ph").and_then(Value::as_str)
                == Some(expected_hash.as_str())
                && field(receipt, "authority", "a").and_then(Value::as_str)
                    == Some(expected_authority.as_str())
        }),
        "receipt does not bind complete lesson: {value}"
    );
}

struct PayloadFixture {
    handler: McpHandler,
    workspace_root: PathBuf,
}

impl PayloadFixture {
    fn new(suffix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace_root = std::env::temp_dir().join(format!("lattice-payload-{suffix}-{nanos}"));
        let refs = workspace_root.join(".git/refs/heads");
        std::fs::create_dir_all(&refs).expect("git refs");
        std::fs::write(workspace_root.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("git head");
        std::fs::write(
            refs.join("main"),
            "1111111111111111111111111111111111111111\n",
        )
        .expect("main ref");

        let memory_store = Arc::new(Mutex::new(MemoryStore::open_in_memory().expect("memory")));
        let event_store = Arc::new(EventStore::open_in_memory().expect("events"));
        let event_writer = Arc::new(
            EventWriter::new(
                event_store,
                workspace_root.to_string_lossy().to_string(),
                4096,
            )
            .with_flush_policy(FlushPolicy::Sync),
        );
        let context_cache = workspace_root.join("context_handles.json");
        let handler = McpHandler::new(
            Arc::new(Mutex::new(QueryEngine::new(CodeGraph::new(), None))),
            Arc::new(Mutex::new(Indexer::new(workspace_root.clone()))),
            memory_store,
            Arc::new(Mutex::new(
                GraphStore::open_in_memory().expect("graph store"),
            )),
            Arc::new(std::sync::OnceLock::new()),
            None,
            workspace_root.clone(),
            context_cache,
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
            workspace_root,
        }
    }
}

impl Drop for PayloadFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.workspace_root);
    }
}

fn nonempty_string(value: &Value, key: &str, dense_key: &str) -> bool {
    field(value, key, dense_key)
        .unwrap_or(&Value::Null)
        .as_str()
        .is_some_and(|field| !field.trim().is_empty())
}

fn field<'a>(value: &'a Value, key: &str, dense_key: &str) -> Option<&'a Value> {
    value.get(key).or_else(|| value.get(dense_key))
}
