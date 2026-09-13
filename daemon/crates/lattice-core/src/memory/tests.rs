use super::model::{
    Memory, MemoryAssertionType, MemoryClass, MemoryEvidence, MemoryFreshnessPolicy,
    MemoryProvenance, MemoryScope, MemoryStructuredFields, MemoryType, MemoryVerificationStatus,
};
use super::store::MemoryStore;
use rusqlite::Connection;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn make_memory(content: &str, memory_type: MemoryType, linked_symbols: Vec<&str>) -> Memory {
    make_memory_with_session(content, memory_type, linked_symbols, "")
}

fn make_memory_with_session(
    content: &str,
    memory_type: MemoryType,
    linked_symbols: Vec<&str>,
    session_id: &str,
) -> Memory {
    Memory {
        id: String::new(),
        session_id: session_id.to_string(),
        content: content.to_string(),
        memory_type,
        scope: MemoryScope::Session,
        confidence: 1.0,
        linked_symbols: linked_symbols.into_iter().map(|s| s.to_string()).collect(),
        linked_files: vec![],
        workspace_id: None,
        branch: None,
        scope_organization_id: None,
        refresh_key: None,
        source_query: None,
        created_at: 0,
        last_accessed: 0,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: crate::memory::MemoryVerificationStatus::Unverified,
    }
}

fn temp_db_path(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("lattice-memory-{name}-{unique}.db"))
}

fn cleanup_db_files(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

#[test]
fn test_store_and_retrieve_memory() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mem = make_memory(
        "The loginUser function validates credentials before creating a session",
        MemoryType::Observation,
        vec!["loginUser"],
    );

    let id = store.store(mem).expect("Failed to store memory");
    assert!(!id.is_empty());

    let all = store.list_all().expect("Failed to list memories");
    assert_eq!(all.len(), 1);
    assert_eq!(
        all[0].content,
        "The loginUser function validates credentials before creating a session"
    );
    assert_eq!(all[0].memory_type, MemoryType::Observation);
    assert_eq!(all[0].linked_symbols, vec!["loginUser".to_string()]);
}

#[test]
fn test_mark_stale_by_symbol() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mem = make_memory(
        "hashPassword uses bcrypt with 12 rounds",
        MemoryType::Decision,
        vec!["hashPassword"],
    );

    store.store(mem).expect("Failed to store memory");

    let updated = store
        .mark_stale_by_symbol("hashPassword", "hashPassword was refactored")
        .expect("Failed to mark stale");
    assert_eq!(updated, 1);

    let all = store.list_all().expect("Failed to list memories");
    assert_eq!(all.len(), 1);
    assert!(all[0].is_stale);
    assert_eq!(
        all[0].stale_reason.as_deref(),
        Some("hashPassword was refactored")
    );
}

#[test]
fn test_search_memories_by_keyword() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mem1 = make_memory(
        "JWT tokens are validated using the shared secret from env",
        MemoryType::Observation,
        vec![],
    );
    let mem2 = make_memory(
        "Database connections use a pool of 10",
        MemoryType::Decision,
        vec![],
    );

    store.store(mem1).expect("Failed to store memory 1");
    store.store(mem2).expect("Failed to store memory 2");

    let results = store
        .search_by_keyword("JWT")
        .expect("Failed to search memories");
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("JWT"));
}

#[test]
fn test_search_memories_by_keyword_keeps_identifier_queries_precise() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    store
        .store(make_memory(
            "Tracks the workspace_setup workflow",
            MemoryType::Pattern,
            vec!["workspace_setup"],
        ))
        .expect("Failed to store identifier memory");
    store
        .store(make_memory(
            "Workspace setup checklist",
            MemoryType::Observation,
            vec![],
        ))
        .expect("Failed to store natural-language memory");

    let results = store
        .search_by_keyword("workspace_setup")
        .expect("Failed to search by identifier");

    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].linked_symbols,
        vec!["workspace_setup".to_string()]
    );
}

#[test]
fn test_open_backfills_fts_for_legacy_memory_rows() {
    let path = temp_db_path("legacy-fts");
    cleanup_db_files(&path);

    {
        let conn = Connection::open(&path).expect("Failed to create legacy memory database");
        conn.execute_batch(
            "CREATE TABLE memories (
                id              TEXT PRIMARY KEY,
                content         TEXT NOT NULL,
                memory_type     TEXT NOT NULL,
                confidence      REAL NOT NULL DEFAULT 1.0,
                linked_symbols  TEXT NOT NULL DEFAULT '[]',
                source_query    TEXT,
                created_at      INTEGER NOT NULL,
                last_accessed   INTEGER NOT NULL,
                access_count    INTEGER NOT NULL DEFAULT 0,
                is_stale        INTEGER NOT NULL DEFAULT 0,
                stale_reason    TEXT,
                is_invalidated  INTEGER NOT NULL DEFAULT 0
            );",
        )
        .expect("Failed to create legacy memory schema");
        conn.execute(
            "INSERT INTO memories
                (id, content, memory_type, confidence, linked_symbols, source_query,
                 created_at, last_accessed, access_count, is_stale, stale_reason, is_invalidated)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, 0, 0, NULL, 0)",
            rusqlite::params![
                "legacy-1",
                "JWT refresh flow uses loginUser",
                "pattern",
                1.0f64,
                "[\"loginUser\"]",
                1i64,
                1i64
            ],
        )
        .expect("Failed to seed legacy memory row");
    }

    let store = MemoryStore::open(&path).expect("Failed to open migrated memory store");
    let migration_count: i64 = store
        .with_connection(|conn| {
            conn.query_row("SELECT COUNT(*) FROM memory_schema_migrations", [], |row| {
                row.get(0)
            })
            .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))
        })
        .expect("Failed to inspect recorded memory schema migrations");
    assert_eq!(
        migration_count, 28,
        "every legacy column migration is recorded"
    );
    let fts_is_dirty: i64 = store
        .with_connection(|conn| {
            conn.query_row(
                "SELECT is_dirty FROM memory_fts_state WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))
        })
        .expect("Failed to inspect FTS state");
    assert_eq!(fts_is_dirty, 0, "legacy backfill leaves FTS clean");
    let jwt_results = store
        .search_by_keyword("JWT refresh")
        .expect("Failed to search migrated content");
    assert_eq!(jwt_results.len(), 1);
    assert_eq!(jwt_results[0].id, "legacy-1");

    let symbol_results = store
        .search_by_keyword("login")
        .expect("Failed to search migrated linked symbol");
    assert_eq!(symbol_results.len(), 1);
    assert_eq!(symbol_results[0].id, "legacy-1");

    cleanup_db_files(&path);
}

