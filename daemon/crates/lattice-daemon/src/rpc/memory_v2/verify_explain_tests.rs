use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use lattice_core::events::{EventStore, EventWriter, FlushPolicy};
use lattice_core::graph::CodeGraph;
use lattice_core::indexer::Indexer;
use lattice_core::memory::MemoryEvidence;
use lattice_core::memory::{
    Memory, MemoryLinkRecord, MemoryScope, MemoryStore, MemoryStructuredFields, MemoryType,
    MemoryVerificationStatus,
};
use lattice_core::query::QueryEngine;
use lattice_core::storage::graph_store::{
    FileIndexEntry, FILE_INDEX_PARSER_VERSION, FILE_INDEX_SCHEMA_VERSION,
};
use lattice_core::storage::GraphStore;
use lattice_core::verification::VerificationStatus;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::super::mcp::McpHandler;
use super::super::server::RequestHandler;
use super::{list_memory_conflicts, verify_explain_memory};

#[test]
fn serde_round_trips_verify_and_conflict_requests_and_responses() {
    let verify_args = verify_explain_memory::VerifyExplainArgs {
        memory_id: serde_json::from_value(json!({
            "workspace_id": "workspace-main",
            "ulid": "mem-1"
        }))
        .expect("memory id"),
        mode: verify_explain_memory::VerifyExplainMode::VerifyAndExplain,
        render_mode: verify_explain_memory::VerifyExplainRenderMode::Diagnostic,
    };
    round_trip(&verify_args);

    let verify_response = verify_explain_memory::VerifyExplainResponse {
        status: VerificationStatus::Verified,
        checks: vec![verify_explain_memory::CheckResult {
            kind: "linked_file_exists".to_string(),
            target: "src/auth.rs".to_string(),
            outcome: verify_explain_memory::CheckOutcome::Passed,
            evidence_ref: "linked_file:src/auth.rs".to_string(),
            detail: "linked file `src/auth.rs` still exists".to_string(),
        }],
        confidence_delta: 0.25,
        expansion_handle: "ctx-handle".to_string(),
        summary_lines: vec!["all verification checks passed".to_string()],
        render_mode: verify_explain_memory::VerifyExplainRenderMode::Full,
        diagnostic_trace: None,
    };
    round_trip(&verify_response);

    let conflict_args = list_memory_conflicts::ListMemoryConflictsArgs {
        anchor: serde_json::from_value(json!({
            "workspace_id": "workspace-main",
            "ulid": "mem-1"
        }))
        .expect("anchor"),
        render_mode: verify_explain_memory::VerifyExplainRenderMode::Full,
        limit: 25,
        cursor: Some(0),
    };
    round_trip(&conflict_args);

    let conflict_response = list_memory_conflicts::ListMemoryConflictsResponse {
        anchor: "memory:workspace-main:mem-1".to_string(),
        conflicts: vec![list_memory_conflicts::ConflictRecord {
            source: "memory:source".to_string(),
            target: "memory:target".to_string(),
            link_type: "contradicts".to_string(),
            link_strength: 1.0,
            created_by: "system".to_string(),
            created_at: 1,
            link_verification_status: VerificationStatus::Contradicted,
            reason: "conflict".to_string(),
        }],
        total: 1,
        next_cursor: None,
        render_mode: verify_explain_memory::VerifyExplainRenderMode::Full,
        summary_lines: vec!["memory:source contradicts memory:target (conflict)".to_string()],
    };
    round_trip(&conflict_response);
}

