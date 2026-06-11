use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use lattice_core::events::{
    EventPayload, EventQuery, EventReader, EventStore, EventWriter, FlushPolicy, QueryOrder,
};
use lattice_core::graph::CodeGraph;
use lattice_core::indexer::Indexer;
use lattice_core::memory::model::{MemoryAssertionType, MemoryProvenance};
use lattice_core::memory::{
    Memory, MemoryClass, MemoryEvidence, MemoryScope, MemoryStore, MemoryStructuredFields,
    MemoryType, MemoryVerificationStatus,
};
use lattice_core::query::QueryEngine;
use lattice_core::storage::GraphStore;
use lattice_core::working_memory::WorkingMemoryState;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::super::mcp::McpHandler;
use super::super::server::RequestHandler;
use super::{
    get_task_memory, propose_memory_evolution, save_memory, save_quick_memory, EvolutionAction,
    TaskMemoryBundle,
};

const TASK_ID: &str = "task-memory-v2";

#[test]
fn serde_round_trips_memory_tool_requests_and_responses() {
    let get_args = get_task_memory::GetTaskMemoryArgs {
        task_id: TASK_ID.to_string(),
        task_statement: Some("refresh auth memory".to_string()),
        intent_hint: Some("refresh auth memory".to_string()),
        focus_files: vec!["src/auth.rs".to_string()],
        focus_dirs: vec!["src".to_string()],
        budget_tokens: Some(800),
    };
    round_trip(&get_args);

    let quick_args = save_quick_memory::SaveQuickMemoryArgs {
        content: "remember the auth retry invariant".to_string(),
        task_id: Some(TASK_ID.to_string()),
        task_statement: Some("refresh auth memory".to_string()),
        memory_class: Some(MemoryClass::Procedure),
        scope: Some(save_memory::MemoryScopeArg::Session),
        confidence: Some(0.9),
        confidence_reason: None,
        linked_files: Vec::new(),
        linked_symbols: Vec::new(),
        linked_docs: Vec::new(),
        linked_tests: Vec::new(),
        linked_memories: Vec::new(),
        validity_conditions: Vec::new(),
        invalidation_triggers: Vec::new(),
        source_query: None,
        refresh_key: None,
        branch: None,
    };
    round_trip(&quick_args);

    let save_args = save_memory::SaveMemoryArgs {
        content: "Auth refresh invariant".to_string(),
        memory_class: MemoryClass::ArchitectureInvariant,
        assertion_type: Some(MemoryAssertionType::Constraint),
        scope: save_memory::MemoryScopeArg::Repo,
        confidence: 0.81,
        confidence_reason: "Derived from code and tests".to_string(),
        freshness_policy: save_memory::FreshnessPolicyArg::ManualReview,
        validity_conditions: vec!["auth router unchanged".to_string()],
        invalidation_triggers: vec!["refresh flow refactor".to_string()],
        provenance_event_ids: vec!["evt-1".to_string()],
        evidence: Vec::new(),
        linked_files: vec!["src/auth.rs".to_string()],
        linked_symbols: vec!["refresh_token".to_string()],
        linked_docs: vec!["docs/auth.md#Refresh".to_string()],
        linked_tests: vec!["tests/auth_refresh.rs::refresh_token".to_string()],
        linked_memories: vec!["mem-1".to_string()],
        source_query: Some("refresh token failures".to_string()),
        refresh_key: Some("auth::refresh".to_string()),
        branch: None,
        organization_id: None,
    };
    round_trip(&save_args);

    let proposal_args = propose_memory_evolution::ProposeMemoryEvolutionArgs {
        action: EvolutionAction::Propose,
        proposal_id: None,
        memory_id: Some("mem-1".to_string()),
        content: Some("Updated content".to_string()),
        linked_files: vec!["src/auth.rs".to_string()],
        linked_symbols: vec!["refresh_token".to_string()],
        linked_docs: vec!["docs/auth.md#Refresh".to_string()],
        linked_tests: vec!["tests/auth_refresh.rs::refresh_token".to_string()],
        linked_memories: vec!["mem-2".to_string()],
        validity_conditions: vec!["router still present".to_string()],
        invalidation_triggers: vec!["handler removed".to_string()],
        superseded_by_memory_id: None,
        invalidate_reason: None,
        reason: Some("refresh contract evolved".to_string()),
        decided_by: Some("assistant".to_string()),
    };
    round_trip(&proposal_args);

    let bundle = TaskMemoryBundle {
        task_id: TASK_ID.to_string(),
        checkpoint_id: Some(7),
        working_memory_verification_status: "verified".to_string(),
        memories: Vec::new(),
    };
    round_trip(&bundle);
}

