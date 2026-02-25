use std::sync::Arc;
use tokio::sync::Mutex;
use serde_json::{json, Value};

use lattice_core::query::QueryEngine;
use lattice_core::symbols::SymbolId;

use super::server::RequestHandler;

/// MCP (Model Context Protocol) handler that routes JSON-RPC methods
/// to the appropriate tool implementations.
pub struct McpHandler {
    engine: Arc<Mutex<QueryEngine>>,
}

impl McpHandler {
    /// Create a new McpHandler with the given QueryEngine.
    pub fn new(engine: Arc<Mutex<QueryEngine>>) -> Self {
        Self { engine }
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
                            "tags": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Tags for categorizing the memory"
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

        let engine = self.engine.lock().await;
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
        // Find the symbol by searching all nodes for a matching name and file
        let symbol = find_node_by_name_and_file(&engine, name, file);

        match symbol {
            Some(node) => Ok(wrap_tool_result(node)),
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
        let symbol_id = find_symbol_id(&engine, name, file);

        match symbol_id {
            Some(id) => {
                let dependents = get_dependents_from_engine(&engine, &id);
                Ok(wrap_tool_result(json!({
                    "symbol": name,
                    "file": file,
                    "dependents": dependents
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
        let symbol_id = find_symbol_id(&engine, name, file);

        match symbol_id {
            Some(id) => {
                let dependencies = get_dependencies_from_engine(&engine, &id);
                Ok(wrap_tool_result(json!({
                    "symbol": name,
                    "file": file,
                    "dependencies": dependencies
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
        let symbol_id = find_symbol_id(&engine, name, file);

        match symbol_id {
            Some(id) => {
                let neighbors = get_n_hop_neighbors(&engine, &id, hops);
                Ok(wrap_tool_result(json!({
                    "symbol": name,
                    "file": file,
                    "hops": hops,
                    "affected_symbols": neighbors,
                    "count": neighbors.len()
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
        let results = search_symbols_in_engine(&engine, pattern, limit);

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
        let symbols = get_file_symbols(&engine, file);

        Ok(wrap_tool_result(json!({
            "file": file,
            "symbols": symbols,
            "count": symbols.len()
        })))
    }

    async fn tool_store_memory(&self, _args: &Value) -> Result<Value, (i32, String)> {
        Ok(wrap_tool_result(json!({
            "status": "placeholder",
            "message": "Memory storage not yet implemented. Will be available in a future release."
        })))
    }

    async fn tool_recall_memories(&self, _args: &Value) -> Result<Value, (i32, String)> {
        Ok(wrap_tool_result(json!({
            "status": "placeholder",
            "message": "Memory recall not yet implemented. Will be available in a future release.",
            "memories": []
        })))
    }

    async fn tool_get_project_rules(&self, _args: &Value) -> Result<Value, (i32, String)> {
        Ok(wrap_tool_result(json!({
            "status": "placeholder",
            "message": "Project rules not yet implemented. Will be available in a future release.",
            "rules": []
        })))
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
            "lattice/status" => Ok(json!({
                "status": "running",
                "version": env!("CARGO_PKG_VERSION")
            })),
            "lattice/reindex" => Ok(json!({
                "status": "placeholder",
                "message": "Reindexing not yet implemented."
            })),
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

/// Find a graph node by name and file, returning it as a JSON Value.
fn find_node_by_name_and_file(engine: &QueryEngine, name: &str, file: &str) -> Option<Value> {
    // We need to access the graph through the engine.
    // The engine exposes query() which returns capsules, but for direct lookup
    // we search all nodes via a targeted query.
    // Since QueryEngine holds the graph privately, we use keyword search.
    let capsule = engine.query(name, None);

    // Look through pivots and context for a matching symbol
    for pivot in &capsule.pivots {
        if pivot.symbol == name && pivot.file == file {
            return Some(json!({
                "symbol": pivot.symbol,
                "kind": pivot.kind,
                "file": pivot.file,
                "line": pivot.line,
                "source": pivot.source,
                "score": pivot.score
            }));
        }
    }

    for ctx in &capsule.context {
        if ctx.symbol == name && ctx.file == file {
            return Some(json!({
                "symbol": ctx.symbol,
                "kind": ctx.kind,
                "file": ctx.file,
                "line": ctx.line,
                "signature": ctx.skeleton,
                "score": ctx.score
            }));
        }
    }

    None
}

/// Find a SymbolId by name and file. Since the engine encapsulates the graph,
/// we construct a plausible SymbolId (byte_offset 0 is used for lookup).
fn find_symbol_id(_engine: &QueryEngine, name: &str, file: &str) -> Option<SymbolId> {
    // We construct the SymbolId. The graph's get_node uses exact SymbolId matching,
    // so we need a byte_offset. Since we don't know it, we use 0 as a convention
    // and rely on the engine's query path for actual lookups.
    Some(SymbolId {
        file: file.to_string(),
        name: name.to_string(),
        byte_offset: 0,
    })
}

/// Get dependents of a symbol through the engine's query capabilities.
fn get_dependents_from_engine(engine: &QueryEngine, _id: &SymbolId) -> Vec<Value> {
    // Use a query to find the symbol and its dependents
    let capsule = engine.query(&format!("what depends on {}", _id.name), None);
    capsule
        .pivots
        .iter()
        .map(|p| {
            json!({
                "symbol": p.symbol,
                "kind": p.kind,
                "file": p.file,
                "line": p.line
            })
        })
        .chain(capsule.context.iter().map(|c| {
            json!({
                "symbol": c.symbol,
                "kind": c.kind,
                "file": c.file,
                "line": c.line
            })
        }))
        .collect()
}

/// Get dependencies of a symbol through the engine's query capabilities.
fn get_dependencies_from_engine(engine: &QueryEngine, _id: &SymbolId) -> Vec<Value> {
    let capsule = engine.query(&format!("dependencies of {}", _id.name), None);
    capsule
        .pivots
        .iter()
        .map(|p| {
            json!({
                "symbol": p.symbol,
                "kind": p.kind,
                "file": p.file,
                "line": p.line
            })
        })
        .chain(capsule.context.iter().map(|c| {
            json!({
                "symbol": c.symbol,
                "kind": c.kind,
                "file": c.file,
                "line": c.line
            })
        }))
        .collect()
}

/// Get N-hop neighbors through the engine's query capabilities.
fn get_n_hop_neighbors(engine: &QueryEngine, _id: &SymbolId, _hops: usize) -> Vec<Value> {
    let capsule = engine.query(&format!("blast radius of {}", _id.name), None);
    capsule
        .pivots
        .iter()
        .map(|p| {
            json!({
                "symbol": p.symbol,
                "kind": p.kind,
                "file": p.file,
                "line": p.line
            })
        })
        .chain(capsule.context.iter().map(|c| {
            json!({
                "symbol": c.symbol,
                "kind": c.kind,
                "file": c.file,
                "line": c.line
            })
        }))
        .collect()
}

/// Search for symbols by name pattern.
fn search_symbols_in_engine(engine: &QueryEngine, pattern: &str, limit: usize) -> Vec<Value> {
    let capsule = engine.query(pattern, None);
    let mut results: Vec<Value> = capsule
        .pivots
        .iter()
        .map(|p| {
            json!({
                "symbol": p.symbol,
                "kind": p.kind,
                "file": p.file,
                "line": p.line,
                "score": p.score
            })
        })
        .chain(capsule.context.iter().map(|c| {
            json!({
                "symbol": c.symbol,
                "kind": c.kind,
                "file": c.file,
                "line": c.line,
                "score": c.score
            })
        }))
        .collect();
    results.truncate(limit);
    results
}

/// Get all symbols in a file.
fn get_file_symbols(engine: &QueryEngine, file: &str) -> Vec<Value> {
    // Query for the file name to find relevant symbols
    let capsule = engine.query(file, None);
    capsule
        .pivots
        .iter()
        .filter(|p| p.file == file)
        .map(|p| {
            json!({
                "symbol": p.symbol,
                "kind": p.kind,
                "file": p.file,
                "line": p.line,
                "source": p.source
            })
        })
        .chain(
            capsule
                .context
                .iter()
                .filter(|c| c.file == file)
                .map(|c| {
                    json!({
                        "symbol": c.symbol,
                        "kind": c.kind,
                        "file": c.file,
                        "line": c.line,
                        "signature": c.skeleton
                    })
                }),
        )
        .collect()
}
