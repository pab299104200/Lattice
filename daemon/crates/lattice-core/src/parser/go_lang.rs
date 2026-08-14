use tree_sitter::{Node, Parser};

use crate::error::LatticeError;
use crate::parser::complexity_profile::{
    node_text as profile_node_text, LanguageComplexityProfile, ProfileApplicability, UnitIdentity,
};
use crate::symbols::{ImportInfo, Language, ParsedFile, Symbol, SymbolId, SymbolKind};

/// Go complexity vocabulary, verified against `tree_sitter_go::LANGUAGE`.
///
/// Decision points: `if` (each `else if` is its own `if_statement`), `for`
/// (including `range` loops), each `case` of an expression switch, type switch
/// or `select`, and each `&&`/`||`. `default_case` is the structural fall-through
/// of a switch already counted by its own cases, so it does not count.
static GO_COMPLEXITY_PROFILE: LanguageComplexityProfile = LanguageComplexityProfile {
    language: Language::Go,
    applicability: ProfileApplicability::Supported,
    function_kinds: &["function_declaration", "method_declaration"],
    branch_kinds: &[
        "if_statement",
        "for_statement",
        "expression_case",
        "type_case",
        "communication_case",
    ],
    boolean_operator_parent_kinds: &["binary_expression"],
    boolean_operator_kinds: &["&&", "||"],
    guarded_kinds: &[],
    nesting_kinds: &[
        "if_statement",
        "for_statement",
        "expression_switch_statement",
        "type_switch_statement",
        "select_statement",
    ],
    // Go attaches `else if` directly as the `alternative` field of the outer
    // `if_statement`, so the field rule keeps a chain at one level.
    nesting_transparent_parent_kinds: &[],
    nesting_transparent_fields: &["alternative"],
    parameter_list_field: "parameters",
    parameter_kinds: &["parameter_declaration", "variadic_parameter_declaration"],
    is_default_branch: None,
    count_parameters: Some(count_go_parameters),
    unit_identity: Some(go_unit_identity),
};

/// The Go complexity profile contributed by this parser.
pub fn complexity_profile() -> &'static LanguageComplexityProfile {
    &GO_COMPLEXITY_PROFILE
}

/// Count declared parameters, expanding grouped declarations (`b, c string`
/// declares two) and excluding the method receiver, which the grammar keeps in
/// its own `receiver` field.
fn count_go_parameters(node: Node, _source: &[u8]) -> Option<u32> {
    let parameters = node.child_by_field_name("parameters")?;
    let mut cursor = parameters.walk();
    let declarations: Vec<Node> = parameters
        .named_children(&mut cursor)
        .filter(|child| {
            GO_COMPLEXITY_PROFILE
                .parameter_kinds
                .contains(&child.kind())
        })
        .collect();

    let mut total = 0u32;
    for declaration in declarations {
        let mut name_cursor = declaration.walk();
        let names = declaration
            .children_by_field_name("name", &mut name_cursor)
            .count() as u32;
        // An unnamed parameter (`func(int)`) still declares one argument.
        total += names.max(1);
    }
    Some(total)
}

/// Name methods `Receiver.method`, exactly as [`extract_method`] names them.
fn go_unit_identity(node: Node, source: &[u8]) -> Option<UnitIdentity> {
    let name_node = node.child_by_field_name("name")?;
    let name = profile_node_text(name_node, source);

    let qualified = match receiver_type_name(node, source) {
        Some(receiver) if !receiver.is_empty() => format!("{}.{}", receiver, name),
        _ => name,
    };

    Some(UnitIdentity {
        name: qualified,
        byte_offset: node.start_byte(),
    })
}

/// Receiver type of a method declaration, with any pointer prefix stripped.
fn receiver_type_name(node: Node, source: &[u8]) -> Option<String> {
    let receiver = node.child_by_field_name("receiver")?;
    let mut cursor = receiver.walk();
    for child in receiver.children(&mut cursor) {
        if child.kind() == "parameter_declaration" {
            if let Some(type_node) = child.child_by_field_name("type") {
                return Some(
                    profile_node_text(type_node, source)
                        .trim_start_matches('*')
                        .to_string(),
                );
            }
        }
    }
    None
}

/// Parse a Go source file and extract symbols.
pub fn parse(file_path: &str, source: &str) -> Result<ParsedFile, LatticeError> {
    let mut parser = Parser::new();

    let ts_language = tree_sitter_go::LANGUAGE.into();

    parser
        .set_language(&ts_language)
        .map_err(|e| LatticeError::Parse {
            file: file_path.to_string(),
            message: format!("Failed to set Go language: {}", e),
        })?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| LatticeError::Parse {
            file: file_path.to_string(),
            message: "Failed to parse Go source".to_string(),
        })?;

    let root = tree.root_node();
    let source_bytes = source.as_bytes();

    let mut symbols = Vec::new();
    let mut imports = Vec::new();

    extract_from_node(root, source_bytes, file_path, &mut symbols, &mut imports);

    Ok(ParsedFile {
        file: file_path.to_string(),
        language: Language::Go,
        symbols,
        imports,
        links: vec![],
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
            "function_declaration" => {
                if let Some(sym) = extract_function(child, source, file_path) {
                    symbols.push(sym);
                }
            }
            "method_declaration" => {
                if let Some(sym) = extract_method(child, source, file_path) {
                    symbols.push(sym);
                }
            }
            "type_declaration" => {
                let mut type_symbols = extract_type_declaration(child, source, file_path);
                symbols.append(&mut type_symbols);
            }
            "const_declaration" => {
                let mut const_symbols =
                    extract_const_or_var(child, source, file_path, SymbolKind::Constant);
                symbols.append(&mut const_symbols);
            }
            "var_declaration" => {
                let mut var_symbols =
                    extract_const_or_var(child, source, file_path, SymbolKind::Variable);
                symbols.append(&mut var_symbols);
            }
            "import_declaration" => {
                let mut import_list = extract_imports(child, source);
                imports.append(&mut import_list);
            }
            _ => {}
        }
    }
}

