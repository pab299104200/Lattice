use tree_sitter::{Node, Parser};

use crate::error::LatticeError;
use crate::symbols::{ImportInfo, Language, ParsedFile, Symbol, SymbolId, SymbolKind};

/// Parse a Rust source file and extract symbols.
pub fn parse(file_path: &str, source: &str) -> Result<ParsedFile, LatticeError> {
    let mut parser = Parser::new();

    let ts_language = tree_sitter_rust::LANGUAGE.into();

    parser.set_language(&ts_language).map_err(|e| LatticeError::Parse {
        file: file_path.to_string(),
        message: format!("Failed to set Rust language: {}", e),
    })?;

    let tree = parser.parse(source, None).ok_or_else(|| LatticeError::Parse {
        file: file_path.to_string(),
        message: "Failed to parse Rust source".to_string(),
    })?;

    let root = tree.root_node();
    let source_bytes = source.as_bytes();

    let mut symbols = Vec::new();
    let mut imports = Vec::new();

    extract_from_node(root, source_bytes, file_path, &mut symbols, &mut imports);

    Ok(ParsedFile {
        file: file_path.to_string(),
        language: Language::Rust,
        symbols,
        imports,
    })
}

/// Walk root-level children and dispatch to extractors.
fn extract_from_node(
    node: Node,
    source: &[u8],
    file_path: &str,
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<ImportInfo>,
) {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_item" => {
                if let Some(sym) = extract_function(child, source, file_path) {
                    symbols.push(sym);
                }
            }
            "struct_item" => {
                if let Some(sym) = extract_struct(child, source, file_path) {
                    symbols.push(sym);
                }
            }
            "enum_item" => {
                if let Some(sym) = extract_enum(child, source, file_path) {
                    symbols.push(sym);
                }
            }
            "impl_item" => {
                let mut impl_symbols = extract_impl(child, source, file_path);
                symbols.append(&mut impl_symbols);
            }
            "trait_item" => {
                if let Some(sym) = extract_trait(child, source, file_path) {
                    symbols.push(sym);
                }
            }
            "type_item" => {
                if let Some(sym) = extract_type_alias(child, source, file_path) {
                    symbols.push(sym);
                }
            }
            "const_item" | "static_item" => {
                if let Some(sym) = extract_constant(child, source, file_path) {
                    symbols.push(sym);
                }
            }
            "use_declaration" => {
                if let Some(imp) = extract_use(child, source) {
                    imports.push(imp);
                }
            }
            "mod_item" => {
                if let Some(sym) = extract_module(child, source, file_path) {
                    symbols.push(sym);
                }
            }
            _ => {}
        }
    }
}

/// Check if a node has a `visibility_modifier` child (i.e., `pub`).
fn has_visibility_modifier(node: Node) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return true;
        }
    }
    false
}

/// Build a signature from a node: everything before the body block `{`.
fn build_signature(node: Node, source: &[u8]) -> String {
    let full_text = node_text(node, source);
    if let Some(brace_pos) = full_text.find('{') {
        full_text[..brace_pos].trim().to_string()
    } else {
        // For items without a body block (e.g., type aliases), use the whole text
        full_text.lines().next().unwrap_or("").trim().to_string()
    }
}

/// Extract a Rust function item.
fn extract_function(
    node: Node,
    source: &[u8],
    file_path: &str,
) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = build_signature(node, source);
    let is_exported = has_visibility_modifier(node);
    let references = extract_references_from_body(node, source);

    Some(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Function,
        name,
        signature,
        body: body_text,
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language: Language::Rust,
        references,
        imports: vec![],
    })
}

/// Extract a Rust struct item.
fn extract_struct(
    node: Node,
    source: &[u8],
    file_path: &str,
) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = build_signature(node, source);
    let is_exported = has_visibility_modifier(node);

    Some(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Struct,
        name,
        signature,
        body: body_text,
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language: Language::Rust,
        references: vec![],
        imports: vec![],
    })
}

/// Extract a Rust enum item.
fn extract_enum(
    node: Node,
    source: &[u8],
    file_path: &str,
) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = build_signature(node, source);
    let is_exported = has_visibility_modifier(node);

    Some(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Enum,
        name,
        signature,
        body: body_text,
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language: Language::Rust,
        references: vec![],
        imports: vec![],
    })
}

