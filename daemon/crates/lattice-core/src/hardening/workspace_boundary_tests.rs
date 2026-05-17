//! Phase 11 hardening tests for workspace boundaries and scope leakage.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Non-Negotiable Product Properties`: no silent broad workspace reads
//! that bypass ignore rules or workspace boundaries.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Risks — Scope Leakage`: scope-aware queries, enforced filters, and
//! negative tests are the control surface.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{params, params_from_iter, Connection};
use tempfile::TempDir;

use crate::events::{
    Actor, AssistantTaskStartedPayload, BranchRef, CompactSummary, EventPayload, EventQuery,
    EventReader, EventStore, EventWriter, PartialEnvelope, QueryOrder, SessionId, TaskId,
};
use crate::graph::CodeGraph;
use crate::identity::FileId;
use crate::indexer::Indexer;
use crate::intelligence::{
    expand_context, find_relevant_tests, impact_from_diff, prepare_change, BundleMode,
    ExpandContextSeed, RulesDetector,
};
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType, MemoryVerificationStatus};
use crate::memory_graph::{
    initialize_schema, FreshnessKind, FreshnessPolicy, MemoryScope as GraphMemoryScope,
    ScopeError as GraphScopeError, ScopeFilter as GraphScopeFilter,
};
use crate::query::QueryEngine;
use crate::security::SecurityFilter;
use crate::verification::{ScopeFilter, SpanReader, WorkspaceFileReader};
use crate::watcher::should_index_file;

fn scoped_memory(
    session_id: &str,
    scope: MemoryScope,
    workspace_id: Option<&str>,
    branch: Option<&str>,
    org_id: Option<&str>,
    content: &str,
) -> Memory {
    Memory {
        id: String::new(),
        session_id: session_id.to_string(),
        content: content.to_string(),
        memory_type: MemoryType::Observation,
        scope,
        confidence: 0.9,
        linked_symbols: Vec::new(),
        linked_files: Vec::new(),
        workspace_id: workspace_id.map(str::to_string),
        branch: branch.map(str::to_string),
        scope_organization_id: org_id.map(str::to_string),
        refresh_key: None,
        source_query: None,
        created_at: 1,
        last_accessed: 1,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: MemoryVerificationStatus::Unverified,
    }
}

fn index_workspace_filtered(root: &Path) -> Indexer {
    let filter = SecurityFilter::new(root);
    let mut indexer = Indexer::new(root.to_path_buf());
    for path in collect_files(root) {
        let rel_path = path
            .strip_prefix(root)
            .expect("repo-relative path")
            .to_string_lossy()
            .replace('\\', "/");
        if should_index_file(&rel_path) && !filter.is_excluded(&rel_path) {
            let content = std::fs::read_to_string(&path).expect("workspace file reads");
            indexer
                .index_file_content(&rel_path, &content)
                .expect("workspace file indexes");
        }
    }
    indexer
}

