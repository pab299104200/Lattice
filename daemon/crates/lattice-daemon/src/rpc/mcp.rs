use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use serde_json::{json, Value};

use lattice_core::indexer::Indexer;
use lattice_core::memory::{Memory, MemoryType, MemoryStore};
use lattice_core::query::QueryEngine;
use lattice_core::storage::GraphStore;
use lattice_core::watcher::should_index_file;

use super::server::RequestHandler;

/// MCP (Model Context Protocol) handler that routes JSON-RPC methods
/// to the appropriate tool implementations.
pub struct McpHandler {
    engine: Arc<Mutex<QueryEngine>>,
    indexer: Arc<Mutex<Indexer>>,
    memory_store: Arc<Mutex<MemoryStore>>,
    graph_store: Arc<Mutex<GraphStore>>,
    workspace_root: PathBuf,
}

impl McpHandler {
    /// Create a new McpHandler with all shared state.
    pub fn new(
        engine: Arc<Mutex<QueryEngine>>,
        indexer: Arc<Mutex<Indexer>>,
        memory_store: Arc<Mutex<MemoryStore>>,
        graph_store: Arc<Mutex<GraphStore>>,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            engine,
            indexer,
            memory_store,
            graph_store,
            workspace_root,
        }
    }

    // ── MCP Protocol Methods ──────────────────────────────────────────

    fn handle_initialize(&self) -> Value {
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
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
                    "name": "query_context",
                    "description": "Query the code graph for relevant context. Returns a Context Capsule with pivots (full source) and context nodes (signatures).",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Natural language query describing what you need context for"
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
                    "name": "blast_radius",
                    "description": "Compute the blast radius of changing a symbol — all transitive dependents up to N hops.",
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
                            }
                        },
                        "required": ["pattern"]
                    }
                },
                {
                    "name": "get_file_context",
                    "description": "Get all symbols defined in a given file.",
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
                    "name": "store_memory",
                    "description": "Store a memory (insight, decision, or pattern) for later recall.",
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
                    "name": "recall_memories",
                    "description": "Recall stored memories relevant to a query.",
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
                    "name": "get_project_rules",
                    "description": "Get project-specific rules and conventions.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {},
                        "required": []
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
            "query_context" => self.tool_query_context(arguments).await,
            "get_symbol" => self.tool_get_symbol(arguments).await,
            "get_dependents" => self.tool_get_dependents(arguments).await,
            "get_dependencies" => self.tool_get_dependencies(arguments).await,
            "blast_radius" => self.tool_blast_radius(arguments).await,
            "search_symbols" => self.tool_search_symbols(arguments).await,
            "get_file_context" => self.tool_get_file_context(arguments).await,
            "store_memory" => self.tool_store_memory(arguments).await,
            "recall_memories" => self.tool_recall_memories(arguments).await,
            "get_project_rules" => self.tool_get_project_rules(arguments).await,
            _ => Err((-32602, format!("Unknown tool: {}", tool_name))),
        }
    }

    // ── Tool Implementations ──────────────────────────────────────────

    async fn tool_query_context(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: query".to_string()))?;

        let mut engine = self.engine.lock().await;
        let capsule = engine.query(query, None);
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

        let engine = self.engine.lock().await;

        // Direct graph lookup: find node matching name and file
        let node = engine.graph().all_nodes().into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let dependents = engine.graph().get_dependents(&n.id);
                let dependencies = engine.graph().get_dependencies(&n.id);
                Ok(wrap_tool_result(json!({
                    "symbol": n.name,
                    "kind": format!("{:?}", n.kind),
                    "file": n.file,
                    "line": n.line,
                    "end_line": n.end_line,
                    "source": n.body,
                    "signature": n.signature,
                    "is_exported": n.is_exported,
                    "dependents": dependents.iter().map(|(dep, edge)| json!({
                        "symbol": dep.name,
                        "file": dep.file,
                        "edge": format!("{:?}", edge)
                    })).collect::<Vec<_>>(),
                    "dependencies": dependencies.iter().map(|(dep, edge)| json!({
                        "symbol": dep.name,
                        "file": dep.file,
                        "edge": format!("{:?}", edge)
                    })).collect::<Vec<_>>()
                })))
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

        // Find the actual node by name and file
        let node = engine.graph().all_nodes().into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let dependents = engine.graph().get_dependents(&n.id);
                let dep_values: Vec<Value> = dependents.iter().map(|(dep, edge)| {
                    json!({
                        "symbol": dep.name,
                        "kind": format!("{:?}", dep.kind),
                        "file": dep.file,
                        "line": dep.line,
                        "edge": format!("{:?}", edge)
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
                        "symbol": dep.name,
                        "kind": format!("{:?}", dep.kind),
                        "file": dep.file,
                        "line": dep.line,
                        "edge": format!("{:?}", edge)
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
        let hops = args["hops"].as_u64().unwrap_or(3) as usize;

        let engine = self.engine.lock().await;

        let node = engine.graph().all_nodes().into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                // Use transitive dependents (incoming edges only)
                let affected = engine.graph().get_transitive_dependents(&n.id, hops);
                let affected_files: HashSet<&str> = affected.iter().map(|a| a.file.as_str()).collect();

                let affected_values: Vec<Value> = affected.iter().map(|a| {
                    json!({
                        "symbol": a.name,
                        "kind": format!("{:?}", a.kind),
                        "file": a.file,
                        "line": a.line
                    })
                }).collect();

                Ok(wrap_tool_result(json!({
                    "symbol": name,
                    "file": file,
                    "hops": hops,
                    "affected_symbols": affected_values,
                    "affected_files": affected_files.into_iter().collect::<Vec<_>>(),
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
        let limit = args["limit"].as_u64().unwrap_or(20) as usize;

        let engine = self.engine.lock().await;
        let pattern_lower = pattern.to_lowercase();

        // Direct graph search: case-insensitive substring match on name
        let mut results: Vec<Value> = engine.graph().all_nodes().into_iter()
            .filter(|n| n.name.to_lowercase().contains(&pattern_lower))
            .map(|n| {
                json!({
                    "symbol": n.name,
                    "kind": format!("{:?}", n.kind),
                    "file": n.file,
                    "line": n.line,
                    "is_exported": n.is_exported,
                    "signature": n.signature
                })
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
            let dependents = engine.graph().get_dependents(&n.id);
            let dependent_files: HashSet<&str> = dependents.iter()
                .map(|(d, _)| d.file.as_str())
                .collect();

            json!({
                "symbol": n.name,
                "kind": format!("{:?}", n.kind),
                "file": n.file,
                "line": n.line,
                "end_line": n.end_line,
                "is_exported": n.is_exported,
                "signature": n.signature,
                "dependent_count": dependents.len(),
                "dependent_files": dependent_files.len()
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

    async fn tool_recall_memories(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: query".to_string()))?;
        let limit = args["limit"].as_u64().unwrap_or(10) as usize;

        let store = self.memory_store.lock().await;
        let mut memories = store.search_by_keyword(query)
            .map_err(|e| (-32603, format!("Failed to recall memories: {}", e)))?;

        memories.truncate(limit);

        let memory_values: Vec<Value> = memories
            .iter()
            .map(|m| {
                json!({
                    "id": m.id,
                    "content": m.content,
                    "memory_type": m.memory_type.as_str(),
                    "confidence": m.confidence,
                    "linked_symbols": m.linked_symbols,
                    "is_stale": m.is_stale,
                    "stale_reason": m.stale_reason,
                    "created_at": m.created_at,
                    "access_count": m.access_count
                })
            })
            .collect();

        Ok(wrap_tool_result(json!({
            "query": query,
            "memories": memory_values,
            "count": memory_values.len()
        })))
    }

    async fn tool_get_project_rules(&self, _args: &Value) -> Result<Value, (i32, String)> {
        Ok(wrap_tool_result(json!({
            "status": "placeholder",
            "message": "Project rules not yet implemented. Will be available in a future release.",
            "rules": []
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

    /// Handle `lattice/reindex` — re-scan all files and rebuild graph.
    async fn handle_reindex(&self) -> Result<Value, (i32, String)> {
        let workspace_root = self.workspace_root.clone();

        // Re-scan the workspace
        let mut files_indexed = 0usize;
        let mut errors = Vec::new();

        {
            let mut indexer = self.indexer.lock().await;
            // Walk the workspace directory
            let entries = walk_directory(&workspace_root);
            for entry_path in &entries {
                let rel_path = entry_path
                    .strip_prefix(&workspace_root)
                    .unwrap_or(entry_path)
                    .to_string_lossy()
                    .replace('\\', "/");

                if !should_index_file(&rel_path) {
                    continue;
                }

                match std::fs::read_to_string(entry_path) {
                    Ok(content) => {
                        if let Err(e) = indexer.index_file_content(&rel_path, &content) {
                            errors.push(format!("{}: {}", rel_path, e));
                        } else {
                            files_indexed += 1;
                        }
                    }
                    Err(e) => {
                        errors.push(format!("{}: {}", rel_path, e));
                    }
                }
            }

            // Update the engine with the new graph
            let new_graph = indexer.graph().clone();
            let stats = new_graph.stats();

            // Save to graph store
            {
                let gs = self.graph_store.lock().await;
                if let Err(e) = gs.save_graph(&new_graph) {
                    tracing::warn!("Failed to save graph to store: {}", e);
                }
            }

            // Update the query engine
            {
                let mut engine = self.engine.lock().await;
                engine.update_graph(new_graph);
            }

            Ok(json!({
                "status": "ok",
                "files_indexed": files_indexed,
                "nodes": stats.node_count,
                "edges": stats.edge_count,
                "errors": errors.len()
            }))
        }
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
                let engine = self.engine.lock().await;
                let stats = engine.graph().stats();
                Ok(json!({
                    "status": "running",
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
            _ => Err((-32601, format!("Method not found: {}", method))),
        }
    }
}

// ── Helper Functions ──────────────────────────────────────────────────

/// Wrap a tool result in the MCP content format.
fn wrap_tool_result(value: Value) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
        }]
    })
}

/// Recursively walk a directory, collecting all file paths.
fn walk_directory(root: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip excluded directories
                if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                    if lattice_core::watcher::EXCLUDED_DIRS.contains(&dir_name) {
                        continue;
                    }
                }
                files.extend(walk_directory(&path));
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files
}
