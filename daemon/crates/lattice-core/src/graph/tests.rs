use crate::graph::model::{CodeGraph, EdgeKind};
use crate::symbols::{Language, SymbolId, SymbolKind};

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
