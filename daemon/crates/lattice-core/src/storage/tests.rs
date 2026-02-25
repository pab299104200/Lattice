use crate::graph::model::{CodeGraph, EdgeKind};
use crate::symbols::{Language, SymbolId, SymbolKind};
use super::graph_store::GraphStore;

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
        node.signature,
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
