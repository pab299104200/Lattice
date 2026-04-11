use super::graph_store::GraphStore;
use crate::graph::model::{CodeGraph, EdgeKind};
use crate::storage::VectorStore;
use crate::storage::{UsearchVectorIndex, VectorIndex};
use crate::symbols::{Language, SymbolId, SymbolKind};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn make_id(file: &str, name: &str, offset: usize) -> SymbolId {
    SymbolId {
        file: file.to_string(),
        name: name.to_string(),
        byte_offset: offset,
    }
}

fn build_sample_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();

    let id_a = make_id("src/auth.ts", "loginUser", 0);
    let id_b = make_id("src/crypto.ts", "hashPassword", 0);

    graph.add_node(
        id_a.clone(),
        SymbolKind::Function,
        "loginUser".to_string(),
        "function loginUser(creds: Credentials): Promise<Session>".to_string(),
        "function loginUser(creds) { return hash(creds); }".to_string(),
        "src/auth.ts".to_string(),
        1,
        5,
        true,
        Language::TypeScript,
    );

    graph.add_node(
        id_b.clone(),
        SymbolKind::Function,
        "hashPassword".to_string(),
        "function hashPassword(plain: string): string".to_string(),
        "function hashPassword(plain) { return bcrypt.hash(plain); }".to_string(),
        "src/crypto.ts".to_string(),
        1,
        3,
        true,
        Language::TypeScript,
    );

    graph.add_edge(&id_a, &id_b, EdgeKind::Calls);

    graph
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("lattice-storage-{name}-{unique}"))
}