#[test]
fn test_fts_search_orders_by_relevance_not_creation_time() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mut concise = make_memory("durable migration safety", MemoryType::Decision, vec![]);
    concise.created_at = 1;
    concise.last_accessed = 1;
    store.store(concise).expect("Failed to store concise match");

    let mut recent_but_noisy = make_memory(
        "durable migration safety filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler",
        MemoryType::Decision,
        vec![],
    );
    recent_but_noisy.created_at = 10_000;
    recent_but_noisy.last_accessed = 10_000;
    store
        .store(recent_but_noisy)
        .expect("Failed to store noisy match");

    let results = store
        .search_by_keyword("durable migration safety")
        .expect("Failed to run FTS search");
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0].created_at, 1,
        "best BM25 match must win over recency"
    );
}

#[test]
fn test_dirty_fts_state_rebuilds_only_when_recovery_is_required() {
    let path = temp_db_path("fts-dirty-recovery");
    cleanup_db_files(&path);
    {
        let store = MemoryStore::open(&path).expect("Failed to open memory store");
        let id = store
            .store(make_memory(
                "rebuildable derived FTS index",
                MemoryType::Observation,
                vec![],
            ))
            .expect("Failed to store memory");
        store
            .with_connection(|conn| {
                conn.execute("DELETE FROM memories_fts WHERE memory_id = ?1", [&id])
                    .and_then(|_| {
                        conn.execute(
                            "UPDATE memory_fts_state SET is_dirty = 1 WHERE singleton = 1",
                            [],
                        )
                    })
                    .map(|_| ())
                    .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))
            })
            .expect("Failed to simulate interrupted FTS update");
    }

    let recovered = MemoryStore::open(&path).expect("Dirty FTS state must recover on open");
    let results = recovered
        .search_by_keyword("rebuildable derived")
        .expect("Failed to query recovered FTS index");
    assert_eq!(results.len(), 1);
    let is_dirty: i64 = recovered
        .with_connection(|conn| {
            conn.query_row(
                "SELECT is_dirty FROM memory_fts_state WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))
        })
        .expect("Failed to inspect recovered FTS state");
    assert_eq!(is_dirty, 0);
    cleanup_db_files(&path);
}

#[test]
fn test_recorded_migration_missing_column_fails_loudly() {
    let path = temp_db_path("inconsistent-migration");
    cleanup_db_files(&path);
    {
        let conn = Connection::open(&path).expect("Failed to create fixture database");
        conn.execute_batch(
            "CREATE TABLE memories (
                id TEXT PRIMARY KEY, content TEXT NOT NULL, memory_type TEXT NOT NULL,
                confidence REAL NOT NULL DEFAULT 1.0, linked_symbols TEXT NOT NULL DEFAULT '[]',
                source_query TEXT, created_at INTEGER NOT NULL, last_accessed INTEGER NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0, is_stale INTEGER NOT NULL DEFAULT 0,
                stale_reason TEXT, is_invalidated INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE memory_schema_migrations (
                version INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, applied_at INTEGER NOT NULL
            );
            INSERT INTO memory_schema_migrations (version, name, applied_at)
            VALUES (1, 'add_session_id', 1);",
        )
        .expect("Failed to seed inconsistent migration fixture");
    }

    let error = match MemoryStore::open(&path) {
        Ok(_) => panic!("inconsistent migration must not be ignored"),
        Err(error) => error,
    };
    assert!(error
        .to_string()
        .contains("recorded but column 'session_id' is missing"));
    cleanup_db_files(&path);
}

#[test]
fn test_invalidate_memory() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mem = make_memory(
        "Temporary observation about auth flow",
        MemoryType::Exploration,
        vec![],
    );

    let id = store.store(mem).expect("Failed to store memory");

    // Verify it exists
    let all = store.list_all().expect("Failed to list memories");
    assert_eq!(all.len(), 1);

    // Invalidate
    store.invalidate(&id).expect("Failed to invalidate memory");

    // Should not appear in list_all
    let all = store.list_all().expect("Failed to list after invalidation");
    assert_eq!(all.len(), 0);
}

#[test]
fn test_session_id_stored_and_retrieved() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mem = make_memory_with_session(
        "Auth uses JWT tokens",
        MemoryType::Observation,
        vec!["auth"],
        "s-abc123",
    );

    store.store(mem).expect("Failed to store memory");

    let all = store.list_all().expect("Failed to list memories");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].session_id, "s-abc123");
}

#[test]
fn test_structured_memory_fields_round_trip() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mut mem = make_memory_with_session(
        "Verified auth decision for structured recall",
        MemoryType::Decision,
        vec!["loginUser", "createSession"],
        "session-structured",
    );
    mem.scope = MemoryScope::Repo;
    mem.confidence = 0.97;
    mem.linked_files = vec!["src/auth.ts".to_string(), "src/session.ts".to_string()];
    mem.workspace_id = Some("workspace-structured".to_string());
    mem.branch = Some("main".to_string());
    mem.refresh_key = Some("auth::jwt::structured".to_string());
    mem.source_query = Some("structured memory round trip".to_string());
    mem.created_at = 1_700_001_000;
    mem.last_accessed = 1_700_001_001;
    mem.access_count = 4;
    mem.is_stale = true;
    mem.stale_reason = Some("seeded for stale recall coverage".to_string());

    let id = store.store(mem.clone()).expect("Failed to store memory");
    let loaded = store
        .get_by_id(&id)
        .expect("Failed to reload memory")
        .expect("Memory should still exist");

    assert_eq!(loaded.id, id);
    assert_eq!(loaded.session_id, mem.session_id);
    assert_eq!(loaded.content, mem.content);
    assert_eq!(loaded.memory_type, mem.memory_type);
    assert_eq!(loaded.scope, mem.scope);
    assert_eq!(loaded.confidence, mem.confidence);
    assert_eq!(loaded.linked_symbols, mem.linked_symbols);
    assert_eq!(loaded.linked_files, mem.linked_files);
    assert_eq!(loaded.workspace_id, mem.workspace_id);
    assert_eq!(loaded.branch, mem.branch);
    assert_eq!(loaded.refresh_key, mem.refresh_key);
    assert_eq!(loaded.source_query, mem.source_query);
    assert_eq!(loaded.created_at, mem.created_at);
    assert_eq!(loaded.last_accessed, mem.last_accessed);
    assert_eq!(loaded.access_count, mem.access_count);
    assert_eq!(loaded.is_stale, mem.is_stale);
    assert_eq!(loaded.stale_reason, mem.stale_reason);
}

