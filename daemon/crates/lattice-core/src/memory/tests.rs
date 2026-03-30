use super::model::{Memory, MemoryScope, MemoryType};
use super::store::MemoryStore;
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
        refresh_key: None,
        source_query: None,
        created_at: 0,
        last_accessed: 0,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
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
fn test_memory_decay_and_pruning() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    // Store a memory with old timestamp
    store
        .store(Memory {
            id: "old-mem".to_string(),
            session_id: String::new(),
            content: "old observation".to_string(),
            memory_type: MemoryType::Observation,
            scope: MemoryScope::Session,
            confidence: 0.5,
            linked_symbols: vec![],
            linked_files: vec![],
            workspace_id: None,
            branch: None,
            refresh_key: None,
            source_query: None,
            created_at: 1000,    // Very old
            last_accessed: 1000, // Never accessed recently
            access_count: 0,
            is_stale: false,
            stale_reason: None,
        })
        .expect("Failed to store memory");

    // Decay should reduce confidence
    let decayed = store.decay_old_memories(0, 0.1).expect("Failed to decay");
    assert!(decayed > 0);

    let memories = store.list_all().expect("Failed to list memories");
    assert!(
        memories[0].confidence < 0.5,
        "Confidence should have decayed"
    );

    // Prune should remove low-confidence old memories
    let pruned = store.prune_old_memories(0.5, 0).expect("Failed to prune");
    assert!(pruned > 0);

    let remaining = store.list_all().expect("Failed to list remaining");
    assert!(
        remaining.is_empty(),
        "Low confidence memory should be pruned"
    );
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

    let mut mem = make_memory(
        "JWT auth decision",
        MemoryType::Decision,
        vec!["loginUser"],
    );
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
fn test_find_by_refresh_key_prefers_matching_workspace_and_branch() {
    let store = MemoryStore::open_in_memory().expect("Failed to open in-memory store");

    let mut repo_mem = make_memory("Repo playbook", MemoryType::Pattern, vec!["loginUser"]);
    repo_mem.scope = MemoryScope::Repo;
    repo_mem.workspace_id = Some("workspace-a".to_string());
    repo_mem.refresh_key = Some("repo_playbook".to_string());
    store.store(repo_mem).expect("Failed to store repo memory");

    let mut branch_mem =
        make_memory("Branch-specific auth playbook", MemoryType::Pattern, vec!["loginUser"]);
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