#[tokio::test]
async fn get_task_memory_builds_implicit_state_from_task_statement_and_focus() {
    let (handler, _memory_store, _event_store, workspace_root, context_cache_path, _session_id) =
        build_handler("implicit-task-memory");
    let response = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "get_task_memory",
                "arguments": {
                    "task_id": "task-implicit",
                    "task_statement": "Investigate auth retry loop",
                    "focus_files": ["src/auth.rs"],
                    "focus_dirs": ["src"]
                }
            }),
        )
        .await
        .expect("tool succeeds");
    let payload = parse_tool_payload(&response);
    assert_eq!(payload["task_id"].as_str(), Some("task-implicit"));
    assert!(payload["memories"].as_array().is_some());
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn save_quick_memory_prefills_links_and_recent_failures_from_task_context() {
    let (handler, memory_store, _event_store, workspace_root, context_cache_path, _session_id) =
        build_handler("save-quick-memory");
    let mut state = WorkingMemoryState::new("Debug auth retry");
    state
        .active_failures
        .push(lattice_core::working_memory::FailureRecord {
            kind: "test".to_string(),
            message: "retry loop observed".to_string(),
            observed_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_secs(),
            evidence_refs: vec!["tests/test_auth.py::test_retry".to_string()],
        });
    handler
        .remember_working_memory_state_for_test("task-quick", state)
        .await;
    let response = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "save_quick_memory",
                "arguments": {
                    "content": "Retry loop root cause depends on middleware ordering.",
                    "task_id": "task-quick",
                    "linked_files": ["src/auth.rs"]
                }
            }),
        )
        .await
        .expect("save_quick_memory succeeds");
    let payload = parse_tool_payload(&response);
    let memory_id = payload["memory_id"].as_str().expect("memory id");
    let store = memory_store.lock().await;
    let fields = store
        .get_structured_fields(memory_id)
        .expect("fields query")
        .expect("fields exist");
    assert_eq!(fields.memory_class, MemoryClass::Observation);
    assert_eq!(fields.evidence.len(), 1);
    drop(store);
    assert_eq!(
        payload["memory"]["linked_files"]
            .as_array()
            .map(|items| items.len()),
        Some(1)
    );
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn get_task_memory_surfaces_inclusion_reason_and_verification_status() {
    let (handler, memory_store, event_store, workspace_root, context_cache_path, _session_id) =
        build_handler("get-task-memory");
    std::fs::create_dir_all(workspace_root.join("docs")).expect("docs dir");
    std::fs::write(
        workspace_root.join("docs/PX-0027-S01.md"),
        "PX-0027-S01 status: resolved and verified",
    )
    .expect("write resolved artifact");
    std::fs::write(
        workspace_root.join("docs/verify-PX-0027-S01.md"),
        "PX-0027-S01 status: blocked on current verification",
    )
    .expect("write blocked artifact");
    {
        let store = memory_store.lock().await;
        let mut memory = seed_memory("refresh token must preserve session", MemoryScope::Repo);
        memory.workspace_id = Some(workspace_root.to_string_lossy().to_string());
        let id = store.store(memory).expect("store memory");
        let mut fields = MemoryStructuredFields::default();
        fields.memory_class = MemoryClass::Constraint;
        fields.assertion_type = MemoryAssertionType::Constraint;
        fields.verification_status = MemoryVerificationStatus::Verified;
        fields.linked_docs = vec![
            "docs/PX-0027-S01.md".to_string(),
            "docs/verify-PX-0027-S01.md".to_string(),
        ];
        fields.evidence = vec![MemoryEvidence {
            kind: "test".to_string(),
            reference: Some("tests/auth.rs::refresh_token".to_string()),
            detail: Some("targeted refresh-token test".to_string()),
            captured_at: None,
            span: None,
            evidence_content_hash: None,
        }];
        fields.provenance = vec![MemoryProvenance {
            source: "git_head_oid".to_string(),
            reference: Some("abc1234".to_string()),
            captured_at: Some(42),
            note: Some("recorded checkout".to_string()),
        }];
        store
            .update_structured_fields(&id, &fields)
            .expect("update fields");
    }
    handler
        .remember_working_memory_state_for_test(
            TASK_ID,
            WorkingMemoryState::new("refresh token session diagnosis"),
        )
        .await;

    let response = handler
        .handle("lattice/tool_call",
            json!({"name": "get_task_memory", "arguments": {"task_id": TASK_ID, "intent_hint": "refresh token"}}),
        )
        .await
        .expect("tool succeeds");
    let payload = parse_tool_payload(&response);
    let memories = payload["memories"].as_array().expect("memories array");
    assert!(
        !memories.is_empty(),
        "expected at least one surfaced memory"
    );
    for memory in memories {
        assert!(memory["inclusion_reason"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
        assert!(memory["verification_status"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
        assert!(memory["trust_status"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
        assert!(memory["trust_reason"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
        assert!(memory["risk_domains"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item.as_str() == Some("security"))));
        assert!(memory["requires_reverification"].as_bool().is_some());
        assert!(memory["reverification_reason"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
        assert!(memory["checkout_state"]["status"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
        assert!(memory["recheck_commands"]
            .as_array()
            .is_some_and(|items| !items.is_empty()));
        let evidence_links = memory["evidence_links"]
            .as_array()
            .expect("evidence links array");
        assert!(evidence_links.iter().any(|link| {
            link["kind"].as_str() == Some("test")
                && link["reference"].as_str() == Some("tests/auth.rs::refresh_token")
                && link["recheck_command"]
                    .as_str()
                    .is_some_and(|command| command.contains("refresh_token"))
        }));
        assert!(evidence_links.iter().any(|link| {
            link["kind"].as_str() == Some("commit")
                && link["reference"].as_str() == Some("abc1234")
                && link["recheck_command"]
                    .as_str()
                    .is_some_and(|command| command.contains("git show --stat abc1234"))
        }));
        let artifact_conflicts = memory["artifact_conflicts"]
            .as_array()
            .expect("artifact conflicts array");
        assert!(!artifact_conflicts.is_empty());
        assert_eq!(artifact_conflicts[0]["key"].as_str(), Some("PX-0027-S01"));
        assert!(artifact_conflicts[0]["positive_refs"]
            .as_array()
            .is_some_and(|items| items
                .iter()
                .any(|item| item.as_str() == Some("docs/PX-0027-S01.md"))));
        assert!(artifact_conflicts[0]["negative_refs"]
            .as_array()
            .is_some_and(|items| items
                .iter()
                .any(|item| item.as_str() == Some("docs/verify-PX-0027-S01.md"))));
    }

    let events = read_events(&event_store, &workspace_root);
    assert!(events
        .iter()
        .any(|event| matches!(event.payload, EventPayload::MemoryRetrieved(_))));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn save_memory_defaults_unverified_persists_fields_emits_event_and_queues_verification() {
    let (handler, memory_store, event_store, workspace_root, context_cache_path, _session_id) =
        build_handler("save-memory");
    let response = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "save_memory",
                "arguments": {
                    "content": "Refresh token invariants rely on session scope",
                    "memory_class": "constraint",
                    "assertion_type": "constraint",
                    "scope": "repo",
                    "confidence": 0.74,
                    "confidence_reason": "Observed in handler and tests",
                    "freshness_policy": "manual_review",
                    "validity_conditions": ["session middleware stays enabled"],
                    "invalidation_triggers": ["refresh handler rewrite"],
                    "provenance_event_ids": ["evt-refresh"],
                    "linked_files": ["src/auth.rs"],
                    "linked_symbols": ["refresh_token"],
                    "linked_docs": ["docs/auth.md#Refresh"],
                    "linked_tests": ["tests/auth_refresh.rs::refresh_token"],
                    "linked_memories": ["mem-upstream"],
                    "source_query": "refresh token failures",
                    "refresh_key": "auth::refresh"
                }
            }),
        )
        .await
        .expect("save_memory succeeds");
    let payload = parse_tool_payload(&response);
    assert_eq!(
        payload["memory"]["verification_status"].as_str(),
        Some("unverified")
    );
    let memory_id = payload["memory_id"].as_str().expect("memory id");

    let store = memory_store.lock().await;
    let fields = store
        .get_structured_fields(memory_id)
        .expect("fields query")
        .expect("fields exist");
    assert_eq!(fields.memory_class, MemoryClass::Constraint);
    assert_eq!(fields.assertion_type, MemoryAssertionType::Constraint);
    assert_eq!(fields.linked_docs, vec!["docs/auth.md#Refresh".to_string()]);
    assert_eq!(
        fields.linked_tests,
        vec!["tests/auth_refresh.rs::refresh_token".to_string()]
    );
    assert_eq!(fields.linked_memories, vec!["mem-upstream".to_string()]);
    assert_eq!(
        fields.validity_conditions,
        vec!["session middleware stays enabled".to_string()]
    );
    assert_eq!(
        fields.invalidation_triggers,
        vec!["refresh handler rewrite".to_string()]
    );
    let queued_jobs: i64 = store
        .with_connection(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM verification_jobs WHERE target_memory_id = ?1 AND status = 'queued'",
                [memory_id],
                |row| row.get(0),
            )
            .map_err(|error| lattice_core::LatticeError::Storage(error.to_string()))
        })
        .expect("verification job count");
    assert_eq!(queued_jobs, 1);
    drop(store);

    let events = read_events(&event_store, &workspace_root);
    assert!(events
        .iter()
        .any(|event| matches!(event.payload, EventPayload::MemoryCreated(_))));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn propose_apply_and_reject_memory_evolution_are_auditable() {
    let (handler, memory_store, event_store, workspace_root, context_cache_path, _session_id) =
        build_handler("memory-evolution");
    let memory_id = {
        let store = memory_store.lock().await;
        let id = store
            .store(seed_memory("refresh behavior old", MemoryScope::Repo))
            .expect("store memory");
        let mut fields = MemoryStructuredFields::default();
        fields.memory_class = MemoryClass::Observation;
        store
            .update_structured_fields(&id, &fields)
            .expect("update fields");
        id
    };

    let propose = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "propose_memory_evolution",
                "arguments": {
                    "action": "propose",
                    "memory_id": memory_id,
                    "content": "refresh behavior new",
                    "linked_docs": ["docs/auth.md#Refresh"],
                    "reason": "refresh path changed"
                }
            }),
        )
        .await
        .expect("propose succeeds");
    let propose_payload = parse_tool_payload(&propose);
    let proposal_id = propose_payload["proposal_id"]
        .as_str()
        .expect("proposal id")
        .to_string();
    {
        let store = memory_store.lock().await;
        let current = store
            .get_by_id(&memory_id)
            .expect("memory query")
            .expect("memory exists");
        assert_eq!(current.content, "refresh behavior old");
        let record = store
            .with_connection(|conn| {
                lattice_core::consolidation::ConsolidationProposal::load_record(conn, &proposal_id)
                    .map_err(Into::into)
                    .and_then(|value| {
                        value.ok_or_else(|| {
                            lattice_core::LatticeError::Storage("missing proposal".to_string())
                        })
                    })
            })
            .expect("proposal record");
        assert_eq!(record.decision.as_str(), "pending");
    }

    let apply = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "propose_memory_evolution",
                "arguments": {
                    "action": "apply",
                    "proposal_id": proposal_id,
                    "reason": "ship updated refresh memory"
                }
            }),
        )
        .await
        .expect("apply succeeds");
    let apply_payload = parse_tool_payload(&apply);
    assert_eq!(apply_payload["decision"].as_str(), Some("applied"));
    {
        let store = memory_store.lock().await;
        let current = store
            .get_by_id(&memory_id)
            .expect("memory query")
            .expect("memory exists");
        assert_eq!(current.content, "refresh behavior new");
    }

    let reject_proposal = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "propose_memory_evolution",
                "arguments": {
                    "action": "propose",
                    "memory_id": memory_id,
                    "content": "refresh behavior rejected",
                    "reason": "candidate rejected"
                }
            }),
        )
        .await
        .expect("second propose succeeds");
    let reject_payload = parse_tool_payload(&reject_proposal);
    let reject_id = reject_payload["proposal_id"]
        .as_str()
        .expect("proposal id")
        .to_string();
    let reject = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "propose_memory_evolution",
                "arguments": {
                    "action": "reject",
                    "proposal_id": reject_id,
                    "reason": "do not accept"
                }
            }),
        )
        .await
        .expect("reject succeeds");
    let reject_payload = parse_tool_payload(&reject);
    assert_eq!(reject_payload["decision"].as_str(), Some("rejected"));

    let events = read_events(&event_store, &workspace_root);
    assert!(events
        .iter()
        .any(|event| matches!(event.payload, EventPayload::MemoryConsolidated(_))));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn get_task_memory_requires_workspace_and_concrete_task_signal() {
    let (handler, memory_store, _event_store, workspace_root, context_cache_path, _session_id) =
        build_handler("get-task-memory-signal");
    {
        let store = memory_store.lock().await;
        let mut portal_memory = seed_memory(
            "Portal remediation IU-0030 fixed product-profile launch claims.",
            MemoryScope::Repo,
        );
        portal_memory.workspace_id = Some(workspace_root.to_string_lossy().to_string());
        portal_memory.linked_files = vec![
            "docs/audit/2026-05-17-2248-remediation-run/04-final-remediation-review.md".to_string(),
        ];
        store.store(portal_memory).expect("store portal memory");

        let mut wrong_workspace = seed_memory(
            "Portal remediation IU-0030 from the wrong workspace should not leak.",
            MemoryScope::Repo,
        );
        wrong_workspace.workspace_id = Some("/home/pete/cadres/rmm".to_string());
        wrong_workspace.linked_files = vec![
            "docs/audit/2026-05-17-2248-remediation-run/04-final-remediation-review.md".to_string(),
        ];
        store
            .store(wrong_workspace)
            .expect("store wrong workspace memory");

        let mut unrelated = seed_memory("RMM marketplace cutover daemon state.", MemoryScope::Repo);
        unrelated.workspace_id = Some(workspace_root.to_string_lossy().to_string());
        unrelated.linked_files.clear();
        unrelated.linked_symbols.clear();
        unrelated.source_query = None;
        store.store(unrelated).expect("store unrelated memory");
    }
    handler
        .remember_working_memory_state_for_test(
            TASK_ID,
            WorkingMemoryState::new(
                "Determine current state for /home/pete/cadres/portal/docs/audit/2026-05-17-2248-remediation-run IU-0030 product-profile",
            ),
        )
        .await;

    let response = handler
        .handle(
            "lattice/tool_call",
            json!({"name": "get_task_memory", "arguments": {"task_id": TASK_ID}}),
        )
        .await
        .expect("get_task_memory succeeds");
    let payload = parse_tool_payload(&response);
    let memories = payload["memories"].as_array().expect("memories");
    assert_eq!(memories.len(), 1);
    assert!(memories[0]["content"]
        .as_str()
        .is_some_and(|content| content.contains("IU-0030")));
    assert!(memories[0]["inclusion_reason"]
        .as_str()
        .is_some_and(|reason| reason.contains("iu-0030")));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn search_memory_falls_back_to_exact_task_ids_under_workspace_scope() {
    let (handler, memory_store, _event_store, workspace_root, context_cache_path, _session_id) =
        build_handler("search-memory-exact-ids");
    {
        let store = memory_store.lock().await;
        let mut portal_memory = seed_memory(
            "IU-0030 product-profile launch claim remediation was completed.",
            MemoryScope::Repo,
        );
        portal_memory.workspace_id = Some(workspace_root.to_string_lossy().to_string());
        portal_memory.linked_files = vec![
            "docs/audit/2026-05-17-2248-remediation-run/04-final-remediation-review.md".to_string(),
        ];
        portal_memory.refresh_key = Some("portal-remediation-IU-0030".to_string());
        store.store(portal_memory).expect("store portal memory");

        let mut wrong_workspace = seed_memory(
            "IU-0030 product-profile launch claim from RMM.",
            MemoryScope::Repo,
        );
        wrong_workspace.workspace_id = Some("/home/pete/cadres/rmm".to_string());
        wrong_workspace.confidence = 0.99;
        store
            .store(wrong_workspace)
            .expect("store wrong workspace memory");
    }

    let response = handler
        .handle("lattice/tool_call",
            json!({
                "name": "search_memory",
                "arguments": {
                    "query": "Portal remediation 2026-05-17 IM-1 IM-2 PX-0039 IU-0030 IU-0022 IU-0004 product-profile",
                    "limit": 10
                }
            }),
        )
        .await
        .expect("search_memory succeeds");
    let payload = parse_tool_payload(&response);
    assert_eq!(payload["count"].as_u64(), Some(1));
    assert_eq!(
        payload["diagnostics"]["exact_term_rerank"].as_bool(),
        Some(true)
    );
    assert_eq!(
        payload["diagnostics"]["matched_exact_terms"],
        json!(["iu-0030"])
    );
    let memory = &payload["memories"].as_array().expect("memories")[0];
    assert!(memory["content"]
        .as_str()
        .is_some_and(|content| content.contains("IU-0030")));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn search_memory_exact_ids_do_not_fall_back_to_generic_terms() {
    let (handler, memory_store, _event_store, workspace_root, context_cache_path, _session_id) =
        build_handler("search-memory-exact-id-hard-anchor");
    {
        let store = memory_store.lock().await;
        let mut generic_governance = seed_memory(
            "IU-0023-S01 governance partial-load remediation was completed.",
            MemoryScope::Repo,
        );
        generic_governance.workspace_id = Some(workspace_root.to_string_lossy().to_string());
        generic_governance.refresh_key = Some("workflow_outcome::0023-governance".to_string());
        store
            .store(generic_governance)
            .expect("store generic governance memory");
    }

    let response = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "search_memory",
                "arguments": {
                    "query": "IU-0031 PX-0040 governance contract Portal remediation",
                    "limit": 10
                }
            }),
        )
        .await
        .expect("search_memory succeeds");
    let payload = parse_tool_payload(&response);
    assert_eq!(payload["count"].as_u64(), Some(0));
    assert_eq!(
        payload["diagnostics"]["query_exact_terms"],
        json!(["iu-0031", "px-0040"])
    );
    assert_eq!(
        payload["diagnostics"]["unmatched_exact_terms"],
        json!(["iu-0031", "px-0040"])
    );
    assert_eq!(
        payload["diagnostics"]["exact_term_status"].as_str(),
        Some("absent_from_durable_memory")
    );
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn search_memory_warns_when_unverified_failure_memory_references_changed_file() {
    let (handler, memory_store, _event_store, workspace_root, context_cache_path, _session_id) =
        build_handler("search-memory-freshness-warning");
    let changed_file = workspace_root.join("docs/claim.md");
    std::fs::create_dir_all(changed_file.parent().expect("parent")).expect("create parent");
    std::fs::write(&changed_file, "current claim").expect("write changed file");
    {
        let store = memory_store.lock().await;
        let mut blocked = seed_memory(
            "PX-0033 product-profile launch claims remain blocked.",
            MemoryScope::Repo,
        );
        blocked.workspace_id = Some(workspace_root.to_string_lossy().to_string());
        blocked.linked_files = vec!["docs/claim.md".to_string()];
        blocked.source_query = Some(format!(
            "Blocked shared edit at {}",
            changed_file.to_string_lossy()
        ));
        store.store(blocked).expect("store blocked memory");
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
    std::fs::write(&changed_file, "current claim fixed after memory")
        .expect("rewrite changed file");
    {
        let store = memory_store.lock().await;
        let mut current = seed_memory(
            "PX-0033 product-profile launch claims are now complete.",
            MemoryScope::Repo,
        );
        current.workspace_id = Some(workspace_root.to_string_lossy().to_string());
        store.store(current).expect("store current memory");
    }

    let response = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "search_memory",
                "arguments": {
                    "query": "PX-0033 product-profile launch claims remediation",
                    "limit": 2
                }
            }),
        )
        .await
        .expect("search_memory succeeds");
    let payload = parse_tool_payload(&response);
    let memories = payload["memories"].as_array().expect("memories");
    assert!(memories[0]["content"]
        .as_str()
        .is_some_and(|content| content.contains("now complete")));
    let memory = memories
        .iter()
        .find(|memory| {
            memory["content"]
                .as_str()
                .is_some_and(|content| content.contains("remain blocked"))
        })
        .expect("blocked memory remains visible with warning");
    assert_eq!(
        memory["freshness_warning"]["kind"].as_str(),
        Some("referenced_files_changed_after_memory")
    );
    assert_eq!(memory["trust_status"].as_str(), Some("advisory"));
    assert!(memory["freshness_warning"]["changed_paths"]
        .as_array()
        .expect("changed paths")
        .iter()
        .any(|path| path.as_str() == Some(changed_file.to_string_lossy().as_ref())));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn search_memory_exact_id_rerank_penalizes_stale_memory() {
    let (handler, memory_store, _event_store, workspace_root, context_cache_path, _session_id) =
        build_handler("search-memory-stale-penalty");
    {
        let store = memory_store.lock().await;
        let mut current = seed_memory(
            "PX-0033 product-profile launch claims are now complete.",
            MemoryScope::Repo,
        );
        current.workspace_id = Some(workspace_root.to_string_lossy().to_string());
        current.refresh_key = Some("remediation-PX-0033-current".to_string());
        store.store(current).expect("store current memory");

        let mut stale = seed_memory(
            "PX-0033 product-profile launch claims remain blocked.",
            MemoryScope::Repo,
        );
        stale.workspace_id = Some(workspace_root.to_string_lossy().to_string());
        stale.refresh_key = Some("remediation-PX-0033-stale".to_string());
        stale.confidence = 1.0;
        stale.is_stale = true;
        stale.verification_status = MemoryVerificationStatus::Stale;
        store.store(stale).expect("store stale memory");
    }

    let response = handler
        .handle(
            "lattice/tool_call",
            json!({
                "name": "search_memory",
                "arguments": {
                    "query": "PX-0033 product-profile launch claims remediation",
                    "limit": 2
                }
            }),
        )
        .await
        .expect("search_memory succeeds");
    let payload = parse_tool_payload(&response);
    let memories = payload["memories"].as_array().expect("memories");
    assert_eq!(memories.len(), 2);
    assert!(memories[0]["content"]
        .as_str()
        .is_some_and(|content| content.contains("now complete")));
    assert_eq!(memories[1]["is_stale"].as_bool(), Some(true));
    cleanup_paths(&workspace_root, &context_cache_path);
}