/// Extract methods from an impl block.
fn extract_impl(
    node: Node,
    source: &[u8],
    file_path: &str,
) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    // Get the type name from the impl block
    let type_name = node.child_by_field_name("type")
        .map(|n| node_text(n, source))
        .unwrap_or_default();

    // Find the body (declaration_list)
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "function_item" {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let method_name = node_text(name_node, source);
                    let qualified_name = format!("{}.{}", type_name, method_name);
                    let body_text = node_text(child, source);
                    let signature = build_signature(child, source);
                    let is_exported = has_visibility_modifier(child);
                    let references = extract_references_from_body(child, source);

                    symbols.push(Symbol {
                        id: SymbolId {
                            file: file_path.to_string(),
                            name: qualified_name.clone(),
                            byte_offset: child.start_byte(),
                        },
                        kind: SymbolKind::Method,
                        name: qualified_name,
                        signature,
                        body: body_text,
                        file: file_path.to_string(),
                        line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        is_exported,
                        language: Language::Rust,
                        references,
                        imports: vec![],
                    });
                }
            }
        }
    }

    symbols
}

/// Extract a Rust trait item.
fn extract_trait(
    node: Node,
    source: &[u8],
    file_path: &str,
) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = build_signature(node, source);
    let is_exported = has_visibility_modifier(node);

    Some(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Trait,
        name,
        signature,
        body: body_text,
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language: Language::Rust,
        references: vec![],
        imports: vec![],
    })
}

/// Extract a Rust type alias.
fn extract_type_alias(
    node: Node,
    source: &[u8],
    file_path: &str,
) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = body_text.trim().to_string();
    let is_exported = has_visibility_modifier(node);

    Some(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::TypeAlias,
        name,
        signature,
        body: body_text,
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language: Language::Rust,
        references: vec![],
        imports: vec![],
    })
}

/// Extract a Rust const or static item.
fn extract_constant(
    node: Node,
    source: &[u8],
    file_path: &str,
) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = body_text.lines().next().unwrap_or("").trim().to_string();
    let is_exported = has_visibility_modifier(node);

    Some(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Constant,
        name,
        signature,
        body: body_text,
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language: Language::Rust,
        references: vec![],
        imports: vec![],
    })
}

/// Extract a Rust use declaration as an import.
fn extract_use(node: Node, source: &[u8]) -> Option<ImportInfo> {
    let full_text = node_text(node, source);
    // Remove "use " prefix and trailing ";"
    let path = full_text
        .trim()
        .strip_prefix("use ")?
        .trim_end_matches(';')
        .trim()
        .to_string();

    // Extract leaf names from the use path
    let names = if path.contains('{') {
        // use std::collections::{HashMap, HashSet};
        if let Some(start) = path.find('{') {
            let end = path.find('}').unwrap_or(path.len());
            path[start + 1..end]
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        } else {
            vec![path.clone()]
        }
    } else {
        // use std::collections::HashMap;
        let leaf = path.rsplit("::").next().unwrap_or(&path).to_string();
        vec![leaf]
    };

    let is_wildcard = path.ends_with('*');

    Some(ImportInfo {
        source: path,
        names,
        is_default: false,
        is_wildcard,
    })
}

/// Extract a Rust module declaration.
fn extract_module(
    node: Node,
    source: &[u8],
    file_path: &str,
) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = body_text.lines().next().unwrap_or("").trim().to_string();
    let is_exported = has_visibility_modifier(node);

    Some(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Module,
        name,
        signature,
        body: body_text,
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language: Language::Rust,
        references: vec![],
        imports: vec![],
    })
}

/// Extract references from a function/method body (call expression identifiers).
fn extract_references_from_body(node: Node, source: &[u8]) -> Vec<String> {
    let mut refs = Vec::new();

    if let Some(body_node) = node.child_by_field_name("body") {
        collect_call_identifiers(body_node, source, &mut refs);
    }

    refs.sort();
    refs.dedup();
    refs
}

/// Recursively collect identifiers from call expressions.
fn collect_call_identifiers(node: Node, source: &[u8], refs: &mut Vec<String>) {
    if node.kind() == "call_expression" {
        if let Some(func_node) = node.child_by_field_name("function") {
            match func_node.kind() {
                "identifier" => {
                    refs.push(node_text(func_node, source));
                }
                "scoped_identifier" | "field_expression" => {
                    // For path::to::func() or obj.method(), collect the full text
                    refs.push(node_text(func_node, source));
                }
                _ => {}
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_call_identifiers(child, source, refs);
    }
}

/// Get the text of a tree-sitter node.
fn node_text(node: Node, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or("").to_string()
}