#[tokio::test]
async fn verify_explain_memory_uses_phase7_status_taxonomy_only() {
    assert_eq!(
        VerificationStatus::VALUES,
        &[
            "verified",
            "unverified",
            "in_review",
            "stale",
            "contradicted",
            "superseded",
            "expired",
            "invalidated",
        ]
    );

    let (
        handler,
        memory_store,
        _event_store,
        indexer,
        graph_store,
        workspace_root,
        context_cache_path,
    ) = build_handler("taxonomy");
    seed_indexed_file(
        &workspace_root,
        &indexer,
        &graph_store,
        "src/auth.rs",
        "fn refresh_token() {}\n",
    )
    .await;
    let memory_id = {
        let store = memory_store.lock().await;
        let id = store
            .store(seed_memory(
                "refresh token exists",
                MemoryScope::Repo,
                &workspace_root.to_string_lossy(),
            ))
            .expect("store memory");
        let mut fields = MemoryStructuredFields::default();
        fields.evidence = vec![MemoryEvidence {
            kind: "file".to_string(),
            reference: Some("src/auth.rs".to_string()),
            detail: None,
            captured_at: Some(1),
            span: None,
            evidence_content_hash: None,
        }];
        store
            .update_structured_fields(&id, &fields)
            .expect("update fields");
        id
    };

    let response = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "verify_explain_memory",
                "arguments": {
                    "memory_id": {
                        "workspace_id": workspace_root.to_string_lossy(),
                        "ulid": memory_id
                    },
                    "mode": "verify_and_explain",
                    "render_mode": "full"
                }
            }),
        )
        .await
        .expect("verify succeeds");
    let payload = parse_tool_payload(&response);
    let status = payload["status"].as_str().expect("status string");
    assert!(VerificationStatus::VALUES.contains(&status));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn every_check_result_carries_an_evidence_reference() {
    let (
        handler,
        memory_store,
        _event_store,
        indexer,
        graph_store,
        workspace_root,
        context_cache_path,
    ) = build_handler("evidence-ref");
    seed_indexed_file(
        &workspace_root,
        &indexer,
        &graph_store,
        "src/auth.rs",
        "fn refresh_token() {}\n",
    )
    .await;
    let memory_id = {
        let store = memory_store.lock().await;
        let mut memory = seed_memory(
            "refresh token invariant",
            MemoryScope::Repo,
            &workspace_root.to_string_lossy(),
        );
        memory.linked_symbols = vec!["refresh_token".to_string()];
        let id = store.store(memory).expect("store memory");
        let mut fields = MemoryStructuredFields::default();
        fields.evidence = vec![
            MemoryEvidence {
                kind: "symbol".to_string(),
                reference: Some("refresh_token".to_string()),
                detail: None,
                captured_at: Some(1),
                span: None,
                evidence_content_hash: None,
            },
            MemoryEvidence {
                kind: "file".to_string(),
                reference: Some("src/auth.rs".to_string()),
                detail: None,
                captured_at: Some(1),
                span: None,
                evidence_content_hash: None,
            },
        ];
        store
            .update_structured_fields(&id, &fields)
            .expect("fields");
        id
    };

    let response = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "verify_explain_memory",
                "arguments": {
                    "memory_id": {
                        "workspace_id": workspace_root.to_string_lossy(),
                        "ulid": memory_id
                    },
                    "render_mode": "diagnostic"
                }
            }),
        )
        .await
        .expect("verify succeeds");
    let payload = parse_tool_payload(&response);
    for check in payload["checks"].as_array().expect("checks") {
        assert!(check["evidence_ref"]
            .as_str()
            .is_some_and(|value| !value.trim().is_empty()));
    }
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn deleted_linked_symbol_transitions_to_stale_with_linked_symbol_missing_failure() {
    let (
        handler,
        memory_store,
        _event_store,
        _indexer,
        _graph_store,
        workspace_root,
        context_cache_path,
    ) = build_handler("symbol-missing");
    let memory_id = {
        let store = memory_store.lock().await;
        let mut memory = seed_memory(
            "refresh token invariant",
            MemoryScope::Repo,
            &workspace_root.to_string_lossy(),
        );
        memory.linked_symbols = vec!["refresh_token".to_string()];
        let id = store.store(memory).expect("store memory");
        let mut fields = MemoryStructuredFields::default();
        fields.evidence = vec![MemoryEvidence {
            kind: "symbol".to_string(),
            reference: Some("refresh_token".to_string()),
            detail: None,
            captured_at: Some(1),
            span: None,
            evidence_content_hash: None,
        }];
        store
            .update_structured_fields(&id, &fields)
            .expect("fields");
        id
    };

    let response = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "verify_explain_memory",
                "arguments": {
                    "memory_id": {
                        "workspace_id": workspace_root.to_string_lossy(),
                        "ulid": memory_id
                    },
                    "render_mode": "full"
                }
            }),
        )
        .await
        .expect("verify succeeds");
    let payload = parse_tool_payload(&response);
    assert_eq!(payload["status"].as_str(), Some("stale"));
    assert!(payload["checks"].as_array().is_some_and(|checks| {
        checks.iter().any(|check| {
            check["kind"].as_str() == Some("linked_symbol_missing")
                && check["outcome"].as_str() == Some("failed")
        })
    }));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn list_memory_conflicts_returns_contradicts_and_supersedes_with_correct_directionality() {
    let (
        handler,
        memory_store,
        _event_store,
        _indexer,
        _graph_store,
        workspace_root,
        context_cache_path,
    ) = build_handler("conflicts");
    let (target_id, source_id) = {
        let store = memory_store.lock().await;
        let target = store
            .store(seed_memory(
                "target memory",
                MemoryScope::Repo,
                &workspace_root.to_string_lossy(),
            ))
            .expect("store target");
        let source = store
            .store(seed_memory(
                "source memory",
                MemoryScope::Repo,
                &workspace_root.to_string_lossy(),
            ))
            .expect("store source");
        store
            .insert_memory_link(&MemoryLinkRecord {
                link_id: "link-contradicts".to_string(),
                source_memory_id: source.clone(),
                target_memory_id: target.clone(),
                link_type: "contradicts".to_string(),
                reason: "new evidence disagrees".to_string(),
                created_at: 10,
                verification_status: "contradicted".to_string(),
            })
            .expect("insert contradicts");
        store
            .insert_memory_link(&MemoryLinkRecord {
                link_id: "link-supersedes".to_string(),
                source_memory_id: source.clone(),
                target_memory_id: target.clone(),
                link_type: "supersedes".to_string(),
                reason: "new evidence replaces old".to_string(),
                created_at: 11,
                verification_status: "superseded".to_string(),
            })
            .expect("insert supersedes");
        (target, source)
    };

    let response = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "list_memory_conflicts",
                "arguments": {
                    "anchor": {
                        "workspace_id": workspace_root.to_string_lossy(),
                        "ulid": target_id
                    },
                    "render_mode": "full"
                }
            }),
        )
        .await
        .expect("list conflicts succeeds");
    let payload = parse_tool_payload(&response);
    let conflicts = payload["conflicts"].as_array().expect("conflicts");
    assert!(conflicts.iter().any(|conflict| {
        conflict["link_type"].as_str() == Some("contradicts")
            && conflict["source"]
                .as_str()
                .is_some_and(|value| value.contains(source_id.as_str()))
            && conflict["target"]
                .as_str()
                .is_some_and(|value| value.contains(target_id.as_str()))
    }));
    assert!(conflicts.iter().any(|conflict| {
        conflict["link_type"].as_str() == Some("supersedes")
            && conflict["source"]
                .as_str()
                .is_some_and(|value| value.contains(source_id.as_str()))
            && conflict["target"]
                .as_str()
                .is_some_and(|value| value.contains(target_id.as_str()))
    }));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn scope_leak_negative_paths_return_explicit_errors() {
    let (
        handler,
        memory_store,
        _event_store,
        _indexer,
        _graph_store,
        workspace_root,
        context_cache_path,
    ) = build_handler("scope-leak");
    let (out_of_scope_id, in_scope_id) = {
        let store = memory_store.lock().await;
        let mut out_of_scope = seed_memory(
            "other branch",
            MemoryScope::Branch,
            &workspace_root.to_string_lossy(),
        );
        out_of_scope.branch = Some("different-branch".to_string());
        let out_of_scope_id = store.store(out_of_scope).expect("store out of scope");

        let in_scope_id = store
            .store(seed_memory(
                "in scope",
                MemoryScope::Repo,
                &workspace_root.to_string_lossy(),
            ))
            .expect("store in scope");
        let mut fields = MemoryStructuredFields::default();
        fields.contradicted_by_memory_ids = vec![out_of_scope_id.clone()];
        store
            .update_structured_fields(&in_scope_id, &fields)
            .expect("fields");
        (out_of_scope_id, in_scope_id)
    };

    let verify = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "verify_explain_memory",
                "arguments": {
                    "memory_id": {
                        "workspace_id": workspace_root.to_string_lossy(),
                        "ulid": out_of_scope_id
                    }
                }
            }),
        )
        .await;
    assert!(verify.is_err());

    let conflicts = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "list_memory_conflicts",
                "arguments": {
                    "anchor": {
                        "workspace_id": workspace_root.to_string_lossy(),
                        "ulid": in_scope_id
                    }
                }
            }),
        )
        .await;
    assert!(conflicts.is_err());
    cleanup_paths(&workspace_root, &context_cache_path);
}