#[test]
fn test_get_session_memories() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    // Session A memories
    store
        .store(make_memory_with_session(
            "mem A1",
            MemoryType::Observation,
            vec![],
            "session-a",
        ))
        .unwrap();
    store
        .store(make_memory_with_session(
            "mem A2",
            MemoryType::Decision,
            vec![],
            "session-a",
        ))
        .unwrap();

    // Session B memories
    store
        .store(make_memory_with_session(
            "mem B1",
            MemoryType::Observation,
            vec![],
            "session-b",
        ))
        .unwrap();

    let session_a = store
        .get_session_memories("session-a", 10)
        .expect("Failed to get session memories");
    assert_eq!(session_a.len(), 2);
    assert!(session_a.iter().all(|m| m.session_id == "session-a"));

    let session_b = store
        .get_session_memories("session-b", 10)
        .expect("Failed to get session memories");
    assert_eq!(session_b.len(), 1);
    assert_eq!(session_b[0].session_id, "session-b");

    let session_c = store
        .get_session_memories("session-c", 10)
        .expect("Failed to get session memories");
    assert_eq!(session_c.len(), 0);
}

#[test]
fn test_search_across_sessions() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    store
        .store(make_memory_with_session(
            "JWT auth pattern",
            MemoryType::Pattern,
            vec![],
            "s1",
        ))
        .unwrap();
    store
        .store(make_memory_with_session(
            "JWT validation bug",
            MemoryType::Observation,
            vec![],
            "s2",
        ))
        .unwrap();
    store
        .store(make_memory_with_session(
            "Database pool config",
            MemoryType::Decision,
            vec![],
            "s1",
        ))
        .unwrap();

    // Search across all sessions
    let results = store
        .search_across_sessions("JWT", None, 10)
        .expect("search failed");
    assert_eq!(results.len(), 2);

    // Search excluding s1
    let results = store
        .search_across_sessions("JWT", Some("s1"), 10)
        .expect("search failed");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].session_id, "s2");

    // Search excluding s2
    let results = store
        .search_across_sessions("JWT", Some("s2"), 10)
        .expect("search failed");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].session_id, "s1");

    // Multi-word search: words matched independently (AND logic)
    let results = store
        .search_across_sessions("auth pattern", None, 10)
        .expect("search failed");
    assert_eq!(
        results.len(),
        1,
        "Should match 'JWT auth pattern' with words 'auth' AND 'pattern'"
    );

    // Multi-word: no single entry contains both words
    let results = store
        .search_across_sessions("JWT pool", None, 10)
        .expect("search failed");
    assert_eq!(
        results.len(),
        0,
        "No single memory contains both 'JWT' and 'pool'"
    );

    // Empty keyword returns all
    let results = store
        .search_across_sessions("", None, 10)
        .expect("search failed");
    assert_eq!(results.len(), 3, "Empty search should return all memories");
}

#[test]
fn test_search_across_sessions_keeps_camel_case_identifier_queries_precise() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    store
        .store(make_memory_with_session(
            "SessionMetrics collects workflow timing",
            MemoryType::Pattern,
            vec!["SessionMetrics"],
            "s1",
        ))
        .expect("Failed to store camel-case identifier memory");
    store
        .store(make_memory_with_session(
            "session metrics dashboard notes",
            MemoryType::Observation,
            vec![],
            "s2",
        ))
        .expect("Failed to store natural-language session memory");

    let results = store
        .search_across_sessions("SessionMetrics", None, 10)
        .expect("search failed");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].session_id, "s1");
}

#[test]
fn test_search_across_sessions_after_reopen_for_new_session() {
    let path = temp_db_path("reopen");
    cleanup_db_files(&path);

    {
        let store = MemoryStore::open(&path).expect("Failed to open file-backed store");
        store
            .store(make_memory_with_session(
                "JWT auth pattern",
                MemoryType::Pattern,
                vec!["loginUser"],
                "session-a",
            ))
            .expect("Failed to store session-a memory");
    }

    {
        let store = MemoryStore::open(&path).expect("Failed to reopen file-backed store");
        let results = store
            .search_across_sessions("", Some("session-b"), 10)
            .expect("search across sessions after reopen failed");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].session_id, "session-a");
    }

    cleanup_db_files(&path);
}

#[test]
fn test_two_connections_can_recall_previous_session_memories() {
    let path = temp_db_path("two-connections");
    cleanup_db_files(&path);

    let store_a = MemoryStore::open(&path).expect("Failed to open first store");
    let store_b = MemoryStore::open(&path).expect("Failed to open second store");

    store_a
        .store(make_memory_with_session(
            "JWT auth pattern",
            MemoryType::Pattern,
            vec!["loginUser"],
            "session-a",
        ))
        .expect("Failed to store session-a memory");

    let results = store_b
        .search_across_sessions("JWT", Some("session-b"), 10)
        .expect("cross-connection recall failed");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].session_id, "session-a");

    drop(store_b);
    drop(store_a);
    cleanup_db_files(&path);
}

#[test]
fn test_mark_stale_by_file() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mut mem = make_memory(
        "Auth route uses loginUser and loginRoute",
        MemoryType::Observation,
        vec!["loginUser"],
    );
    mem.linked_files = vec!["src/auth.ts".to_string()];

    store.store(mem).expect("Failed to store memory");

    let updated = store
        .mark_stale_by_file("src/auth.ts", "auth.ts changed")
        .expect("Failed to mark stale by file");
    assert_eq!(updated, 1);

    let all = store.list_all().expect("Failed to list memories");
    assert!(all[0].is_stale);
    assert_eq!(all[0].stale_reason.as_deref(), Some("auth.ts changed"));
}

