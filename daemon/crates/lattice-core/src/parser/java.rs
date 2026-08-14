use tree_sitter::{Node, Parser};

use crate::error::LatticeError;
use crate::parser::complexity_profile::{
    enclosing_owner_name, node_text as profile_node_text, LanguageComplexityProfile,
    ProfileApplicability, UnitIdentity,
};
use crate::symbols::{ImportInfo, Language, ParsedFile, Symbol, SymbolId, SymbolKind};

/// Java complexity vocabulary, verified against `tree_sitter_java::LANGUAGE`.
///
/// Decision points: `if` (each `else if` is its own `if_statement`), `for`,
/// enhanced `for`, `while`, `do`, each `catch` clause, each non-default `switch`
/// label (the grammar uses `switch_label` for both the classic `case x:` and the
/// arrow `case x ->` forms), the ternary operator, and each `&&`/`||`.
/// `default:` labels and `finally` are unconditional and do not count.
static JAVA_COMPLEXITY_PROFILE: LanguageComplexityProfile = LanguageComplexityProfile {
    language: Language::Java,
    applicability: ProfileApplicability::Supported,
    function_kinds: &["method_declaration", "constructor_declaration"],
    branch_kinds: &[
        "if_statement",
        "for_statement",
        "enhanced_for_statement",
        "while_statement",
        "do_statement",
        "catch_clause",
        "switch_label",
        "ternary_expression",
    ],
    boolean_operator_parent_kinds: &["binary_expression"],
    boolean_operator_kinds: &["&&", "||"],
    guarded_kinds: &[],
    nesting_kinds: &[
        "if_statement",
        "for_statement",
        "enhanced_for_statement",
        "while_statement",
        "do_statement",
        "try_statement",
        "switch_expression",
    ],
    // Java attaches `else if` directly as the `alternative` field.
    nesting_transparent_parent_kinds: &[],
    nesting_transparent_fields: &["alternative"],
    parameter_list_field: "parameters",
    parameter_kinds: &["formal_parameter", "spread_parameter"],
    is_default_branch: Some(is_default_switch_label),
    count_parameters: None,
    unit_identity: Some(java_unit_identity),
};

/// The Java complexity profile contributed by this parser.
pub fn complexity_profile() -> &'static LanguageComplexityProfile {
    &JAVA_COMPLEXITY_PROFILE
}

/// A `default:` (or `default ->`) label is the structural fall-through of a
/// switch whose cases are already counted, so it is not a decision point.
fn is_default_switch_label(node: Node, source: &[u8]) -> bool {
    node.kind() == "switch_label" && profile_node_text(node, source).trim_start().starts_with("default")
}

/// Name methods `Owner.method`, exactly as [`extract_method`] names them.
fn java_unit_identity(node: Node, source: &[u8]) -> Option<UnitIdentity> {
    let name_node = node.child_by_field_name("name")?;
    let name = profile_node_text(name_node, source);

    let qualified = match enclosing_owner_name(
        node,
        source,
        &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
        ],
        "name",
    ) {
        Some(owner) => format!("{}.{}", owner, name),
        None => name,
    };

    Some(UnitIdentity {
        name: qualified,
        byte_offset: node.start_byte(),
    })
}

/// Parse a Java source file and extract symbols.
pub fn parse(file_path: &str, source: &str) -> Result<ParsedFile, LatticeError> {
    let mut parser = Parser::new();

    let ts_language = tree_sitter_java::LANGUAGE.into();

    parser
        .set_language(&ts_language)
        .map_err(|e| LatticeError::Parse {
            file: file_path.to_string(),
            message: format!("Failed to set Java language: {}", e),
        })?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| LatticeError::Parse {
            file: file_path.to_string(),
            message: "Failed to parse Java source".to_string(),
        })?;

    let root = tree.root_node();
    let source_bytes = source.as_bytes();

    let mut symbols = Vec::new();
    let mut imports = Vec::new();

    extract_from_node(root, source_bytes, file_path, &mut symbols, &mut imports);

    Ok(ParsedFile {
        file: file_path.to_string(),
        language: Language::Java,
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
            "class_declaration" => {
                let mut class_symbols = extract_class(child, source, file_path);
                symbols.append(&mut class_symbols);
            }
            "interface_declaration" => {
                let mut iface_symbols = extract_interface(child, source, file_path);
                symbols.append(&mut iface_symbols);
            }
            "enum_declaration" => {
                if let Some(sym) = extract_enum(child, source, file_path) {
                    symbols.push(sym);
                }
            }
            "import_declaration" => {
                if let Some(imp) = extract_import(child, source) {
                    imports.push(imp);
                }
            }
            _ => {}
        }
    }
}