fn round_trip<T>(value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).expect("serialize");
    let restored: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(&restored, value);
}

fn build_handler(
    suffix: &str,
) -> (
    McpHandler,
    Arc<Mutex<MemoryStore>>,
    Arc<EventStore>,
    Arc<Mutex<Indexer>>,
    Arc<Mutex<GraphStore>>,
    PathBuf,
    PathBuf,
) {
    let workspace_root = unique_test_path(&format!("lattice-verify-explain-{suffix}"));
    std::fs::create_dir_all(&workspace_root).expect("workspace dir");
    let context_cache_path = workspace_root.join("context_handles.json");
    let memory_store = Arc::new(Mutex::new(
        MemoryStore::open_in_memory().expect("memory store"),
    ));
    let graph_store = Arc::new(Mutex::new(
        GraphStore::open_in_memory().expect("graph store"),
    ));
    let indexer = Arc::new(Mutex::new(Indexer::new(workspace_root.clone())));
    let event_store = Arc::new(EventStore::open_in_memory().expect("event store"));
    let event_writer = Arc::new(
        EventWriter::new(
            event_store.clone(),
            workspace_root.to_string_lossy().to_string(),
            4096,
        )
        .with_flush_policy(FlushPolicy::Sync),
    );
    let handler = McpHandler::new(
        Arc::new(Mutex::new(QueryEngine::new(CodeGraph::new(), None, None))),
        indexer.clone(),
        memory_store.clone(),
        graph_store.clone(),
        Arc::new(std::sync::OnceLock::new()),
        None,
        workspace_root.clone(),
        context_cache_path.clone(),
        format!("session-test-{suffix}"),
        None,
        vec![workspace_root.clone()],
        Arc::new(AtomicBool::new(false)),
        Some(event_writer),
        Vec::new(),
        Vec::new(),
    );
    (
        handler,
        memory_store,
        event_store,
        indexer,
        graph_store,
        workspace_root,
        context_cache_path,
    )
}