#[test]
fn test_mark_stale_preserves_structured_metadata_for_contradictions() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mut mem = make_memory(
        "Verified auth decision that later gets contradicted",
        MemoryType::Decision,
        vec!["loginUser"],
    );
    mem.scope = MemoryScope::Repo;
    mem.confidence = 0.93;
    mem.linked_files = vec!["src/auth.ts".to_string()];
    mem.workspace_id = Some("workspace-a".to_string());
    mem.branch = Some("main".to_string());
    mem.refresh_key = Some("auth::jwt".to_string());
    mem.source_query = Some("contradiction coverage".to_string());

    let id = store.store(mem).expect("Failed to store memory");

    let updated = store
        .mark_stale_by_symbol("loginUser", "contradicted by newer verified memory")
        .expect("Failed to mark stale");
    assert_eq!(updated, 1);

    let stale = store
        .list_stale(Some("auth"), 10)
        .expect("Failed to list stale memories");
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].id, id);
    assert_eq!(stale[0].confidence, 0.93);
    assert_eq!(stale[0].workspace_id.as_deref(), Some("workspace-a"));
    assert_eq!(stale[0].branch.as_deref(), Some("main"));
    assert_eq!(stale[0].refresh_key.as_deref(), Some("auth::jwt"));
    assert_eq!(
        stale[0].source_query.as_deref(),
        Some("contradiction coverage")
    );
    assert_eq!(stale[0].linked_files, vec!["src/auth.ts".to_string()]);
    assert!(stale[0].is_stale);
    assert_eq!(
        stale[0].stale_reason.as_deref(),
        Some("contradicted by newer verified memory")
    );
}

#[test]
fn test_promote_memory_updates_scope_and_metadata() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mem = make_memory(
        "JWT auth should stay repo-wide",
        MemoryType::Decision,
        vec!["loginUser"],
    );
    let id = store.store(mem).expect("Failed to store memory");

    store
        .promote_memory(
            &id,
            MemoryScope::Repo,
            Some(&["src/auth.ts".to_string(), "src/session.ts".to_string()]),
            Some("workspace-a"),
            Some("feature/memory"),
            Some("auth::jwt"),
        )
        .expect("Failed to promote memory");

    let all = store.list_all().expect("Failed to list memories");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].scope, MemoryScope::Repo);
    assert_eq!(all[0].workspace_id.as_deref(), Some("workspace-a"));
    assert_eq!(all[0].branch.as_deref(), Some("feature/memory"));
    assert_eq!(all[0].refresh_key.as_deref(), Some("auth::jwt"));
    assert_eq!(
        all[0].linked_files,
        vec!["src/auth.ts".to_string(), "src/session.ts".to_string()]
    );
}

#[test]
fn test_list_stale_memories_with_query() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mut auth_mem = make_memory("JWT auth decision", MemoryType::Decision, vec!["loginUser"]);
    auth_mem.linked_files = vec!["src/auth.ts".to_string()];
    auth_mem.is_stale = true;
    auth_mem.stale_reason = Some("auth.ts changed".to_string());

    let mut db_mem = make_memory("DB pool note", MemoryType::Observation, vec!["dbPool"]);
    db_mem.linked_files = vec!["src/db.ts".to_string()];
    db_mem.is_stale = true;
    db_mem.stale_reason = Some("db.ts changed".to_string());

    store.store(auth_mem).expect("Failed to store auth memory");
    store.store(db_mem).expect("Failed to store db memory");

    let stale = store
        .list_stale(Some("auth"), 10)
        .expect("Failed to list stale memories");
    assert_eq!(stale.len(), 1);
    assert!(stale[0].content.contains("auth"));
}

#[test]
fn test_refresh_memory_updates_content_and_metadata() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mut mem = make_memory("JWT auth decision", MemoryType::Decision, vec!["loginUser"]);
    mem.scope = MemoryScope::Branch;
    mem.linked_files = vec!["src/auth.ts".to_string()];
    mem.workspace_id = Some("workspace-a".to_string());
    mem.branch = Some("feature/auth".to_string());
    mem.refresh_key = Some("auth::jwt".to_string());
    mem.source_query = Some("auth decisions".to_string());
    mem.is_stale = true;
    mem.stale_reason = Some("auth.ts changed".to_string());

    let id = store.store(mem).expect("Failed to store memory");

    let refreshed = store
        .refresh_memory(
            &id,
            Some("JWT auth decision refreshed after login refactor"),
            Some(MemoryType::Pattern),
            Some(MemoryScope::Repo),
            Some(&["loginUser".to_string(), "createSession".to_string()]),
            Some(&["src/auth.ts".to_string(), "src/session.ts".to_string()]),
            Some("workspace-b"),
            Some("main"),
            Some("auth::jwt::v2"),
            Some("refresh auth memory"),
            Some(0.85),
        )
        .expect("Failed to refresh memory");

    assert_eq!(refreshed.id, id);
    assert_eq!(refreshed.memory_type, MemoryType::Pattern);
    assert_eq!(refreshed.scope, MemoryScope::Repo);
    assert_eq!(
        refreshed.content,
        "JWT auth decision refreshed after login refactor"
    );
    assert_eq!(
        refreshed.linked_symbols,
        vec!["loginUser".to_string(), "createSession".to_string()]
    );
    assert_eq!(
        refreshed.linked_files,
        vec!["src/auth.ts".to_string(), "src/session.ts".to_string()]
    );
    assert_eq!(refreshed.workspace_id.as_deref(), Some("workspace-b"));
    assert_eq!(refreshed.branch.as_deref(), Some("main"));
    assert_eq!(refreshed.refresh_key.as_deref(), Some("auth::jwt::v2"));
    assert_eq!(
        refreshed.source_query.as_deref(),
        Some("refresh auth memory")
    );
    assert_eq!(refreshed.confidence, 0.85);
    assert!(!refreshed.is_stale);
    assert_eq!(refreshed.stale_reason, None);
    assert_eq!(refreshed.access_count, 1);

    let stored = store
        .get_by_id(&id)
        .expect("Failed to reload memory")
        .expect("Memory should still exist");
    assert_eq!(stored.content, refreshed.content);
    assert_eq!(stored.refresh_key, refreshed.refresh_key);
    assert!(!stored.is_stale);
}

