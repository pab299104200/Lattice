use crate::graph::model::{CodeGraph, EdgeKind};
use crate::symbols::{Language, SymbolId, SymbolKind};

// ─── Short code tests ────────────────────────────────────────────────

#[test]
fn test_symbol_kind_short_codes() {
    assert_eq!(SymbolKind::Function.short_code(), "fn");
    assert_eq!(SymbolKind::Class.short_code(), "cls");
    assert_eq!(SymbolKind::Interface.short_code(), "ifc");
    assert_eq!(SymbolKind::TypeAlias.short_code(), "type");
    assert_eq!(SymbolKind::Enum.short_code(), "enum");
    assert_eq!(SymbolKind::Module.short_code(), "mod");
    assert_eq!(SymbolKind::Variable.short_code(), "var");
    assert_eq!(SymbolKind::Constant.short_code(), "const");
    assert_eq!(SymbolKind::Method.short_code(), "meth");
    assert_eq!(SymbolKind::Trait.short_code(), "trait");
    assert_eq!(SymbolKind::Struct.short_code(), "struct");
    assert_eq!(SymbolKind::Document.short_code(), "doc");
    assert_eq!(SymbolKind::Section.short_code(), "sec");
}

#[test]
fn test_edge_kind_short_codes() {
    assert_eq!(EdgeKind::Calls.short_code(), "C");
    assert_eq!(EdgeKind::Imports.short_code(), "I");
    assert_eq!(EdgeKind::Implements.short_code(), "M");
    assert_eq!(EdgeKind::Extends.short_code(), "E");
    assert_eq!(EdgeKind::TypeRef.short_code(), "T");
    assert_eq!(EdgeKind::Contains.short_code(), "N");
    assert_eq!(EdgeKind::LinksTo.short_code(), "L");
    assert_eq!(EdgeKind::Mentions.short_code(), "R");
    assert_eq!(EdgeKind::CoChanges.short_code(), "X");
}

// ─── Graph tests ─────────────────────────────────────────────────────

fn make_id(file: &str, name: &str) -> SymbolId {
    SymbolId {
        file: file.to_string(),
        name: name.to_string(),
        byte_offset: 0,
    }
}

#[test]
fn test_add_and_retrieve_node() {
    let mut graph = CodeGraph::new();
    let id = make_id("src/main.ts", "main");

    graph.add_node(
        id.clone(),
        SymbolKind::Function,
        "main".to_string(),
        "function main()".to_string(),
        "function main() {}".to_string(),
        "src/main.ts".to_string(),
        1,
        3,
        true,
        Language::TypeScript,
    );

    let node = graph.get_node(&id).expect("node should exist");
    assert_eq!(node.name, "main");
    assert_eq!(graph.node_count(), 1);
}

#[test]
fn test_add_edge_and_get_dependents() {
    let mut graph = CodeGraph::new();
    let id_a = make_id("src/auth.ts", "loginUser");
    let id_b = make_id("src/crypto.ts", "hashPassword");

    graph.add_node(
        id_a.clone(),
        SymbolKind::Function,
        "loginUser".to_string(),
        "function loginUser()".to_string(),
        "function loginUser() {}".to_string(),
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
        "function hashPassword()".to_string(),
        "function hashPassword() {}".to_string(),
        "src/crypto.ts".to_string(),
        1,
        3,
        true,
        Language::TypeScript,
    );

    graph.add_edge(&id_a, &id_b, EdgeKind::Calls);

    // loginUser depends on hashPassword
    let deps = graph.get_dependencies(&id_a);
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0].0.name, "hashPassword");
    assert_eq!(deps[0].1, EdgeKind::Calls);

    // hashPassword is depended on by loginUser
    let dependents = graph.get_dependents(&id_b);
    assert_eq!(dependents.len(), 1);
    assert_eq!(dependents[0].0.name, "loginUser");
    assert_eq!(dependents[0].1, EdgeKind::Calls);
}

#[test]
fn test_n_hop_neighbors() {
    let mut graph = CodeGraph::new();
    let id_a = make_id("src/a.ts", "a");
    let id_b = make_id("src/b.ts", "b");
    let id_c = make_id("src/c.ts", "c");
    let id_d = make_id("src/d.ts", "d");

    for (id, name) in [
        (&id_a, "a"),
        (&id_b, "b"),
        (&id_c, "c"),
        (&id_d, "d"),
    ] {
        graph.add_node(
            id.clone(),
            SymbolKind::Function,
            name.to_string(),
            String::new(),
            String::new(),
            format!("src/{}.ts", name),
            1,
            1,
            false,
            Language::TypeScript,
        );
    }

    // Chain: a -> b -> c -> d
    graph.add_edge(&id_a, &id_b, EdgeKind::Calls);
    graph.add_edge(&id_b, &id_c, EdgeKind::Calls);
    graph.add_edge(&id_c, &id_d, EdgeKind::Calls);

    assert_eq!(graph.n_hop_neighbors(&id_a, 1).len(), 1); // b
    assert_eq!(graph.n_hop_neighbors(&id_a, 2).len(), 2); // b, c
    assert_eq!(graph.n_hop_neighbors(&id_a, 3).len(), 3); // b, c, d
}

