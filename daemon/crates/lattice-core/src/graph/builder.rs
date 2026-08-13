use crate::graph::model::{CodeGraph, EdgeKind};
use crate::symbols::{Language, ParsedFile, SymbolId, SymbolKind};
use std::collections::HashMap;

/// Builds a CodeGraph from parsed source files using a two-pass approach:
/// 1. Add all symbols as nodes and build a name→SymbolId lookup.
/// 2. Resolve edges: imports → Calls, class→method containment → Contains.
pub struct GraphBuilder {
    files: Vec<ParsedFile>,
}

impl GraphBuilder {
    /// Create a new empty builder.
    pub fn new() -> Self {
        Self { files: Vec::new() }
    }

    /// Add a parsed file to the builder.
    pub fn add_file(&mut self, file: ParsedFile) {
        self.files.push(file);
    }

    /// Consume the builder and produce a CodeGraph.
    pub fn build(self) -> CodeGraph {
        Self::build_from_files(self.files.iter())
    }

    /// Build directly from borrowed parsed files.
    ///
    /// Index rebuilds already retain parsed files in the indexer cache. Borrowing
    /// them avoids materializing a second full parsed-file collection while a
    /// replacement graph is under construction.
    pub fn build_from_files<'a>(files: impl IntoIterator<Item = &'a ParsedFile>) -> CodeGraph {
        let files = files.into_iter().collect::<Vec<_>>();
        let mut graph = CodeGraph::new();

        // name_lookup: symbol name → list of SymbolIds with that name
        let mut name_lookup: HashMap<String, Vec<SymbolId>> = HashMap::new();

        // file_lookup: resolved file path → list of SymbolIds in that file
        let mut file_lookup: HashMap<String, Vec<SymbolId>> = HashMap::new();
        let mut kind_lookup: HashMap<SymbolId, SymbolKind> = HashMap::new();

        // ---- Pass 1: Add all symbols as nodes ----
        for file in &files {
            for symbol in &file.symbols {
                graph.add_node(
                    symbol.id.clone(),
                    symbol.kind,
                    symbol.name.clone(),
                    symbol.signature.clone(),
                    symbol.body.clone(),
                    symbol.file.clone(),
                    symbol.line,
                    symbol.end_line,
                    symbol.is_exported,
                    symbol.language,
                );
                kind_lookup.insert(symbol.id.clone(), symbol.kind);

                // Index by simple name (last component for qualified names like "Class.method")
                let simple_name = symbol.name.rsplit('.').next().unwrap_or(&symbol.name);
                name_lookup
                    .entry(simple_name.to_string())
                    .or_default()
                    .push(symbol.id.clone());

                // Also index by full name
                if simple_name != symbol.name {
                    name_lookup
                        .entry(symbol.name.clone())
                        .or_default()
                        .push(symbol.id.clone());
                }

                file_lookup
                    .entry(symbol.file.clone())
                    .or_default()
                    .push(symbol.id.clone());
            }
        }

        // ---- Pass 2: Resolve edges ----
        for file in &files {
            // 2a: Resolve import edges
            for import in &file.imports {
                // Resolve the import source to a target file path
                let target_file = resolve_import_path(&file.file, &import.source);

                for imported_name in &import.names {
                    // Find the target symbol by name in the target file
                    let target_id = find_symbol_in_file(
                        imported_name,
                        &target_file,
                        &name_lookup,
                        &file_lookup,
                    );

                    if let Some(target_id) = target_id {
                        // Find symbols in this file that reference the imported name
                        for symbol in &file.symbols {
                            if symbol.references.contains(imported_name) {
                                graph.add_edge(&symbol.id, &target_id, EdgeKind::Calls);
                            }
                        }
                    }
                }
            }

            // 2b: Contains edges — classes contain methods and documents contain sections
            let classes: Vec<_> = file
                .symbols
                .iter()
                .filter(|s| s.kind == SymbolKind::Class)
                .collect();

            let methods: Vec<_> = file
                .symbols
                .iter()
                .filter(|s| s.kind == SymbolKind::Method)
                .collect();

            for class in &classes {
                for method in &methods {
                    // Method is contained in class if its line range is within the class's range
                    if method.line >= class.line && method.end_line <= class.end_line {
                        graph.add_edge(&class.id, &method.id, EdgeKind::Contains);
                    }
                }
            }

            let documents: Vec<_> = file
                .symbols
                .iter()
                .filter(|s| s.kind == SymbolKind::Document)
                .collect();
            let sections: Vec<_> = file
                .symbols
                .iter()
                .filter(|s| s.kind == SymbolKind::Section)
                .collect();

            for document in &documents {
                for section in &sections {
                    if section.id != document.id
                        && section.line >= document.line
                        && section.end_line <= document.end_line
                    {
                        graph.add_edge(&document.id, &section.id, EdgeKind::Contains);
                    }
                }
            }

            // 2c: Markdown links — resolve document and section links
            for link in &file.links {
                let Some(target_id) = resolve_document_link_target(
                    &file.file,
                    &link.target,
                    link.heading.as_deref(),
                    &file_lookup,
                    &kind_lookup,
                ) else {
                    continue;
                };

                if target_id != link.from {
                    graph.add_edge(&link.from, &target_id, EdgeKind::LinksTo);
                }
            }

            // 2d: Reference-based edges — resolve Symbol.references to graph edges
            // Parsers extract function calls from bodies into references.
            // Priority: same-file match first, then any global match.
            for symbol in &file.symbols {
                for ref_name in &symbol.references {
                    // Skip self-references
                    let simple_name = ref_name.rsplit('.').next().unwrap_or(ref_name);
                    if simple_name == symbol.name.rsplit('.').next().unwrap_or(&symbol.name) {
                        continue;
                    }

                    // Try same-file first
                    let target = if let Some(file_ids) = file_lookup.get(&file.file) {
                        if let Some(ref_ids) = name_lookup.get(simple_name) {
                            ref_ids.iter().find(|id| file_ids.contains(id)).cloned()
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    // Fall back to any global match
                    let target = target.or_else(|| {
                        name_lookup
                            .get(simple_name)
                            .and_then(|ids| ids.first().cloned())
                    });

                    if let Some(target_id) = target {
                        // Don't create self-loops
                        if target_id != symbol.id {
                            let edge_kind = if file.language == Language::Markdown {
                                EdgeKind::Mentions
                            } else {
                                EdgeKind::Calls
                            };
                            graph.add_edge(&symbol.id, &target_id, edge_kind);
                        }
                    }
                }
            }
        }

        graph
    }
}

fn resolve_document_link_target(
    from_file: &str,
    target: &str,
    heading: Option<&str>,
    file_lookup: &HashMap<String, Vec<SymbolId>>,
    kind_lookup: &HashMap<SymbolId, SymbolKind>,
) -> Option<SymbolId> {
    let target_file = resolve_markdown_target_path(from_file, target, file_lookup)?;

    if let Some(heading) = heading {
        if let Some(section_id) =
            find_section_in_file(heading, &target_file, file_lookup, kind_lookup)
        {
            return Some(section_id);
        }
    }

    find_symbol_in_file_by_kind(SymbolKind::Document, &target_file, file_lookup, kind_lookup)
        .or_else(|| {
            find_symbol_in_file_by_kind(SymbolKind::Section, &target_file, file_lookup, kind_lookup)
        })
}

fn resolve_markdown_target_path(
    from_file: &str,
    target: &str,
    file_lookup: &HashMap<String, Vec<SymbolId>>,
) -> Option<String> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Some(from_file.to_string());
    }

    if trimmed == from_file || file_lookup.contains_key(trimmed) {
        return Some(trimmed.to_string());
    }

    if trimmed.starts_with('#') {
        return Some(from_file.to_string());
    }

    if trimmed.starts_with('.') {
        let resolved = resolve_relative_path(from_file, trimmed, "md");
        if file_lookup.contains_key(&resolved) {
            return Some(resolved);
        }
    }

    let with_extension = if trimmed.contains('.') {
        trimmed.to_string()
    } else {
        format!("{}.md", trimmed)
    };
    if file_lookup.contains_key(&with_extension) {
        return Some(with_extension);
    }

    let target_name = trimmed
        .trim_end_matches(".md")
        .rsplit('/')
        .next()
        .unwrap_or(trimmed);
    let target_name_lower = target_name.to_lowercase();

    file_lookup.keys().find_map(|candidate| {
        let stem = candidate
            .rsplit('/')
            .next()
            .unwrap_or(candidate)
            .trim_end_matches(".md")
            .to_lowercase();
        if stem == target_name_lower {
            Some(candidate.clone())
        } else {
            None
        }
    })
}

