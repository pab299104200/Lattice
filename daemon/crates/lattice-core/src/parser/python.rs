use tree_sitter::{Node, Parser};

use crate::error::LatticeError;
use crate::parser::complexity_profile::{
    node_text as profile_node_text, LanguageComplexityProfile, ProfileApplicability, UnitIdentity,
};
use crate::symbols::{ImportInfo, Language, ParsedFile, Symbol, SymbolId, SymbolKind};

/// Python complexity vocabulary, verified against `tree_sitter_python::LANGUAGE`.
///
/// Decision points: `if`, each `elif`, `for`, `while`, each `except` handler,
/// each non-wildcard `match` case, conditional expressions, comprehension `if`
/// guards, and each `and`/`or` (the grammar models both as `boolean_operator`,
/// one node per operator). `else`/`finally` are unconditional and `case _:` is
/// the structural default, so neither counts.
static PYTHON_COMPLEXITY_PROFILE: LanguageComplexityProfile = LanguageComplexityProfile {
    language: Language::Python,
    applicability: ProfileApplicability::Supported,
    function_kinds: &["function_definition"],
    branch_kinds: &[
        "if_statement",
        "elif_clause",
        "for_statement",
        "while_statement",
        "except_clause",
        "except_group_clause",
        "conditional_expression",
        "case_clause",
        "boolean_operator",
        "if_clause",
    ],
    // `boolean_operator` is itself the decision point, so no operator-token scan.
    boolean_operator_parent_kinds: &[],
    boolean_operator_kinds: &[],
    guarded_kinds: &[],
    nesting_kinds: &[
        "if_statement",
        "for_statement",
        "while_statement",
        "try_statement",
        "with_statement",
        "match_statement",
    ],
    // `elif` is an `elif_clause` inside the same `if_statement`, so an elif chain
    // is already one level; no transparency rule is needed.
    nesting_transparent_parent_kinds: &[],
    nesting_transparent_fields: &[],
    parameter_list_field: "parameters",
    parameter_kinds: &[
        "identifier",
        "default_parameter",
        "typed_parameter",
        "typed_default_parameter",
        "list_splat_pattern",
        "dictionary_splat_pattern",
    ],
    is_default_branch: Some(is_wildcard_case_clause),
    count_parameters: Some(count_python_parameters),
    unit_identity: Some(python_unit_identity),
};

/// The Python complexity profile contributed by this parser.
pub fn complexity_profile() -> &'static LanguageComplexityProfile {
    &PYTHON_COMPLEXITY_PROFILE
}

/// `case _:` is the structural default of a `match` and is not a decision point.
fn is_wildcard_case_clause(node: Node, source: &[u8]) -> bool {
    if node.kind() != "case_clause" {
        return false;
    }
    let mut cursor = node.walk();
    let mut patterns = node
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "case_pattern");
    match patterns.next() {
        Some(pattern) => {
            patterns.next().is_none() && profile_node_text(pattern, source).trim() == "_"
        }
        None => false,
    }
}

/// Count declared parameters, excluding an implicit `self`/`cls` receiver.
///
/// The receiver is bound by the call protocol rather than declared by the
/// caller, so it is excluded exactly as Go method receivers and Rust `self` are.
fn count_python_parameters(node: Node, source: &[u8]) -> Option<u32> {
    let parameters = node.child_by_field_name("parameters")?;
    let mut cursor = parameters.walk();
    let declared: Vec<Node> = parameters
        .named_children(&mut cursor)
        .filter(|child| {
            PYTHON_COMPLEXITY_PROFILE
                .parameter_kinds
                .contains(&child.kind())
        })
        .collect();

    let is_method = nearest_owner(node, source).is_some();
    let skip_receiver = is_method
        && declared
            .first()
            .map(|first| {
                first.kind() == "identifier"
                    && matches!(profile_node_text(*first, source).trim(), "self" | "cls")
            })
            .unwrap_or(false);

    Some((declared.len() - usize::from(skip_receiver)) as u32)
}

/// Name methods `Class.method`, exactly as [`extract_method`] names them.
fn python_unit_identity(node: Node, source: &[u8]) -> Option<UnitIdentity> {
    let name_node = node.child_by_field_name("name")?;
    let name = profile_node_text(name_node, source);
    let qualified = match nearest_owner(node, source) {
        Some(class_name) => format!("{}.{}", class_name, name),
        None => name,
    };
    Some(UnitIdentity {
        name: qualified,
        byte_offset: node.start_byte(),
    })
}

/// The class a function is defined directly in, if any. A function nested inside
/// another function is not a method, so the search stops at a function boundary.
fn nearest_owner(node: Node, source: &[u8]) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "class_definition" => {
                return parent
                    .child_by_field_name("name")
                    .map(|name| profile_node_text(name, source));
            }
            "function_definition" => return None,
            _ => current = parent.parent(),
        }
    }
    None
}