#[test]
fn test_get_and_update_structured_fields_round_trip() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let id = store
        .store(make_memory(
            "Constraint memory",
            MemoryType::Observation,
            vec!["loginUser"],
        ))
        .expect("Failed to store memory");

    let expected = MemoryStructuredFields {
        memory_class: MemoryClass::Constraint,
        assertion_type: MemoryAssertionType::Constraint,
        verification_status: MemoryVerificationStatus::Verified,
        confidence_reason: Some("Validated against tenant boundary checks".to_string()),
        supersedes_memory_id: Some("prior-memory".to_string()),
        superseded_by_memory_id: Some("replacement-memory".to_string()),
        contradicts_memory_ids: vec!["older-constraint".to_string()],
        contradicted_by_memory_ids: vec!["newer-constraint".to_string()],
        freshness_policy: MemoryFreshnessPolicy::ManualReview,
        freshness_policy_detail: Some("Recheck when auth rules change".to_string()),
        validity_conditions: vec!["tenant boundary checks stay enforced".to_string()],
        invalidation_triggers: vec!["auth rules change".to_string()],
        provenance: vec![MemoryProvenance {
            source: "test".to_string(),
            reference: Some("structured-round-trip".to_string()),
            captured_at: Some(1_700_003_000),
            note: Some("seeded for direct store coverage".to_string()),
        }],
        evidence: vec![MemoryEvidence {
            kind: "file".to_string(),
            reference: Some("src/auth.ts".to_string()),
            detail: Some("Tenant boundary validation".to_string()),
            captured_at: Some(1_700_003_001),
            span: None,
            evidence_content_hash: None,
        }],
        linked_docs: vec!["docs/auth.md#TenantBoundaries".to_string()],
        linked_tests: vec!["tests/test_auth.py::test_tenant_boundaries".to_string()],
        linked_memories: vec!["sibling-memory".to_string()],
    };

    store
        .update_structured_fields(&id, &expected)
        .expect("Failed to update structured fields");

    let actual = store
        .get_structured_fields(&id)
        .expect("Failed to load structured fields")
        .expect("Expected structured fields");

    assert_eq!(actual, expected);
}

#[test]
fn test_store_rewrite_preserves_extended_assertion_type() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mut memory = make_memory("Workflow memory", MemoryType::Decision, vec!["loginUser"]);
    memory.refresh_key = Some("auth::workflow".to_string());

    let id = store.store(memory).expect("Failed to store memory");

    let mut fields = store
        .get_structured_fields(&id)
        .expect("Failed to load structured fields")
        .expect("Expected structured fields");
    fields.assertion_type = MemoryAssertionType::WorkflowOutcome;
    fields.verification_status = MemoryVerificationStatus::Verified;
    store
        .update_structured_fields(&id, &fields)
        .expect("Failed to persist workflow assertion type");

    let mut rewritten = store
        .get_by_id(&id)
        .expect("Failed to reload memory")
        .expect("Expected stored memory");
    rewritten.content = "Workflow memory rewritten".to_string();
    store
        .store(rewritten)
        .expect("Failed to rewrite memory through store()");

    let persisted = store
        .get_structured_fields(&id)
        .expect("Failed to reload structured fields")
        .expect("Expected structured fields after rewrite");

    assert_eq!(
        persisted.assertion_type,
        MemoryAssertionType::WorkflowOutcome
    );
    assert_eq!(
        persisted.verification_status,
        MemoryVerificationStatus::Verified
    );
}

#[test]
fn test_mark_memory_superseded_updates_structured_fields() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let prior_id = store
        .store(make_memory(
            "Old workflow",
            MemoryType::Observation,
            vec!["loginUser"],
        ))
        .expect("Failed to store prior memory");
    let replacement_id = store
        .store(make_memory(
            "Replacement workflow",
            MemoryType::Observation,
            vec!["loginUser"],
        ))
        .expect("Failed to store replacement memory");

    store
        .mark_memory_superseded(&prior_id, &replacement_id)
        .expect("Failed to mark memory superseded");

    let prior_fields = store
        .get_structured_fields(&prior_id)
        .expect("Failed to reload prior structured fields")
        .expect("Expected structured fields for superseded memory");

    assert_eq!(
        prior_fields.verification_status,
        MemoryVerificationStatus::Superseded
    );
    assert_eq!(
        prior_fields.superseded_by_memory_id.as_deref(),
        Some(replacement_id.as_str())
    );
}

#[test]
fn test_mark_memory_contradicted_persists_reverse_edges() {
    let path = temp_db_path("structured-contradiction");
    cleanup_db_files(&path);
    let contradicted_id;
    let contradictor_id;

    {
        let store = MemoryStore::open(&path).expect("Failed to open memory store");

        contradicted_id = store
            .store(make_memory(
                "Older auth note",
                MemoryType::Observation,
                vec!["loginUser"],
            ))
            .expect("Failed to store contradicted memory");
        contradictor_id = store
            .store(make_memory(
                "Newer auth note",
                MemoryType::Observation,
                vec!["loginUser"],
            ))
            .expect("Failed to store contradictor memory");

        store
            .mark_memory_contradicted(&contradicted_id, &contradictor_id)
            .expect("Failed to mark contradiction");

        let contradicted = store
            .get_structured_fields(&contradicted_id)
            .expect("Failed to load contradicted fields")
            .expect("Expected contradicted fields");
        let contradictor = store
            .get_structured_fields(&contradictor_id)
            .expect("Failed to load contradictor fields")
            .expect("Expected contradictor fields");

        assert_eq!(
            contradicted.verification_status,
            MemoryVerificationStatus::Contradicted
        );
        assert_eq!(
            contradicted.contradicted_by_memory_ids,
            vec![contradictor_id.clone()]
        );
        assert_eq!(
            contradictor.contradicts_memory_ids,
            vec![contradicted_id.clone()]
        );
    }

    let reopened = MemoryStore::open(&path).expect("Failed to reopen memory store");
    let older = reopened
        .get_structured_fields(&contradicted_id)
        .expect("Failed to load reopened structured fields")
        .expect("Expected contradicted fields after reopen");
    let newer = reopened
        .get_structured_fields(&contradictor_id)
        .expect("Failed to load reopened reverse structured fields")
        .expect("Expected contradictor fields after reopen");
    assert_eq!(
        older.contradicted_by_memory_ids,
        vec![contradictor_id.clone()]
    );
    assert_eq!(newer.contradicts_memory_ids, vec![contradicted_id.clone()]);
    assert_eq!(
        older.verification_status,
        MemoryVerificationStatus::Contradicted
    );

    cleanup_db_files(&path);
}

