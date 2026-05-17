use std::collections::HashMap;

use crate::graph::CodeGraph;
use crate::identity::ambiguity::ResolveOutcome;
use crate::storage::graph_store::FileIndexEntry;
use crate::symbols::{Language, ParsedFile, SymbolId as LegacySymbolId, SymbolKind};

use super::{EventId, IdentityResolver};

const WORKSPACE: &str = "repo";

#[test]
fn resolve_path_returns_stable_file_id() {
    let resolver = fixture();
    let resolved = resolver
        .resolve_path(&WORKSPACE.to_string(), "src/lib.rs")
        .expect("path should resolve");
    assert_eq!(resolved.workspace_id, WORKSPACE);
    assert_eq!(resolved.repo_relative_path, "src/lib.rs");
    assert_eq!(resolved.content_hash, "aaaaaaaa");
}

#[test]
fn resolve_symbol_reports_ambiguity_for_duplicate_names() {
    let resolver = fixture();
    let resolved = resolver.resolve_symbol(&WORKSPACE.to_string(), "shared");
    match resolved {
        ResolveOutcome::Ambiguous(report) => {
            assert_eq!(report.candidates.len(), 2);
            assert!(report.disambiguation_hint.contains("file path"));
        }
        other => panic!("expected ambiguity, got {other:?}"),
    }
}

#[test]
fn resolve_legacy_symbol_name_uses_default_workspace() {
    let resolver = fixture();
    let resolved = resolver.resolve_legacy_symbol_name("only_here");
    match resolved {
        ResolveOutcome::Unique(symbol_id) => {
            assert_eq!(symbol_id.file.workspace_id, WORKSPACE);
            assert_eq!(symbol_id.file.repo_relative_path, "src/unique.rs");
        }
        other => panic!("expected unique symbol, got {other:?}"),
    }
}

fn fixture() -> IdentityResolver<'static> {
    let workspace = WORKSPACE.to_string();
    let graph = Box::leak(Box::new(make_graph()));
    let file_index = Box::leak(Box::new(make_file_index()));
    let parsed_files = Box::leak(Box::new(HashMap::<String, ParsedFile>::new()));
    IdentityResolver::new(
        graph,
        file_index,
        parsed_files,
        workspace.clone(),
        vec![EventId {
            workspace_id: workspace.clone(),
            ulid: "01J0000000000000000000000A".to_string(),
        }],
    )
}

fn make_graph() -> CodeGraph {
    let mut graph = CodeGraph::new();
    add_node(
        &mut graph,
        "repo/src/lib.rs",
        "shared",
        10,
        SymbolKind::Function,
    );
    add_node(
        &mut graph,
        "repo/src/other.rs",
        "shared",
        22,
        SymbolKind::Function,
    );
    add_node(
        &mut graph,
        "repo/src/unique.rs",
        "only_here",
        34,
        SymbolKind::Function,
    );
    add_node(
        &mut graph,
        "repo/tests/auth_test.rs",
        "test_login",
        48,
        SymbolKind::Function,
    );
    graph
}

fn add_node(graph: &mut CodeGraph, file: &str, name: &str, byte_offset: usize, kind: SymbolKind) {
    graph.add_node(
        LegacySymbolId {
            file: file.to_string(),
            name: name.to_string(),
            byte_offset,
        },
        kind,
        name.to_string(),
        name.to_string(),
        String::new(),
        file.to_string(),
        1,
        1,
        true,
        Language::Rust,
    );
}

fn make_file_index() -> HashMap<String, FileIndexEntry> {
    [
        ("repo/src/lib.rs", "aaaaaaaa"),
        ("repo/src/other.rs", "bbbbbbbb"),
        ("repo/src/unique.rs", "cccccccc"),
        ("repo/tests/auth_test.rs", "dddddddd"),
    ]
    .into_iter()
    .map(|(file, content_hash)| {
        (
            file.to_string(),
            FileIndexEntry {
                file: file.to_string(),
                content_hash: content_hash.to_string(),
                mtime_ns: 0,
                size_bytes: 0,
                parser_version: 1,
                schema_version: 1,
                last_indexed_at: 0,
            },
        )
    })
    .collect()
}
