use std::time::Duration;

use rusqlite::{params, params_from_iter, Connection};

use super::classes::{AssertionType, FreshnessKind, FreshnessPolicy, MemoryClass, MemoryScope};
use super::scope::{ScopeError, ScopeFilter};
use super::streams::{classify_stream, default_policy, MemoryStream};

const MEMORY_GRAPH_SCHEMA_SQL: &str = include_str!("schema.sql");

struct TestMemoryStore {
    conn: Connection,
}

impl TestMemoryStore {
    fn open() -> Self {
        let conn = Connection::open_in_memory().expect("memory graph test db opens");
        conn.execute_batch(MEMORY_GRAPH_SCHEMA_SQL)
            .expect("memory graph schema applies");
        Self { conn }
    }

    fn insert_memory(
        &self,
        memory_id: &str,
        scope: MemoryScope,
        scope_session_id: Option<&str>,
        scope_branch: Option<&str>,
        scope_workspace_id: Option<&str>,
        scope_user_id: Option<&str>,
        scope_org_id: Option<&str>,
        content: &str,
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
                    'verified', 0.9, 'seeded for scope test',
                    ?9, '[]', '[]', '[]', '[]',
                    '[]', '[]', '[]', '[]',
                    '[]', '[]', '[]',
                    '[]', NULL, NULL,
                    0.0, 1,
                    1, 'test', 1, 'test', NULL, 1
                )",
                params![
                    memory_id,
                    content,
                    scope.as_str(),
                    scope_session_id,
                    scope_branch,
                    scope_workspace_id,
                    scope_user_id,
                    scope_org_id,
                    freshness_policy_json,
                ],
            )
            .expect("test memory inserts");
    }

    fn query(&self, scope: &ScopeFilter) -> Result<Vec<String>, ScopeError> {
        if scope.is_empty() {
            return Err(ScopeError::Underspecified);
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

        Ok(rows
            .map(|row| row.expect("row loads"))
            .collect::<Vec<String>>())
    }

    fn query_unscoped_admin(&self) -> Vec<String> {
        let mut statement = self
            .conn
            .prepare("SELECT memory_id FROM memories ORDER BY memory_id")
            .expect("admin query prepares");
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("admin query runs");
        rows.map(|row| row.expect("row loads")).collect()
    }
}

#[test]
fn test_scope_query_without_explicit_scope_returns_underspecified() {
    let store = TestMemoryStore::open();
    assert_eq!(
        store.query(&ScopeFilter::new()),
        Err(ScopeError::Underspecified)
    );
}

#[test]
fn test_branch_scope_never_leaks_across_branches() {
    let store = TestMemoryStore::open();
    store.insert_memory(
        "branch-a-memory",
        MemoryScope::Branch,
        None,
        Some("feature/a"),
        Some("workspace-1"),
        None,
        None,
        "feature A memory",
    );
    store.insert_memory(
        "branch-b-memory",
        MemoryScope::Branch,
        None,
        Some("feature/b"),
        Some("workspace-1"),
        None,
        None,
        "feature B memory",
    );

    let branch_a = store
        .query(&ScopeFilter::branch("workspace-1", "feature/a"))
        .expect("branch query succeeds");
    assert_eq!(branch_a, vec!["branch-a-memory".to_string()]);
}

#[test]
fn test_user_scope_never_leaks_across_users() {
    let store = TestMemoryStore::open();
    store.insert_memory(
        "user-a-memory",
        MemoryScope::User,
        None,
        None,
        None,
        Some("user-a"),
        None,
        "user A preference",
    );
    store.insert_memory(
        "user-b-memory",
        MemoryScope::User,
        None,
        None,
        None,
        Some("user-b"),
        None,
        "user B preference",
    );

    let user_a = store
        .query(&ScopeFilter::user("user-a"))
        .expect("user query succeeds");
    assert_eq!(user_a, vec!["user-a-memory".to_string()]);
}

#[test]
fn test_org_scope_requires_explicit_opt_in() {
    let store = TestMemoryStore::open();
    store.insert_memory(
        "branch-memory",
        MemoryScope::Branch,
        None,
        Some("feature/auth"),
        Some("workspace-1"),
        None,
        None,
        "branch auth memory",
    );
    store.insert_memory(
        "org-memory",
        MemoryScope::Organization,
        None,
        None,
        None,
        None,
        Some("org-1"),
        "org-wide contract memory",
    );

    let branch_only = store
        .query(&ScopeFilter::branch("workspace-1", "feature/auth"))
        .expect("branch query succeeds");
    assert_eq!(branch_only, vec!["branch-memory".to_string()]);

    let branch_and_org = store
        .query(&ScopeFilter::branch("workspace-1", "feature/auth").with_organization("org-1"))
        .expect("branch and org query succeeds");
    assert_eq!(
        branch_and_org,
        vec!["branch-memory".to_string(), "org-memory".to_string()]
    );
}

