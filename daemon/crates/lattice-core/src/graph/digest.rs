//! Deterministic, bounded per-source-file summaries derived exclusively from a [`CodeGraph`].
//!
//! Digest generation belongs on the index publication path. It intentionally performs no
//! filesystem, clock, random, model, or query-dependent work, so identical graph facts always
//! produce identical payload bytes regardless of graph insertion order.

use crate::graph::{CodeGraph, EdgeKind, GraphNode};
use crate::symbols::{Language, SymbolKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use thiserror::Error;

pub const MODULE_DIGEST_SCHEMA_VERSION: u32 = 1;
pub const MODULE_DIGEST_GENERATOR_VERSION: u32 = 1;

const MAX_EXPORTED_ANCHORS: usize = 4;
const MAX_HEADINGS: usize = 6;
const MAX_RELATIONSHIPS: usize = 6;
const MAX_FACTS: usize = 10;
const MAX_SYMBOL_KINDS: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleDigestPayload {
    pub schema_version: u32,
    pub module_path: String,
    pub module_role: String,
    pub language: String,
    pub entry_anchor: DigestAnchor,
    pub symbol_kinds: Vec<String>,
    pub exported_anchors: Vec<DigestAnchor>,
    pub headings: Vec<DigestAnchor>,
    pub relationships: Vec<DigestRelationship>,
    pub facts: Vec<DigestFact>,
    pub search_terms: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestAnchor {
    pub name: String,
    pub kind: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestRelationship {
    pub kind: String,
    pub from: DigestEndpoint,
    pub to: DigestEndpoint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestEndpoint {
    pub path: String,
    pub name: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestFact {
    pub fact_id: String,
    pub sentence: String,
    pub citations: Vec<DigestCitation>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DigestCitation {
    pub path: String,
    pub line: usize,
}

/// A generated payload plus the integrity metadata required by persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedModuleDigest {
    pub module_path: String,
    pub generator_version: u32,
    pub input_fingerprint: String,
    pub payload_sha256: String,
    /// Compact canonical JSON. Field order is fixed by [`ModuleDigestPayload`] and every array is
    /// sorted before serialization.
    pub payload_json: String,
    pub payload: ModuleDigestPayload,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ModuleDigestError {
    #[error("module digest path is not a normalized workspace-relative path: {path:?} ({reason})")]
    InvalidModulePath { path: String, reason: &'static str },
    #[error("module digest cannot cite {path:?} at line 0")]
    InvalidNodeLine { path: String },
    #[error(
        "module {path:?} contains inconsistent languages: expected {expected}, found {actual}"
    )]
    InconsistentLanguage {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("failed to serialize module digest for {path:?}: {message}")]
    Serialization { path: String, message: String },
}

#[derive(Clone)]
struct CanonicalNode<'a> {
    node: &'a GraphNode,
    path: String,
}

#[derive(Clone)]
struct CanonicalEdge<'a> {
    from: CanonicalNode<'a>,
    to: CanonicalNode<'a>,
    kind: EdgeKind,
}

/// Generate exactly one digest for each distinct normalized file represented in `graph`.
///
/// The graph is traversed a constant number of times. Per-module sorting makes the result stable
/// without performing a full-graph traversal for every module.
pub fn generate_module_digests(
    graph: &CodeGraph,
) -> Result<Vec<GeneratedModuleDigest>, ModuleDigestError> {
    let mut normalized_paths = HashMap::<&str, String>::new();
    let mut modules = BTreeMap::<String, Vec<CanonicalNode<'_>>>::new();

    for node in graph.all_nodes() {
        let path = normalize_module_path(&node.file)?;
        if node.line == 0 {
            return Err(ModuleDigestError::InvalidNodeLine { path });
        }
        normalized_paths.insert(node.file.as_str(), path.clone());
        modules
            .entry(path.clone())
            .or_default()
            .push(CanonicalNode { node, path });
    }

    for nodes in modules.values_mut() {
        nodes.sort_by(compare_nodes);
    }

    let mut edges_by_module = BTreeMap::<String, Vec<CanonicalEdge<'_>>>::new();
    let mut degree = HashMap::<_, usize>::new();
    for (from, to, kind) in graph.all_edges() {
        let from_path = normalized_paths
            .get(from.file.as_str())
            .expect("every edge endpoint is a graph node");
        let to_path = normalized_paths
            .get(to.file.as_str())
            .expect("every edge endpoint is a graph node");
        let edge = CanonicalEdge {
            from: CanonicalNode {
                node: from,
                path: from_path.clone(),
            },
            to: CanonicalNode {
                node: to,
                path: to_path.clone(),
            },
            kind,
        };
        edges_by_module
            .entry(from_path.clone())
            .or_default()
            .push(edge.clone());
        if from_path != to_path {
            edges_by_module
                .entry(to_path.clone())
                .or_default()
                .push(edge);
        }
        *degree.entry(&from.id).or_default() += 1;
        *degree.entry(&to.id).or_default() += 1;
    }
    for edges in edges_by_module.values_mut() {
        edges.sort_by(compare_edges);
    }

    modules
        .into_iter()
        .map(|(module_path, nodes)| {
            let edges = edges_by_module.remove(&module_path).unwrap_or_default();
            generate_one(module_path, nodes, edges, &degree)
        })
        .collect()
}

fn generate_one(
    module_path: String,
    nodes: Vec<CanonicalNode<'_>>,
    edges: Vec<CanonicalEdge<'_>>,
    degree: &HashMap<&crate::symbols::SymbolId, usize>,
) -> Result<GeneratedModuleDigest, ModuleDigestError> {
    let first_language = nodes[0].node.language;
    for node in &nodes[1..] {
        if node.node.language != first_language {
            return Err(ModuleDigestError::InconsistentLanguage {
                path: module_path,
                expected: language_name(first_language).to_string(),
                actual: language_name(node.node.language).to_string(),
            });
        }
    }

    let entry = nodes
        .iter()
        .min_by(|left, right| compare_entry(left, right, degree))
        .expect("a module always has at least one node");
    let exported = nodes
        .iter()
        .filter(|node| node.node.is_exported && !is_document_kind(node.node.kind))
        .take(MAX_EXPORTED_ANCHORS)
        .map(|node| anchor(node.node))
        .collect::<Vec<_>>();
    let headings = nodes
        .iter()
        .filter(|node| node.node.kind == SymbolKind::Section)
        .take(MAX_HEADINGS)
        .map(|node| anchor(node.node))
        .collect::<Vec<_>>();
    let relationships = edges
        .iter()
        .take(MAX_RELATIONSHIPS)
        .map(relationship)
        .collect::<Vec<_>>();

    let mut symbol_kinds = nodes
        .iter()
        .map(|node| symbol_kind_name(node.node.kind).to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_SYMBOL_KINDS)
        .collect::<Vec<_>>();
    // `BTreeSet` already establishes the documented lexical order; this makes that invariant
    // explicit at the serialization boundary.
    symbol_kinds.sort();

    let mut facts = Vec::with_capacity(MAX_FACTS);
    facts.push(entry_fact(&module_path, entry.node));
    for anchor in &exported {
        if facts.len() == MAX_FACTS {
            break;
        }
        if anchor.name != entry.node.name || anchor.line != entry.node.line {
            facts.push(export_fact(&module_path, anchor));
        }
    }
    for relation in &relationships {
        if facts.len() == MAX_FACTS {
            break;
        }
        facts.push(relationship_fact(relation));
    }
    for heading in &headings {
        if facts.len() == MAX_FACTS {
            break;
        }
        if heading.name != entry.node.name || heading.line != entry.node.line {
            facts.push(heading_fact(&module_path, heading));
        }
    }

    let module_role = module_role(&module_path, first_language).to_string();
    let language = language_name(first_language).to_string();
    let mut search_terms = BTreeSet::new();
    add_search_terms(&mut search_terms, &module_path);
    add_search_terms(&mut search_terms, &language);
    add_search_terms(&mut search_terms, &module_role);
    add_search_terms(&mut search_terms, &entry.node.name);
    for kind in &symbol_kinds {
        add_search_terms(&mut search_terms, kind);
    }
    for selected in exported.iter().chain(&headings) {
        add_search_terms(&mut search_terms, &selected.name);
        add_search_terms(&mut search_terms, &selected.kind);
    }
    for relation in &relationships {
        add_search_terms(&mut search_terms, edge_kind_name_from_str(&relation.kind));
        add_search_terms(&mut search_terms, &relation.from.path);
        add_search_terms(&mut search_terms, &relation.from.name);
        add_search_terms(&mut search_terms, &relation.to.path);
        add_search_terms(&mut search_terms, &relation.to.name);
    }

    let input_fingerprint = fingerprint(&module_path, &nodes, &edges);
    let payload = ModuleDigestPayload {
        schema_version: MODULE_DIGEST_SCHEMA_VERSION,
        module_path: module_path.clone(),
        module_role,
        language,
        entry_anchor: anchor(entry.node),
        symbol_kinds,
        exported_anchors: exported,
        headings,
        relationships,
        facts,
        search_terms: search_terms.into_iter().collect(),
    };
    let payload_json =
        serde_json::to_string(&payload).map_err(|error| ModuleDigestError::Serialization {
            path: module_path.clone(),
            message: error.to_string(),
        })?;
    let payload_sha256 = sha256_hex(payload_json.as_bytes());

    Ok(GeneratedModuleDigest {
        module_path,
        generator_version: MODULE_DIGEST_GENERATOR_VERSION,
        input_fingerprint,
        payload_sha256,
        payload_json,
        payload,
    })
}

fn normalize_module_path(path: &str) -> Result<String, ModuleDigestError> {
    if path.is_empty() {
        return Err(invalid_path(path, "path is empty"));
    }
    let replaced = path.replace('\\', "/");
    if replaced.starts_with('/') || replaced.starts_with("//") {
        return Err(invalid_path(path, "absolute paths are forbidden"));
    }
    if replaced.as_bytes().get(1) == Some(&b':') {
        return Err(invalid_path(path, "drive-qualified paths are forbidden"));
    }
    let mut components = Vec::new();
    for component in replaced.split('/') {
        match component {
            "" | "." => {}
            ".." => return Err(invalid_path(path, "parent components are forbidden")),
            value => components.push(value),
        }
    }
    if components.is_empty() {
        return Err(invalid_path(path, "path has no file component"));
    }
    Ok(components.join("/"))
}

fn invalid_path(path: &str, reason: &'static str) -> ModuleDigestError {
    ModuleDigestError::InvalidModulePath {
        path: path.to_string(),
        reason,
    }
}

fn compare_nodes(left: &CanonicalNode<'_>, right: &CanonicalNode<'_>) -> Ordering {
    node_key(left.node)
        .cmp(&node_key(right.node))
        .then_with(|| left.node.id.file.cmp(&right.node.id.file))
        .then_with(|| left.node.id.name.cmp(&right.node.id.name))
        .then_with(|| left.node.end_line.cmp(&right.node.end_line))
        .then_with(|| left.node.is_exported.cmp(&right.node.is_exported))
        .then_with(|| language_name(left.node.language).cmp(language_name(right.node.language)))
}

fn node_key(node: &GraphNode) -> (usize, usize, &'static str, &str, &str) {
    (
        node.line,
        node.id.byte_offset,
        symbol_kind_name(node.kind),
        &node.name,
        &node.signature,
    )
}

fn compare_entry(
    left: &CanonicalNode<'_>,
    right: &CanonicalNode<'_>,
    degree: &HashMap<&crate::symbols::SymbolId, usize>,
) -> Ordering {
    (!(left.node.is_exported && !is_document_kind(left.node.kind)))
        .cmp(&(!(right.node.is_exported && !is_document_kind(right.node.kind))))
        .then_with(|| {
            degree
                .get(&right.node.id)
                .unwrap_or(&0)
                .cmp(degree.get(&left.node.id).unwrap_or(&0))
        })
        .then_with(|| node_key(left.node).cmp(&node_key(right.node)))
}

fn compare_edges(left: &CanonicalEdge<'_>, right: &CanonicalEdge<'_>) -> Ordering {
    edge_key(left).cmp(&edge_key(right))
}

#[allow(clippy::type_complexity)]
fn edge_key<'a>(
    edge: &'a CanonicalEdge<'a>,
) -> (
    &'static str,
    &'a str,
    usize,
    &'a str,
    &'a str,
    usize,
    &'a str,
    (usize, usize, &'a str, &'a str, &'a str, &'a str),
) {
    (
        edge_kind_name(edge.kind),
        edge.from.path.as_str(),
        edge.from.node.line,
        &edge.from.node.name,
        edge.to.path.as_str(),
        edge.to.node.line,
        &edge.to.node.name,
        (
            edge.from.node.id.byte_offset,
            edge.to.node.id.byte_offset,
            symbol_kind_name(edge.from.node.kind),
            symbol_kind_name(edge.to.node.kind),
            &edge.from.node.signature,
            &edge.to.node.signature,
        ),
    )
}

fn anchor(node: &GraphNode) -> DigestAnchor {
    DigestAnchor {
        name: node.name.clone(),
        kind: symbol_kind_name(node.kind).to_string(),
        line: node.line,
    }
}

fn relationship(edge: &CanonicalEdge<'_>) -> DigestRelationship {
    DigestRelationship {
        kind: edge_kind_name(edge.kind).to_string(),
        from: endpoint(&edge.from),
        to: endpoint(&edge.to),
    }
}

fn endpoint(node: &CanonicalNode<'_>) -> DigestEndpoint {
    DigestEndpoint {
        path: node.path.clone(),
        name: node.node.name.clone(),
        line: node.node.line,
    }
}

fn entry_fact(module_path: &str, node: &GraphNode) -> DigestFact {
    let citation = DigestCitation {
        path: module_path.to_string(),
        line: node.line,
    };
    DigestFact {
        fact_id: format!("entry:{}:{}", node.name, node.line),
        sentence: format!(
            "Start with `{}` at `{module_path}:{}`, the module's {} graph anchor.",
            node.name,
            node.line,
            if node.is_exported {
                "exported"
            } else {
                "primary"
            }
        ),
        citations: vec![citation],
    }
}

fn export_fact(module_path: &str, value: &DigestAnchor) -> DigestFact {
    DigestFact {
        fact_id: format!("export:{}:{}", value.name, value.line),
        sentence: format!(
            "`{}` is an exported {} at `{module_path}:{}`.",
            value.name, value.kind, value.line
        ),
        citations: vec![DigestCitation {
            path: module_path.to_string(),
            line: value.line,
        }],
    }
}

fn heading_fact(module_path: &str, value: &DigestAnchor) -> DigestFact {
    DigestFact {
        fact_id: format!("heading:{}:{}", value.name, value.line),
        sentence: format!(
            "The heading `{}` is indexed at `{module_path}:{}`.",
            value.name, value.line
        ),
        citations: vec![DigestCitation {
            path: module_path.to_string(),
            line: value.line,
        }],
    }
}

fn relationship_fact(value: &DigestRelationship) -> DigestFact {
    let from = DigestCitation {
        path: value.from.path.clone(),
        line: value.from.line,
    };
    let to = DigestCitation {
        path: value.to.path.clone(),
        line: value.to.line,
    };
    let mut citations = vec![from, to];
    citations.sort();
    citations.dedup();
    DigestFact {
        fact_id: format!(
            "relationship:{}:{}:{}:{}:{}:{}:{}",
            value.kind,
            value.from.path,
            value.from.name,
            value.from.line,
            value.to.path,
            value.to.name,
            value.to.line
        ),
        sentence: format!(
            "`{}` at `{}:{}` {} `{}` at `{}:{}`.",
            value.from.name,
            value.from.path,
            value.from.line,
            relationship_verb(&value.kind),
            value.to.name,
            value.to.path,
            value.to.line
        ),
        citations,
    }
}

fn fingerprint(
    module_path: &str,
    nodes: &[CanonicalNode<'_>],
    edges: &[CanonicalEdge<'_>],
) -> String {
    let mut bytes = Vec::new();
    append_field(&mut bytes, b"module-digest-input-v1");
    append_field(&mut bytes, module_path.as_bytes());
    append_u64(&mut bytes, nodes.len());
    for value in nodes {
        let node = value.node;
        append_field(&mut bytes, value.path.as_bytes());
        append_field(&mut bytes, node.id.file.as_bytes());
        append_field(&mut bytes, node.id.name.as_bytes());
        append_u64(&mut bytes, node.id.byte_offset);
        append_field(&mut bytes, symbol_kind_name(node.kind).as_bytes());
        append_field(&mut bytes, node.name.as_bytes());
        append_field(&mut bytes, node.signature.as_bytes());
        append_u64(&mut bytes, node.line);
        append_u64(&mut bytes, node.end_line);
        append_u64(&mut bytes, usize::from(node.is_exported));
        append_field(&mut bytes, language_name(node.language).as_bytes());
    }
    append_u64(&mut bytes, edges.len());
    for edge in edges {
        append_field(&mut bytes, edge_kind_name(edge.kind).as_bytes());
        append_edge_endpoint(&mut bytes, &edge.from);
        append_edge_endpoint(&mut bytes, &edge.to);
    }
    sha256_hex(&bytes)
}

fn append_edge_endpoint(bytes: &mut Vec<u8>, value: &CanonicalNode<'_>) {
    append_field(bytes, value.path.as_bytes());
    append_u64(bytes, value.node.line);
    append_u64(bytes, value.node.id.byte_offset);
    append_field(bytes, symbol_kind_name(value.node.kind).as_bytes());
    append_field(bytes, value.node.name.as_bytes());
    append_field(bytes, value.node.signature.as_bytes());
}

fn append_field(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u64).to_be_bytes());
    output.extend_from_slice(value);
}

fn append_u64(output: &mut Vec<u8>, value: usize) {
    output.extend_from_slice(&(value as u64).to_be_bytes());
}

fn sha256_hex(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in hash {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn add_search_terms(terms: &mut BTreeSet<String>, value: &str) {
    let mut token = String::new();
    let mut previous_lowercase = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if character.is_ascii_uppercase() && previous_lowercase && !token.is_empty() {
                terms.insert(token.clone());
                token.clear();
            }
            token.push(character.to_ascii_lowercase());
            previous_lowercase = character.is_ascii_lowercase() || character.is_ascii_digit();
        } else {
            if !token.is_empty() {
                terms.insert(std::mem::take(&mut token));
            }
            previous_lowercase = false;
        }
    }
    if !token.is_empty() {
        terms.insert(token);
    }
}

fn module_role(path: &str, language: Language) -> &'static str {
    if language == Language::Markdown {
        "documentation"
    } else if path
        .split('/')
        .any(|component| matches!(component, "test" | "tests" | "spec" | "specs" | "__tests__"))
        || path.rsplit('/').next().is_some_and(|name| {
            name.contains("_test.") || name.contains(".test.") || name.contains(".spec.")
        })
    {
        "test"
    } else {
        "code"
    }
}

fn is_document_kind(kind: SymbolKind) -> bool {
    matches!(kind, SymbolKind::Document | SymbolKind::Section)
}

fn symbol_kind_name(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Class => "class",
        SymbolKind::Interface => "interface",
        SymbolKind::TypeAlias => "type_alias",
        SymbolKind::Enum => "enum",
        SymbolKind::Module => "module",
        SymbolKind::Variable => "variable",
        SymbolKind::Constant => "constant",
        SymbolKind::Method => "method",
        SymbolKind::Trait => "trait",
        SymbolKind::Struct => "struct",
        SymbolKind::Document => "document",
        SymbolKind::Section => "section",
    }
}

