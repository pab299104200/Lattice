use crate::graph::builder::GraphBuilder;
use crate::intelligence::{
    find_stale_docs, get_backlinks, get_docs_capsule, get_outgoing_links, DocsTargetKind,
};
use crate::parser::parse_file;

fn build_docs_graph() -> crate::graph::CodeGraph {
    let auth_doc = r#"
# Auth Guide

## Login Flow

Use `loginUser` to authenticate a user.
See [[runbook#Checklist]] for operational steps.
"#;
    let runbook = r#"
# Runbook

## Checklist

Run `prepare_change` before touching auth.
"#;
    let code = r#"
export function loginUser(username: string, password: string): string {
    return `${username}:${password}`;
}

export function prepare_change(): string {
    return "ready";
}
"#;

    let mut builder = GraphBuilder::new();
    builder.add_file(parse_file("docs/auth-guide.md", auth_doc).unwrap());
    builder.add_file(parse_file("docs/runbook.md", runbook).unwrap());
    builder.add_file(parse_file("src/auth.ts", code).unwrap());
    builder.build()
}

#[test]
fn test_get_docs_capsule_returns_matching_sections_and_symbols() {
    let graph = build_docs_graph();

    let report = get_docs_capsule(
        &graph,
        "login guide",
        &["src/auth.ts".to_string()],
        &["loginUser".to_string()],
        5,
    );

    assert!(
        report.docs.iter().any(|doc| doc.symbol == "Login Flow"),
        "expected Login Flow section in docs capsule, got: {:?}",
        report
            .docs
            .iter()
            .map(|doc| format!("{}:{}", doc.file, doc.symbol))
            .collect::<Vec<_>>()
    );
    assert!(
        report.related_symbols.iter().any(|symbol| symbol.symbol == "loginUser"),
        "expected related loginUser symbol in docs capsule"
    );
}

#[test]
fn test_get_backlinks_finds_markdown_mentions_for_symbol() {
    let graph = build_docs_graph();

    let report = get_backlinks(&graph, "loginUser", DocsTargetKind::Symbol, 10)
        .expect("expected backlinks report");

    assert_eq!(report.resolved_kind, "symbol");
    assert!(
        report
            .backlinks
            .iter()
            .any(|item| item.symbol == "Login Flow" && item.relationship == "mentions"),
        "expected Login Flow markdown section to backlink loginUser"
    );
}

#[test]
fn test_get_outgoing_links_returns_doc_links_and_code_mentions() {
    let graph = build_docs_graph();

    let report = get_outgoing_links(
        &graph,
        "docs/auth-guide.md#Login Flow",
        DocsTargetKind::Auto,
        10,
    )
    .expect("expected outgoing links report");

    assert_eq!(report.resolved_kind, "section");
    assert!(
        report
            .links
            .iter()
            .any(|item| item.symbol == "Checklist" && item.relationship == "links_to"),
        "expected outgoing doc link to Checklist section"
    );
    assert!(
        report
            .links
            .iter()
            .any(|item| item.symbol == "loginUser" && item.relationship == "mentions"),
        "expected outgoing mention to loginUser symbol"
    );
}

#[test]
fn test_find_stale_docs_returns_sections_that_mention_changed_code() {
    let graph = build_docs_graph();

    let report = find_stale_docs(
        &graph,
        &["src/auth.ts".to_string()],
        &["loginUser".to_string()],
        10,
    );

    assert_eq!(report.resolved_files, vec!["src/auth.ts".to_string()]);
    assert_eq!(report.resolved_symbols, vec!["loginUser".to_string()]);
    assert!(
        report.docs.iter().any(|doc| {
            doc.symbol == "Login Flow"
                && doc.matched_files.contains(&"src/auth.ts".to_string())
                && doc.matched_symbols.contains(&"loginUser".to_string())
        }),
        "expected Login Flow section to be flagged as stale against auth changes"
    );
}

#[test]
fn test_find_stale_docs_returns_docs_that_link_to_changed_markdown() {
    let graph = build_docs_graph();

    let report = find_stale_docs(&graph, &["docs/runbook.md".to_string()], &[], 10);

    assert!(
        report
            .docs
            .iter()
            .any(|doc| doc.symbol == "Login Flow" && doc.reasons.iter().any(|reason| reason.contains("changed doc"))),
        "expected Login Flow section to be flagged because it links to the changed runbook"
    );
}
