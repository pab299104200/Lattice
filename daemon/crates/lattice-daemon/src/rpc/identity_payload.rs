use std::collections::HashMap;

use lattice_core::graph::model::{CodeGraph, GraphNode};
use lattice_core::identity::{
    serialize_outcome, AmbiguityReport, ContextHandleId, FileId, Identity, IdentityPayload,
    IdentityPayloadOrAmbiguity, ResolveError, ResolveOutcome, SymbolId,
};
use lattice_core::storage::FileIndexEntry;
use lattice_core::symbols::ParsedFile;
use serde_json::{json, Value};

const DEFAULT_PARSER_VERSION: i64 = 1;
const DEFAULT_SCHEMA_VERSION: i64 = 1;

pub fn decorate_get_context_capsule_payload(
    value: &mut Value,
    graph: &CodeGraph,
    parsed_files: &HashMap<String, ParsedFile>,
    workspace_id: &str,
    context_handle_id: &ContextHandleId,
) {
    let file_index = synthetic_file_index(parsed_files, graph);
    let nodes = graph.all_nodes();

    if let Some(object) = value.as_object_mut() {
        object.insert(
            "context_handle_identity".to_string(),
            json!(IdentityPayload::new(
                Identity::ContextHandle(context_handle_id.clone()),
                object
                    .get("context_handle")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            )),
        );
        decorate_capsule_entries(object.get_mut("pivots"), &nodes, &file_index, workspace_id);
        decorate_capsule_entries(object.get_mut("context"), &nodes, &file_index, workspace_id);
        decorate_seed_symbol_identities(object.get_mut("stats"), &nodes, &file_index, workspace_id);
    }
}

fn decorate_capsule_entries(
    entries: Option<&mut Value>,
    nodes: &[&GraphNode],
    file_index: &HashMap<String, FileIndexEntry>,
    workspace_id: &str,
) {
    let Some(entries) = entries.and_then(Value::as_array_mut) else {
        return;
    };

    for entry in entries {
        let Some(object) = entry.as_object_mut() else {
            continue;
        };
        let file = object
            .get("file")
            .and_then(Value::as_str)
            .map(str::to_string);
        let symbol = object
            .get("symbol")
            .and_then(Value::as_str)
            .map(str::to_string);
        let line = object
            .get("line")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or_default();

        if let Some(file) = file.as_deref() {
            object.insert(
                "file_identity".to_string(),
                json!(IdentityPayload::new(
                    Identity::File(file_identity(workspace_id, file, file_index)),
                    Some(file.to_string()),
                )),
            );
        }
        if let Some(symbol) = symbol.as_deref() {
            let payload = exact_symbol_payload(
                nodes,
                file_index,
                workspace_id,
                file.as_deref(),
                symbol,
                line,
            )
            .map(|payload| IdentityPayloadOrAmbiguity::Resolved { payload })
            .unwrap_or_else(|| {
                serialize_outcome(
                    resolve_symbol_outcome(nodes, file_index, workspace_id, symbol),
                    Some(symbol),
                )
            });
            object.insert("symbol_identity".to_string(), json!(payload));
        }
    }
}

fn decorate_seed_symbol_identities(
    stats: Option<&mut Value>,
    nodes: &[&GraphNode],
    file_index: &HashMap<String, FileIndexEntry>,
    workspace_id: &str,
) {
    let Some(stats) = stats.and_then(Value::as_object_mut) else {
        return;
    };
    let Some(seed_symbols) = stats.get("seed_symbols").and_then(Value::as_array) else {
        return;
    };

    let payloads = seed_symbols
        .iter()
        .filter_map(Value::as_str)
        .map(|symbol| {
            serialize_outcome(
                resolve_symbol_outcome(nodes, file_index, workspace_id, symbol),
                Some(symbol),
            )
        })
        .collect::<Vec<_>>();
    stats.insert("seed_symbol_identities".to_string(), json!(payloads));
}

fn exact_symbol_payload(
    nodes: &[&GraphNode],
    file_index: &HashMap<String, FileIndexEntry>,
    workspace_id: &str,
    file: Option<&str>,
    symbol: &str,
    line: usize,
) -> Option<IdentityPayload> {
    let node = nodes.iter().copied().find(|node| {
        file.is_some_and(|candidate| candidate == node.file)
            && node.name == symbol
            && node.line == line
    })?;
    let identity = Identity::Symbol(SymbolId {
        file: file_identity(workspace_id, &node.file, file_index),
        qualified_name: node.name.clone(),
        byte_offset: node.id.byte_offset,
        kind: format!("{:?}", node.kind).to_lowercase(),
    });
    Some(IdentityPayload::new(identity, Some(symbol.to_string())))
}

fn file_identity(
    workspace_id: &str,
    file: &str,
    file_index: &HashMap<String, FileIndexEntry>,
) -> FileId {
    FileId {
        workspace_id: workspace_id.to_string(),
        repo_relative_path: file.to_string(),
        content_hash: file_index
            .get(file)
            .map(|entry| entry.content_hash.clone())
            .unwrap_or_else(|| stable_content_hash(file.as_bytes())),
    }
}