fn cleanup_vector_backend(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("vectors.db"));
    let _ = std::fs::remove_file(dir.join("vectors.db-wal"));
    let _ = std::fs::remove_file(dir.join("vectors.db-shm"));
    let _ = std::fs::remove_file(dir.join("vectors.usearch"));
    let _ = std::fs::remove_file(dir.join("vectors.usearch.meta.json"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn test_save_and_load_graph() {
    let store = GraphStore::open_in_memory().expect("Failed to open in-memory store");

    let graph = build_sample_graph();
    store.save_graph(&graph).expect("Failed to save graph");

    let loaded = store.load_graph().expect("Failed to load graph");

    assert_eq!(loaded.node_count(), 2);
    assert_eq!(loaded.edge_count(), 1);

    let id_a = make_id("src/auth.ts", "loginUser", 0);
    let node = loaded.get_node(&id_a).expect("loginUser node should exist");
    assert_eq!(
        node.signature.as_ref(),
        "function loginUser(creds: Credentials): Promise<Session>"
    );
}

#[test]
fn test_save_replaces_previous() {
    let store = GraphStore::open_in_memory().expect("Failed to open in-memory store");

    // Save graph with 2 nodes
    let graph1 = build_sample_graph();
    store.save_graph(&graph1).expect("Failed to save graph1");
    assert_eq!(store.load_graph().unwrap().node_count(), 2);

    // Save graph with 1 node — should replace the previous
    let mut graph2 = CodeGraph::new();
    let id = make_id("src/single.ts", "onlyOne", 0);
    graph2.add_node(
        id,
        SymbolKind::Function,
        "onlyOne".to_string(),
        "function onlyOne()".to_string(),
        "function onlyOne() {}".to_string(),
        "src/single.ts".to_string(),
        1,
        1,
        false,
        Language::TypeScript,
    );

    store.save_graph(&graph2).expect("Failed to save graph2");

    let loaded = store.load_graph().expect("Failed to load graph");
    assert_eq!(loaded.node_count(), 1);
}

#[test]
fn test_file_based_store() {
    let tmp_dir = std::env::temp_dir().join("lattice_test_store");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let db_path = tmp_dir.join("test_graph.db");

    // Clean up any previous test run
    let _ = std::fs::remove_file(&db_path);

    // Open, save, close
    {
        let store = GraphStore::open(&db_path).expect("Failed to open file store");
        let graph = build_sample_graph();
        store.save_graph(&graph).expect("Failed to save graph");
    }

    // Reopen and verify
    {
        let store = GraphStore::open(&db_path).expect("Failed to reopen file store");
        let loaded = store.load_graph().expect("Failed to load graph");
        assert_eq!(loaded.node_count(), 2);
    }

    // Clean up
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_dir(&tmp_dir);
}

#[test]
fn test_vector_store_and_search() {
    let store = VectorStore::open_in_memory().unwrap();
    store.initialize(384).unwrap();

    let vec_a = vec![1.0f32; 384];
    let mut vec_b = vec![0.0f32; 384];
    vec_b[0] = 1.0;
    let vec_c = vec![-1.0f32; 384];

    store
        .upsert_vector("src/auth.ts", "loginUser", 0, &vec_a)
        .unwrap();
    store
        .upsert_vector("src/crypto.ts", "hashPassword", 0, &vec_b)
        .unwrap();
    store
        .upsert_vector("src/other.ts", "unrelated", 0, &vec_c)
        .unwrap();

    let results = store.search(&vec_a, 2).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "loginUser"); // exact match first
}

#[test]
fn test_vector_delete_by_file() {
    let store = VectorStore::open_in_memory().unwrap();
    store.initialize(384).unwrap();

    let vec_a = vec![1.0f32; 384];
    store
        .upsert_vector("src/auth.ts", "login", 0, &vec_a)
        .unwrap();
    store
        .upsert_vector("src/auth.ts", "logout", 10, &vec_a)
        .unwrap();
    store
        .upsert_vector("src/other.ts", "helper", 0, &vec_a)
        .unwrap();

    store.delete_by_file("src/auth.ts").unwrap();

    let results = store.search(&vec_a, 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "helper");
}

#[test]
fn test_vector_store_cache_performance() {
    let store = VectorStore::open_in_memory().unwrap();
    store.initialize(384).unwrap();

    // Insert 100 vectors
    for i in 0..100 {
        let mut vec = vec![0.0f32; 384];
        vec[i % 384] = 1.0;
        store
            .upsert_vector(&format!("file{}.ts", i), &format!("func{}", i), 0, &vec)
            .unwrap();
    }

    store.load_cache().unwrap();

    let query = vec![1.0f32; 384];
    let results = store.search(&query, 5).unwrap();
    assert_eq!(results.len(), 5);

    // Delete and verify cache is updated
    store.delete_by_file("file0.ts").unwrap();
    let results2 = store.search(&query, 100).unwrap();
    assert_eq!(results2.len(), 99);
}

#[test]
fn test_usearch_index_persists_and_searches() {
    let dir = unique_temp_dir("usearch-persist");
    std::fs::create_dir_all(&dir).unwrap();
    let sqlite_path = dir.join("vectors.db");
    let ann_path = dir.join("vectors.usearch");

    {
        let index =
            UsearchVectorIndex::open(sqlite_path.to_string_lossy().as_ref(), ann_path).unwrap();
        index.initialize(384).unwrap();
        index.warm().unwrap();

        let vec_a = vec![1.0f32; 384];
        let mut vec_b = vec![0.0f32; 384];
        vec_b[0] = 1.0;
        let vec_c = vec![-1.0f32; 384];

        index
            .upsert_vector("src/auth.ts", "loginUser", 0, &vec_a)
            .unwrap();
        index
            .upsert_vector("src/crypto.ts", "hashPassword", 0, &vec_b)
            .unwrap();
        index
            .upsert_vector("src/other.ts", "unrelated", 0, &vec_c)
            .unwrap();
        index.flush().unwrap();

        let results = index.search(&vec_a, 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "loginUser");
    }

    {
        let index = UsearchVectorIndex::open(
            sqlite_path.to_string_lossy().as_ref(),
            dir.join("vectors.usearch"),
        )
        .unwrap();
        index.initialize(384).unwrap();
        index.warm().unwrap();

        let query = vec![1.0f32; 384];
        let results = index.search(&query, 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "loginUser");
    }

    cleanup_vector_backend(&dir);
}

#[test]
fn test_usearch_delete_by_file_updates_persisted_index() {
    let dir = unique_temp_dir("usearch-delete");
    std::fs::create_dir_all(&dir).unwrap();
    let sqlite_path = dir.join("vectors.db");

    {
        let index = UsearchVectorIndex::open(
            sqlite_path.to_string_lossy().as_ref(),
            dir.join("vectors.usearch"),
        )
        .unwrap();
        index.initialize(384).unwrap();

        let vec_a = vec![1.0f32; 384];
        index
            .upsert_vector("src/auth.ts", "login", 0, &vec_a)
            .unwrap();
        index
            .upsert_vector("src/auth.ts", "logout", 10, &vec_a)
            .unwrap();
        index
            .upsert_vector("src/other.ts", "helper", 0, &vec_a)
            .unwrap();
        index.flush().unwrap();

        index.delete_by_file("src/auth.ts").unwrap();
        index.flush().unwrap();
    }

    {
        let index = UsearchVectorIndex::open(
            sqlite_path.to_string_lossy().as_ref(),
            dir.join("vectors.usearch"),
        )
        .unwrap();
        index.initialize(384).unwrap();
        index.warm().unwrap();

        let query = vec![1.0f32; 384];
        let results = index.search(&query, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "helper");
    }

    cleanup_vector_backend(&dir);
}