#[test]
fn test_remove_file_nodes() {
    let mut graph = CodeGraph::new();
    let id_a = make_id("src/auth.ts", "login");
    let id_b = make_id("src/auth.ts", "logout");
    let id_c = make_id("src/crypto.ts", "hash");

    graph.add_node(
        id_a.clone(),
        SymbolKind::Function,
        "login".to_string(),
        String::new(),
        String::new(),
        "src/auth.ts".to_string(),
        1,
        5,
        true,
        Language::TypeScript,
    );

    graph.add_node(
        id_b.clone(),
        SymbolKind::Function,
        "logout".to_string(),
        String::new(),
        String::new(),
        "src/auth.ts".to_string(),
        7,
        10,
        true,
        Language::TypeScript,
    );

    graph.add_node(
        id_c.clone(),
        SymbolKind::Function,
        "hash".to_string(),
        String::new(),
        String::new(),
        "src/crypto.ts".to_string(),
        1,
        3,
        true,
        Language::TypeScript,
    );

    assert_eq!(graph.node_count(), 3);

    graph.remove_file_nodes("src/auth.ts");

    assert_eq!(graph.node_count(), 1);
    assert!(graph.get_node(&id_c).is_some());
    assert!(graph.get_node(&id_a).is_none());
    assert!(graph.get_node(&id_b).is_none());
}

#[test]
fn test_graph_stats() {
    let mut graph = CodeGraph::new();
    let id_a = make_id("src/auth.ts", "login");
    let id_b = make_id("src/crypto.ts", "hash");

    graph.add_node(
        id_a.clone(),
        SymbolKind::Function,
        "login".to_string(),
        String::new(),
        String::new(),
        "src/auth.ts".to_string(),
        1,
        5,
        true,
        Language::TypeScript,
    );

    graph.add_node(
        id_b.clone(),
        SymbolKind::Function,
        "hash".to_string(),
        String::new(),
        String::new(),
        "src/crypto.ts".to_string(),
        1,
        3,
        true,
        Language::TypeScript,
    );

    graph.add_edge(&id_a, &id_b, EdgeKind::Calls);

    let stats = graph.stats();
    assert_eq!(stats.node_count, 2);
    assert_eq!(stats.edge_count, 1);
    assert_eq!(stats.file_count, 2);
}

#[test]
fn test_build_graph_from_parsed_files() {
    use crate::parser::parse_file;
    use crate::graph::builder::GraphBuilder;

    let auth_source = r#"
import { hashPassword } from './crypto';

export function loginUser(username: string, password: string): Promise<User> {
    const hashed = hashPassword(password);
    return authenticate(username, hashed);
}
"#;
    let crypto_source = r#"
export function hashPassword(plain: string): string {
    return bcrypt.hash(plain, 10);
}
"#;

    let auth_parsed = parse_file("src/auth.ts", auth_source).unwrap();
    let crypto_parsed = parse_file("src/crypto.ts", crypto_source).unwrap();

    let mut builder = GraphBuilder::new();
    builder.add_file(auth_parsed);
    builder.add_file(crypto_parsed);
    let graph = builder.build();

    assert!(graph.node_count() >= 2);
    assert!(graph.stats().edge_count > 0);
}