fn find_symbol_in_file_by_kind(
    kind: SymbolKind,
    target_file: &str,
    file_lookup: &HashMap<String, Vec<SymbolId>>,
    kind_lookup: &HashMap<SymbolId, SymbolKind>,
) -> Option<SymbolId> {
    file_lookup.get(target_file).and_then(|ids| {
        ids.iter()
            .find(|id| kind_lookup.get(*id).copied() == Some(kind))
            .cloned()
    })
}

fn find_section_in_file(
    heading: &str,
    target_file: &str,
    file_lookup: &HashMap<String, Vec<SymbolId>>,
    kind_lookup: &HashMap<SymbolId, SymbolKind>,
) -> Option<SymbolId> {
    let target_anchor = normalize_anchor(heading);
    let file_symbols = file_lookup.get(target_file)?;

    file_symbols.iter().find_map(|id| {
        if kind_lookup.get(id).copied() != Some(SymbolKind::Section) {
            return None;
        }

        if normalize_anchor(&id.name) == target_anchor {
            Some(id.clone())
        } else {
            None
        }
    })
}

fn normalize_anchor(value: &str) -> String {
    let mut normalized = String::new();
    let mut last_was_dash = false;

    for ch in value.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch);
            last_was_dash = false;
        } else if ch.is_ascii_whitespace() || ch == '-' || ch == '_' {
            if !last_was_dash && !normalized.is_empty() {
                normalized.push('-');
                last_was_dash = true;
            }
        }
    }

    normalized.trim_matches('-').to_string()
}