fn edge_kind_name(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Calls => "calls",
        EdgeKind::Imports => "imports",
        EdgeKind::Implements => "implements",
        EdgeKind::Extends => "extends",
        EdgeKind::TypeRef => "type_ref",
        EdgeKind::Contains => "contains",
        EdgeKind::LinksTo => "links_to",
        EdgeKind::Mentions => "mentions",
        EdgeKind::CoChanges => "co_changes",
    }
}

fn edge_kind_name_from_str(kind: &str) -> &str {
    kind
}

fn relationship_verb(kind: &str) -> &'static str {
    match kind {
        "calls" => "calls",
        "imports" => "imports",
        "implements" => "implements",
        "extends" => "extends",
        "type_ref" => "references the type",
        "contains" => "contains",
        "links_to" => "links to",
        "mentions" => "mentions",
        "co_changes" => "co-changes with",
        _ => "relates to",
    }
}

fn language_name(language: Language) -> &'static str {
    match language {
        Language::TypeScript => "TypeScript",
        Language::JavaScript => "JavaScript",
        Language::Python => "Python",
        Language::Rust => "Rust",
        Language::Go => "Go",
        Language::Java => "Java",
        Language::Markdown => "Markdown",
        Language::Unknown => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::SymbolId;

    fn id(file: &str, name: &str, byte_offset: usize) -> SymbolId {
        SymbolId {
            file: file.to_string(),
            name: name.to_string(),
            byte_offset,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add(
        graph: &mut CodeGraph,
        file: &str,
        name: &str,
        offset: usize,
        line: usize,
        kind: SymbolKind,
        exported: bool,
        language: Language,
    ) -> SymbolId {
        let id = id(file, name, offset);
        graph.add_node(
            id.clone(),
            kind,
            name.to_string(),
            format!("signature {name}"),
            format!("body {name}"),
            file.to_string(),
            line,
            line + 2,
            exported,
            language,
        );
        id
    }

    fn fixture(reverse: bool) -> CodeGraph {
        let definitions = [
            (
                "lib.rs",
                "run",
                30,
                8,
                SymbolKind::Function,
                true,
                Language::Rust,
            ),
            (
                "lib.rs",
                "helper",
                80,
                20,
                SymbolKind::Function,
                false,
                Language::Rust,
            ),
            (
                "src/parser.rs",
                "Parser",
                5,
                3,
                SymbolKind::Struct,
                true,
                Language::Rust,
            ),
            (
                "tests/parser_test.rs",
                "parses_input",
                4,
                4,
                SymbolKind::Function,
                false,
                Language::Rust,
            ),
            (
                "docs/guide.md",
                "Guide",
                0,
                1,
                SymbolKind::Document,
                false,
                Language::Markdown,
            ),
            (
                "docs/guide.md",
                "Parsing",
                40,
                5,
                SymbolKind::Section,
                false,
                Language::Markdown,
            ),
        ];
        let mut graph = CodeGraph::new();
        let iterator: Box<dyn Iterator<Item = &_>> = if reverse {
            Box::new(definitions.iter().rev())
        } else {
            Box::new(definitions.iter())
        };
        for &(file, name, offset, line, kind, exported, language) in iterator {
            add(
                &mut graph, file, name, offset, line, kind, exported, language,
            );
        }
        let edges = [
            (
                id("lib.rs", "run", 30),
                id("src/parser.rs", "Parser", 5),
                EdgeKind::Calls,
            ),
            (
                id("tests/parser_test.rs", "parses_input", 4),
                id("lib.rs", "run", 30),
                EdgeKind::Calls,
            ),
            (
                id("docs/guide.md", "Guide", 0),
                id("docs/guide.md", "Parsing", 40),
                EdgeKind::Contains,
            ),
        ];
        let edge_iterator: Box<dyn Iterator<Item = &_>> = if reverse {
            Box::new(edges.iter().rev())
        } else {
            Box::new(edges.iter())
        };
        for (from, to, kind) in edge_iterator {
            graph.add_edge(from, to, *kind);
        }
        graph
    }

    #[test]
    fn exact_payload_snapshot_has_bounded_cited_facts() {
        let digests = generate_module_digests(&fixture(false)).unwrap();
        let digest = digests
            .iter()
            .find(|value| value.module_path == "lib.rs")
            .unwrap();
        assert_eq!(
            digest.payload_json,
            r#"{"schema_version":1,"module_path":"lib.rs","module_role":"code","language":"Rust","entry_anchor":{"name":"run","kind":"function","line":8},"symbol_kinds":["function"],"exported_anchors":[{"name":"run","kind":"function","line":8}],"headings":[],"relationships":[{"kind":"calls","from":{"path":"lib.rs","name":"run","line":8},"to":{"path":"src/parser.rs","name":"Parser","line":3}},{"kind":"calls","from":{"path":"tests/parser_test.rs","name":"parses_input","line":4},"to":{"path":"lib.rs","name":"run","line":8}}],"facts":[{"fact_id":"entry:run:8","sentence":"Start with `run` at `lib.rs:8`, the module's exported graph anchor.","citations":[{"path":"lib.rs","line":8}]},{"fact_id":"relationship:calls:lib.rs:run:8:src/parser.rs:Parser:3","sentence":"`run` at `lib.rs:8` calls `Parser` at `src/parser.rs:3`.","citations":[{"path":"lib.rs","line":8},{"path":"src/parser.rs","line":3}]},{"fact_id":"relationship:calls:tests/parser_test.rs:parses_input:4:lib.rs:run:8","sentence":"`parses_input` at `tests/parser_test.rs:4` calls `run` at `lib.rs:8`.","citations":[{"path":"lib.rs","line":8},{"path":"tests/parser_test.rs","line":4}]}],"search_terms":["calls","code","function","input","lib","parser","parses","rs","run","rust","src","test","tests"]}"#
        );
        assert_eq!(
            digest.payload_sha256,
            sha256_hex(digest.payload_json.as_bytes())
        );
        assert_eq!(digest.input_fingerprint.len(), 64);
    }

    #[test]
    fn generation_is_independent_of_node_and_edge_insertion_order() {
        let forward = generate_module_digests(&fixture(false)).unwrap();
        let reverse = generate_module_digests(&fixture(true)).unwrap();
        assert_eq!(forward, reverse);
    }

    #[test]
    fn produces_one_digest_per_normalized_source_file_with_roles() {
        let mut graph = fixture(false);
        add(
            &mut graph,
            "./src\\nested.rs",
            "nested",
            0,
            1,
            SymbolKind::Function,
            false,
            Language::Rust,
        );
        let digests = generate_module_digests(&graph).unwrap();
        assert_eq!(
            digests
                .iter()
                .map(|value| value.module_path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "docs/guide.md",
                "lib.rs",
                "src/nested.rs",
                "src/parser.rs",
                "tests/parser_test.rs"
            ]
        );
        assert_eq!(digests[0].payload.module_role, "documentation");
        assert_eq!(digests[4].payload.module_role, "test");
    }

    #[test]
    fn body_only_changes_do_not_change_digest_inputs_or_payload() {
        let mut changed = fixture(false);
        let run = id("lib.rs", "run", 30);
        changed.add_node(
            run,
            SymbolKind::Function,
            "run".to_string(),
            "signature run",
            "completely different body",
            "lib.rs".to_string(),
            8,
            10,
            true,
            Language::Rust,
        );
        assert_eq!(
            generate_module_digests(&fixture(false)).unwrap(),
            generate_module_digests(&changed).unwrap()
        );
    }

    #[test]
    fn cross_module_edge_changes_both_endpoint_fingerprints() {
        let before = generate_module_digests(&fixture(false)).unwrap();
        let mut changed = fixture(false);
        changed.add_edge(
            &id("src/parser.rs", "Parser", 5),
            &id("lib.rs", "helper", 80),
            EdgeKind::TypeRef,
        );
        let after = generate_module_digests(&changed).unwrap();
        for path in ["lib.rs", "src/parser.rs"] {
            let old = before
                .iter()
                .find(|value| value.module_path == path)
                .unwrap();
            let new = after
                .iter()
                .find(|value| value.module_path == path)
                .unwrap();
            assert_ne!(old.input_fingerprint, new.input_fingerprint, "{path}");
        }
        let unaffected_old = before
            .iter()
            .find(|value| value.module_path == "docs/guide.md")
            .unwrap();
        let unaffected_new = after
            .iter()
            .find(|value| value.module_path == "docs/guide.md")
            .unwrap();
        assert_eq!(unaffected_old, unaffected_new);
    }

    #[test]
    fn rejects_paths_that_cannot_be_workspace_relative_citations() {
        for path in [
            "",
            "/tmp/file.rs",
            "../file.rs",
            "src/../file.rs",
            "C:\\file.rs",
        ] {
            let mut graph = CodeGraph::new();
            add(
                &mut graph,
                path,
                "bad",
                0,
                1,
                SymbolKind::Function,
                false,
                Language::Rust,
            );
            assert!(matches!(
                generate_module_digests(&graph),
                Err(ModuleDigestError::InvalidModulePath { .. })
            ));
        }
    }
}