#[test]
fn test_refresh_memory_preserves_extended_assertion_type() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let id = store
        .store(make_memory(
            "Constraint note",
            MemoryType::Observation,
            vec!["loginUser"],
        ))
        .expect("Failed to store memory");

    let mut fields = store
        .get_structured_fields(&id)
        .expect("Failed to load structured fields")
        .expect("Expected structured fields");
    fields.assertion_type = MemoryAssertionType::Constraint;
    fields.verification_status = MemoryVerificationStatus::InReview;
    store
        .update_structured_fields(&id, &fields)
        .expect("Failed to update structured fields");

    store
        .refresh_memory(
            &id,
            Some("Constraint note refreshed"),
            Some(MemoryType::Pattern),
            Some(MemoryScope::Repo),
            None,
            None,
            Some("workspace-a"),
            Some("main"),
            Some("auth::constraint"),
            Some("refresh constraint"),
            Some(0.82),
        )
        .expect("Failed to refresh memory");

    let refreshed_fields = store
        .get_structured_fields(&id)
        .expect("Failed to reload structured fields")
        .expect("Expected structured fields after refresh");

    assert_eq!(
        refreshed_fields.assertion_type,
        MemoryAssertionType::Constraint
    );
    assert_eq!(
        refreshed_fields.verification_status,
        MemoryVerificationStatus::InReview
    );
}

#[test]
fn test_refresh_memory_preserves_unset_fields() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mut mem = make_memory("Auth note", MemoryType::Observation, vec!["loginUser"]);
    mem.scope = MemoryScope::Branch;
    mem.linked_files = vec!["src/auth.ts".to_string()];
    mem.workspace_id = Some("workspace-a".to_string());
    mem.branch = Some("feature/auth".to_string());
    mem.refresh_key = Some("auth::jwt".to_string());

    let id = store.store(mem).expect("Failed to store memory");

    let refreshed = store
        .refresh_memory(
            &id,
            Some("Auth note updated"),
            None,
            None,
            None,
            None,
            None,
            None,
            Some("auth::jwt::v2"),
            None,
            None,
        )
        .expect("Failed to refresh memory");

    assert_eq!(refreshed.memory_type, MemoryType::Observation);
    assert_eq!(refreshed.scope, MemoryScope::Branch);
    assert_eq!(refreshed.linked_files, vec!["src/auth.ts".to_string()]);
    assert_eq!(refreshed.workspace_id.as_deref(), Some("workspace-a"));
    assert_eq!(refreshed.branch.as_deref(), Some("feature/auth"));
    assert_eq!(refreshed.refresh_key.as_deref(), Some("auth::jwt::v2"));
    assert_eq!(refreshed.content, "Auth note updated");
}

#[test]
fn test_find_by_refresh_key_prefers_stronger_verified_memory_over_newer_weaker_memory() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mut verified = make_memory(
        "Verified auth decision",
        MemoryType::Decision,
        vec!["loginUser"],
    );
    verified.scope = MemoryScope::Repo;
    verified.confidence = 0.98;
    verified.workspace_id = Some("workspace-a".to_string());
    verified.branch = Some("main".to_string());
    verified.refresh_key = Some("auth::jwt".to_string());
    verified.source_query = Some("verified from code and tests".to_string());
    verified.created_at = 1_700_002_000;
    verified.last_accessed = 1_700_002_000;

    store
        .store(verified.clone())
        .expect("Failed to store verified memory");

    let mut weaker = make_memory(
        "Weaker contradictory note",
        MemoryType::Observation,
        vec!["loginUser"],
    );
    weaker.scope = MemoryScope::Repo;
    weaker.confidence = 0.25;
    weaker.workspace_id = Some("workspace-a".to_string());
    weaker.branch = Some("main".to_string());
    weaker.refresh_key = Some("auth::jwt".to_string());
    weaker.source_query = Some("late contradictory observation".to_string());
    weaker.is_stale = true;
    weaker.stale_reason = Some("contradicted by verified memory".to_string());
    weaker.created_at = 1_700_002_100;
    weaker.last_accessed = 1_700_002_100;

    store
        .store(weaker.clone())
        .expect("Failed to store weaker memory");

    let recalled = store
        .find_by_refresh_key("auth::jwt", Some("workspace-a"), Some("main"))
        .expect("Failed to recall by refresh key")
        .expect("Expected a recalled memory");

    assert_eq!(recalled.content, verified.content);
    assert_eq!(recalled.confidence, verified.confidence);
    assert_eq!(recalled.source_query, verified.source_query);
    assert!(
        recalled.confidence > weaker.confidence,
        "verified memory should win over newer weaker contradiction"
    );
}

#[test]
fn test_find_by_refresh_key_prefers_matching_workspace_and_branch() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mut repo_mem = make_memory("Repo playbook", MemoryType::Pattern, vec!["loginUser"]);
    repo_mem.scope = MemoryScope::Repo;
    repo_mem.workspace_id = Some("workspace-a".to_string());
    repo_mem.refresh_key = Some("repo_playbook".to_string());
    store.store(repo_mem).expect("Failed to store repo memory");

    let mut branch_mem = make_memory(
        "Branch-specific auth playbook",
        MemoryType::Pattern,
        vec!["loginUser"],
    );
    branch_mem.scope = MemoryScope::Branch;
    branch_mem.workspace_id = Some("workspace-a".to_string());
    branch_mem.branch = Some("feature/auth".to_string());
    branch_mem.refresh_key = Some("subsystem_playbook::auth".to_string());
    store
        .store(branch_mem)
        .expect("Failed to store branch playbook memory");

    let repo = store
        .find_by_refresh_key("repo_playbook", Some("workspace-a"), None)
        .expect("Failed to load repo playbook")
        .expect("expected repo playbook memory");
    assert_eq!(repo.content, "Repo playbook");

    let branch = store
        .find_by_refresh_key(
            "subsystem_playbook::auth",
            Some("workspace-a"),
            Some("feature/auth"),
        )
        .expect("Failed to load branch playbook")
        .expect("expected branch playbook memory");
    assert_eq!(branch.content, "Branch-specific auth playbook");
}