/// Check if a Go identifier is exported (first letter is uppercase).
fn is_exported_go(name: &str) -> bool {
    name.chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
}

/// Build a signature: everything before the body block `{`.
fn build_signature(node: Node, source: &[u8]) -> String {
    let full_text = node_text(node, source);
    if let Some(brace_pos) = full_text.find('{') {
        full_text[..brace_pos].trim().to_string()
    } else {
        full_text.lines().next().unwrap_or("").trim().to_string()
    }
}

/// Extract a Go function declaration.
fn extract_function(node: Node, source: &[u8], file_path: &str) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = build_signature(node, source);
    let is_exported = is_exported_go(&name);
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
        language: Language::Go,
        references,
        imports: vec![],
    })
}

/// Extract a Go method declaration.
fn extract_method(node: Node, source: &[u8], file_path: &str) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let method_name = node_text(name_node, source);

    // Get the receiver type name
    let receiver_name = node
        .child_by_field_name("receiver")
        .and_then(|recv| {
            // The receiver is a parameter_list; find the type inside
            let mut cursor = recv.walk();
            for child in recv.children(&mut cursor) {
                if child.kind() == "parameter_declaration" {
                    // Get the type from the parameter
                    if let Some(type_node) = child.child_by_field_name("type") {
                        let type_text = node_text(type_node, source);
                        // Strip pointer prefix
                        return Some(type_text.trim_start_matches('*').to_string());
                    }
                }
            }
            None
        })
        .unwrap_or_default();

    let qualified_name = if receiver_name.is_empty() {
        method_name.clone()
    } else {
        format!("{}.{}", receiver_name, method_name)
    };

    let body_text = node_text(node, source);
    let signature = build_signature(node, source);
    let is_exported = is_exported_go(&method_name);
    let references = extract_references_from_body(node, source);

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
        is_exported,
        language: Language::Go,
        references,
        imports: vec![],
    })
}

/// Extract type declarations (struct, interface).
fn extract_type_declaration(node: Node, source: &[u8], file_path: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "type_spec" {
            if let Some(sym) = extract_type_spec(child, source, file_path) {
                symbols.push(sym);
            }
        }
    }

    symbols
}

/// Extract a single type spec (struct_type, interface_type, etc.).
fn extract_type_spec(node: Node, source: &[u8], file_path: &str) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let is_exported = is_exported_go(&name);

    // Determine the kind based on the type child
    let type_node = node.child_by_field_name("type")?;
    let kind = match type_node.kind() {
        "struct_type" => SymbolKind::Struct,
        "interface_type" => SymbolKind::Interface,
        _ => SymbolKind::TypeAlias,
    };

    let signature = build_signature(node, source);

    Some(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind,
        name,
        signature,
        body: body_text,
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language: Language::Go,
        references: vec![],
        imports: vec![],
    })
}

/// Extract const or var declarations.
fn extract_const_or_var(
    node: Node,
    source: &[u8],
    file_path: &str,
    kind: SymbolKind,
) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "const_spec" || child.kind() == "var_spec" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, source);
                let body_text = node_text(child, source);
                let signature = body_text.trim().to_string();
                let is_exported = is_exported_go(&name);

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
                    line: child.start_position().row + 1,
                    end_line: child.end_position().row + 1,
                    is_exported,
                    language: Language::Go,
                    references: vec![],
                    imports: vec![],
                });
            }
        }
    }

    symbols
}

/// Extract import declarations.
fn extract_imports(node: Node, source: &[u8]) -> Vec<ImportInfo> {
    let mut imports = Vec::new();

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "import_spec" {
            let path_text = child.child_by_field_name("path").map(|n| {
                let text = node_text(n, source);
                text.trim_matches('"').to_string()
            });

            if let Some(path) = path_text {
                let leaf = path.rsplit('/').next().unwrap_or(&path).to_string();
                let name = child
                    .child_by_field_name("name")
                    .map(|n| node_text(n, source))
                    .unwrap_or_else(|| leaf.clone());

                imports.push(ImportInfo {
                    source: path,
                    names: vec![name],
                    is_default: false,
                    is_wildcard: false,
                });
            }
        } else if child.kind() == "import_spec_list" {
            // Grouped imports: import ( ... )
            let mut list_cursor = child.walk();
            for spec in child.children(&mut list_cursor) {
                if spec.kind() == "import_spec" {
                    let path_text = spec.child_by_field_name("path").map(|n| {
                        let text = node_text(n, source);
                        text.trim_matches('"').to_string()
                    });

                    if let Some(path) = path_text {
                        let leaf = path.rsplit('/').next().unwrap_or(&path).to_string();
                        let name = spec
                            .child_by_field_name("name")
                            .map(|n| node_text(n, source))
                            .unwrap_or_else(|| leaf.clone());

                        imports.push(ImportInfo {
                            source: path,
                            names: vec![name],
                            is_default: false,
                            is_wildcard: false,
                        });
                    }
                }
            }
        }
    }

    imports
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
                "selector_expression" => {
                    // For pkg.Func(), collect the identifier
                    if let Some(field) = func_node.child_by_field_name("field") {
                        refs.push(node_text(field, source));
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
