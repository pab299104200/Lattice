//! Phase 11 hardening tests for partial-index handling.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Non-Negotiable Product Properties`: no silent broad reads or dropped
//! results while the index is degraded.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 11: Hardening`: partial-index handling is a required regression
//! surface.

use std::path::PathBuf;
use std::sync::Arc;

use crate::events::ToolResultStatus;
use crate::indexer::{BatchIndexReport, IndexFailureKind, Indexer};
use crate::intelligence::{
    find_relevant_tests, impact_from_diff, prepare_change, BundleMode, RulesDetector,
};
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType, MemoryVerificationStatus};
use crate::query::QueryEngine;

fn auth_memory(session_id: &str, file: &str, symbol: &str) -> Memory {
    Memory {
        id: String::new(),
        session_id: session_id.to_string(),
        content: format!("{symbol} was verified before the graph changed"),
        memory_type: MemoryType::Observation,
        scope: MemoryScope::Repo,
        confidence: 0.9,
        linked_symbols: vec![symbol.to_string()],
        linked_files: vec![file.to_string()],
        workspace_id: Some("workspace-main".to_string()),
        branch: Some("main".to_string()),
        scope_organization_id: None,
        refresh_key: Some(format!("refresh::{symbol}")),
        source_query: None,
        created_at: 1,
        last_accessed: 1,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: MemoryVerificationStatus::Verified,
    }
}

fn prepare_bundle(
    indexer: &Indexer,
    query_text: &str,
    entry_file: &str,
) -> crate::intelligence::TaskBundle {
    let graph = indexer.graph().clone();
    let mut engine = QueryEngine::new(graph.clone(), None);
    let capsule = engine.query(query_text, None, false);
    let rules = RulesDetector::new().detect_rules(
        &graph
            .all_nodes()
            .iter()
            .map(|node| node.file.clone())
            .collect::<Vec<_>>(),
    );
    prepare_change(
        &graph,
        &capsule,
        &[entry_file.to_string()],
        &[],
        &rules,
        BundleMode::Compact,
        None,
    )
}

fn partial_result_status(report: &BatchIndexReport) -> ToolResultStatus {
    if report.is_partial {
        ToolResultStatus::Partial
    } else {
        ToolResultStatus::Succeeded
    }
}

#[test]
fn test_syntax_error_file_keeps_surviving_symbols_queryable_without_panic() {
    let mut indexer = Indexer::new(PathBuf::from("/workspace"));
    let source = r#"
export function healthyLogin(user: string): string {
    return issueToken(user);
}

export function brokenSegment(
    const nope = ;

export function issueToken(user: string): string {
    return user;
}
"#;

    indexer
        .index_file_content("src/auth.ts", source)
        .expect("tree-sitter should retain surviving symbols");

    assert!(
        indexer
            .graph()
            .all_nodes()
            .iter()
            .any(|node| node.name == "healthyLogin"),
        "partial parse should preserve the valid symbol before the syntax error"
    );

    let bundle = prepare_bundle(&indexer, "fix healthy login auth flow", "src/auth.ts");
    assert!(
        bundle
            .primary_files
            .iter()
            .any(|file| file.file == "src/auth.ts"),
        "partial graph must stay queryable for prepare_change: {:?}",
        bundle.primary_files
    );
}

#[tokio::test]
async fn test_parse_failure_is_reported_per_file_without_dropping_other_results() {
    let mut indexer = Indexer::new(PathBuf::from("/workspace"));
    let report = indexer
        .index_file_batch_contents_with_report(vec![
            (
                "src/auth.ts".to_string(),
                "export function loginUser(): string { return 'ok'; }".to_string(),
            ),
            (
                "notes.txt".to_string(),
                "unsupported language should fail fast".to_string(),
            ),
        ])
        .await
        .expect("batch indexing should return a report");

    assert_eq!(report.indexed_count, 1);
    assert!(report.is_partial);
    assert_eq!(partial_result_status(&report), ToolResultStatus::Partial);
    assert!(
        report.failures.iter().any(|failure| {
            failure.file == "notes.txt" && failure.kind == IndexFailureKind::ParseError
        }),
        "expected a typed parse failure for notes.txt, got {:?}",
        report.failures
    );
    assert!(
        indexer
            .graph()
            .all_nodes()
            .iter()
            .any(|node| node.name == "loginUser"),
        "successful files must remain queryable after a neighboring parse failure"
    );
}

#[tokio::test]
async fn test_worker_panic_does_not_abort_remaining_batch_and_surviving_graph_is_queryable() {
    let mut indexer = Indexer::new(PathBuf::from("/workspace"));
    let parser = Arc::new(|rel_path: &str, content: &str| {
        if rel_path == "src/panic.ts" {
            panic!("synthetic parser timeout");
        }
        crate::parser::parse_file(rel_path, content)
    });

    let report = indexer
        .index_file_batch_contents_with_test_parser(
            vec![
                (
                    "src/session.ts".to_string(),
                    "export function createSession(): string { return 'ok'; }".to_string(),
                ),
                (
                    "src/panic.ts".to_string(),
                    "export function explode(): void {}".to_string(),
                ),
            ],
            parser,
        )
        .await;

    assert_eq!(report.indexed_count, 1);
    assert!(report.is_partial);
    assert!(
        report
            .failures
            .iter()
            .any(|failure| failure.kind == IndexFailureKind::WorkerPanic),
        "expected a worker-panic failure entry, got {:?}",
        report.failures
    );

    let bundle = prepare_bundle(&indexer, "session creation flow", "src/session.ts");
    assert!(
        bundle
            .symbols
            .iter()
            .any(|symbol| symbol.symbol == "createSession"),
        "remaining files must still drive workflow recommendations after one worker panics"
    );
}

#[tokio::test]
async fn test_ignored_import_target_does_not_create_edges_or_trigger_broad_fallback() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(tempdir.path().join(".gitignore"), "src/secret.ts\n").expect("gitignore");
    std::fs::create_dir_all(tempdir.path().join("src")).expect("src dir");
    std::fs::write(
        tempdir.path().join("src/app.ts"),
        "import { secretToken } from './secret';\nexport function runApp(): string { return 'ok'; }\n",
    )
    .expect("app source");
    std::fs::write(
        tempdir.path().join("src/secret.ts"),
        "export function secretToken(): string { return 'hidden'; }\n",
    )
    .expect("secret source");

    let filter = crate::security::SecurityFilter::new(tempdir.path());
    let mut indexer = Indexer::new(tempdir.path().to_path_buf());
    indexer
        .index_file_content(
            "src/app.ts",
            &std::fs::read_to_string(tempdir.path().join("src/app.ts")).expect("app reads"),
        )
        .expect("app indexes");
    let app_symbol = indexer
        .graph()
        .all_nodes()
        .iter()
        .find(|node| node.name == "runApp")
        .expect("runApp symbol exists")
        .id
        .clone();

    assert!(filter.is_excluded("src/secret.ts"));
    assert!(
        indexer
            .graph()
            .all_nodes()
            .iter()
            .all(|node| node.file != "src/secret.ts"),
        "ignored files must never be loaded into the graph"
    );
    assert!(
        indexer.graph().get_dependencies(&app_symbol).is_empty(),
        "ignored import targets must not be resolved via a broad workspace read"
    );
}