#[test]
fn test_find_by_refresh_key_prefers_repo_workflow_outcome_over_newer_session_observation() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mut stronger = make_memory(
        "Verified repo workflow",
        MemoryType::Decision,
        vec!["loginUser"],
    );
    stronger.scope = MemoryScope::Repo;
    stronger.confidence = 0.61;
    stronger.workspace_id = Some("workspace-a".to_string());
    stronger.refresh_key = Some("auth::runbook".to_string());
    stronger.created_at = 1_700_004_000;
    stronger.last_accessed = 1_700_004_000;
    let stronger_id = store
        .store(stronger)
        .expect("Failed to store stronger memory");

    let mut stronger_fields = store
        .get_structured_fields(&stronger_id)
        .expect("Failed to load stronger fields")
        .expect("Expected stronger structured fields");
    stronger_fields.assertion_type = MemoryAssertionType::WorkflowOutcome;
    stronger_fields.verification_status = MemoryVerificationStatus::Verified;
    store
        .update_structured_fields(&stronger_id, &stronger_fields)
        .expect("Failed to persist stronger structured fields");

    let mut weaker = make_memory(
        "Newer session observation",
        MemoryType::Observation,
        vec!["loginUser"],
    );
    weaker.scope = MemoryScope::Session;
    weaker.confidence = 0.99;
    weaker.workspace_id = Some("workspace-a".to_string());
    weaker.refresh_key = Some("auth::runbook".to_string());
    weaker.created_at = 1_700_004_100;
    weaker.last_accessed = 1_700_004_100;
    let weaker_id = store.store(weaker).expect("Failed to store weaker memory");

    let mut weaker_fields = store
        .get_structured_fields(&weaker_id)
        .expect("Failed to load weaker fields")
        .expect("Expected weaker structured fields");
    weaker_fields.verification_status = MemoryVerificationStatus::Verified;
    store
        .update_structured_fields(&weaker_id, &weaker_fields)
        .expect("Failed to persist weaker structured fields");

    let recalled = store
        .find_by_refresh_key("auth::runbook", Some("workspace-a"), None)
        .expect("Failed to recall by refresh key")
        .expect("Expected a recalled memory");

    assert_eq!(recalled.id, stronger_id);
    assert_eq!(recalled.content, "Verified repo workflow");
}

#[test]
fn exact_path_recall_survives_ten_thousand_newer_irrelevant_stale_and_wrong_branch_rows() {
    let store = MemoryStore::open_in_memory().unwrap();
    let mut lesson = make_memory("Apply integer rounding once", MemoryType::Decision, vec![]);
    lesson.scope = MemoryScope::Repo;
    lesson.workspace_id = Some("repository".into());
    lesson.linked_files = vec!["src/invoice.rs".into()];
    lesson.created_at = 1;
    lesson.confidence = 0.5;
    let id = store.store(lesson).unwrap();
    store.with_connection(|conn| {
        let mut stmt=conn.prepare("PRAGMA table_info(memories)").unwrap();
        let columns=stmt.query_map([],|row|row.get::<_,String>(1)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        let expressions:Vec<String>=columns.iter().map(|column|match column.as_str() {
            "id"=>"'noise-'||n".into(),
            "created_at"=>"2000000000+n".into(),
            "confidence"=>"1.0".into(),
            "content"=>"'Invoice investigation observation without a correction'".into(),
            "scope"=>"CASE WHEN n%3=0 THEN 'branch' ELSE 'repo' END".into(),
            "branch"=>"CASE WHEN n%3=0 THEN 'other' ELSE NULL END".into(),
            "retention_stale"=>"CASE WHEN n%3=1 THEN 1 ELSE 0 END".into(),
            "linked_files"=>"CASE WHEN n%3=2 THEN '[\"unrelated.rs\"]' ELSE '[\"src/invoice.rs\"]' END".into(),
            _=>format!("memories.\"{column}\""),
        }).collect();
        conn.execute(&format!("WITH RECURSIVE noise(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM noise WHERE n<10000) INSERT INTO memories ({}) SELECT {} FROM memories CROSS JOIN noise WHERE memories.id=?1", columns.iter().map(|c|format!("\"{c}\"")).collect::<Vec<_>>().join(","),expressions.join(",")),[&id]).unwrap();
        Ok(())
    }).unwrap();
    let scope = crate::verification::ScopeFilter::new("repository", None, None);
    let found = store
        .recall_candidates(
            Some("src/invoice.rs"),
            1,
            &scope,
            Some("checkout"),
            super::retrieval::RecallOptions::default(),
        )
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, id);
    let stale = store
        .recall_candidates(
            Some("src/invoice.rs"),
            1,
            &scope,
            Some("checkout"),
            super::retrieval::RecallOptions {
                include_retention_stale: true,
            },
        )
        .unwrap();
    assert!(stale[0].id.starts_with("noise-"));
}

#[test]
fn natural_language_recall_ranks_lexical_coverage_before_confidence_and_filters_scope() {
    let store = MemoryStore::open_in_memory().expect("open memory store");
    let scope = crate::verification::ScopeFilter::new("repository", None, None);

    let mut lesson = make_memory(
        "Keep fixture writes atomic; partial writes corrupt recovery.",
        MemoryType::Decision,
        vec![],
    );
    lesson.scope = MemoryScope::Repo;
    lesson.workspace_id = Some("repository".to_string());
    lesson.confidence = 0.2;
    let lesson_id = store.store(lesson).expect("store relevant lesson");

    for index in 0..8 {
        let mut noise = make_memory(
            "Fixture scratch note for a routine check.",
            MemoryType::Observation,
            vec![],
        );
        noise.scope = MemoryScope::Repo;
        noise.workspace_id = Some("repository".to_string());
        noise.confidence = 0.99;
        noise.created_at = 10_000 + index;
        store.store(noise).expect("store common-word noise");
    }

    let mut wrong_scope = make_memory(
        "Keep fixture writes atomic; partial writes corrupt recovery.",
        MemoryType::Decision,
        vec![],
    );
    wrong_scope.scope = MemoryScope::Repo;
    wrong_scope.workspace_id = Some("other-repository".to_string());
    store.store(wrong_scope).expect("store wrong-scope lesson");

    let mut stale = make_memory(
        "Keep fixture writes atomic; partial writes corrupt recovery.",
        MemoryType::Decision,
        vec![],
    );
    stale.scope = MemoryScope::Repo;
    stale.workspace_id = Some("repository".to_string());
    stale.is_stale = true;
    store.store(stale).expect("store stale lesson");

    let recalled = store
        .recall_candidates(
            Some("How should fixture writes avoid corrupt recovery?"),
            1,
            &scope,
            Some("checkout"),
            super::retrieval::RecallOptions::default(),
        )
        .expect("natural-language indexed recall");
    assert_eq!(
        recalled.iter().map(|memory| &memory.id).collect::<Vec<_>>(),
        vec![&lesson_id]
    );

    let safe = store
        .recall_candidates(
            Some("fixture \"writes\"; (avoid) recovery\\"),
            1,
            &scope,
            Some("checkout"),
            super::retrieval::RecallOptions::default(),
        )
        .expect("sanitized lexical terms must remain valid FTS syntax");
    assert_eq!(safe[0].id, lesson_id);
}