async fn seed_indexed_file(
    workspace_root: &PathBuf,
    indexer: &Arc<Mutex<Indexer>>,
    graph_store: &Arc<Mutex<GraphStore>>,
    rel_path: &str,
    content: &str,
) {
    let path = workspace_root.join(rel_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent dir");
    }
    std::fs::write(&path, content).expect("write file");
    let mut indexer = indexer.lock().await;
    indexer
        .index_file_content(rel_path, content)
        .expect("index content");
    let graph = indexer.graph().clone();
    drop(indexer);

    let graph_store = graph_store.lock().await;
    graph_store.save_graph(&graph).expect("save graph");
    graph_store
        .save_file_index(&[FileIndexEntry {
            file: rel_path.to_string(),
            content_hash: "hash".to_string(),
            mtime_ns: 1,
            size_bytes: content.len() as i64,
            parser_version: FILE_INDEX_PARSER_VERSION,
            schema_version: FILE_INDEX_SCHEMA_VERSION,
            last_indexed_at: 10,
        }])
        .expect("save file index");
}

fn seed_memory(content: &str, scope: MemoryScope, workspace_id: &str) -> Memory {
    Memory {
        id: String::new(),
        session_id: "session-seed".to_string(),
        content: content.to_string(),
        memory_type: MemoryType::Observation,
        scope,
        confidence: 0.81,
        linked_symbols: Vec::new(),
        linked_files: vec!["src/auth.rs".to_string()],
        workspace_id: Some(workspace_id.to_string()),
        branch: None,
        scope_organization_id: None,
        refresh_key: None,
        source_query: Some("refresh token".to_string()),
        created_at: 1,
        last_accessed: 0,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: MemoryVerificationStatus::Unverified,
    }
}

fn parse_tool_payload(response: &Value) -> Value {
    let text = response["content"][0]["text"].as_str().expect("tool text");
    serde_json::from_str(text).expect("tool payload json")
}

fn cleanup_paths(workspace_root: &PathBuf, context_cache_path: &PathBuf) {
    let _ = std::fs::remove_file(context_cache_path);
    let _ = std::fs::remove_dir_all(workspace_root);
}

fn unique_test_path(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{nanos}"))
}