#[tokio::test]
async fn get_task_memory_respects_scope_boundaries() {
    let (handler, memory_store, _event_store, workspace_root, context_cache_path, _session_id) =
        build_handler("scope-boundary");
    {
        let store = memory_store.lock().await;
        let mut branch_memory = seed_memory("other branch memory", MemoryScope::Branch);
        branch_memory.workspace_id = Some(workspace_root.to_string_lossy().to_string());
        branch_memory.branch = Some("different-branch".to_string());
        store.store(branch_memory).expect("store branch memory");
    }
    handler
        .remember_working_memory_state_for_test(TASK_ID, WorkingMemoryState::new("branch mismatch"))
        .await;

    let response = handler
        .handle(
            "lattice/tool_call",
            json!({"name": "get_task_memory", "arguments": {"task_id": TASK_ID}}),
        )
        .await
        .expect("get_task_memory succeeds");
    let payload = parse_tool_payload(&response);
    let memories = payload["memories"].as_array().expect("memories");
    assert!(memories
        .iter()
        .all(|memory| memory["content"].as_str() != Some("other branch memory")));
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
    PathBuf,
    PathBuf,
    String,
) {
    let workspace_root = unique_test_path(&format!("lattice-memory-v2-{suffix}"));
    std::fs::create_dir_all(&workspace_root).expect("workspace dir");
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
    let session_id = format!("session-test-{suffix}");
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
    (
        handler,
        memory_store,
        event_store,
        workspace_root,
        context_cache_path,
        session_id,
    )
}

