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
    assert!(greet_node.is_some(), "greet function should be in the graph");
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
