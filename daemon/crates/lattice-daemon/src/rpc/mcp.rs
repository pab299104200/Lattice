use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;
use serde_json::{json, Value};

use lattice_core::embeddings::EmbeddingEngine;
use lattice_core::indexer::Indexer;
use lattice_core::memory::{Memory, MemoryType, MemoryStore};
use lattice_core::query::QueryEngine;
use lattice_core::storage::GraphStore;
use lattice_core::security::SecurityFilter;
use lattice_core::watcher::should_index_file;
use lattice_core::workspace::WorkspaceManager;

use super::server::RequestHandler;

/// MCP (Model Context Protocol) handler that routes JSON-RPC methods
/// to the appropriate tool implementations.
pub struct McpHandler {
    engine: Arc<Mutex<QueryEngine>>,
    indexer: Arc<Mutex<Indexer>>,
    memory_store: Arc<Mutex<MemoryStore>>,
    graph_store: Arc<Mutex<GraphStore>>,
    embedding_engine: Arc<OnceLock<Arc<EmbeddingEngine>>>,
    workspace_root: PathBuf,
    session_id: String,
    #[allow(dead_code)]
    workspace_manager: Option<Arc<Mutex<WorkspaceManager>>>,
    workspace_roots: Vec<PathBuf>,
    indexing: Arc<AtomicBool>,
}

impl McpHandler {
    /// Create a new McpHandler with all shared state.
    pub fn new(
        engine: Arc<Mutex<QueryEngine>>,
        indexer: Arc<Mutex<Indexer>>,
        memory_store: Arc<Mutex<MemoryStore>>,
        graph_store: Arc<Mutex<GraphStore>>,
        embedding_engine: Arc<OnceLock<Arc<EmbeddingEngine>>>,
        workspace_root: PathBuf,
        session_id: String,
        workspace_manager: Option<Arc<Mutex<WorkspaceManager>>>,
        workspace_roots: Vec<PathBuf>,
        indexing: Arc<AtomicBool>,
    ) -> Self {
        Self {
            engine,
            indexer,
            memory_store,
            graph_store,
            embedding_engine,
            workspace_root,
            session_id,
            workspace_manager,
            workspace_roots,
            indexing,
        }
    }

    // ── MCP Protocol Methods ──────────────────────────────────────────