fn collect_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).expect("read_dir");
        for entry in entries {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn search_symbols(graph: &CodeGraph, pattern: &str) -> Vec<String> {
    let needle = pattern.to_lowercase();
    graph
        .all_nodes()
        .iter()
        .filter(|node| node.name.to_lowercase().contains(&needle))
        .map(|node| node.file.clone())
        .collect()
}

fn assert_no_ignored_paths(paths: impl IntoIterator<Item = String>, scenario: &str) {
    let leaked = paths
        .into_iter()
        .find(|path| path.contains("ignored.ts") || path.contains("shadow.ts"));
    assert!(
        leaked.is_none(),
        "{scenario}: leaked ignored path {leaked:?}"
    );
}

fn tool_seed(bundle: &crate::intelligence::TaskBundle) -> ExpandContextSeed {
    ExpandContextSeed {
        query: Some(bundle.query.clone()),
        files: bundle
            .primary_files
            .iter()
            .chain(bundle.secondary_files.iter())
            .map(|file| file.file.clone())
            .collect(),
        symbols: bundle
            .symbols
            .iter()
            .map(|symbol| symbol.symbol.clone())
            .collect(),
        tests: bundle.tests.iter().map(|test| test.file.clone()).collect(),
        memories: bundle.memories.clone(),
    }
}

fn fixture_graph() -> (TempDir, CodeGraph) {
    let dir = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("src dir");
    std::fs::create_dir_all(dir.path().join("tests")).expect("tests dir");
    std::fs::write(dir.path().join(".gitignore"), "src/ignored.ts\n").expect("gitignore");
    std::fs::write(dir.path().join(".latticeignore"), "src/shadow.ts\n").expect("latticeignore");
    std::fs::write(
        dir.path().join("src/auth.ts"),
        "export function loginUser(): string { return issueToken(); }\nexport function issueToken(): string { return 'ok'; }\n",
    )
    .expect("auth file");
    std::fs::write(
        dir.path().join("tests/auth.test.ts"),
        "import { loginUser } from '../src/auth';\ntest('loginUser returns a token', () => { loginUser(); });\n",
    )
    .expect("auth test");
    std::fs::write(
        dir.path().join("src/ignored.ts"),
        "export function ignoredSecret(): string { return 'leak'; }\n",
    )
    .expect("ignored file");
    std::fs::write(
        dir.path().join("src/shadow.ts"),
        "export function shadowSecret(): string { return 'shadow'; }\n",
    )
    .expect("shadow file");

    let indexer = index_workspace_filtered(dir.path());
    (dir, indexer.graph().clone())
}

struct GraphScopeStore {
    conn: Connection,
}

impl GraphScopeStore {
    fn open() -> Self {
        let conn = Connection::open_in_memory().expect("memory graph db opens");
        initialize_schema(&conn).expect("memory graph schema");
        Self { conn }
    }

    fn insert_memory(
        &self,
        memory_id: &str,
        scope: GraphMemoryScope,
        session_id: Option<&str>,
        branch: Option<&str>,
        workspace_id: Option<&str>,
        user_id: Option<&str>,
        org_id: Option<&str>,
    ) {
        let freshness_policy_json = serde_json::to_string(&FreshnessPolicy {
            kind: FreshnessKind::RepoScoped,
            ttl: Some(Duration::from_secs(3600)),
            recheck_interval: Some(Duration::from_secs(600)),
        })
        .expect("freshness policy serializes");

        self.conn
            .execute(
                "INSERT INTO memories (
                    memory_id, content, class, assertion_type, scope,
                    scope_session_id, scope_branch, scope_workspace_id, scope_user_id, scope_org_id,
                    verification_status, confidence, confidence_reason,
                    freshness_policy_json, validity_conditions_json, invalidation_triggers_json,
                    provenance_event_ids_json, evidence_references_json,
                    linked_files_json, linked_symbols_json, linked_docs_json, linked_tests_json,
                    linked_memories_json, contradiction_links_json, supersession_links_json,
                    access_history_json, last_verified_event_id, last_verified_state,
                    usefulness_score, usefulness_score_updated_at,
                    created_at, created_by, updated_at, updated_by, superseded_by, schema_version
                ) VALUES (
                    ?1, ?2, 'observation', 'observation', ?3,
                    ?4, ?5, ?6, ?7, ?8,
                    'verified', 0.9, 'seeded for hardening scope test',
                    ?9, '[]', '[]', '[]', '[]',
                    '[]', '[]', '[]', '[]',
                    '[]', '[]', '[]',
                    '[]', NULL, NULL,
                    0.0, 1,
                    1, 'test', 1, 'test', NULL, 1
                )",
                params![
                    memory_id,
                    format!("content for {memory_id}"),
                    scope.as_str(),
                    session_id,
                    branch,
                    workspace_id,
                    user_id,
                    org_id,
                    freshness_policy_json,
                ],
            )
            .expect("graph-scoped memory inserts");
    }

    fn query(&self, scope: &GraphScopeFilter) -> Result<Vec<String>, GraphScopeError> {
        if scope.is_empty() {
            return Err(GraphScopeError::Underspecified);
        }
        let predicate = scope.to_sql_predicate();
        let sql = format!(
            "SELECT memory_id FROM memories WHERE {} ORDER BY memory_id",
            predicate.where_clause
        );
        let mut statement = self.conn.prepare(&sql).expect("query prepares");
        let rows = statement
            .query_map(params_from_iter(predicate.bind_values), |row| {
                row.get::<_, String>(0)
            })
            .expect("query runs");
        Ok(rows.map(|row| row.expect("row")).collect())
    }
}