/// Parse a Python source file and extract symbols.
pub fn parse(file_path: &str, source: &str) -> Result<ParsedFile, LatticeError> {
    let mut parser = Parser::new();

    let ts_language = tree_sitter_python::LANGUAGE.into();

    parser
        .set_language(&ts_language)
        .map_err(|e| LatticeError::Parse {
            file: file_path.to_string(),
            message: format!("Failed to set Python language: {}", e),
        })?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| LatticeError::Parse {
            file: file_path.to_string(),
            message: "Failed to parse Python source".to_string(),
        })?;

    let root = tree.root_node();
    let source_bytes = source.as_bytes();

    let mut symbols = Vec::new();
    let mut imports = Vec::new();

    extract_from_node(root, source_bytes, file_path, &mut symbols, &mut imports);

    Ok(ParsedFile {
        file: file_path.to_string(),
        language: Language::Python,
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
            "function_definition" => {
                if let Some(sym) = extract_function(child, source, file_path) {
                    symbols.push(sym);
                }
            }
            "class_definition" => {
                let mut class_symbols = extract_class(child, source, file_path);
                symbols.append(&mut class_symbols);
            }
            "decorated_definition" => {
                // A decorated definition wraps a function or class
                extract_decorated(child, source, file_path, symbols);
            }
            "import_statement" => {
                if let Some(imp) = extract_import(child, source) {
                    imports.push(imp);
                }
            }
            "import_from_statement" => {
                if let Some(imp) = extract_import_from(child, source) {
                    imports.push(imp);
                }
            }
            _ => {}
        }
    }
}

/// Extract a decorated definition (e.g., @staticmethod def ...).
fn extract_decorated(node: Node, source: &[u8], file_path: &str, symbols: &mut Vec<Symbol>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                if let Some(sym) = extract_function(child, source, file_path) {
                    symbols.push(sym);
                }
            }
            "class_definition" => {
                let mut class_symbols = extract_class(child, source, file_path);
                symbols.append(&mut class_symbols);
            }
            _ => {}
        }
    }
}

/// Extract a Python function definition.
fn extract_function(node: Node, source: &[u8], file_path: &str) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);
    let body_text = node_text(node, source);

    // Signature = first line of the def, strip trailing ":"
    let signature = build_python_signature(node, source);

    // is_exported: name doesn't start with '_'
    let is_exported = !name.starts_with('_');

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
        language: Language::Python,
        references,
        imports: vec![],
    })
}

/// Build a Python function signature: first line of def, strip trailing ':'
fn build_python_signature(node: Node, source: &[u8]) -> String {
    let full_text = node_text(node, source);
    let first_line = full_text.lines().next().unwrap_or("").trim();
    // Strip trailing colon
    first_line.trim_end_matches(':').trim().to_string()
}

/// Extract a Python class definition and its methods.
fn extract_class(node: Node, source: &[u8], file_path: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => return symbols,
    };
    let class_name = node_text(name_node, source);
    let body_text = node_text(node, source);

    let is_exported = !class_name.starts_with('_');

    // Signature: first line, strip trailing ':'
    let signature = body_text
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches(':')
        .trim()
        .to_string();

    // Extract references from class body (Column, relationship, ForeignKey calls, etc.)
    let class_refs = if let Some(body_node) = node.child_by_field_name("body") {
        let mut refs = Vec::new();
        collect_call_identifiers(body_node, source, &mut refs);
        collect_string_model_refs(body_node, source, &mut refs);
        refs.sort();
        refs.dedup();
        refs
    } else {
        vec![]
    };

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
        language: Language::Python,
        references: class_refs,
        imports: vec![],
    });

    // Extract methods from the class body
    if let Some(body_node) = node.child_by_field_name("body") {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            match child.kind() {
                "function_definition" => {
                    if let Some(method) = extract_method(child, source, file_path, &class_name) {
                        symbols.push(method);
                    }
                }
                "decorated_definition" => {
                    // Decorated methods in a class
                    let mut dec_cursor = child.walk();
                    for dec_child in child.children(&mut dec_cursor) {
                        if dec_child.kind() == "function_definition" {
                            if let Some(method) =
                                extract_method(dec_child, source, file_path, &class_name)
                            {
                                symbols.push(method);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    symbols
}

/// Extract a method from a class body.
fn extract_method(node: Node, source: &[u8], file_path: &str, class_name: &str) -> Option<Symbol> {
    let name_node = node.child_by_field_name("name")?;
    let method_name = node_text(name_node, source);
    let body_text = node_text(node, source);
    let signature = build_python_signature(node, source);
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
        language: Language::Python,
        references,
        imports: vec![],
    })
}

/// Extract a plain `import X` statement.
fn extract_import(node: Node, source: &[u8]) -> Option<ImportInfo> {
    // `import os` or `import os.path` or `import os, sys`
    let mut names = Vec::new();
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        if child.kind() == "dotted_name" {
            names.push(node_text(child, source));
        } else if child.kind() == "aliased_import" {
            // import os as operating_system
            if let Some(name_node) = child.child_by_field_name("name") {
                names.push(node_text(name_node, source));
            }
        }
    }

    if names.is_empty() {
        return None;
    }

    // For `import os`, the source is the module name itself
    let source_path = names[0].clone();

    Some(ImportInfo {
        source: source_path,
        names,
        is_default: false,
        is_wildcard: false,
    })
}

/// Extract a `from X import Y` statement.
fn extract_import_from(node: Node, source: &[u8]) -> Option<ImportInfo> {
    // Find the module name (after "from")
    let module_name = node
        .child_by_field_name("module_name")
        .map(|n| node_text(n, source));

    let source_path = module_name.unwrap_or_default();

    let mut names = Vec::new();
    let mut is_wildcard = false;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "dotted_name" => {
                // This could be the module name (handled above) or an imported name
                // Skip if it matches the source path
                let text = node_text(child, source);
                if text != source_path {
                    names.push(text);
                }
            }
            "aliased_import" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    names.push(node_text(name_node, source));
                }
            }
            "wildcard_import" => {
                is_wildcard = true;
                names.push("*".to_string());
            }
            "import_prefix" => {
                // relative import dots, skip
            }
            _ => {}
        }
    }

    Some(ImportInfo {
        source: source_path,
        names,
        is_default: false,
        is_wildcard,
    })
}

