use super::model::{Memory, MemoryType};
use super::store::MemoryStore;

fn make_memory(content: &str, memory_type: MemoryType, linked_symbols: Vec<&str>) -> Memory {
    Memory {
        id: String::new(),
        content: content.to_string(),
        memory_type,
        confidence: 1.0,
        linked_symbols: linked_symbols.into_iter().map(|s| s.to_string()).collect(),
        source_query: None,
        created_at: 0,
        last_accessed: 0,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
    }
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
    assert_eq!(all[0].content, "The loginUser function validates credentials before creating a session");
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
    assert_eq!(all[0].stale_reason.as_deref(), Some("hashPassword was refactored"));
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
            content: "old observation".to_string(),
            memory_type: MemoryType::Observation,
            confidence: 0.5,
            linked_symbols: vec![],
            source_query: None,
            created_at: 1000, // Very old
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
    let pruned = store
        .prune_old_memories(0.5, 0)
        .expect("Failed to prune");
    assert!(pruned > 0);

    let remaining = store.list_all().expect("Failed to list remaining");
    assert!(
        remaining.is_empty(),
        "Low confidence memory should be pruned"
    );
}
