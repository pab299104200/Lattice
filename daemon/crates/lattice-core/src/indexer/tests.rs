use super::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

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

#[test]
fn test_replace_shared_index_reuses_graph_snapshot() {
    let mut source = Indexer::new(PathBuf::from("/project"));
    source
        .index_file_content("src/shared.ts", "export function shared(): void {}")
        .expect("should parse source");
    let graph = source.graph_arc();

    let mut target = Indexer::new(PathBuf::from("/project"));
    target.replace_shared_index(graph.clone(), HashMap::new());

    assert!(Arc::ptr_eq(&graph, &target.graph_arc()));
    assert!(target
        .graph()
        .all_nodes()
        .iter()
        .any(|n| n.name == "shared"));
}

#[test]
fn test_rebuild_graph_preserves_existing_node_bodies_when_cached_symbols_are_slim() {
    let mut indexer = Indexer::new(PathBuf::from("/project"));
    indexer
        .index_file_content(
            "src/shared.ts",
            "export function shared(): void { return; }",
        )
        .expect("should parse source");

    let cached_body_is_stripped = indexer
        .parsed_files()
        .get("src/shared.ts")
        .and_then(|file| file.symbols.iter().find(|symbol| symbol.name == "shared"))
        .map(|symbol| symbol.body.is_empty())
        .unwrap_or(false);
    assert!(cached_body_is_stripped, "parsed-file cache should be slim");

    indexer
        .index_file_content(
            "src/other.ts",
            "export function other(): void { shared(); }",
        )
        .expect("should parse second file");

    let shared_node = indexer
        .graph()
        .all_nodes()
        .into_iter()
        .find(|node| node.name == "shared")
        .expect("shared node should remain in graph");
    assert!(
        !shared_node.body.is_empty(),
        "graph node body should survive rebuilds even when parsed cache bodies are stripped"
    );
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
fn watcher_batch_applies_upserts_and_removals_with_one_graph_rebuild() {
    let mut indexer = Indexer::new(PathBuf::from("/project"));
    indexer
        .index_file_content("src/removed.ts", "export function removed(): void {}")
        .expect("seed removed file");
    let before_snapshot = indexer.graph_snapshot_id();

    let report = indexer.apply_file_batch_contents(
        vec![
            (
                "src/alpha.ts".to_string(),
                "export function alpha(): void {}".to_string(),
            ),
            (
                "src/beta.ts".to_string(),
                "export function beta(): void { alpha(); }".to_string(),
            ),
        ],
        vec!["src/removed.ts".to_string()],
    );

    assert_eq!(report.indexed_count, 2);
    assert!(!report.is_partial);
    assert_eq!(indexer.graph_snapshot_id(), before_snapshot + 1);
    let names = indexer
        .graph()
        .all_nodes()
        .into_iter()
        .map(|node| node.name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"beta"));
    assert!(!names.contains(&"removed"));
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
            scope_organization_id: None,
            refresh_key: None,
            source_query: None,
            created_at: 0,
            last_accessed: 0,
            access_count: 0,
            is_stale: false,
            stale_reason: None,
            verification_status: crate::memory::MemoryVerificationStatus::Unverified,
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
            scope_organization_id: None,
            refresh_key: None,
            source_query: None,
            created_at: 0,
            last_accessed: 0,
            access_count: 0,
            is_stale: false,
            stale_reason: None,
            verification_status: crate::memory::MemoryVerificationStatus::Unverified,
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

#[derive(Debug, PartialEq, Eq)]
struct CanonicalGraph {
    nodes: Vec<String>,
    edges: Vec<String>,
}

fn canonical_graph(graph: &CodeGraph) -> CanonicalGraph {
    let mut nodes = graph
        .all_nodes()
        .into_iter()
        .map(|node| {
            format!(
                "{}:{}:{}:{:?}:{}:{}:{}:{}:{}:{}:{:?}:{}",
                node.id.file,
                node.id.name,
                node.id.byte_offset,
                node.kind,
                node.name,
                node.signature,
                node.body,
                node.file,
                node.line,
                node.end_line,
                node.language,
                node.is_exported
            )
        })
        .collect::<Vec<_>>();
    nodes.sort();

    let mut edges = graph
        .all_edges()
        .into_iter()
        .map(|(source, target, kind)| {
            format!(
                "{}:{}:{}->{:?}->{}:{}:{}",
                source.id.file,
                source.id.name,
                source.id.byte_offset,
                kind,
                target.id.file,
                target.id.name,
                target.id.byte_offset
            )
        })
        .collect::<Vec<_>>();
    edges.sort();

    CanonicalGraph { nodes, edges }
}

fn assert_incremental_matches_full_rebuild(indexer: &Indexer) {
    let mut full = GraphBuilder::build_from_files(indexer.parsed_files().values());
    full.hydrate_missing_bodies_from(indexer.graph());
    assert_eq!(
        canonical_graph(indexer.graph()),
        canonical_graph(&full),
        "incremental graph must be structurally identical to a full build"
    );
}

#[test]
fn incremental_graph_matches_full_rebuild_across_randomized_change_sequences() {
    const FILE_COUNT: usize = 12;
    const CHANGE_COUNT: usize = 80;

    fn source(file: usize, revision: u64, target: usize) -> String {
        let own_name = format!("slot_{}", (file + revision as usize) % 5);
        let target_name = format!("slot_{}", (target + revision as usize) % 5);
        format!(
            "import {{ {target_name} }} from \"./file{target}\";\n\
             export function {own_name}(): number {{ return {revision}; }}\n\
             export function caller_{file}(): number {{ return {target_name}(); }}\n"
        )
    }

    let mut indexer = Indexer::new(PathBuf::from("/project"));
    for file in 0..FILE_COUNT {
        let target = (file + 1) % FILE_COUNT;
        indexer
            .index_file_content(&format!("src/file{file}.ts"), &source(file, 0, target))
            .expect("seed file should parse");
    }
    assert_incremental_matches_full_rebuild(&indexer);

    // Fixed LCG seed keeps the test deterministic while exercising updates,
    // removals, re-additions, duplicate names, and changing cross-file edges.
    let mut state = 0x4d59_5df4_d0f3_3173_u64;
    for revision in 1..=CHANGE_COUNT as u64 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let file = state as usize % FILE_COUNT;
        let path = format!("src/file{file}.ts");

        if state % 7 == 0 {
            indexer.remove_file(&path);
        } else {
            let target = ((state >> 16) as usize % FILE_COUNT + file + 1) % FILE_COUNT;
            indexer
                .index_file_content(&path, &source(file, revision, target))
                .expect("changed file should parse");
        }

        assert_incremental_matches_full_rebuild(&indexer);
    }
}