fn resolve_symbol_outcome(
    nodes: &[&GraphNode],
    file_index: &HashMap<String, FileIndexEntry>,
    workspace_id: &str,
    symbol: &str,
) -> ResolveOutcome<SymbolId> {
    let candidates = nodes
        .iter()
        .copied()
        .filter(|node| node.name == symbol)
        .map(|node| SymbolId {
            file: file_identity(workspace_id, &node.file, file_index),
            qualified_name: node.name.clone(),
            byte_offset: node.id.byte_offset,
            kind: format!("{:?}", node.kind).to_lowercase(),
        })
        .collect::<Vec<_>>();
    match candidates.len() {
        0 => ResolveOutcome::NotFound(ResolveError::NotFound {
            kind: "symbol",
            query: symbol.to_string(),
        }),
        1 => ResolveOutcome::Unique(candidates[0].clone()),
        _ => ResolveOutcome::Ambiguous(AmbiguityReport::new(
            symbol,
            candidates,
            "Add the parent module or file path, or use the stable symbol identity.",
        )),
    }
}

fn synthetic_file_index(
    parsed_files: &HashMap<String, ParsedFile>,
    graph: &CodeGraph,
) -> HashMap<String, FileIndexEntry> {
    let mut entries = HashMap::new();
    for (file, parsed_file) in parsed_files {
        entries.insert(
            file.clone(),
            synthetic_file_index_entry(file, Some(parsed_file)),
        );
    }
    for node in graph.all_nodes() {
        entries
            .entry(node.file.clone())
            .or_insert_with(|| synthetic_file_index_entry(&node.file, None));
    }
    entries
}

fn synthetic_file_index_entry(file: &str, parsed_file: Option<&ParsedFile>) -> FileIndexEntry {
    let content_hash = parsed_file
        .and_then(|parsed| serde_json::to_vec(parsed).ok())
        .map(|bytes| stable_content_hash(&bytes))
        .unwrap_or_else(|| stable_content_hash(file.as_bytes()));
    FileIndexEntry {
        file: file.to_string(),
        content_hash,
        mtime_ns: 0,
        size_bytes: 0,
        parser_version: DEFAULT_PARSER_VERSION,
        schema_version: DEFAULT_SCHEMA_VERSION,
        last_indexed_at: 0,
    }
}