/// Check if a node has a `public` modifier.
fn has_public_modifier(node: Node, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let mod_text = node_text(child, source);
            return mod_text.contains("public");
        }
    }
    false
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

/// Extract a Java class and its methods/fields.
fn extract_class(node: Node, source: &[u8], file_path: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => return symbols,
    };
    let class_name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = build_signature(node, source);
    let is_exported = has_public_modifier(node, source);

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
        language: Language::Java,
        references: vec![],
        imports: vec![],
    });

    // Extract methods and fields from the class body
    if let Some(body_node) = node.child_by_field_name("body") {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            match child.kind() {
                "method_declaration" | "constructor_declaration" => {
                    if let Some(sym) = extract_method(child, source, file_path, &class_name) {
                        symbols.push(sym);
                    }
                }
                "field_declaration" => {
                    let mut field_symbols = extract_field(child, source, file_path, &class_name);
                    symbols.append(&mut field_symbols);
                }
                _ => {}
            }
        }
    }

    symbols
}

/// Extract a Java interface and its methods.
fn extract_interface(node: Node, source: &[u8], file_path: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => return symbols,
    };
    let iface_name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = build_signature(node, source);
    let is_exported = has_public_modifier(node, source);

    symbols.push(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: iface_name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Interface,
        name: iface_name.clone(),
        signature,
        body: body_text,
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language: Language::Java,
        references: vec![],
        imports: vec![],
    });

    // Extract method signatures from the interface body
    if let Some(body_node) = node.child_by_field_name("body") {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            if child.kind() == "method_declaration" {
                if let Some(sym) = extract_method(child, source, file_path, &iface_name) {
                    symbols.push(sym);
                }
            }
        }
    }

    symbols
}

/// Extract a Java enum declaration.
fn extract_enum(node: Node, source: &[u8], file_path: &str) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = build_signature(node, source);
    let is_exported = has_public_modifier(node, source);

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
        language: Language::Java,
        references: vec![],
        imports: vec![],
    })
}

/// Extract a Java method from a class or interface body.
fn extract_method(node: Node, source: &[u8], file_path: &str, class_name: &str) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let method_name = node_text(name_node, source);
    let qualified_name = format!("{}.{}", class_name, method_name);
    let body_text = node_text(node, source);
    let signature = build_signature(node, source);
    let is_exported = has_public_modifier(node, source);
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
        language: Language::Java,
        references,
        imports: vec![],
    })
}

/// Extract Java field declarations.
fn extract_field(node: Node, source: &[u8], file_path: &str, class_name: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let field_name = node_text(name_node, source);
                let qualified_name = format!("{}.{}", class_name, field_name);
                let body_text = node_text(node, source);
                let signature = body_text.trim().to_string();
                let is_exported = has_public_modifier(node, source);

                symbols.push(Symbol {
                    id: SymbolId {
                        file: file_path.to_string(),
                        name: qualified_name.clone(),
                        byte_offset: child.start_byte(),
                    },
                    kind: SymbolKind::Variable,
                    name: qualified_name,
                    signature,
                    body: body_text.clone(),
                    file: file_path.to_string(),
                    line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    is_exported,
                    language: Language::Java,
                    references: vec![],
                    imports: vec![],
                });
            }
        }
    }

    symbols
}

/// Extract a Java import declaration.
fn extract_import(node: Node, source: &[u8]) -> Option<ImportInfo> {
    let full_text = node_text(node, source).trim().to_string();

    // Parse "import foo.bar.Baz;" or "import static foo.bar.Baz.method;"
    let path = full_text
        .strip_prefix("import ")?
        .trim_start_matches("static ")
        .trim_end_matches(';')
        .trim()
        .to_string();

    let leaf = path.rsplit('.').next().unwrap_or(&path).to_string();
    let is_wildcard = leaf == "*";

    Some(ImportInfo {
        source: path,
        names: vec![leaf],
        is_default: false,
        is_wildcard,
    })
}

/// Extract references from a method body (method invocation identifiers).
fn extract_references_from_body(node: Node, source: &[u8]) -> Vec<String> {
    let mut refs = Vec::new();

    if let Some(body_node) = node.child_by_field_name("body") {
        collect_call_identifiers(body_node, source, &mut refs);
    }

    refs.sort();
    refs.dedup();
    refs
}

/// Recursively collect identifiers from method invocations.
fn collect_call_identifiers(node: Node, source: &[u8], refs: &mut Vec<String>) {
    if node.kind() == "method_invocation" {
        if let Some(name_node) = node.child_by_field_name("name") {
            refs.push(node_text(name_node, source));
        }
        if let Some(obj_node) = node.child_by_field_name("object") {
            if obj_node.kind() == "identifier" {
                refs.push(node_text(obj_node, source));
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
