use super::*;
use std::path::PathBuf;

#[test]
fn test_index_single_file() {
    let mut indexer = Indexer::new(PathBuf::from("/project"));

    let source = r#"
export function greet(name: string): string {
    return `Hello, ${name}!`;
}
"#;

    indexer
        .index_file_content("src/greet.ts", source)
        .expect("should parse successfully");

    assert_eq!(indexer.file_count(), 1);
    assert!(indexer.graph().node_count() >= 1);

    // Verify the specific node exists
    let nodes = indexer.graph().all_nodes();
    let greet_node = nodes.iter().find(|n| n.name == "greet");
    assert!(
        greet_node.is_some(),
        "greet function should be in the graph"
    );
}

#[test]
fn test_incremental_update() {
    let mut indexer = Indexer::new(PathBuf::from("/project"));

    // First version: two functions
    let source_v1 = r#"
export function alpha(): void {
    console.log("alpha");
}

export function beta(): void {
    console.log("beta");
}
"#;

    indexer
        .index_file_content("src/helpers.ts", source_v1)
        .expect("should parse v1");

    let nodes_v1 = indexer.graph().all_nodes();
    assert!(
        nodes_v1.iter().any(|n| n.name == "alpha"),
        "alpha should exist after v1"
    );
    assert!(
        nodes_v1.iter().any(|n| n.name == "beta"),
        "beta should exist after v1"
    );

    // Second version: replace alpha/beta with gamma/delta
    let source_v2 = r#"
export function gamma(): void {
    console.log("gamma");
}

export function delta(): void {
    console.log("delta");
}
"#;

    indexer
        .index_file_content("src/helpers.ts", source_v2)
        .expect("should parse v2");

    let nodes_v2 = indexer.graph().all_nodes();

    // Old functions should be gone
    assert!(
        !nodes_v2.iter().any(|n| n.name == "alpha"),
        "alpha should NOT exist after v2"
    );
    assert!(
        !nodes_v2.iter().any(|n| n.name == "beta"),
        "beta should NOT exist after v2"
    );

    // New functions should be present
    assert!(
        nodes_v2.iter().any(|n| n.name == "gamma"),
        "gamma should exist after v2"
    );
    assert!(
        nodes_v2.iter().any(|n| n.name == "delta"),
        "delta should exist after v2"
    );

    // Still only 1 file tracked
    assert_eq!(indexer.file_count(), 1);
}

// ---- Lazy indexer tests ----

use super::lazy::{IndexPriority, LazyIndexQueue};

#[test]
fn test_lazy_queue_priority_ordering() {
    let mut queue = LazyIndexQueue::new();
    queue.enqueue("src/background.ts".to_string(), IndexPriority::Background);
    queue.enqueue("src/import.ts".to_string(), IndexPriority::DirectImport);
    queue.enqueue("src/editor.ts".to_string(), IndexPriority::OpenInEditor);

    let first = queue.dequeue().expect("should have first entry");
    assert_eq!(
        first.path, "src/editor.ts",
        "highest priority (OpenInEditor) should come first"
    );
    assert_eq!(first.priority, IndexPriority::OpenInEditor);

    let second = queue.dequeue().expect("should have second entry");
    assert_eq!(
        second.path, "src/import.ts",
        "DirectImport should come second"
    );

    let third = queue.dequeue().expect("should have third entry");
    assert_eq!(
        third.path, "src/background.ts",
        "Background should come last"
    );

    assert!(queue.dequeue().is_none(), "queue should be empty");
}

#[test]
fn test_lazy_queue_no_duplicates() {
    let mut queue = LazyIndexQueue::new();
    queue.enqueue("src/utils.ts".to_string(), IndexPriority::Background);
    queue.enqueue("src/utils.ts".to_string(), IndexPriority::Background);

    let entry = queue.dequeue().expect("should dequeue once");
    assert_eq!(entry.path, "src/utils.ts");

    // Second dequeue should return None since the file is already indexed
    assert!(
        queue.dequeue().is_none(),
        "duplicate should not be dequeued"
    );
    assert!(
        queue.is_indexed("src/utils.ts"),
        "file should be marked as indexed"
    );
    assert_eq!(queue.indexed_count(), 1);
}

#[test]
fn test_lazy_queue_boost_priority() {
    let mut queue = LazyIndexQueue::new();
    queue.enqueue("src/app.ts".to_string(), IndexPriority::Background);
    queue.enqueue("src/other.ts".to_string(), IndexPriority::SameDirectory);

    // Boost app.ts to OpenInEditor
    queue.boost_priority("src/app.ts", IndexPriority::OpenInEditor);

    // The boosted entry should come out first (highest priority)
    let first = queue.dequeue().expect("should have first entry");
    assert_eq!(first.path, "src/app.ts", "boosted entry should come first");
    assert_eq!(first.priority, IndexPriority::OpenInEditor);

    let second = queue.dequeue().expect("should have second entry");
    assert_eq!(second.path, "src/other.ts");
}