#[test]
fn test_file_change_marks_memories_stale_with_snapshot_gap_visible_to_queries() {
    let mut indexer = Indexer::new(PathBuf::from("/workspace"));
    let store = MemoryStore::open_in_memory().expect("memory store");

    indexer
        .index_file_content(
            "src/auth.ts",
            "export function loginUser(): string { return 'v1'; }",
        )
        .expect("initial auth file indexes");

    let memory_id = store
        .store(auth_memory("session-a", "src/auth.ts", "loginUser"))
        .expect("memory stores");
    store
        .set_last_verified_graph_snapshot_id(&memory_id, indexer.graph_snapshot_id())
        .expect("snapshot id stores");

    indexer
        .index_file_with_stale_detection(
            "src/auth.ts",
            "export function loginUser(): string { return 'v2'; }",
            Some(&store),
        )
        .expect("updated auth file indexes");

    let memory = store
        .get_by_id(&memory_id)
        .expect("memory reloads")
        .expect("memory exists");

    assert!(
        memory.is_stale,
        "stale flag must be explicit after the file changes"
    );
    assert_eq!(memory.verification_status, MemoryVerificationStatus::Stale);
    assert!(
        memory
            .stale_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("loginUser") && reason.contains("src/auth.ts")),
        "expected an explicit symbol/file stale reason, got {:?}",
        memory.stale_reason
    );
    assert_eq!(
        store
            .get_last_verified_graph_snapshot_id(&memory_id)
            .expect("snapshot id reloads"),
        Some(1),
        "the stored verification snapshot should remain older than the live graph snapshot"
    );
    assert!(
        indexer.graph_snapshot_id() > 1,
        "the graph snapshot must advance so callers can detect the freshness gap"
    );
}

#[tokio::test]
async fn test_partial_batch_report_keeps_cold_start_queries_bounded_and_explicit() {
    let mut indexer = Indexer::new(PathBuf::from("/workspace"));
    let report = indexer
        .index_file_batch_contents_with_report(vec![
            (
                "src/auth.ts".to_string(),
                "export function loginUser(): string { return 'ok'; }".to_string(),
            ),
            (
                "src/unsupported.txt".to_string(),
                "cold-start noise".to_string(),
            ),
        ])
        .await
        .expect("batch report returns");

    let graph = indexer.graph().clone();
    let mut engine = QueryEngine::new(graph.clone(), None);
    let capsule = engine.query("login user", None, false);
    let tests = find_relevant_tests(
        &graph,
        &["src/auth.ts".to_string()],
        &["loginUser".to_string()],
        None,
        &[],
        8,
    );
    let diff = impact_from_diff(
        &graph,
        "diff --git a/src/auth.ts b/src/auth.ts\n--- a/src/auth.ts\n+++ b/src/auth.ts\n@@\n-loginUser\n+loginUser\n",
        &["src/auth.ts".to_string()],
        &["loginUser".to_string()],
        &[],
        BundleMode::Compact,
        2,
        None,
    );

    assert_eq!(partial_result_status(&report), ToolResultStatus::Partial);
    assert!(
        report.is_partial,
        "cold-start partial state must stay explicit"
    );
    assert!(
        capsule.pivots.len() <= 5 && tests.tests.len() <= 8 && diff.changed_files.len() <= 1,
        "partial-index queries must remain bounded instead of dumping the workspace"
    );
}