#[test]
fn test_ignored_files_never_appear_in_query_or_workflow_tool_results() {
    let (_dir, graph) = fixture_graph();
    let mut engine = QueryEngine::new(graph.clone(), None, None);
    let capsule = engine.query("login auth workflow", None, false);
    let rules = RulesDetector::new().detect_rules(
        &graph
            .all_nodes()
            .iter()
            .map(|node| node.file.clone())
            .collect::<Vec<_>>(),
    );
    let bundle = prepare_change(
        &graph,
        &capsule,
        &["src/auth.ts".to_string()],
        &[],
        &rules,
        BundleMode::Compact,
    );
    let expanded = expand_context(&graph, &tool_seed(&bundle), "file:src/auth.ts", 800);
    let diff = impact_from_diff(
        &graph,
        "diff --git a/src/auth.ts b/src/auth.ts\n--- a/src/auth.ts\n+++ b/src/auth.ts\n@@ -1,1 +1,1 @@\n-export function loginUser(): string { return issueToken(); }\n+export function loginUser(): string { return issueToken(); }\n",
        &["src/auth.ts".to_string()],
        &["loginUser".to_string()],
        &rules,
        BundleMode::Compact,
        2,
    );
    let tests = find_relevant_tests(
        &graph,
        &["src/auth.ts".to_string()],
        &["loginUser".to_string()],
        None,
        &rules,
        8,
    );

    assert_no_ignored_paths(search_symbols(&graph, "secret"), "search_symbols");
    assert_no_ignored_paths(
        capsule
            .pivots
            .iter()
            .map(|pivot| pivot.file.clone())
            .chain(capsule.context.iter().map(|node| node.file.clone()))
            .collect::<Vec<_>>(),
        "get_context_capsule",
    );
    assert_no_ignored_paths(
        bundle
            .primary_files
            .iter()
            .chain(bundle.secondary_files.iter())
            .map(|file| file.file.clone())
            .chain(bundle.symbols.iter().map(|symbol| symbol.file.clone()))
            .collect::<Vec<_>>(),
        "prepare_change",
    );
    assert_no_ignored_paths(
        expanded
            .files
            .iter()
            .map(|file| file.file.clone())
            .chain(expanded.symbols.iter().map(|symbol| symbol.file.clone()))
            .chain(expanded.tests.iter().map(|test| test.file.clone()))
            .collect::<Vec<_>>(),
        "expand_context",
    );
    assert_no_ignored_paths(
        diff.changed_files
            .iter()
            .map(|file| file.file.clone())
            .chain(
                diff.changed_symbols
                    .iter()
                    .map(|symbol| symbol.file.clone()),
            )
            .chain(
                diff.affected_symbols
                    .iter()
                    .map(|symbol| symbol.file.clone()),
            )
            .collect::<Vec<_>>(),
        "impact_from_diff",
    );
    assert_no_ignored_paths(
        tests
            .tests
            .iter()
            .map(|test| test.file.clone())
            .collect::<Vec<_>>(),
        "find_relevant_tests",
    );
}

#[test]
fn test_unresolved_queries_do_not_fall_back_to_broad_workspace_dump() {
    let (_dir, graph) = fixture_graph();
    let mut engine = QueryEngine::new(graph.clone(), None, None);
    let capsule = engine.query("completely absent anchor token", None, false);
    let bundle = prepare_change(&graph, &capsule, &[], &[], &[], BundleMode::Compact);

    assert!(
        capsule.pivots.is_empty(),
        "unexpected pivots: {:?}",
        capsule.pivots
    );
    assert!(
        capsule.context.is_empty(),
        "unexpected context: {:?}",
        capsule.context
    );
    assert!(
        bundle.primary_files.is_empty()
            && bundle.secondary_files.is_empty()
            && bundle.symbols.is_empty()
            && bundle.tests.is_empty(),
        "unresolved queries must stay empty and bounded instead of scanning the whole graph"
    );
}

#[test]
fn test_repo_user_and_organization_scope_queries_require_explicit_opt_in() {
    let store = MemoryStore::open_in_memory().expect("memory store");
    store
        .store(scoped_memory(
            "session-a",
            MemoryScope::Repo,
            Some("workspace-a"),
            None,
            None,
            "repo memory workspace-a",
        ))
        .expect("repo memory stores");
    store
        .store(scoped_memory(
            "session-a",
            MemoryScope::Organization,
            None,
            None,
            Some("org-a"),
            "org memory org-a",
        ))
        .expect("org memory stores");

    let wrong_workspace = ScopeFilter::new("workspace-b".to_string(), None, None);
    let wrong_org = ScopeFilter::new("workspace-a".to_string(), None, None);
    let org_filter = ScopeFilter::new("workspace-a".to_string(), None, Some("org-a".to_string()));

    assert!(
        store
            .list_all_scoped(&wrong_workspace)
            .expect("wrong workspace query succeeds")
            .is_empty(),
        "repo-scoped memories from workspace-a must not leak into workspace-b"
    );
    assert!(
        store
            .list_all_scoped(&wrong_org)
            .expect("org-less query succeeds")
            .iter()
            .all(|memory| memory.scope != MemoryScope::Organization),
        "organization memories must stay hidden until the caller opts in with organization_id"
    );
    assert!(
        store
            .list_all_scoped(&org_filter)
            .expect("org query succeeds")
            .iter()
            .any(|memory| memory.scope == MemoryScope::Organization),
        "organization scope must work once the explicit opt-in is present"
    );

    let graph_store = GraphScopeStore::open();
    graph_store.insert_memory(
        "user-a-memory",
        GraphMemoryScope::User,
        None,
        None,
        None,
        Some("user-a"),
        None,
    );

    assert_eq!(
        graph_store
            .query(&GraphScopeFilter::repo("workspace-b"))
            .expect("repo query succeeds"),
        Vec::<String>::new(),
        "user-scoped memories must not leak into repo-scoped queries"
    );
    assert_eq!(
        graph_store
            .query(&GraphScopeFilter::repo("workspace-b").with_user("user-a"))
            .expect("explicit user opt-in succeeds"),
        vec!["user-a-memory".to_string()],
        "user-scoped memories should appear only after explicit opt-in"
    );
}

