use tree_sitter::{Node, Parser};

use crate::error::LatticeError;
use crate::symbols::{ImportInfo, Language, ParsedFile, Symbol, SymbolId, SymbolKind};

/// Parse a TypeScript or JavaScript source file and extract symbols.
pub fn parse(file_path: &str, source: &str, language: Language) -> Result<ParsedFile, LatticeError> {
    let mut parser = Parser::new();

    let ts_language = match language {
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        _ => {
            return Err(LatticeError::Parse {
                file: file_path.to_string(),
                message: format!("Unsupported language for TS/JS parser: {:?}", language),
            });
        }
    };

    parser.set_language(&ts_language).map_err(|e| LatticeError::Parse {
        file: file_path.to_string(),
        message: format!("Failed to set language: {}", e),
    })?;

    let tree = parser.parse(source, None).ok_or_else(|| LatticeError::Parse {
        file: file_path.to_string(),
        message: "Failed to parse source".to_string(),
    })?;

    let root = tree.root_node();
    let source_bytes = source.as_bytes();

    let mut symbols = Vec::new();
    let mut imports = Vec::new();

    extract_from_node(root, source_bytes, file_path, language, &mut symbols, &mut imports);

    Ok(ParsedFile {
        file: file_path.to_string(),
        language,
        symbols,
        imports,
    })
}

/// Walk the AST children of a node and dispatch to the appropriate extractor.
fn extract_from_node(
    node: Node,
    source: &[u8],
    file_path: &str,
    language: Language,
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<ImportInfo>,
) {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "export_statement" => {
                extract_declaration(child, source, file_path, language, symbols, imports, true);
            }
            "function_declaration" => {
                if let Some(sym) = extract_function(child, source, file_path, language, false) {
                    symbols.push(sym);
                }
            }
            "class_declaration" => {
                let mut class_symbols = extract_class(child, source, file_path, language, false);
                symbols.append(&mut class_symbols);
            }
            "interface_declaration" => {
                if let Some(sym) = extract_interface(child, source, file_path, language, false) {
                    symbols.push(sym);
                }
            }
            "type_alias_declaration" => {
                if let Some(sym) = extract_type_alias(child, source, file_path, language, false) {
                    symbols.push(sym);
                }
            }
            "enum_declaration" => {
                if let Some(sym) = extract_enum(child, source, file_path, language, false) {
                    symbols.push(sym);
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut var_symbols = extract_variable(child, source, file_path, language, false);
                symbols.append(&mut var_symbols);
            }
            "import_statement" => {
                if let Some(imp) = extract_import(child, source) {
                    imports.push(imp);
                }
            }
            _ => {}
        }
    }
}

/// Handle exported declarations — unwrap the export_statement and extract the inner declaration.
fn extract_declaration(
    node: Node,
    source: &[u8],
    file_path: &str,
    language: Language,
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<ImportInfo>,
    is_exported: bool,
) {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(sym) = extract_function(child, source, file_path, language, is_exported) {
                    symbols.push(sym);
                }
            }
            "class_declaration" => {
                let mut class_symbols = extract_class(child, source, file_path, language, is_exported);
                symbols.append(&mut class_symbols);
            }
            "interface_declaration" => {
                if let Some(sym) = extract_interface(child, source, file_path, language, is_exported) {
                    symbols.push(sym);
                }
            }
            "type_alias_declaration" => {
                if let Some(sym) = extract_type_alias(child, source, file_path, language, is_exported) {
                    symbols.push(sym);
                }
            }
            "enum_declaration" => {
                if let Some(sym) = extract_enum(child, source, file_path, language, is_exported) {
                    symbols.push(sym);
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut var_symbols = extract_variable(child, source, file_path, language, is_exported);
                symbols.append(&mut var_symbols);
            }
            "import_statement" => {
                if let Some(imp) = extract_import(child, source) {
                    imports.push(imp);
                }
            }
            _ => {}
        }
    }
}

/// Extract a function declaration as a Symbol.
fn extract_function(
    node: Node,
    source: &[u8],
    file_path: &str,
    language: Language,
    is_exported: bool,
) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = build_function_signature(node, source);
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
        language,
        references,
        imports: vec![],
    })
}

/// Build function signature — everything before the function body `{`.
fn build_function_signature(node: Node, source: &[u8]) -> String {
    let full_text = node_text(node, source);

    // Find the opening brace to get the signature
    if let Some(brace_pos) = full_text.find('{') {
        full_text[..brace_pos].trim().to_string()
    } else {
        full_text.lines().next().unwrap_or("").trim().to_string()
    }
}

/// Extract a class declaration and its methods.
fn extract_class(
    node: Node,
    source: &[u8],
    file_path: &str,
    language: Language,
    is_exported: bool,
) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => return symbols,
    };
    let class_name = node_text(name_node, source);
    let body_text = node_text(node, source);

    // Build class signature (first line)
    let signature = body_text.lines().next().unwrap_or("").trim().to_string();

    symbols.push(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: class_name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Class,
        name: class_name.clone(),
        signature,
        body: body_text,
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language,
        references: vec![],
        imports: vec![],
    });

    // Extract methods from the class body
    if let Some(body_node) = node.child_by_field_name("body") {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            if child.kind() == "method_definition" {
                if let Some(method) = extract_method(child, source, file_path, language, &class_name) {
                    symbols.push(method);
                }
            }
        }
    }

    symbols
}