#[test]
fn incremental_graph_re_resolves_document_links_after_target_changes() {
    let mut indexer = Indexer::new(PathBuf::from("/project"));
    indexer
        .index_file_content(
            "docs/source.md",
            "# Source\n\nSee [details](target.md#old).",
        )
        .expect("source document should parse");
    indexer
        .index_file_content("docs/target.md", "# Target\n\n## Old\n")
        .expect("target document should parse");
    assert_incremental_matches_full_rebuild(&indexer);

    indexer
        .index_file_content("docs/target.md", "# Target\n\n## New\n")
        .expect("changed target document should parse");
    assert_incremental_matches_full_rebuild(&indexer);

    indexer.remove_file("docs/target.md");
    assert_incremental_matches_full_rebuild(&indexer);
}

#[test]
fn incremental_update_preserves_retained_arc_snapshots() {
    let mut indexer = Indexer::new(PathBuf::from("/project"));
    indexer
        .index_file_content("src/value.ts", "export function before(): void {}")
        .expect("initial file should parse");
    let retained_snapshot = indexer.graph_arc();

    indexer
        .index_file_content("src/value.ts", "export function after(): void {}")
        .expect("changed file should parse");

    assert!(retained_snapshot
        .all_nodes()
        .iter()
        .any(|node| node.name == "before"));
    assert!(!retained_snapshot
        .all_nodes()
        .iter()
        .any(|node| node.name == "after"));
    assert!(indexer
        .graph()
        .all_nodes()
        .iter()
        .any(|node| node.name == "after"));
    assert!(!indexer
        .graph()
        .all_nodes()
        .iter()
        .any(|node| node.name == "before"));
}