fn seed_memory(content: &str, scope: MemoryScope) -> Memory {
    Memory {
        id: String::new(),
        session_id: "session-seed".to_string(),
        content: content.to_string(),
        memory_type: MemoryType::Observation,
        scope,
        confidence: 0.81,
        linked_symbols: vec!["refresh_token".to_string()],
        linked_files: vec!["src/auth.rs".to_string()],
        workspace_id: None,
        branch: None,
        scope_organization_id: None,
        refresh_key: None,
        source_query: Some("refresh token".to_string()),
        created_at: 0,
        last_accessed: 0,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: MemoryVerificationStatus::Unverified,
    }
}

fn read_events(
    store: &Arc<EventStore>,
    workspace_root: &PathBuf,
) -> Vec<lattice_core::events::EventEnvelope> {
    let workspace_id = workspace_root.to_string_lossy().to_string();
    // McpHandler events use the workspace branch (defaults to "unknown" when there's
    // no git repo); proposal apply emits MemoryConsolidated events on branch "main".
    // Fold both branches so apply/reject is auditable end-to-end.
    let reader = EventReader::new(store.clone());
    let mut events = reader
        .execute(
            EventQuery::new()
                .workspace(workspace_id.clone())
                .branch("unknown")
                .order(QueryOrder::OldestFirst)
                .limit(128),
        )
        .expect("event query");
    let mut consolidation = reader
        .execute(
            EventQuery::new()
                .workspace(workspace_id)
                .branch("main")
                .order(QueryOrder::OldestFirst)
                .limit(128),
        )
        .unwrap_or_default();
    events.append(&mut consolidation);
    events
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