/// Extract a method from a class body.
fn extract_method(
    node: Node,
    source: &[u8],
    file_path: &str,
    language: Language,
    class_name: &str,
) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let method_name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = build_function_signature(node, source);
    let references = extract_references_from_body(node, source);

    let qualified_name = format!("{}.{}", class_name, method_name);

    Some(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: qualified_name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Method,
        name: qualified_name,
        signature,
        body: body_text,
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported: false,
        language,
        references,
        imports: vec![],
    })
}

/// Extract an interface declaration.
fn extract_interface(
    node: Node,
    source: &[u8],
    file_path: &str,
    language: Language,
    is_exported: bool,
) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = body_text.lines().next().unwrap_or("").trim().to_string();

    Some(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Interface,
        name,
        signature,
        body: body_text,
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language,
        references: vec![],
        imports: vec![],
    })
}

/// Extract a type alias declaration.
fn extract_type_alias(
    node: Node,
    source: &[u8],
    file_path: &str,
    language: Language,
    is_exported: bool,
) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = body_text.trim().to_string();

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
        language,
        references: vec![],
        imports: vec![],
    })
}

/// Extract an enum declaration.
fn extract_enum(
    node: Node,
    source: &[u8],
    file_path: &str,
    language: Language,
    is_exported: bool,
) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = body_text.lines().next().unwrap_or("").trim().to_string();

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
        language,
        references: vec![],
        imports: vec![],
    })
}

/// Extract variable declarations (const/let/var).
fn extract_variable(
    node: Node,
    source: &[u8],
    file_path: &str,
    language: Language,
    is_exported: bool,
) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, source);
                let body_text = node_text(node, source);
                let signature = body_text.trim().to_string();

                // Determine if it's const (Constant) or let/var (Variable)
                let kind = if node_text(node, source).starts_with("const") {
                    SymbolKind::Constant
                } else {
                    SymbolKind::Variable
                };

                symbols.push(Symbol {
                    id: SymbolId {
                        file: file_path.to_string(),
                        name: name.clone(),
                        byte_offset: child.start_byte(),
                    },
                    kind,
                    name,
                    signature,
                    body: body_text,
                    file: file_path.to_string(),
                    line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    is_exported,
                    language,
                    references: vec![],
                    imports: vec![],
                });
            }
        }
    }

    symbols
}

/// Extract an import statement into an ImportInfo.
fn extract_import(node: Node, source: &[u8]) -> Option<ImportInfo> {
    // Find the source/module path (the string literal at the end)
    let source_path = node.child_by_field_name("source")
        .map(|n| {
            let text = node_text(n, source);
            // Strip quotes from the string literal
            text.trim_matches(|c| c == '\'' || c == '"').to_string()
        })?;

    let mut names = Vec::new();
    let mut is_default = false;
    let mut is_wildcard = false;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import_clause" => {
                // Process the import clause children
                let mut clause_cursor = child.walk();
                for clause_child in child.children(&mut clause_cursor) {
                    match clause_child.kind() {
                        "identifier" => {
                            // Default import: import Foo from "..."
                            is_default = true;
                            names.push(node_text(clause_child, source));
                        }
                        "named_imports" => {
                            // Named imports: import { a, b } from "..."
                            let mut named_cursor = clause_child.walk();
                            for spec in clause_child.children(&mut named_cursor) {
                                if spec.kind() == "import_specifier" {
                                    // Could have alias: import { a as b }
                                    if let Some(name_node) = spec.child_by_field_name("name") {
                                        names.push(node_text(name_node, source));
                                    }
                                }
                            }
                        }
                        "namespace_import" => {
                            // Wildcard import: import * as foo from "..."
                            is_wildcard = true;
                            // Get the alias name after "as"
                            let mut ns_cursor = clause_child.walk();
                            for ns_child in clause_child.children(&mut ns_cursor) {
                                if ns_child.kind() == "identifier" {
                                    names.push(node_text(ns_child, source));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    Some(ImportInfo {
        source: source_path,
        names,
        is_default,
        is_wildcard,
    })
}

/// Extract references (identifiers from call expressions) from a function/method body.
fn extract_references_from_body(node: Node, source: &[u8]) -> Vec<String> {
    let mut refs = Vec::new();

    // Find the body node (statement_block)
    if let Some(body_node) = node.child_by_field_name("body") {
        collect_call_identifiers(body_node, source, &mut refs);
    }

    // Deduplicate
    refs.sort();
    refs.dedup();
    refs
}

/// Recursively collect identifiers from call expressions.
fn collect_call_identifiers(node: Node, source: &[u8], refs: &mut Vec<String>) {
    if node.kind() == "call_expression" {
        // Get the function being called
        if let Some(func_node) = node.child_by_field_name("function") {
            match func_node.kind() {
                "identifier" => {
                    refs.push(node_text(func_node, source));
                }
                "member_expression" => {
                    // For a.b(), collect "a" (the object)
                    if let Some(obj) = func_node.child_by_field_name("object") {
                        if obj.kind() == "identifier" {
                            refs.push(node_text(obj, source));
                        }
                    }
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