/// Extract references from a function/method body (call expression identifiers).
/// Also walks the parameters node to capture dependency-injected calls like
/// FastAPI's `Depends(get_current_user)` which appear as default values in
/// function parameters, not in the body.
fn extract_references_from_body(node: Node, source: &[u8]) -> Vec<String> {
    let mut refs = Vec::new();

    if let Some(body_node) = node.child_by_field_name("body") {
        collect_call_identifiers(body_node, source, &mut refs);
    }

    // Walk parameters to capture calls in default values (e.g., Depends(get_current_user)).
    if let Some(params_node) = node.child_by_field_name("parameters") {
        collect_call_identifiers(params_node, source, &mut refs);
    }

    refs.sort();
    refs.dedup();
    refs
}

/// Recursively collect identifiers from call expressions.
fn collect_call_identifiers(node: Node, source: &[u8], refs: &mut Vec<String>) {
    if node.kind() == "call" {
        if let Some(func_node) = node.child_by_field_name("function") {
            match func_node.kind() {
                "identifier" => {
                    refs.push(node_text(func_node, source));
                }
                "attribute" => {
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
        // Collect bare identifier arguments — function references passed as
        // callbacks. Depends(get_current_user) has get_current_user as a direct
        // identifier child of argument_list, not as a call expression.
        if let Some(args_node) = node.child_by_field_name("arguments") {
            let mut args_cursor = args_node.walk();
            for arg in args_node.children(&mut args_cursor) {
                if arg.kind() == "identifier" {
                    refs.push(node_text(arg, source));
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_call_identifiers(child, source, refs);
    }
}

/// Extract string literal arguments from framework calls like `relationship("Host")`
/// and `ForeignKey("hosts.id")`. These create model-to-model edges that static
/// analysis would otherwise miss.
fn collect_string_model_refs(node: Node, source: &[u8], refs: &mut Vec<String>) {
    if node.kind() == "call" {
        if let Some(func_node) = node.child_by_field_name("function") {
            let func_name = match func_node.kind() {
                "identifier" => node_text(func_node, source),
                "attribute" => {
                    // e.g., orm.relationship — use the attribute name
                    func_node
                        .child_by_field_name("attribute")
                        .map(|a| node_text(a, source))
                        .unwrap_or_default()
                }
                _ => String::new(),
            };

            if func_name == "relationship" || func_name == "ForeignKey" {
                // Extract the first string argument
                if let Some(args_node) = node.child_by_field_name("arguments") {
                    let mut args_cursor = args_node.walk();
                    for arg in args_node.children(&mut args_cursor) {
                        if arg.kind() == "string" {
                            let raw = node_text(arg, source);
                            // Strip quotes and extract model/table name
                            let cleaned = raw.trim_matches(|c| c == '"' || c == '\'');
                            if !cleaned.is_empty() {
                                // For ForeignKey("hosts.id"), extract "hosts"
                                // For relationship("Host"), use "Host" directly
                                let model_ref = if func_name == "ForeignKey" {
                                    cleaned.split('.').next().unwrap_or(cleaned).to_string()
                                } else {
                                    cleaned.to_string()
                                };
                                refs.push(model_ref);
                            }
                            break; // Only first string arg matters
                        }
                    }
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_string_model_refs(child, source, refs);
    }
}

/// Get the text of a tree-sitter node.
fn node_text(node: Node, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or("").to_string()
}