fn stable_content_hash(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use lattice_core::graph::model::CodeGraph;
    use lattice_core::query::{CapsuleStats, ContextCapsule, ContextNode, PivotNode, QueryIntent};
    use lattice_core::symbols::{
        Language, ParsedFile, Symbol, SymbolId as LegacySymbolId, SymbolKind,
    };
    use serde_json::json;

    use super::decorate_get_context_capsule_payload;
    use lattice_core::identity::ContextHandleId;

    #[test]
    fn full_tool_response_includes_identity_payloads_and_legacy_fields() {
        let (graph, parsed_files) = fixture(false);
        let mut value = serde_json::to_value(ContextCapsule {
            query: "login auth".to_string(),
            intent: QueryIntent::Explore,
            pivots: vec![PivotNode {
                symbol: "loginUser".to_string(),
                kind: "function".to_string(),
                file: "src/auth.ts".to_string(),
                line: 4,
                source: "function loginUser() {}".to_string(),
                score: 0.9,
                reason: "Lead auth pivot".to_string(),
            }],
            context: vec![ContextNode {
                symbol: "hashPassword".to_string(),
                kind: "function".to_string(),
                file: "src/auth.ts".to_string(),
                line: 10,
                skeleton: "function hashPassword(password)".to_string(),
                relationship: "callee".to_string(),
                score: 0.6,
            }],
            memories: Vec::new(),
            stats: CapsuleStats {
                tokens_used: 40,
                tokens_saved: 10,
                nodes_evaluated: 2,
                nodes_included: 2,
                engine_version: "test".to_string(),
                seed_count: 1,
                seed_symbols: vec!["loginUser".to_string()],
            },
        })
        .expect("serialize capsule");
        if let Some(object) = value.as_object_mut() {
            object.insert("context_handle".to_string(), json!("ctx-123"));
        }

        decorate_get_context_capsule_payload(
            &mut value,
            &graph,
            &parsed_files,
            "workspace-main",
            &ContextHandleId {
                workspace_id: "workspace-main".to_string(),
                session_id: "session-main".to_string(),
                ulid: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            },
        );

        assert_eq!(value["pivots"][0]["symbol"].as_str(), Some("loginUser"));
        assert_eq!(value["pivots"][0]["file"].as_str(), Some("src/auth.ts"));
        assert_eq!(
            value["pivots"][0]["symbol_identity"]["status"].as_str(),
            Some("resolved")
        );
        assert_eq!(
            value["pivots"][0]["symbol_identity"]["payload"]["legacy_name"].as_str(),
            Some("loginUser")
        );
        assert_eq!(
            value["pivots"][0]["file_identity"]["legacy_name"].as_str(),
            Some("src/auth.ts")
        );
        assert_eq!(
            value["context_handle_identity"]["fields"]["session_id"].as_str(),
            Some("session-main")
        );
        assert_eq!(
            value["stats"]["seed_symbol_identities"][0]["status"].as_str(),
            Some("resolved")
        );
    }

    #[test]
    fn ambiguous_resolver_outcome_surfaces_diagnostic() {
        let (graph, parsed_files) = fixture(true);
        let mut value = json!({
            "query": "login auth",
            "intent": "Explore",
            "pivots": [],
            "context": [],
            "memories": [],
            "stats": {
                "tokens_used": 10,
                "tokens_saved": 5,
                "nodes_evaluated": 2,
                "nodes_included": 0,
                "engine_version": "test",
                "seed_count": 1,
                "seed_symbols": ["loginUser"]
            },
            "context_handle": "ctx-123"
        });

        decorate_get_context_capsule_payload(
            &mut value,
            &graph,
            &parsed_files,
            "workspace-main",
            &ContextHandleId {
                workspace_id: "workspace-main".to_string(),
                session_id: "session-main".to_string(),
                ulid: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            },
        );

        assert_eq!(
            value["stats"]["seed_symbol_identities"][0]["status"].as_str(),
            Some("ambiguous")
        );
        assert_eq!(
            value["stats"]["seed_symbol_identities"][0]["query"].as_str(),
            Some("loginUser")
        );
        assert!(
            value["stats"]["seed_symbol_identities"][0]["disambiguation_hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("file path"))
        );
    }

    fn fixture(include_duplicate_symbol: bool) -> (CodeGraph, HashMap<String, ParsedFile>) {
        let mut graph = CodeGraph::new();
        let primary_symbol = LegacySymbolId {
            file: "src/auth.ts".to_string(),
            name: "loginUser".to_string(),
            byte_offset: 12,
        };
        graph.add_node(
            primary_symbol.clone(),
            SymbolKind::Function,
            "loginUser".to_string(),
            "function loginUser()".to_string(),
            "function loginUser() {}".to_string(),
            "src/auth.ts".to_string(),
            4,
            8,
            true,
            Language::TypeScript,
        );
        graph.add_node(
            LegacySymbolId {
                file: "src/auth.ts".to_string(),
                name: "hashPassword".to_string(),
                byte_offset: 44,
            },
            SymbolKind::Function,
            "hashPassword".to_string(),
            "function hashPassword()".to_string(),
            "function hashPassword() {}".to_string(),
            "src/auth.ts".to_string(),
            10,
            14,
            true,
            Language::TypeScript,
        );
        let mut parsed_files = HashMap::from([(
            "src/auth.ts".to_string(),
            ParsedFile {
                file: "src/auth.ts".to_string(),
                language: Language::TypeScript,
                symbols: vec![
                    Symbol {
                        id: primary_symbol,
                        kind: SymbolKind::Function,
                        name: "loginUser".to_string(),
                        signature: "function loginUser()".to_string(),
                        body: "function loginUser() {}".to_string(),
                        file: "src/auth.ts".to_string(),
                        line: 4,
                        end_line: 8,
                        is_exported: true,
                        language: Language::TypeScript,
                        references: Vec::new(),
                        imports: Vec::new(),
                    },
                    Symbol {
                        id: LegacySymbolId {
                            file: "src/auth.ts".to_string(),
                            name: "hashPassword".to_string(),
                            byte_offset: 44,
                        },
                        kind: SymbolKind::Function,
                        name: "hashPassword".to_string(),
                        signature: "function hashPassword()".to_string(),
                        body: "function hashPassword() {}".to_string(),
                        file: "src/auth.ts".to_string(),
                        line: 10,
                        end_line: 14,
                        is_exported: true,
                        language: Language::TypeScript,
                        references: Vec::new(),
                        imports: Vec::new(),
                    },
                ],
                imports: Vec::new(),
                links: Vec::new(),
            },
        )]);

        if include_duplicate_symbol {
            let duplicate_symbol = LegacySymbolId {
                file: "src/alt_auth.ts".to_string(),
                name: "loginUser".to_string(),
                byte_offset: 18,
            };
            graph.add_node(
                duplicate_symbol.clone(),
                SymbolKind::Function,
                "loginUser".to_string(),
                "function loginUser()".to_string(),
                "function loginUser() {}".to_string(),
                "src/alt_auth.ts".to_string(),
                4,
                8,
                true,
                Language::TypeScript,
            );
            parsed_files.insert(
                "src/alt_auth.ts".to_string(),
                ParsedFile {
                    file: "src/alt_auth.ts".to_string(),
                    language: Language::TypeScript,
                    symbols: vec![Symbol {
                        id: duplicate_symbol,
                        kind: SymbolKind::Function,
                        name: "loginUser".to_string(),
                        signature: "function loginUser()".to_string(),
                        body: "function loginUser() {}".to_string(),
                        file: "src/alt_auth.ts".to_string(),
                        line: 4,
                        end_line: 8,
                        is_exported: true,
                        language: Language::TypeScript,
                        references: Vec::new(),
                        imports: Vec::new(),
                    }],
                    imports: Vec::new(),
                    links: Vec::new(),
                },
            );
        }

        (graph, parsed_files)
    }
}