#[test]
fn test_enforce_subset_rejects_scope_widening() {
    let requested = ScopeFilter::branch("workspace-1", "feature/auth").with_organization("org-1");
    let allowed = ScopeFilter::branch("workspace-1", "feature/auth");

    let error = requested
        .enforce_subset(&allowed)
        .expect_err("scope widening should fail");
    assert_eq!(
        error,
        ScopeError::CrossScopeViolation {
            requested: MemoryScope::Organization,
            allowed: MemoryScope::Branch,
        }
    );
}

#[test]
fn test_enforce_subset_rejects_identity_mismatch() {
    let requested = ScopeFilter::branch("workspace-2", "feature/auth");
    let allowed = ScopeFilter::branch("workspace-1", "feature/auth");

    let error = requested
        .enforce_subset(&allowed)
        .expect_err("identity mismatch should fail");
    assert_eq!(
        error,
        ScopeError::IdentityMismatch {
            field: "scope_workspace_id",
        }
    );
}

#[test]
fn test_admin_query_path_is_explicit_and_unscoped() {
    let store = TestMemoryStore::open();
    store.insert_memory(
        "branch-memory",
        MemoryScope::Branch,
        None,
        Some("feature/auth"),
        Some("workspace-1"),
        None,
        None,
        "branch auth memory",
    );
    store.insert_memory(
        "org-memory",
        MemoryScope::Organization,
        None,
        None,
        None,
        None,
        Some("org-1"),
        "org memory",
    );

    assert_eq!(
        store.query_unscoped_admin(),
        vec!["branch-memory".to_string(), "org-memory".to_string()]
    );
}

#[test]
fn test_classify_stream_mapping_is_exhaustive_over_memory_class() {
    let mappings = vec![
        map_class_stream(MemoryClass::Observation),
        map_class_stream(MemoryClass::Decision),
        map_class_stream(MemoryClass::Constraint),
        map_class_stream(MemoryClass::Pattern),
        map_class_stream(MemoryClass::AntiPattern),
        map_class_stream(MemoryClass::WorkflowOutcome),
        map_class_stream(MemoryClass::FailurePattern),
        map_class_stream(MemoryClass::Procedure),
        map_class_stream(MemoryClass::Preference),
        map_class_stream(MemoryClass::ArchitectureInvariant),
        map_class_stream(MemoryClass::DocsContract),
        map_class_stream(MemoryClass::OpenQuestion),
        map_class_stream(MemoryClass::CounterMemory),
    ];

    assert!(mappings.contains(&(MemoryClass::Observation, MemoryStream::CodeTopology)));
    assert!(mappings.contains(&(MemoryClass::Decision, MemoryStream::ArchitectureDecisions)));
    assert!(mappings.contains(&(MemoryClass::FailurePattern, MemoryStream::FailurePatterns)));
    assert!(mappings.contains(&(
        MemoryClass::DocsContract,
        MemoryStream::DocsAndContractState,
    )));
}

#[test]
fn test_default_policy_matches_expected_defaults() {
    let policy = default_policy(MemoryStream::ArchitectureDecisions);
    assert_eq!(policy.default_scope, MemoryScope::Repo);
    assert_eq!(policy.ranking_profile, "architecture_decisions_v1");
    assert_eq!(policy.freshness_default.kind, FreshnessKind::RepoScoped);
}

fn map_class_stream(class: MemoryClass) -> (MemoryClass, MemoryStream) {
    let stream = match class {
        MemoryClass::Observation => classify_stream(class, AssertionType::Observation),
        MemoryClass::Decision => classify_stream(class, AssertionType::Decision),
        MemoryClass::Constraint => classify_stream(class, AssertionType::Constraint),
        MemoryClass::Pattern => classify_stream(class, AssertionType::Observation),
        MemoryClass::AntiPattern => classify_stream(class, AssertionType::Hypothesis),
        MemoryClass::WorkflowOutcome => classify_stream(class, AssertionType::Outcome),
        MemoryClass::FailurePattern => classify_stream(class, AssertionType::Observation),
        MemoryClass::Procedure => classify_stream(class, AssertionType::Procedure),
        MemoryClass::Preference => classify_stream(class, AssertionType::Preference),
        MemoryClass::ArchitectureInvariant => classify_stream(class, AssertionType::Decision),
        MemoryClass::DocsContract => classify_stream(class, AssertionType::Constraint),
        MemoryClass::OpenQuestion => classify_stream(class, AssertionType::Question),
        MemoryClass::CounterMemory => classify_stream(class, AssertionType::Counter),
    };
    (class, stream)
}