#[test]
fn recall_vm_budget_interrupts_without_poisoning_the_next_query() {
    let store = MemoryStore::open_in_memory().expect("open memory store");
    let scope = crate::verification::ScopeFilter::new("repository", None, None);
    for index in 0..512 {
        let mut memory = make_memory(
            &format!("highfrequency posting payload {index}"),
            MemoryType::Observation,
            vec![],
        );
        memory.scope = MemoryScope::Repo;
        memory.workspace_id = Some("repository".to_string());
        store.store(memory).expect("store posting fixture");
    }
    let mut unique = make_memory("needle-unique-boundary", MemoryType::Decision, vec![]);
    unique.scope = MemoryScope::Repo;
    unique.workspace_id = Some("repository".to_string());
    let unique_id = store.store(unique).expect("store unique fixture");

    let error = store
        .recall_candidates_with_instruction_budget(
            Some("highfrequency"),
            128,
            &scope,
            Some("checkout"),
            super::retrieval::RecallOptions::default(),
            1,
        )
        .expect_err("low VM budget must interrupt posting expansion");
    assert!(error.to_string().contains("bounded SQLite work allowance"));

    let next = store
        .recall_candidates(
            Some("needle-unique-boundary"),
            1,
            &scope,
            Some("checkout"),
            super::retrieval::RecallOptions::default(),
        )
        .expect("RAII progress cleanup permits the next query");
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].id, unique_id);
}

#[test]
fn recall_filters_invalid_tokens_before_the_usable_term_cap() {
    let store = MemoryStore::open_in_memory().expect("open memory store");
    let scope = crate::verification::ScopeFilter::new("repository", None, None);
    let mut memory = make_memory("lateusabletoken", MemoryType::Decision, vec![]);
    memory.scope = MemoryScope::Repo;
    memory.workspace_id = Some("repository".to_string());
    let id = store.store(memory).expect("store lexical fixture");
    let query = format!("{} lateusabletoken", "() ".repeat(64));

    let recalled = store
        .recall_candidates(
            Some(&query),
            1,
            &scope,
            Some("checkout"),
            super::retrieval::RecallOptions::default(),
        )
        .expect("invalid noise is discarded before the term cap");
    assert_eq!(recalled.len(), 1);
    assert_eq!(recalled[0].id, id);

    let invalid_only = store
        .recall_candidates(
            Some("() ;;; ```"),
            10,
            &scope,
            Some("checkout"),
            super::retrieval::RecallOptions::default(),
        )
        .expect("invalid-only query is a bounded empty search");
    assert!(invalid_only.is_empty());
}

#[test]
fn recall_deduplicates_groups_before_the_term_cap() {
    let store = MemoryStore::open_in_memory().expect("open memory store");
    let scope = crate::verification::ScopeFilter::new("repository", None, None);
    let mut relevant = make_memory("commonword rareboundaryterm", MemoryType::Decision, vec![]);
    relevant.scope = MemoryScope::Repo;
    relevant.workspace_id = Some("repository".to_string());
    relevant.confidence = 0.1;
    let relevant_id = store.store(relevant).expect("store relevant memory");
    let mut noise = make_memory("commonword", MemoryType::Observation, vec![]);
    noise.scope = MemoryScope::Repo;
    noise.workspace_id = Some("repository".to_string());
    noise.confidence = 1.0;
    store.store(noise).expect("store common-term noise");

    let query = format!("{} rareboundaryterm", "commonword ".repeat(64));
    let recalled = store
        .recall_candidates(
            Some(&query),
            1,
            &scope,
            Some("checkout"),
            super::retrieval::RecallOptions::default(),
        )
        .expect("duplicate groups do not displace a later unique group");
    assert_eq!(recalled[0].id, relevant_id);
}

#[test]
fn recall_rejects_oversized_queries_and_terms_before_sql() {
    let store = MemoryStore::open_in_memory().expect("open memory store");
    let scope = crate::verification::ScopeFilter::new("repository", None, None);
    let options = super::retrieval::RecallOptions::default();

    let oversized_query = "q".repeat(32 * 1024 + 1);
    let error = store
        .recall_candidates(Some(&oversized_query), 1, &scope, Some("checkout"), options)
        .expect_err("oversized query must be rejected");
    assert!(error.to_string().contains("maximum is 32768"));

    let oversized_term = "t".repeat(513);
    let error = store
        .recall_candidates(Some(&oversized_term), 1, &scope, Some("checkout"), options)
        .expect_err("oversized term must be rejected");
    assert!(error.to_string().contains("maximum is 512"));
}

#[test]
fn recall_vm_budget_configuration_rejects_invalid_values() {
    for invalid in ["0", "not-a-number", "1000000001"] {
        let error = super::store::parse_recall_vm_instruction_budget(Some(invalid.into()))
            .expect_err("invalid VM budget must fail instead of silently using the default");
        assert!(error
            .to_string()
            .contains("LATTICE_MEMORY_RECALL_VM_INSTRUCTIONS"));
    }

    assert_eq!(
        super::store::parse_recall_vm_instruction_budget(Some("5000".into())).unwrap(),
        5000
    );
}

#[cfg(unix)]
#[test]
fn recall_vm_budget_configuration_rejects_non_unicode_values() {
    use std::os::unix::ffi::OsStringExt;

    let error =
        super::store::parse_recall_vm_instruction_budget(Some(std::ffi::OsString::from_vec(vec![
            0xff,
        ])))
        .expect_err("non-Unicode VM budget must fail instead of silently using the default");
    assert!(error.to_string().contains("UTF-8 positive integer"));
}