#[tokio::test]
async fn test_parallel_indexing() {
    let dir = std::env::temp_dir().join("lattice_parallel_test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();

    // Create 10 test files
    for i in 0..10 {
        std::fs::write(
            dir.join(format!("src/mod{}.ts", i)),
            format!("export function func{}(): void {{}}", i),
        )
        .unwrap();
    }

    let mut indexer = Indexer::new(dir.clone());
    let count = indexer.index_directory_parallel(&dir).await.unwrap();
    assert_eq!(count, 10);
    assert!(indexer.graph().node_count() >= 10);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn test_batch_indexing_rebuilds_once_for_multiple_files() {
    let mut indexer = Indexer::new(PathBuf::from("/project"));
    let files = vec![
        (
            "docs/auth.md".to_string(),
            "# Auth\n\nUse `loginUser`.\n".to_string(),
        ),
        (
            "src/auth.ts".to_string(),
            "export function loginUser(): void {}".to_string(),
        ),
    ];

    let count = indexer.index_file_batch_contents(files).await.unwrap();
    assert_eq!(count, 2);
    assert_eq!(indexer.file_count(), 2);
    assert!(
        indexer
            .graph()
            .all_nodes()
            .iter()
            .any(|node| node.file == "docs/auth.md"),
        "markdown file should be present after batch indexing"
    );
    assert!(
        indexer
            .graph()
            .all_nodes()
            .iter()
            .any(|node| node.name == "loginUser"),
        "code symbol should be present after batch indexing"
    );
}

#[test]
fn test_stale_memory_on_file_change() {
    use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType};

    let store = MemoryStore::open_in_memory().unwrap();

    // Store a memory linked to "loginUser"
    store
        .store(Memory {
            id: String::new(),
            session_id: String::new(),
            content: "loginUser uses bcrypt".to_string(),
            memory_type: MemoryType::Observation,
            scope: MemoryScope::Session,
            confidence: 0.9,
            linked_symbols: vec!["loginUser".to_string()],
            linked_files: vec!["src/auth.ts".to_string()],
            workspace_id: None,
            branch: None,
            refresh_key: None,
            source_query: None,
            created_at: 0,
            last_accessed: 0,
            access_count: 0,
            is_stale: false,
            stale_reason: None,
        })
        .unwrap();

    let mut indexer = Indexer::new(PathBuf::from("/test"));

    // Index original
    indexer
        .index_file_content(
            "src/auth.ts",
            r#"
export function loginUser(): void { bcrypt(); }
"#,
        )
        .unwrap();

    // Re-index with changes — should mark memory stale
    let changes = indexer
        .index_file_with_stale_detection(
            "src/auth.ts",
            r#"export function loginUser(): void { argon2(); }"#,
            Some(&store),
        )
        .unwrap();

    assert!(!changes.is_empty());
    let memories = store.list_all().unwrap();
    assert!(
        memories[0].is_stale,
        "Memory should be marked stale after symbol modification"
    );
}

#[test]
fn test_file_linked_memory_stales_even_without_symbol_diff() {
    use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType};

    let store = MemoryStore::open_in_memory().unwrap();

    store
        .store(Memory {
            id: String::new(),
            session_id: String::new(),
            content: "File-level auth notes".to_string(),
            memory_type: MemoryType::Observation,
            scope: MemoryScope::Session,
            confidence: 0.8,
            linked_symbols: vec![],
            linked_files: vec!["src/auth.ts".to_string()],
            workspace_id: None,
            branch: None,
            refresh_key: None,
            source_query: None,
            created_at: 0,
            last_accessed: 0,
            access_count: 0,
            is_stale: false,
            stale_reason: None,
        })
        .unwrap();

    let mut indexer = Indexer::new(PathBuf::from("/test"));

    indexer
        .index_file_content(
            "src/auth.ts",
            "// initial auth note\nexport function loginUser(): void { return; }",
        )
        .unwrap();

    let changes = indexer
        .index_file_with_stale_detection(
            "src/auth.ts",
            "// updated auth note\nexport function loginUser(): void { return; }",
            Some(&store),
        )
        .unwrap();

    assert!(
        changes.is_empty(),
        "Comment-only change should not create symbol diff"
    );

    let memories = store.list_all().unwrap();
    assert!(
        memories[0].is_stale,
        "File-linked memory should be marked stale after file edit"
    );
    assert_eq!(
        memories[0].stale_reason.as_deref(),
        Some("src/auth.ts changed")
    );
}
