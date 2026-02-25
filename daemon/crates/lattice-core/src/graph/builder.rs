use std::collections::HashMap;
use crate::graph::model::{CodeGraph, EdgeKind};
use crate::symbols::{ParsedFile, SymbolId, SymbolKind};

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
        let mut graph = CodeGraph::new();

        // name_lookup: symbol name → list of SymbolIds with that name
        let mut name_lookup: HashMap<String, Vec<SymbolId>> = HashMap::new();

        // file_lookup: resolved file path → list of SymbolIds in that file
        let mut file_lookup: HashMap<String, Vec<SymbolId>> = HashMap::new();

        // ---- Pass 1: Add all symbols as nodes ----
        for file in &self.files {
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
        for file in &self.files {
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

            // 2b: Contains edges — class contains methods (by line range)
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
        }

        graph
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
    if resolved.contains('.') && resolved.rsplit('/').next().map_or(false, |f| f.contains('.')) {
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