fn resolve_relative_path(from_file: &str, target: &str, default_ext: &str) -> String {
    let dir = if let Some(pos) = from_file.rfind('/') {
        &from_file[..pos]
    } else {
        "."
    };

    let mut parts: Vec<&str> = dir.split('/').collect();
    for segment in target.split('/') {
        match segment {
            "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }

    let resolved = parts.join("/");
    if resolved.contains('.')
        && resolved
            .rsplit('/')
            .next()
            .map_or(false, |item| item.contains('.'))
    {
        resolved
    } else {
        format!("{}.{}", resolved, default_ext)
    }
}

impl Default for GraphBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve a relative import path to a file path.
/// E.g., from "src/auth.ts" importing "./crypto" → "src/crypto.ts"
fn resolve_import_path(from_file: &str, import_source: &str) -> String {
    // Only handle relative imports (starting with . or ..)
    if !import_source.starts_with('.') {
        return import_source.to_string();
    }

    // Get the directory of the importing file
    let dir = if let Some(pos) = from_file.rfind('/') {
        &from_file[..pos]
    } else {
        "."
    };

    // Resolve the relative path
    let mut parts: Vec<&str> = dir.split('/').collect();

    for segment in import_source.split('/') {
        match segment {
            "." => {} // current directory, skip
            ".." => {
                parts.pop();
            }
            other => {
                parts.push(other);
            }
        }
    }

    let resolved = parts.join("/");

    // Try common extensions
    let from_ext = from_file.rsplit('.').next().unwrap_or("ts");

    // If the resolved path already has an extension, use it as-is
    if resolved.contains('.')
        && resolved
            .rsplit('/')
            .next()
            .map_or(false, |f| f.contains('.'))
    {
        resolved
    } else {
        // Append the same extension as the source file
        format!("{}.{}", resolved, from_ext)
    }
}

/// Find a symbol by name in a specific target file.
/// Falls back to matching by name across all files if no match in target file.
fn find_symbol_in_file(
    name: &str,
    target_file: &str,
    name_lookup: &HashMap<String, Vec<SymbolId>>,
    file_lookup: &HashMap<String, Vec<SymbolId>>,
) -> Option<SymbolId> {
    // First try: find symbol with matching name in the target file
    if let Some(file_symbols) = file_lookup.get(target_file) {
        if let Some(name_symbols) = name_lookup.get(name) {
            for id in name_symbols {
                if file_symbols.contains(id) {
                    return Some(id.clone());
                }
            }
        }
    }

    // Fallback: find any exported symbol with that name
    if let Some(name_symbols) = name_lookup.get(name) {
        if let Some(id) = name_symbols.first() {
            return Some(id.clone());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::GraphBuilder;
    use crate::graph::model::{CodeGraph, EdgeKind, GraphNode};
    use crate::parser::parse_file;
    use crate::symbols::SymbolId;

    fn graph_snapshot(graph: &CodeGraph) -> (Vec<GraphNode>, Vec<(SymbolId, SymbolId, EdgeKind)>) {
        let nodes = graph.all_nodes().into_iter().cloned().collect();
        let edges = graph
            .all_edges()
            .into_iter()
            .map(|(from, to, kind)| (from.id.clone(), to.id.clone(), kind))
            .collect();
        (nodes, edges)
    }

    #[test]
    fn borrowed_files_build_the_same_graph_without_consuming_the_cache() {
        let caller = parse_file(
            "src/caller.ts",
            r#"
import { helper } from './helper';

export function caller(): string {
    return helper();
}
"#,
        )
        .expect("caller fixture should parse");
        let helper = parse_file(
            "src/helper.ts",
            r#"
export function helper(): string {
    return "ready";
}
"#,
        )
        .expect("helper fixture should parse");
        let cached_files = vec![caller, helper];

        let borrowed_graph = GraphBuilder::build_from_files(cached_files.iter());

        assert_eq!(cached_files.len(), 2);
        assert_eq!(cached_files[0].file, "src/caller.ts");
        assert!(!cached_files[0].symbols.is_empty());

        let mut owned_builder = GraphBuilder::new();
        for file in cached_files.clone() {
            owned_builder.add_file(file);
        }
        let owned_graph = owned_builder.build();

        assert_eq!(
            graph_snapshot(&borrowed_graph),
            graph_snapshot(&owned_graph)
        );
    }
}