    fn handle_initialize(&self) -> Value {
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {
                    "listChanged": false
                }
            },
            "serverInfo": {
                "name": "lattice",
                "version": env!("CARGO_PKG_VERSION")
            }
        })
    }

    fn handle_tools_list(&self) -> Value {
        json!({
            "tools": [
                {
                    "name": "get_context_capsule",
                    "description": "Most relevant code for your task — returns pivots (full source) and context nodes (signatures).",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Natural language query describing what you need context for"
                            },
                            "mode": {
                                "type": "string",
                                "description": "Result mode: 'full' (default, multiple pivots + context) or 'focused' (max 1 pivot, max 5 context, minimal budget)",
                                "enum": ["full", "focused"],
                                "default": "full"
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "get_symbol",
                    "description": "Get detailed information about a specific symbol by name and file.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Symbol name"
                            },
                            "file": {
                                "type": "string",
                                "description": "File path containing the symbol"
                            },
                            "detail": {
                                "type": "string",
                                "description": "Detail level: 'summary' (default) or 'full' (includes source, end_line, is_exported, dep lists)",
                                "enum": ["summary", "full"],
                                "default": "summary"
                            }
                        },
                        "required": ["name", "file"]
                    }
                },
                {
                    "name": "get_dependents",
                    "description": "Get all symbols that depend on the given symbol (incoming edges).",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Symbol name"
                            },
                            "file": {
                                "type": "string",
                                "description": "File path containing the symbol"
                            }
                        },
                        "required": ["name", "file"]
                    }
                },
                {
                    "name": "get_dependencies",
                    "description": "Get all symbols that the given symbol depends on (outgoing edges).",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Symbol name"
                            },
                            "file": {
                                "type": "string",
                                "description": "File path containing the symbol"
                            }
                        },
                        "required": ["name", "file"]
                    }
                },
                {
                    "name": "get_impact_graph",
                    "description": "What breaks if a symbol changes — all transitive dependents up to N hops.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Symbol name"
                            },
                            "file": {
                                "type": "string",
                                "description": "File path containing the symbol"
                            },
                            "hops": {
                                "type": "integer",
                                "description": "Number of hops to traverse (default: 3)",
                                "default": 3
                            }
                        },
                        "required": ["name", "file"]
                    }
                },
                {
                    "name": "search_symbols",
                    "description": "Search for symbols by name pattern across the code graph.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "pattern": {
                                "type": "string",
                                "description": "Substring to search for in symbol names (case-insensitive)"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of results (default: 20)",
                                "default": 20
                            },
                            "detail": {
                                "type": "string",
                                "description": "Detail level: 'summary' (default) or 'full' (adds kind, exported, signature)",
                                "enum": ["summary", "full"],
                                "default": "summary"
                            }
                        },
                        "required": ["pattern"]
                    }
                },
                {
                    "name": "get_skeleton",
                    "description": "Token-efficient file structure view — symbols, kinds, and dependent counts.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "file": {
                                "type": "string",
                                "description": "File path to get context for"
                            }
                        },
                        "required": ["file"]
                    }
                },
                {
                    "name": "save_observation",
                    "description": "Store an observation, decision, or pattern for later recall.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "The memory content to store"
                            },
                            "memory_type": {
                                "type": "string",
                                "description": "Type of memory: observation, decision, exploration, pattern, or anti_pattern (default: observation)",
                                "enum": ["observation", "decision", "exploration", "pattern", "anti_pattern"]
                            },
                            "linked_symbols": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Symbol names this memory is linked to"
                            }
                        },
                        "required": ["content"]
                    }
                },
                {
                    "name": "get_session_context",
                    "description": "Get memories from the current session plus relevant memories from previous sessions.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Optional query to filter memories"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of memories to return (default: 20)",
                                "default": 20
                            }
                        },
                        "required": []
                    }
                },
                {
                    "name": "search_memory",
                    "description": "Search all sessions for memories matching a query.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Query to search memories with"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of memories to return (default: 10)",
                                "default": 10
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "search_logic_flow",
                    "description": "Execution paths between functions — finds call chains from one symbol to another.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "from": {
                                "type": "string",
                                "description": "Source symbol name"
                            },
                            "to": {
                                "type": "string",
                                "description": "Target symbol name"
                            },
                            "from_file": {
                                "type": "string",
                                "description": "Optional file path to disambiguate source symbol"
                            },
                            "to_file": {
                                "type": "string",
                                "description": "Optional file path to disambiguate target symbol"
                            },
                            "max_depth": {
                                "type": "integer",
                                "description": "Maximum path depth (default: 5)",
                                "default": 5
                            }
                        },
                        "required": ["from", "to"]
                    }
                },
                {
                    "name": "submit_lsp_edges",
                    "description": "Submit high-confidence edges from LSP call hierarchy to enrich the graph.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "edges": {
                                "type": "array",
                                "description": "Array of edges to add",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "from_name": { "type": "string" },
                                        "from_file": { "type": "string" },
                                        "to_name": { "type": "string" },
                                        "to_file": { "type": "string" },
                                        "kind": { "type": "string", "default": "Calls" }
                                    },
                                    "required": ["from_name", "from_file", "to_name", "to_file"]
                                }
                            }
                        },
                        "required": ["edges"]
                    }
                },
                {
                    "name": "workspace_setup",
                    "description": "Get workspace conventions, language breakdown, and recommended configuration.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "format": {
                                "type": "string",
                                "description": "Output format: 'json' or 'markdown' (default: 'markdown')",
                                "enum": ["json", "markdown"],
                                "default": "markdown"
                            }
                        },
                        "required": []
                    }
                },
                {
                    "name": "index_status",
                    "description": "Get current indexing status, graph stats, and language breakdown.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {},
                        "required": []
                    }
                },
                {
                    "name": "get_project_rules",
                    "description": "Get project-specific rules and conventions.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {},
                        "required": []
                    }
                },
                {
                    "name": "list_observations",
                    "description": "List stored observations and memories. Returns all non-invalidated memories, newest first.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session_id": {
                                "type": "string",
                                "description": "Optional: filter to memories from this session only"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of results to return (default: 50, max: 200)",
                                "default": 50
                            }
                        },
                        "required": []
                    }
                },
                {
                    "name": "delete_observation",
                    "description": "Delete a stored observation by ID. The memory will no longer appear in listings or search results.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "The memory ID to delete (from list_observations or save_observation response)"
                            }
                        },
                        "required": ["id"]
                    }
                },
                {
                    "name": "update_observation",
                    "description": "Update the content of an existing observation in-place. Clears stale flags.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "The memory ID to update"
                            },
                            "content": {
                                "type": "string",
                                "description": "The new content for this observation"
                            }
                        },
                        "required": ["id", "content"]
                    }
                }
            ]
        })
    }

    async fn handle_tools_call(&self, params: &Value) -> Result<Value, (i32, String)> {
        let tool_name = params["name"]
            .as_str()
            .ok_or((-32602, "Missing tool name".to_string()))?;
        let arguments = &params["arguments"];

        match tool_name {
            "get_context_capsule" | "query_context" => self.tool_query_context(arguments).await,
            "get_symbol" => self.tool_get_symbol(arguments).await,
            "get_dependents" => self.tool_get_dependents(arguments).await,
            "get_dependencies" => self.tool_get_dependencies(arguments).await,
            "get_impact_graph" | "blast_radius" => self.tool_blast_radius(arguments).await,
            "search_symbols" => self.tool_search_symbols(arguments).await,
            "get_skeleton" | "get_file_context" => self.tool_get_file_context(arguments).await,
            "save_observation" | "store_memory" => self.tool_store_memory(arguments).await,
            "get_session_context" => self.tool_get_session_context(arguments).await,
            "search_memory" | "recall_memories" => self.tool_search_memory(arguments).await,
            "list_observations" => self.tool_list_observations(arguments).await,
            "delete_observation" => self.tool_delete_observation(arguments).await,
            "update_observation" => self.tool_update_observation(arguments).await,
            "search_logic_flow" => self.tool_search_logic_flow(arguments).await,
            "submit_lsp_edges" => self.tool_submit_lsp_edges(arguments).await,
            "workspace_setup" => self.tool_workspace_setup(arguments).await,
            "index_status" => self.tool_index_status(arguments).await,
            "get_project_rules" => self.tool_get_project_rules(arguments).await,
            _ => Err((-32602, format!("Unknown tool: {}", tool_name))),
        }
    }

    // ── Tool Implementations ──────────────────────────────────────────

    async fn tool_query_context(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: query".to_string()))?;
        let focused = args["mode"].as_str().unwrap_or("full") == "focused";

        // Embed query text if embedding engine is available (graceful fallback to keyword)
        let embedding = self.embedding_engine.get()
            .and_then(|eng| eng.embed(query).ok());

        let mut engine = self.engine.lock().await;
        let capsule = engine.query(query, embedding.as_deref(), focused);
        serde_json::to_value(&capsule)
            .map(|v| wrap_tool_result(v))
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))
    }

    async fn tool_get_symbol(&self, args: &Value) -> Result<Value, (i32, String)> {
        let name = args["name"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: name".to_string()))?;
        let file = args["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;
        let detail = args["detail"].as_str().unwrap_or("summary");

        let engine = self.engine.lock().await;

        let node = engine.graph().all_nodes().into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let dependents = engine.graph().get_dependents(&n.id);
                let dependencies = engine.graph().get_dependencies(&n.id);

                let mut result = json!({
                    "symbol": n.name,
                    "kind": n.kind.short_code(),
                    "file": n.file,
                    "line": n.line,
                    "signature": n.signature,
                    "dependents": dependents.len(),
                    "dependencies": dependencies.len()
                });

                if detail == "full" {
                    if let Some(obj) = result.as_object_mut() {
                        obj.insert("source".to_string(), json!(n.body));
                        obj.insert("end_line".to_string(), json!(n.end_line));
                        obj.insert("is_exported".to_string(), json!(n.is_exported));
                        obj.insert("dep_list".to_string(), json!(
                            dependents.iter().map(|(dep, edge)| json!({
                                "s": dep.name, "f": dep.file, "e": edge.short_code()
                            })).collect::<Vec<_>>()
                        ));
                        obj.insert("deps_list".to_string(), json!(
                            dependencies.iter().map(|(dep, edge)| json!({
                                "s": dep.name, "f": dep.file, "e": edge.short_code()
                            })).collect::<Vec<_>>()
                        ));
                    }
                }

                Ok(wrap_tool_result(result))
            }
            None => Ok(wrap_tool_result(json!({
                "error": format!("Symbol '{}' not found in '{}'", name, file)
            }))),
        }
    }

    async fn tool_get_dependents(&self, args: &Value) -> Result<Value, (i32, String)> {
        let name = args["name"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: name".to_string()))?;
        let file = args["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;

        let engine = self.engine.lock().await;

        let node = engine.graph().all_nodes().into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let dependents = engine.graph().get_dependents(&n.id);
                let dep_values: Vec<Value> = dependents.iter().map(|(dep, edge)| {
                    json!({
                        "s": dep.name,
                        "k": dep.kind.short_code(),
                        "f": dep.file,
                        "l": dep.line,
                        "e": edge.short_code()
                    })
                }).collect();

                Ok(wrap_tool_result(json!({
                    "symbol": name,
                    "file": file,
                    "dependents": dep_values,
                    "count": dep_values.len()
                })))
            }
            None => Ok(wrap_tool_result(json!({
                "error": format!("Symbol '{}' not found in '{}'", name, file)
            }))),
        }
    }

    async fn tool_get_dependencies(&self, args: &Value) -> Result<Value, (i32, String)> {
        let name = args["name"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: name".to_string()))?;
        let file = args["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;

        let engine = self.engine.lock().await;

        let node = engine.graph().all_nodes().into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let dependencies = engine.graph().get_dependencies(&n.id);
                let dep_values: Vec<Value> = dependencies.iter().map(|(dep, edge)| {
                    json!({
                        "s": dep.name,
                        "k": dep.kind.short_code(),
                        "f": dep.file,
                        "l": dep.line,
                        "e": edge.short_code()
                    })
                }).collect();

                Ok(wrap_tool_result(json!({
                    "symbol": name,
                    "file": file,
                    "dependencies": dep_values,
                    "count": dep_values.len()
                })))
            }
            None => Ok(wrap_tool_result(json!({
                "error": format!("Symbol '{}' not found in '{}'", name, file)
            }))),
        }
    }

    async fn tool_blast_radius(&self, args: &Value) -> Result<Value, (i32, String)> {
        let name = args["name"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: name".to_string()))?;
        let file = args["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;
        let hops = (args["hops"].as_u64().unwrap_or(3) as usize).min(10);

        let engine = self.engine.lock().await;

        let node = engine.graph().all_nodes().into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let affected = engine.graph().get_transitive_dependents(&n.id, hops);
                let affected_files: HashSet<&str> = affected.iter().map(|a| a.file.as_str()).collect();

                let affected_values: Vec<Value> = affected.iter().map(|a| {
                    json!({
                        "s": a.name,
                        "k": a.kind.short_code(),
                        "f": a.file,
                        "l": a.line
                    })
                }).collect();

                Ok(wrap_tool_result(json!({
                    "symbol": name,
                    "file": file,
                    "hops": hops,
                    "affected": affected_values,
                    "files": affected_files.into_iter().collect::<Vec<_>>(),
                    "count": affected_values.len()
                })))
            }
            None => Ok(wrap_tool_result(json!({
                "error": format!("Symbol '{}' not found in '{}'", name, file)
            }))),
        }
    }

    async fn tool_search_symbols(&self, args: &Value) -> Result<Value, (i32, String)> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: pattern".to_string()))?;
        let limit = (args["limit"].as_u64().unwrap_or(20) as usize).min(200);
        let detail = args["detail"].as_str().unwrap_or("summary");

        let engine = self.engine.lock().await;
        let pattern_lower = pattern.to_lowercase();

        let mut results: Vec<Value> = engine.graph().all_nodes().into_iter()
            .filter(|n| n.name.to_lowercase().contains(&pattern_lower))
            .map(|n| {
                let mut obj = json!({
                    "symbol": n.name,
                    "file": n.file,
                    "line": n.line
                });
                if detail == "full" {
                    if let Some(m) = obj.as_object_mut() {
                        m.insert("kind".to_string(), json!(n.kind.short_code()));
                        m.insert("exported".to_string(), json!(n.is_exported));
                        m.insert("signature".to_string(), json!(n.signature));
                    }
                }
                obj
            })
            .collect();

        results.truncate(limit);

        Ok(wrap_tool_result(json!({
            "pattern": pattern,
            "results": results,
            "count": results.len()
        })))
    }

    async fn tool_get_file_context(&self, args: &Value) -> Result<Value, (i32, String)> {
        let file = args["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;

        let engine = self.engine.lock().await;

        // Direct graph lookup: all symbols in the file
        let file_nodes = engine.file_symbols(file);
        let symbols: Vec<Value> = file_nodes.iter().map(|n| {
            let dep_count = engine.graph().get_dependents(&n.id).len();
            json!({
                "symbol": n.name,
                "kind": n.kind.short_code(),
                "line": n.line,
                "exported": n.is_exported,
                "dependents": dep_count
            })
        }).collect();

        Ok(wrap_tool_result(json!({
            "file": file,
            "symbols": symbols,
            "count": symbols.len()
        })))
    }

    async fn tool_store_memory(&self, args: &Value) -> Result<Value, (i32, String)> {
        let content = args["content"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: content".to_string()))?;

        let memory_type_str = args["memory_type"].as_str().unwrap_or("observation");
        let memory_type = MemoryType::from_str(memory_type_str);

        let linked_symbols: Vec<String> = args["linked_symbols"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let memory = Memory {
            id: String::new(),
            session_id: self.session_id.clone(),
            content: content.to_string(),
            memory_type: memory_type.clone(),
            confidence: 1.0,
            linked_symbols: linked_symbols.clone(),
            source_query: None,
            created_at: 0,
            last_accessed: 0,
            access_count: 0,
            is_stale: false,
            stale_reason: None,
        };

        let store = self.memory_store.lock().await;
        let id = store.store(memory)
            .map_err(|e| (-32603, format!("Failed to store memory: {}", e)))?;

        Ok(wrap_tool_result(json!({
            "status": "stored",
            "id": id,
            "memory_type": memory_type.as_str(),
            "linked_symbols": linked_symbols
        })))
    }

    async fn tool_get_session_context(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"].as_str();
        let limit = (args["limit"].as_u64().unwrap_or(20) as usize).min(100);

        let store = self.memory_store.lock().await;

        // Current session memories (always included)
        let current = store.get_session_memories(&self.session_id, limit)
            .map_err(|e| (-32603, format!("Failed to get session memories: {}", e)))?;

        // Previous session memories: if query provided, search; otherwise get recent across sessions
        let remaining = limit.saturating_sub(current.len());
        let previous = if remaining > 0 {
            let keyword = query.unwrap_or("");
            if keyword.is_empty() {
                // Get recent memories from other sessions
                store.search_across_sessions("", Some(&self.session_id), remaining)
                    .unwrap_or_default()
            } else {
                store.search_across_sessions(keyword, Some(&self.session_id), remaining)
                    .unwrap_or_default()
            }
        } else {
            vec![]
        };

        let format_memory = |m: &Memory, is_current: bool| {
            let mut obj = json!({
                "id": m.id,
                "content": m.content,
                "type": m.memory_type.as_str(),
                "linked_symbols": m.linked_symbols
            });
            if !is_current {
                if let Some(o) = obj.as_object_mut() {
                    o.insert("session".to_string(), json!(m.session_id));
                    if m.is_stale {
                        o.insert("stale".to_string(), json!(true));
                    }
                }
            }
            obj
        };

        let current_values: Vec<Value> = current.iter().map(|m| format_memory(m, true)).collect();
        let previous_values: Vec<Value> = previous.iter().map(|m| format_memory(m, false)).collect();

        Ok(wrap_tool_result(json!({
            "session_id": self.session_id,
            "current": current_values,
            "previous": previous_values
        })))
    }

    async fn tool_search_memory(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: query".to_string()))?;
        let limit = (args["limit"].as_u64().unwrap_or(10) as usize).min(100);

        let store = self.memory_store.lock().await;
        let memories = store.search_across_sessions(query, None, limit)
            .map_err(|e| (-32603, format!("Failed to search memories: {}", e)))?;

        let memory_values: Vec<Value> = memories
            .iter()
            .map(|m| {
                json!({
                    "id": m.id,
                    "content": m.content,
                    "type": m.memory_type.as_str(),
                    "linked_symbols": m.linked_symbols,
                    "session": m.session_id
                })
            })
            .collect();

        Ok(wrap_tool_result(json!({
            "query": query,
            "memories": memory_values,
            "count": memory_values.len()
        })))
    }

    async fn tool_list_observations(&self, args: &Value) -> Result<Value, (i32, String)> {
        let session_id = args["session_id"].as_str();
        let limit = (args["limit"].as_u64().unwrap_or(50) as usize).min(200);

        let store = self.memory_store.lock().await;

        let memories = if let Some(sid) = session_id {
            store.get_session_memories(sid, limit)
                .map_err(|e| (-32603, format!("Failed to list observations: {}", e)))?
        } else {
            let all = store.list_all()
                .map_err(|e| (-32603, format!("Failed to list observations: {}", e)))?;
            all.into_iter().take(limit).collect()
        };

        let entries: Vec<Value> = memories.iter().map(|m| {
            json!({
                "id": m.id,
                "session_id": m.session_id,
                "content": m.content,
                "type": m.memory_type.as_str(),
                "confidence": m.confidence,
                "linked_symbols": m.linked_symbols,
                "created_at": m.created_at,
                "is_stale": m.is_stale,
                "stale_reason": m.stale_reason,
            })
        }).collect();

        Ok(wrap_tool_result(json!({
            "count": entries.len(),
            "memories": entries
        })))
    }

    async fn tool_delete_observation(&self, args: &Value) -> Result<Value, (i32, String)> {
        let id = args["id"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: id".to_string()))?;

        let store = self.memory_store.lock().await;
        store.invalidate(id)
            .map_err(|e| (-32603, format!("Failed to delete observation: {}", e)))?;

        Ok(wrap_tool_result(json!({
            "status": "deleted",
            "id": id
        })))
    }

    async fn tool_update_observation(&self, args: &Value) -> Result<Value, (i32, String)> {
        let id = args["id"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: id".to_string()))?;
        let content = args["content"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: content".to_string()))?;

        let store = self.memory_store.lock().await;
        store.update_content(id, content)
            .map_err(|e| (-32603, format!("Failed to update observation: {}", e)))?;

        Ok(wrap_tool_result(json!({
            "status": "updated",
            "id": id
        })))
    }

    async fn tool_submit_lsp_edges(&self, args: &Value) -> Result<Value, (i32, String)> {
        let edges = args["edges"]
            .as_array()
            .ok_or((-32602, "Missing required parameter: edges (array)".to_string()))?;

        let mut indexer = self.indexer.lock().await;
        let mut added = 0usize;
        let mut skipped = 0usize;
        let mut skip_reasons: Vec<Value> = Vec::new();

        for edge in edges {
            let from_name = edge["from_name"].as_str().unwrap_or("");
            let from_file = edge["from_file"].as_str().unwrap_or("");
            let to_name = edge["to_name"].as_str().unwrap_or("");
            let to_file = edge["to_file"].as_str().unwrap_or("");
            let kind_str = edge["kind"].as_str().unwrap_or("Calls");

            let edge_kind = match kind_str {
                "Calls" | "C" => lattice_core::graph::model::EdgeKind::Calls,
                "Imports" | "I" => lattice_core::graph::model::EdgeKind::Imports,
                "TypeRef" | "T" => lattice_core::graph::model::EdgeKind::TypeRef,
                "Implements" | "M" => lattice_core::graph::model::EdgeKind::Implements,
                "Extends" | "E" => lattice_core::graph::model::EdgeKind::Extends,
                _ => lattice_core::graph::model::EdgeKind::Calls,
            };

            // Find the nodes in the graph
            let graph = indexer.graph();
            let from_id = graph.all_nodes().iter()
                .find(|n| n.name == from_name && n.file == from_file)
                .map(|n| n.id.clone());
            let to_id = graph.all_nodes().iter()
                .find(|n| n.name == to_name && n.file == to_file)
                .map(|n| n.id.clone());

            match (from_id, to_id) {
                (Some(fid), Some(tid)) => {
                    indexer.graph_mut().add_edge(&fid, &tid, edge_kind);
                    added += 1;
                }
                (None, None) => {
                    skip_reasons.push(json!({
                        "from": from_name, "to": to_name,
                        "reason": format!("both '{}::{}' and '{}::{}' not found in graph", from_file, from_name, to_file, to_name)
                    }));
                    skipped += 1;
                }
                (None, Some(_)) => {
                    skip_reasons.push(json!({
                        "from": from_name, "to": to_name,
                        "reason": format!("source '{}::{}' not found in graph", from_file, from_name)
                    }));
                    skipped += 1;
                }
                (Some(_), None) => {
                    skip_reasons.push(json!({
                        "from": from_name, "to": to_name,
                        "reason": format!("target '{}::{}' not found in graph", to_file, to_name)
                    }));
                    skipped += 1;
                }
            }
        }

        // Propagate updated graph to engine
        let new_graph = indexer.graph().clone();
        {
            let mut engine = self.engine.lock().await;
            engine.update_graph(new_graph);
        }

        Ok(wrap_tool_result(json!({
            "added": added,
            "skipped": skipped,
            "skip_reasons": skip_reasons
        })))
    }

    async fn tool_workspace_setup(&self, args: &Value) -> Result<Value, (i32, String)> {
        let format = args["format"].as_str().unwrap_or("markdown");

        let engine = self.engine.lock().await;
        let all_nodes = engine.graph().all_nodes();
        let stats = engine.graph().stats();

        // Collect unique files and language breakdown
        let mut file_set = HashSet::new();
        let mut lang_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for node in &all_nodes {
            file_set.insert(node.file.clone());
            *lang_counts.entry(format!("{:?}", node.language)).or_insert(0) += 1;
        }
        let files: Vec<String> = file_set.into_iter().collect();

        // Detect project rules
        let detector = lattice_core::intelligence::RulesDetector::new();
        let rules = detector.detect_rules(&files);

        if format == "markdown" {
            let mut md = String::new();
            md.push_str(&format!("# Workspace Setup\n\n"));
            md.push_str(&format!("**Files:** {} | **Symbols:** {} | **Edges:** {}\n\n", stats.file_count, stats.node_count, stats.edge_count));
            md.push_str("## Languages\n\n");
            for (lang, count) in &lang_counts {
                md.push_str(&format!("- {}: {} symbols\n", lang, count));
            }
            md.push_str("\n## Detected Conventions\n\n");
            for rule in &rules {
                md.push_str(&format!("- {} (confidence: {:.0}%, {} occurrences)\n", rule.description, rule.confidence * 100.0, rule.occurrences));
            }
            Ok(wrap_tool_result(json!({ "markdown": md })))
        } else {
            let rule_values: Vec<Value> = rules.iter().map(|r| json!({
                "description": r.description,
                "confidence": r.confidence,
                "occurrences": r.occurrences,
            })).collect();

            Ok(wrap_tool_result(json!({
                "files": stats.file_count,
                "symbols": stats.node_count,
                "edges": stats.edge_count,
                "languages": lang_counts,
                "rules": rule_values
            })))
        }
    }

    async fn tool_index_status(&self, _args: &Value) -> Result<Value, (i32, String)> {
        let is_indexing = self.indexing.load(Ordering::Relaxed);

        let engine = self.engine.lock().await;
        let stats = engine.graph().stats();
        let all_nodes = engine.graph().all_nodes();

        let mut lang_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for node in &all_nodes {
            *lang_counts.entry(format!("{:?}", node.language)).or_insert(0) += 1;
        }

        let mut result = json!({
            "status": if is_indexing { "indexing" } else { "ready" },
            "version": env!("CARGO_PKG_VERSION"),
            "workspace": self.workspace_root.to_string_lossy(),
            "nodes": stats.node_count,
            "edges": stats.edge_count,
            "files": stats.file_count,
            "languages": lang_counts
        });

        // Add multi-repo info if applicable
        if self.workspace_roots.len() > 1 {
            let obj = match result.as_object_mut() {
                Some(o) => o,
                None => return Ok(result),
            };
            let roots: Vec<String> = self.workspace_roots.iter()
                .map(|r| r.to_string_lossy().to_string())
                .collect();
            obj.insert("workspaces".to_string(), json!(roots));
            obj.insert("multi_repo".to_string(), json!(true));

            if let Some(wm) = &self.workspace_manager {
                let wm = wm.lock().await;
                let repo_stats: Vec<Value> = wm.repo_stats().iter().map(|s| json!({
                    "name": s.name,
                    "files": s.file_count,
                    "nodes": s.node_count,
                    "edges": s.edge_count
                })).collect();
                obj.insert("repos".to_string(), json!(repo_stats));
            }
        }

        Ok(wrap_tool_result(result))
    }

    async fn tool_get_project_rules(&self, _args: &Value) -> Result<Value, (i32, String)> {
        let engine = self.engine.lock().await;
        let all_nodes = engine.graph().all_nodes();

        // Collect unique file paths from the graph
        let files: Vec<String> = {
            let mut file_set = HashSet::new();
            for node in &all_nodes {
                file_set.insert(node.file.clone());
            }
            file_set.into_iter().collect()
        };

        let detector = lattice_core::intelligence::RulesDetector::new();
        let rules = detector.detect_rules(&files);

        let rule_values: Vec<Value> = rules
            .iter()
            .map(|r| {
                json!({
                    "description": r.description,
                    "confidence": r.confidence,
                    "occurrences": r.occurrences,
                    "example_files": r.example_files
                })
            })
            .collect();

        Ok(wrap_tool_result(json!({
            "status": "ok",
            "rules": rule_values,
            "count": rule_values.len()
        })))
    }

    async fn tool_search_logic_flow(&self, args: &Value) -> Result<Value, (i32, String)> {
        let from_name = args["from"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: from".to_string()))?;
        let to_name = args["to"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: to".to_string()))?;
        let from_file = args["from_file"].as_str();
        let to_file = args["to_file"].as_str();
        let max_depth = (args["max_depth"].as_u64().unwrap_or(5) as usize).min(15);

        let engine = self.engine.lock().await;
        let all_nodes = engine.graph().all_nodes();

        // Find source symbol — supports exact, qualified (Class.method), and suffix matching
        let from_node = find_symbol_fuzzy(&all_nodes, from_name, from_file);
        // Find target symbol
        let to_node = find_symbol_fuzzy(&all_nodes, to_name, to_file);

        let (from_node, to_node) = match (from_node, to_node) {
            (Some(f), Some(t)) => (f, t),
            (None, _) => return Ok(wrap_tool_result(json!({
                "error": format!("Source symbol '{}' not found", from_name)
            }))),
            (_, None) => return Ok(wrap_tool_result(json!({
                "error": format!("Target symbol '{}' not found", to_name)
            }))),
        };

        let paths = engine.graph().find_call_paths(&from_node.id, &to_node.id, max_depth, 10);

        let path_values: Vec<Value> = paths.iter().map(|path| {
            json!(path.iter().map(|n| json!({
                "s": n.name,
                "f": n.file,
                "l": n.line,
                "k": n.kind.short_code()
            })).collect::<Vec<_>>())
        }).collect();

        Ok(wrap_tool_result(json!({
            "from": from_name,
            "to": to_name,
            "paths": path_values,
            "count": path_values.len()
        })))
    }

    // ── Daemon Method Handlers ────────────────────────────────────────

    /// Handle `lattice/file_symbols` — symbols in a specific file with dependent counts.
    async fn handle_file_symbols(&self, params: &Value) -> Result<Value, (i32, String)> {
        let file = params["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;

        let engine = self.engine.lock().await;
        let file_nodes = engine.file_symbols(file);

        let symbols: Vec<Value> = file_nodes.iter().map(|n| {
            let dependents = engine.graph().get_dependents(&n.id);
            let dependent_files: HashSet<&str> = dependents.iter()
                .map(|(d, _)| d.file.as_str())
                .collect();

            json!({
                "name": n.name,
                "line": n.line,
                "dependentCount": dependents.len(),
                "fileCount": dependent_files.len()
            })
        }).collect();

        Ok(json!({ "symbols": symbols }))
    }

    /// Handle `lattice/symbol_info` — detailed info about a single symbol.
    async fn handle_symbol_info(&self, params: &Value) -> Result<Value, (i32, String)> {
        let name = params["name"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: name".to_string()))?;
        let file = params["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;
        let _line = params["line"].as_u64().unwrap_or(0);

        let engine = self.engine.lock().await;
        let node = engine.graph().all_nodes().into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let dependents = engine.graph().get_dependents(&n.id);
                let cross_repo_count = 0usize; // Cross-repo is 0 for single workspace

                // Top 3 callers: dependents that call this symbol
                let top_callers: Vec<String> = dependents.iter()
                    .take(3)
                    .map(|(d, _)| format!("{}:{}", d.file, d.name))
                    .collect();

                // Hotspot score from edit_count
                let hotspot = n.edit_count as f64;

                Ok(json!({
                    "name": n.name,
                    "dependentCount": dependents.len(),
                    "crossRepoCount": cross_repo_count,
                    "topCallers": top_callers,
                    "hotspot": hotspot,
                    "lastModified": n.last_modified.to_string()
                }))
            }
            None => Err((-32602, format!("Symbol '{}' not found in '{}'", name, file))),
        }
    }

    /// Handle `lattice/dependents` — list of dependent symbols.
    async fn handle_dependents(&self, params: &Value) -> Result<Value, (i32, String)> {
        let name = params["name"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: name".to_string()))?;
        let file = params["file"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: file".to_string()))?;

        let engine = self.engine.lock().await;
        let node = engine.graph().all_nodes().into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let dependents = engine.graph().get_dependents(&n.id);
                let dep_values: Vec<Value> = dependents.iter().map(|(dep, edge)| {
                    json!({
                        "name": dep.name,
                        "file": dep.file,
                        "line": dep.line,
                        "edge": format!("{:?}", edge)
                    })
                }).collect();

                Ok(json!(dep_values))
            }
            None => Err((-32602, format!("Symbol '{}' not found in '{}'", name, file))),
        }
    }

    /// Handle `lattice/clear_memory` or `lattice/clear` — clear all memories.
    async fn handle_clear_memory(&self) -> Result<Value, (i32, String)> {
        let store = self.memory_store.lock().await;
        let cleared = store.clear_all()
            .map_err(|e| (-32603, format!("Failed to clear memories: {}", e)))?;

        Ok(json!({
            "status": "ok",
            "cleared": cleared
        }))
    }

    /// Handle `lattice/reindex` — spawn background re-scan, return immediately.
    async fn handle_reindex(&self) -> Result<Value, (i32, String)> {
        let workspace_root = self.workspace_root.clone();
        let indexer = Arc::clone(&self.indexer);
        let engine = Arc::clone(&self.engine);
        let graph_store = Arc::clone(&self.graph_store);
        let indexing = Arc::clone(&self.indexing);

        indexing.store(true, Ordering::Relaxed);
        tokio::spawn(async move {
            let security_filter = SecurityFilter::new(&workspace_root);
            let mut files_indexed = 0usize;
            let mut errors = 0usize;

            {
                let mut idx = indexer.lock().await;
                let entries = walk_directory_filtered(&workspace_root, &security_filter);
                for entry_path in &entries {
                    let rel_path = entry_path
                        .strip_prefix(&workspace_root)
                        .unwrap_or(entry_path)
                        .to_string_lossy()
                        .replace('\\', "/");

                    if !should_index_file(&rel_path) || security_filter.is_excluded(&rel_path) {
                        continue;
                    }

                    match std::fs::read_to_string(entry_path) {
                        Ok(content) => {
                            if idx.index_file_content(&rel_path, &content).is_ok() {
                                files_indexed += 1;
                            } else {
                                errors += 1;
                            }
                        }
                        Err(_) => { errors += 1; }
                    }
                }

                let new_graph = idx.graph().clone();

                if let Ok(gs) = graph_store.try_lock() {
                    let _ = gs.save_graph(&new_graph);
                }

                let mut eng = engine.lock().await;
                eng.update_graph(new_graph);
            }

            indexing.store(false, Ordering::Relaxed);
            tracing::info!("Reindex complete: {} files indexed, {} errors", files_indexed, errors);
        });

        Ok(json!({
            "status": "started",
            "message": "Re-index started in background"
        }))
    }
}

#[async_trait::async_trait]
impl RequestHandler for McpHandler {
    async fn handle(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, (i32, String)> {
        match method {
            "initialize" => Ok(self.handle_initialize()),
            "tools/list" => Ok(self.handle_tools_list()),
            "tools/call" => self.handle_tools_call(&params).await,
            "ping" => Ok(json!({})),
            "lattice/status" => {
                let is_indexing = self.indexing.load(Ordering::Relaxed);
                let engine = self.engine.lock().await;
                let stats = if engine.graph().stats().node_count == 0 && is_indexing {
                    if let Ok(idx) = self.indexer.try_lock() {
                        idx.graph().stats()
                    } else {
                        engine.graph().stats()
                    }
                } else {
                    engine.graph().stats()
                };
                Ok(json!({
                    "status": if is_indexing { "indexing" } else { "ready" },
                    "version": env!("CARGO_PKG_VERSION"),
                    "workspace": self.workspace_root.to_string_lossy(),
                    "nodes": stats.node_count,
                    "edges": stats.edge_count,
                    "files": stats.file_count
                }))
            }
            "lattice/reindex" => self.handle_reindex().await,
            "lattice/file_symbols" => self.handle_file_symbols(&params).await,
            "lattice/symbol_info" => self.handle_symbol_info(&params).await,
            "lattice/dependents" => self.handle_dependents(&params).await,
            "lattice/clear_memory" | "lattice/clear" => self.handle_clear_memory().await,
            // MCP notifications — acknowledge silently
            "notifications/initialized"
            | "notifications/cancelled"
            | "notifications/progress"
            | "notifications/roots/list_changed" => Ok(json!({})),
            _ => Err((-32601, format!("Method not found: {}", method))),
        }
    }
}

// ── Helper Functions ──────────────────────────────────────────────────

/// Find a symbol by name with fuzzy matching.
/// Supports:
///   1. Exact match: "TokenBlacklist.is_blacklisted"
///   2. Suffix match: "is_blacklisted" matches "TokenBlacklist.is_blacklisted"
///   3. Unqualified match: "is_blacklisted" matches the method name part
/// Optional file filter narrows results.
fn find_symbol_fuzzy<'a>(
    nodes: &'a [&lattice_core::graph::model::GraphNode],
    name: &str,
    file_filter: Option<&str>,
) -> Option<&'a lattice_core::graph::model::GraphNode> {
    let matches_file = |n: &&lattice_core::graph::model::GraphNode| {
        file_filter.map_or(true, |f| n.file == f)
    };

    // 1. Exact match
    if let Some(node) = nodes.iter().find(|n| n.name == name && matches_file(n)) {
        return Some(node);
    }

    // 2. Suffix match: "is_blacklisted" matches "TokenBlacklist.is_blacklisted"
    let dot_suffix = format!(".{}", name);
    if let Some(node) = nodes.iter().find(|n| n.name.ends_with(&dot_suffix) && matches_file(n)) {
        return Some(node);
    }

    // 3. Case-insensitive exact match
    let name_lower = name.to_lowercase();
    if let Some(node) = nodes.iter().find(|n| n.name.to_lowercase() == name_lower && matches_file(n)) {
        return Some(node);
    }

    // 4. Case-insensitive suffix match
    let dot_suffix_lower = format!(".{}", name_lower);
    nodes.iter().find(|n| n.name.to_lowercase().ends_with(&dot_suffix_lower) && matches_file(n)).copied()
}

/// Wrap a tool result in the MCP content format.
fn wrap_tool_result(value: Value) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string(&value).unwrap_or_else(|_| value.to_string())
        }]
    })
}

/// Recursively walk a directory, collecting all file paths.
fn walk_directory_filtered(root: &std::path::Path, filter: &SecurityFilter) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                    if filter.is_excluded_dir(dir_name) {
                        continue;
                    }
                }
                files.extend(walk_directory_filtered(&path, filter));
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files
}