#[test]
fn test_branch_and_session_scoped_memories_do_not_leak() {
    let store = MemoryStore::open_in_memory().expect("memory store");
    store
        .store(scoped_memory(
            "session-a",
            MemoryScope::Branch,
            Some("workspace-a"),
            Some("main"),
            None,
            "branch memory main",
        ))
        .expect("branch memory stores");
    store
        .store(scoped_memory(
            "session-s1",
            MemoryScope::Session,
            None,
            None,
            None,
            "session memory s1",
        ))
        .expect("session memory stores");

    let feature_scope = ScopeFilter::new(
        "workspace-a".to_string(),
        Some(BranchRef {
            name: "feature/x".to_string(),
        }),
        None,
    );
    let session_scope = ScopeFilter::new("workspace-a".to_string(), None, None)
        .for_session("session-s2".to_string());

    assert!(
        store
            .list_all_scoped(&feature_scope)
            .expect("feature query succeeds")
            .is_empty(),
        "branch-scoped memories from main must not leak into feature/x"
    );
    assert!(
        store
            .list_all_scoped(&session_scope)
            .expect("session query succeeds")
            .is_empty(),
        "session-scoped memories from session-s1 must not leak into session-s2"
    );
}

#[test]
fn test_event_queries_remain_workspace_scoped() {
    let store = Arc::new(EventStore::open_in_memory().expect("event store"));
    let writer_a = EventWriter::new(store.clone(), "workspace-a".to_string(), 4096);
    let writer_b = EventWriter::new(store.clone(), "workspace-b".to_string(), 4096);
    let reader = EventReader::new(store);

    append_event(&writer_a, "workspace-a", "session-a", "task-a", "alpha");
    append_event(&writer_b, "workspace-b", "session-b", "task-b", "beta");

    let events = reader
        .execute(
            EventQuery::new()
                .workspace("workspace-b")
                .branch("main")
                .order(QueryOrder::OldestFirst)
                .limit(10),
        )
        .expect("workspace-b query succeeds");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].workspace_id, "workspace-b");
}

#[test]
fn test_workspace_file_reader_rejects_path_traversal_outside_root() {
    let dir = TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join("inside.txt"), "inside").expect("inside file");
    let reader = WorkspaceFileReader::new(dir.path().to_path_buf());
    let file_id = FileId {
        workspace_id: "workspace-a".to_string(),
        repo_relative_path: "../outside.txt".to_string(),
        content_hash: "hash".to_string(),
    };

    let error = reader
        .read_range(&file_id, 0, 1)
        .expect_err("path traversal must be rejected");

    assert!(
        matches!(
            error,
            crate::verification::SpanValidationError::OutsideWorkspace { .. }
        ),
        "expected an explicit outside-workspace error, got {error:?}"
    );
}

fn append_event(
    writer: &EventWriter,
    workspace_id: &str,
    session_id: &str,
    task_id: &str,
    objective: &str,
) {
    let payload = EventPayload::AssistantTaskStarted(AssistantTaskStartedPayload {
        context_handle_id: None,
        seed_event_ids: Vec::new(),
        initial_memory_ids: Vec::new(),
        objective: objective.to_string(),
    });
    writer
        .append(PartialEnvelope {
            workspace_id: Some(workspace_id.to_string()),
            branch: BranchRef {
                name: "main".to_string(),
            },
            session_id: SessionId {
                value: session_id.to_string(),
            },
            task_id: Some(TaskId {
                value: task_id.to_string(),
            }),
            actor: Actor::Assistant {
                model: "gpt-5.5".to_string(),
            },
            kind: payload.kind(),
            references: Vec::new(),
            summary: CompactSummary::new(format!("summary for {task_id}")).expect("summary"),
            payload,
        })
        .expect("event appends");
}