#[test]
fn test_build_graph_from_markdown_links() {
    use crate::graph::builder::GraphBuilder;
    use crate::parser::parse_file;

    let overview_source = r#"
# Overview

See [[guide#Setup]] for the workflow.
"#;
    let guide_source = r#"
# Guide

## Setup

Run `prepare_change` first.
"#;

    let overview = parse_file("docs/overview.md", overview_source).unwrap();
    let guide = parse_file("docs/guide.md", guide_source).unwrap();

    let mut builder = GraphBuilder::new();
    builder.add_file(overview);
    builder.add_file(guide);
    let graph = builder.build();

    let overview_section_id = graph
        .all_nodes()
        .iter()
        .find(|node| node.file == "docs/overview.md" && node.kind == SymbolKind::Section)
        .map(|node| node.id.clone())
        .expect("overview section should exist");

    let deps = graph.get_dependencies(&overview_section_id);
    assert!(
        deps.iter()
            .any(|(node, edge)| node.name == "Setup" && *edge == EdgeKind::LinksTo),
        "overview section should link to the Setup section"
    );
}

#[test]
fn test_markdown_sections_mention_code_symbols() {
    use crate::graph::builder::GraphBuilder;
    use crate::parser::parse_file;

    let doc_source = r#"
# Workflow

Use `prepare_change` before editing code.
"#;
    let code_source = r#"
export function prepare_change(): string {
    return "ready";
}
"#;

    let doc = parse_file("docs/workflow.md", doc_source).unwrap();
    let code = parse_file("src/workflow.ts", code_source).unwrap();

    let mut builder = GraphBuilder::new();
    builder.add_file(doc);
    builder.add_file(code);
    let graph = builder.build();

    let workflow_section_id = graph
        .all_nodes()
        .iter()
        .find(|node| node.file == "docs/workflow.md" && node.kind == SymbolKind::Section)
        .map(|node| node.id.clone())
        .expect("workflow section should exist");

    let deps = graph.get_dependencies(&workflow_section_id);
    assert!(
        deps.iter()
            .any(|(node, edge)| node.name == "prepare_change" && *edge == EdgeKind::Mentions),
        "workflow section should mention prepare_change"
    );
}

#[test]
fn test_intra_file_call_edges() {
    use crate::parser::parse_file;
    use crate::graph::builder::GraphBuilder;

    // Two functions in the same file where helper() is called by main_func()
    let source = r#"
function helper(): string {
    return "hello";
}

function main_func(): string {
    return helper();
}
"#;

    let parsed = parse_file("src/utils.ts", source).unwrap();
    let mut builder = GraphBuilder::new();
    builder.add_file(parsed);
    let graph = builder.build();

    // Find the IDs
    let main_id = graph.all_nodes().iter()
        .find(|n| n.name == "main_func")
        .map(|n| n.id.clone());
    let helper_id = graph.all_nodes().iter()
        .find(|n| n.name == "helper")
        .map(|n| n.id.clone());

    assert!(main_id.is_some(), "main_func should exist in graph");
    assert!(helper_id.is_some(), "helper should exist in graph");

    // main_func should have a Calls edge to helper
    let deps = graph.get_dependencies(&main_id.unwrap());
    let calls_helper = deps.iter().any(|(n, edge)| n.name == "helper" && *edge == EdgeKind::Calls);
    assert!(calls_helper, "main_func should have a Calls edge to helper, deps: {:?}",
        deps.iter().map(|(n, e)| (&n.name, e)).collect::<Vec<_>>());
}

#[test]
fn test_no_self_loop_edges() {
    use crate::parser::parse_file;
    use crate::graph::builder::GraphBuilder;

    // Recursive function — should NOT create a self-loop edge
    let source = r#"
function recursive(n: number): number {
    if (n <= 0) return 0;
    return recursive(n - 1);
}
"#;

    let parsed = parse_file("src/rec.ts", source).unwrap();
    let mut builder = GraphBuilder::new();
    builder.add_file(parsed);
    let graph = builder.build();

    let rec_id = graph.all_nodes().iter()
        .find(|n| n.name == "recursive")
        .map(|n| n.id.clone())
        .expect("recursive should exist");

    let deps = graph.get_dependencies(&rec_id);
    let self_loop = deps.iter().any(|(n, _)| n.name == "recursive");
    assert!(!self_loop, "recursive should NOT have a self-loop Calls edge");
}

#[test]
fn test_find_call_paths() {
    let mut graph = CodeGraph::new();
    let id_a = make_id("src/a.ts", "a");
    let id_b = make_id("src/b.ts", "b");
    let id_c = make_id("src/c.ts", "c");
    let id_d = make_id("src/d.ts", "d");

    for (id, name) in [
        (&id_a, "a"),
        (&id_b, "b"),
        (&id_c, "c"),
        (&id_d, "d"),
    ] {
        graph.add_node(
            id.clone(), SymbolKind::Function, name.to_string(),
            String::new(), String::new(), format!("src/{}.ts", name),
            1, 1, false, Language::TypeScript,
        );
    }

    // Chain: a -> b -> c -> d
    graph.add_edge(&id_a, &id_b, EdgeKind::Calls);
    graph.add_edge(&id_b, &id_c, EdgeKind::Calls);
    graph.add_edge(&id_c, &id_d, EdgeKind::Calls);

    // Should find path a -> b -> c -> d
    let paths = graph.find_call_paths(&id_a, &id_d, 5, 10);
    assert_eq!(paths.len(), 1);
    let names: Vec<&str> = paths[0].iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, vec!["a", "b", "c", "d"]);

    // No path from d to a (directed)
    let no_paths = graph.find_call_paths(&id_d, &id_a, 5, 10);
    assert!(no_paths.is_empty(), "Should be no path from d to a");

    // max_depth limits results
    let short = graph.find_call_paths(&id_a, &id_d, 2, 10);
    assert!(short.is_empty(), "max_depth=2 should not reach d from a (needs 3 hops)");
}

#[test]
fn test_find_call_paths_disconnected() {
    let mut graph = CodeGraph::new();
    let id_a = make_id("src/a.ts", "a");
    let id_b = make_id("src/b.ts", "b");

    graph.add_node(
        id_a.clone(), SymbolKind::Function, "a".to_string(),
        String::new(), String::new(), "src/a.ts".to_string(),
        1, 1, false, Language::TypeScript,
    );
    graph.add_node(
        id_b.clone(), SymbolKind::Function, "b".to_string(),
        String::new(), String::new(), "src/b.ts".to_string(),
        1, 1, false, Language::TypeScript,
    );

    // No edges — should find no paths
    let paths = graph.find_call_paths(&id_a, &id_b, 5, 10);
    assert!(paths.is_empty());
}
