use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

use lattice_core::embeddings::EmbeddingEngine;
use lattice_core::indexer::Indexer;
use lattice_core::intelligence::{
    diagnose_failure, expand_context, find_relevant_tests, find_stale_docs,
    get_backlinks, get_docs_capsule, get_outgoing_links, get_repo_playbook,
    get_working_set_context, impact_from_diff, prepare_change, summarize_subsystem,
    BundleMode, DiffImpactReport, DocsTargetKind, ExpandContextSeed, FailureDiagnosis,
    MemoryHighlight, RepoPlaybook, RulesDetector, SubsystemSummary, TaskBundle,
    WorkingSetContext,
};
use lattice_core::memory::{Memory, MemoryScope, MemoryStore, MemoryType};
use lattice_core::query::{ContextCapsule, QueryEngine};
use lattice_core::security::SecurityFilter;
use lattice_core::storage::GraphStore;
use lattice_core::watcher::should_index_file;
use lattice_core::workspace::WorkspaceManager;

use super::context_cache::ContextHandleCache;
use super::session_metrics::{SessionMetrics, SessionMetricsReport, ToolCallMetadata};
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
    context_cache: Arc<Mutex<ContextHandleCache>>,
    session_metrics: Arc<Mutex<SessionMetrics>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestedBundleMode {
    Auto,
    Compact,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowBudget {
    Auto,
    Tiny,
    Compact,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowWireFormat {
    Auto,
    Standard,
    Dense,
}

#[derive(Debug, Clone)]
struct WorkflowResponseOptions {
    budget: WorkflowBudget,
    max_tokens: Option<usize>,
    wire_format: WorkflowWireFormat,
}

#[derive(Debug, Clone)]
struct WorkflowRunMetadata {
    delivery_mode: String,
    wire_format: String,
    single_anchor_used: bool,
    _mode_reason: String,
    semantic_fallback_used: bool,
    outcome_memory_reuse_count: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct SessionPruningProfile {
    prefer_tiny: bool,
    prune_memory_highlights: bool,
    prefer_dense: bool,
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
        context_cache_path: PathBuf,
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
            context_cache: Arc::new(Mutex::new(ContextHandleCache::new_with_persistence(
                context_cache_path,
            ))),
            session_metrics: Arc::new(Mutex::new(SessionMetrics::new())),
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
                    "name": "prepare_change",
                    "description": "Agent-oriented change bundle: likely edit files, symbols, tests, memories, and risks for a coding task.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Natural language task such as 'fix login timeout' or 'add OAuth refresh'"
                            },
                            "entry_files": {
                                "type": "array",
                                "description": "Optional files to bias the change plan toward",
                                "items": { "type": "string" }
                            },
                            "entry_symbols": {
                                "type": "array",
                                "description": "Optional symbols to bias the change plan toward",
                                "items": { "type": "string" }
                            },
                            "mode": {
                                "type": "string",
                                "description": "Result mode: 'auto' (default), 'compact', or 'full'",
                                "enum": ["auto", "compact", "full"],
                                "default": "auto"
                            },
                            "budget": {
                                "type": "string",
                                "description": "Output budget: 'tiny', 'compact', or 'full' (default auto chooses for you)",
                                "enum": ["tiny", "compact", "full"]
                            },
                            "max_tokens": {
                                "type": "integer",
                                "description": "Optional approximate hard cap for the returned payload"
                            },
                            "wire_format": {
                                "type": "string",
                                "description": "Response wire format: 'standard' or 'dense' (optional; auto may choose dense for strict budgets)",
                                "enum": ["standard", "dense"]
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "find_relevant_tests",
                    "description": "Find tests that are most relevant to a set of files, symbols, or a diff.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "files": {
                                "type": "array",
                                "description": "Optional source files to anchor test selection",
                                "items": { "type": "string" }
                            },
                            "symbols": {
                                "type": "array",
                                "description": "Optional symbols to anchor test selection",
                                "items": { "type": "string" }
                            },
                            "diff": {
                                "type": "string",
                                "description": "Optional unified diff text; file paths will be extracted from it"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum tests to return (default: 8)",
                                "default": 8
                            }
                        }
                    }
                },
                {
                    "name": "impact_from_diff",
                    "description": "Analyze a unified diff to find changed symbols, downstream impact, risky areas, and relevant tests.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "diff": {
                                "type": "string",
                                "description": "Unified diff text to analyze"
                            },
                            "files": {
                                "type": "array",
                                "description": "Optional extra files to bias impact and test selection",
                                "items": { "type": "string" }
                            },
                            "symbols": {
                                "type": "array",
                                "description": "Optional extra symbols to bias impact and test selection",
                                "items": { "type": "string" }
                            },
                            "mode": {
                                "type": "string",
                                "description": "Result mode: 'auto' (default), 'compact', or 'full'",
                                "enum": ["auto", "compact", "full"],
                                "default": "auto"
                            },
                            "budget": {
                                "type": "string",
                                "description": "Output budget: 'tiny', 'compact', or 'full' (default auto chooses for you)",
                                "enum": ["tiny", "compact", "full"]
                            },
                            "max_tokens": {
                                "type": "integer",
                                "description": "Optional approximate hard cap for the returned payload"
                            },
                            "wire_format": {
                                "type": "string",
                                "description": "Response wire format: 'standard' or 'dense' (optional; auto may choose dense for strict budgets)",
                                "enum": ["standard", "dense"]
                            },
                            "hops": {
                                "type": "integer",
                                "description": "Dependent traversal depth (default: 2)",
                                "default": 2
                            }
                        },
                        "required": ["diff"]
                    }
                },
                {
                    "name": "get_working_set_context",
                    "description": "Build a compact working-set bundle from active files, focused symbols, recent memories, and likely tests.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Optional task hint used to bias nearby symbols and memory recall"
                            },
                            "files": {
                                "type": "array",
                                "description": "Files already in the active working set, such as open editors or recently changed files",
                                "items": { "type": "string" }
                            },
                            "symbols": {
                                "type": "array",
                                "description": "Focused symbols already in the working set",
                                "items": { "type": "string" }
                            },
                            "mode": {
                                "type": "string",
                                "description": "Result mode: 'auto' (default), 'compact', or 'full'",
                                "enum": ["auto", "compact", "full"],
                                "default": "auto"
                            },
                            "budget": {
                                "type": "string",
                                "description": "Output budget: 'tiny', 'compact', or 'full' (default auto chooses for you)",
                                "enum": ["tiny", "compact", "full"]
                            },
                            "max_tokens": {
                                "type": "integer",
                                "description": "Optional approximate hard cap for the returned payload"
                            },
                            "wire_format": {
                                "type": "string",
                                "description": "Response wire format: 'standard' or 'dense' (optional; auto may choose dense for strict budgets)",
                                "enum": ["standard", "dense"]
                            }
                        },
                        "required": []
                    }
                },
                {
                    "name": "summarize_subsystem",
                    "description": "Return a compressed subsystem map with key files, symbols, tests, and durable memory highlights.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Natural language subsystem or domain you want summarized"
                            },
                            "files": {
                                "type": "array",
                                "description": "Optional files to anchor the subsystem summary",
                                "items": { "type": "string" }
                            },
                            "symbols": {
                                "type": "array",
                                "description": "Optional symbols to anchor the subsystem summary",
                                "items": { "type": "string" }
                            },
                            "mode": {
                                "type": "string",
                                "description": "Result mode: 'auto' (default), 'compact', or 'full'",
                                "enum": ["auto", "compact", "full"],
                                "default": "auto"
                            },
                            "budget": {
                                "type": "string",
                                "description": "Output budget: 'tiny', 'compact', or 'full' (default auto chooses for you)",
                                "enum": ["tiny", "compact", "full"]
                            },
                            "max_tokens": {
                                "type": "integer",
                                "description": "Optional approximate hard cap for the returned payload"
                            },
                            "wire_format": {
                                "type": "string",
                                "description": "Response wire format: 'standard' or 'dense' (optional; auto may choose dense for strict budgets)",
                                "enum": ["standard", "dense"]
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "get_repo_playbook",
                    "description": "Return a compact repo playbook with architecture, conventions, high-signal files, and durable patterns.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "mode": {
                                "type": "string",
                                "description": "Result mode: 'auto' (default), 'compact', or 'full'",
                                "enum": ["auto", "compact", "full"],
                                "default": "auto"
                            },
                            "budget": {
                                "type": "string",
                                "description": "Output budget: 'tiny', 'compact', or 'full' (default auto chooses for you)",
                                "enum": ["tiny", "compact", "full"]
                            },
                            "max_tokens": {
                                "type": "integer",
                                "description": "Optional approximate hard cap for the returned payload"
                            },
                            "wire_format": {
                                "type": "string",
                                "description": "Response wire format: 'standard' or 'dense' (optional; auto may choose dense for strict budgets)",
                                "enum": ["standard", "dense"]
                            }
                        },
                        "required": []
                    }
                },
                {
                    "name": "get_docs_capsule",
                    "description": "Return the most relevant Markdown docs and sections for a query, plus related code symbols mentioned from those docs.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Natural language question or topic to find docs for"
                            },
                            "files": {
                                "type": "array",
                                "description": "Optional source files to bias the doc ranking toward",
                                "items": { "type": "string" }
                            },
                            "symbols": {
                                "type": "array",
                                "description": "Optional symbols to bias the doc ranking toward",
                                "items": { "type": "string" }
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum docs or sections to return (default: 6)",
                                "default": 6
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "get_backlinks",
                    "description": "Return inbound Markdown references to a symbol, file, document, or section.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "target": {
                                "type": "string",
                                "description": "Target symbol, file path, doc path, or section reference such as docs/guide.md#Setup"
                            },
                            "kind": {
                                "type": "string",
                                "description": "Optional target kind hint",
                                "enum": ["auto", "file", "symbol", "doc", "section"],
                                "default": "auto"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum backlinks to return (default: 12)",
                                "default": 12
                            }
                        },
                        "required": ["target"]
                    }
                },
                {
                    "name": "get_outgoing_links",
                    "description": "Return outgoing Markdown links and code mentions from a document, section, or file target.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "target": {
                                "type": "string",
                                "description": "Target doc path, section reference, symbol, or file path"
                            },
                            "kind": {
                                "type": "string",
                                "description": "Optional target kind hint",
                                "enum": ["auto", "file", "symbol", "doc", "section"],
                                "default": "auto"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum outgoing links to return (default: 12)",
                                "default": 12
                            }
                        },
                        "required": ["target"]
                    }
                },
                {
                    "name": "find_stale_docs",
                    "description": "Find Markdown docs and sections that likely need review because they mention changed symbols, changed files, or changed docs.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "files": {
                                "type": "array",
                                "description": "Changed file paths to check docs against",
                                "items": { "type": "string" }
                            },
                            "symbols": {
                                "type": "array",
                                "description": "Changed symbol names to check docs against",
                                "items": { "type": "string" }
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum stale docs or sections to return (default: 12)",
                                "default": 12
                            }
                        },
                        "required": []
                    }
                },
                {
                    "name": "diagnose_failure",
                    "description": "Turn compiler errors, failing tests, or stack traces into likely culprit symbols, nearby code, and suggested tests.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "input": {
                                "type": "string",
                                "description": "Raw error text, stack trace, failing test output, or compiler diagnostic"
                            },
                            "kind": {
                                "type": "string",
                                "description": "Optional hint such as 'compiler', 'test', or 'runtime'"
                            },
                            "mode": {
                                "type": "string",
                                "description": "Result mode: 'auto' (default), 'compact', or 'full'",
                                "enum": ["auto", "compact", "full"],
                                "default": "auto"
                            },
                            "budget": {
                                "type": "string",
                                "description": "Output budget: 'tiny', 'compact', or 'full' (default auto chooses for you)",
                                "enum": ["tiny", "compact", "full"]
                            },
                            "max_tokens": {
                                "type": "integer",
                                "description": "Optional approximate hard cap for the returned payload"
                            },
                            "wire_format": {
                                "type": "string",
                                "description": "Response wire format: 'standard' or 'dense' (optional; auto may choose dense for strict budgets)",
                                "enum": ["standard", "dense"]
                            }
                        },
                        "required": ["input"]
                    }
                },
                {
                    "name": "record_workflow_outcome",
                    "description": "Distill a successful or failed coding outcome into durable repo or branch memory for future agent workflows.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "task": {
                                "type": "string",
                                "description": "Short task label such as 'fix login timeout'"
                            },
                            "status": {
                                "type": "string",
                                "description": "Outcome status",
                                "enum": ["success", "failure"],
                                "default": "success"
                            },
                            "summary": {
                                "type": "string",
                                "description": "Optional terse summary of what worked or failed"
                            },
                            "context_handle": {
                                "type": "string",
                                "description": "Optional workflow context handle to inherit files, symbols, tests, and task intent"
                            },
                            "files": {
                                "type": "array",
                                "description": "Optional source files changed or confirmed relevant",
                                "items": { "type": "string" }
                            },
                            "symbols": {
                                "type": "array",
                                "description": "Optional symbols that proved relevant",
                                "items": { "type": "string" }
                            },
                            "tests": {
                                "type": "array",
                                "description": "Optional tests that verified the outcome",
                                "items": { "type": "string" }
                            }
                        },
                        "required": ["task"]
                    }
                },
                {
                    "name": "expand_context",
                    "description": "Expand a cached workflow handle into focused delta context for one symbol, file, test, or memory target.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "handle": {
                                "type": "string",
                                "description": "A context handle returned by a prior workflow tool such as prepare_change or get_working_set_context"
                            },
                            "focus": {
                                "type": "string",
                                "description": "Target to expand, such as symbol:loginUser, file:src/auth.ts, test:tests/auth.test.ts, or memory:0"
                            },
                            "max_tokens": {
                                "type": "integer",
                                "description": "Approximate maximum response size budget (default: 1200)",
                                "default": 1200
                            }
                        },
                        "required": ["handle", "focus"]
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
                            },
                            "linked_files": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "File paths this memory is linked to"
                            },
                            "scope": {
                                "type": "string",
                                "description": "Durability scope for the memory (default: session)",
                                "enum": ["session", "branch", "repo"],
                                "default": "session"
                            },
                            "workspace_id": {
                                "type": "string",
                                "description": "Optional workspace identifier; defaults to the current workspace root"
                            },
                            "branch": {
                                "type": "string",
                                "description": "Optional branch name for branch-scoped memories"
                            },
                            "refresh_key": {
                                "type": "string",
                                "description": "Optional freshness key used to group and refresh related memories"
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
                    "name": "get_session_metrics",
                    "description": "Inspect live assistant-session workflow metrics such as tool-call counts, payload sizes, handle reuse, and automatic memory writes.",
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
                    "name": "list_stale_memories",
                    "description": "List stale memories that likely need review or refresh.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Optional keyword filter applied to memory content, linked symbols, and linked files"
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
                    "name": "promote_observation",
                    "description": "Promote a session memory into longer-lived branch or repo scope and attach durable metadata.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "The memory ID to promote"
                            },
                            "scope": {
                                "type": "string",
                                "description": "Target durability scope",
                                "enum": ["branch", "repo"]
                            },
                            "linked_files": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Optional replacement linked-file list for the promoted memory"
                            },
                            "workspace_id": {
                                "type": "string",
                                "description": "Optional workspace identifier; defaults to the current workspace root"
                            },
                            "branch": {
                                "type": "string",
                                "description": "Optional branch name for branch-scoped memories"
                            },
                            "refresh_key": {
                                "type": "string",
                                "description": "Optional freshness key used to regroup related memories"
                            }
                        },
                        "required": ["id", "scope"]
                    }
                },
                {
                    "name": "refresh_memory",
                    "description": "Refresh an existing memory with fresh evidence and metadata while preserving its ID and clearing stale state.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "The memory ID to refresh"
                            },
                            "content": {
                                "type": "string",
                                "description": "Optional replacement content summarizing the refreshed memory"
                            },
                            "memory_type": {
                                "type": "string",
                                "description": "Optional replacement memory type",
                                "enum": ["observation", "decision", "exploration", "pattern", "anti_pattern"]
                            },
                            "scope": {
                                "type": "string",
                                "description": "Optional replacement durability scope",
                                "enum": ["session", "branch", "repo"]
                            },
                            "linked_symbols": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Optional replacement symbol links"
                            },
                            "linked_files": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Optional replacement file links"
                            },
                            "workspace_id": {
                                "type": "string",
                                "description": "Optional replacement workspace identifier"
                            },
                            "branch": {
                                "type": "string",
                                "description": "Optional replacement branch"
                            },
                            "refresh_key": {
                                "type": "string",
                                "description": "Optional replacement freshness key"
                            },
                            "source_query": {
                                "type": "string",
                                "description": "Optional query or note describing the new evidence"
                            },
                            "confidence": {
                                "type": "number",
                                "description": "Optional replacement confidence between 0.0 and 1.0"
                            }
                        },
                        "required": ["id"]
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

        let result = match tool_name {
            "get_context_capsule" | "query_context" => self.tool_query_context(arguments).await,
            "prepare_change" => self.tool_prepare_change(arguments).await,
            "find_relevant_tests" => self.tool_find_relevant_tests(arguments).await,
            "impact_from_diff" => self.tool_impact_from_diff(arguments).await,
            "get_working_set_context" => self.tool_get_working_set_context(arguments).await,
            "summarize_subsystem" => self.tool_summarize_subsystem(arguments).await,
            "get_repo_playbook" => self.tool_get_repo_playbook(arguments).await,
            "get_docs_capsule" => self.tool_get_docs_capsule(arguments).await,
            "get_backlinks" => self.tool_get_backlinks(arguments).await,
            "get_outgoing_links" => self.tool_get_outgoing_links(arguments).await,
            "find_stale_docs" => self.tool_find_stale_docs(arguments).await,
            "diagnose_failure" => self.tool_diagnose_failure(arguments).await,
            "record_workflow_outcome" => self.tool_record_workflow_outcome(arguments).await,
            "expand_context" => self.tool_expand_context(arguments).await,
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
            "list_stale_memories" => self.tool_list_stale_memories(arguments).await,
            "promote_observation" => self.tool_promote_observation(arguments).await,
            "refresh_memory" => self.tool_refresh_memory(arguments).await,
            "delete_observation" => self.tool_delete_observation(arguments).await,
            "update_observation" => self.tool_update_observation(arguments).await,
            "search_logic_flow" => self.tool_search_logic_flow(arguments).await,
            "submit_lsp_edges" => self.tool_submit_lsp_edges(arguments).await,
            "workspace_setup" => self.tool_workspace_setup(arguments).await,
            "index_status" => self.tool_index_status(arguments).await,
            "get_session_metrics" => self.tool_get_session_metrics(arguments).await,
            "get_project_rules" => self.tool_get_project_rules(arguments).await,
            _ => Err((-32602, format!("Unknown tool: {}", tool_name))),
        };

        if let Ok(ref value) = result {
            if tool_name != "get_session_metrics" {
                self.record_tool_metrics(tool_name, value).await;
            }
        }

        result
    }

    // ── Tool Implementations ──────────────────────────────────────────

    async fn tool_query_context(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: query".to_string()))?;
        let focused = args["mode"].as_str().unwrap_or("full") == "focused";

        // Embed query text if embedding engine is available (graceful fallback to keyword)
        let embedding = self
            .embedding_engine
            .get()
            .and_then(|eng| eng.embed(query).ok());

        let mut engine = self.engine.lock().await;
        let capsule = engine.query(query, embedding.as_deref(), focused);
        serde_json::to_value(&capsule)
            .map(|v| wrap_tool_result(v))
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))
    }

    async fn tool_prepare_change(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: query".to_string()))?;
        let requested_mode = parse_requested_bundle_mode(args);
        let response_options = parse_workflow_response_options(args);
        let entry_files = parse_string_array(args, "entry_files");
        let entry_symbols = parse_string_array(args, "entry_symbols");

        let embedding = self
            .embedding_engine
            .get()
            .and_then(|eng| eng.embed(query).ok());

        let (mut capsule, project_rules, semantic_fallback_used) = {
            let mut engine = self.engine.lock().await;
            let project_rules = detect_project_rules(engine.graph());
            let mut keyword_capsule = engine.query(query, None, false);
            let mut semantic_fallback_used = false;

            if should_try_prepare_change_semantic_fallback(
                &keyword_capsule,
                &entry_files,
                &entry_symbols,
            ) {
                if let Some(ref embedding) = embedding {
                    let semantic_capsule = engine.query(query, Some(embedding.as_slice()), false);
                    if prepare_change_capsule_quality(&semantic_capsule, &entry_files, &entry_symbols)
                        > prepare_change_capsule_quality(
                            &keyword_capsule,
                            &entry_files,
                            &entry_symbols,
                        )
                    {
                        keyword_capsule = semantic_capsule;
                        semantic_fallback_used = true;
                    }
                }
            }

            (keyword_capsule, project_rules, semantic_fallback_used)
        };

        capsule.memories = self
            .augment_memory_values_with_playbooks(
                query,
                &entry_files,
                &entry_symbols,
                capsule.memories,
                5,
            )
            .await?;
        let outcome_memory_reuse_count = count_outcome_memory_reuse(&capsule.memories);
        let (bundle, metadata) = {
            let engine = self.engine.lock().await;
            let compact_bundle = prepare_change(
                engine.graph(),
                &capsule,
                &entry_files,
                &entry_symbols,
                &project_rules,
                BundleMode::Compact,
            );
            let (delivery_mode, mode_reason) =
                select_task_bundle_mode(requested_mode, &compact_bundle);
            let bundle = if matches!(delivery_mode, BundleMode::Full) {
                prepare_change(
                    engine.graph(),
                    &capsule,
                    &entry_files,
                    &entry_symbols,
                    &project_rules,
                    BundleMode::Full,
                )
            } else {
                compact_bundle
            };

            (
                bundle,
                WorkflowRunMetadata {
                    delivery_mode: delivery_mode.as_str().to_string(),
                    wire_format: "standard".to_string(),
                    single_anchor_used: false,
                    _mode_reason: mode_reason,
                    semantic_fallback_used,
                    outcome_memory_reuse_count,
                },
            )
        };
        let handle = self
            .store_context_handle("prepare_change", seed_from_task_bundle(&bundle))
            .await;

        self.serialize_workflow_with_context_handle(
            "prepare_change",
            bundle,
            &handle,
            "prepare_change",
            &metadata,
            &response_options,
        )
        .await
    }

    async fn tool_find_relevant_tests(&self, args: &Value) -> Result<Value, (i32, String)> {
        let files = parse_string_array(args, "files");
        let symbols = parse_string_array(args, "symbols");
        let diff = args["diff"].as_str();
        let limit = (args["limit"].as_u64().unwrap_or(8) as usize).min(50);

        let engine = self.engine.lock().await;
        let project_rules = detect_project_rules(engine.graph());
        let report = find_relevant_tests(
            engine.graph(),
            &files,
            &symbols,
            diff,
            &project_rules,
            limit,
        );

        serde_json::to_value(&report)
            .map(|v| wrap_tool_result(v))
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))
    }

    async fn tool_impact_from_diff(&self, args: &Value) -> Result<Value, (i32, String)> {
        let diff = args["diff"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: diff".to_string()))?;
        let files = parse_string_array(args, "files");
        let symbols = parse_string_array(args, "symbols");
        let requested_mode = parse_requested_bundle_mode(args);
        let response_options = parse_workflow_response_options(args);
        let hops = (args["hops"].as_u64().unwrap_or(2) as usize).min(5);

        let (report, metadata) = {
            let engine = self.engine.lock().await;
            let project_rules = detect_project_rules(engine.graph());
            let compact_report = impact_from_diff(
                engine.graph(),
                diff,
                &files,
                &symbols,
                &project_rules,
                BundleMode::Compact,
                hops,
            );
            let (delivery_mode, mode_reason) =
                select_diff_impact_mode(requested_mode, &compact_report);
            let report = if matches!(delivery_mode, BundleMode::Full) {
                impact_from_diff(
                    engine.graph(),
                    diff,
                    &files,
                    &symbols,
                    &project_rules,
                    BundleMode::Full,
                    hops,
                )
            } else {
                compact_report
            };

            (
                report,
                WorkflowRunMetadata {
                    delivery_mode: delivery_mode.as_str().to_string(),
                    wire_format: "standard".to_string(),
                    single_anchor_used: false,
                    _mode_reason: mode_reason,
                    semantic_fallback_used: false,
                    outcome_memory_reuse_count: 0,
                },
            )
        };
        let handle = self
            .store_context_handle("impact_from_diff", seed_from_diff_impact(&report))
            .await;

        self.serialize_workflow_with_context_handle(
            "impact_from_diff",
            report,
            &handle,
            "impact_from_diff",
            &metadata,
            &response_options,
        )
        .await
    }

    async fn tool_get_working_set_context(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"].as_str();
        let files = parse_string_array(args, "files");
        let symbols = parse_string_array(args, "symbols");
        let requested_mode = parse_requested_bundle_mode(args);
        let response_options = parse_workflow_response_options(args);
        let memory_limit = if matches!(requested_mode, RequestedBundleMode::Full) {
            8
        } else {
            5
        };
        let memory_query = build_memory_query(query, &files, &symbols);

        let memories = self
            .augment_memory_values_with_playbooks(
                query.unwrap_or(memory_query.as_deref().unwrap_or("working set")),
                &files,
                &symbols,
                self.load_relevant_memory_values(query, &files, &symbols, memory_limit)
                    .await?,
                memory_limit,
            )
            .await?;
        let outcome_memory_reuse_count = count_outcome_memory_reuse(&memories);
        let (report, metadata) = {
            let engine = self.engine.lock().await;
            let project_rules = detect_project_rules(engine.graph());
            let compact_report = get_working_set_context(
                engine.graph(),
                &files,
                &symbols,
                query,
                &memories,
                &project_rules,
                BundleMode::Compact,
            );
            let (delivery_mode, mode_reason) =
                select_working_set_mode(requested_mode, &compact_report);
            let report = if matches!(delivery_mode, BundleMode::Full) {
                get_working_set_context(
                    engine.graph(),
                    &files,
                    &symbols,
                    query,
                    &memories,
                    &project_rules,
                    BundleMode::Full,
                )
            } else {
                compact_report
            };

            (
                report,
                WorkflowRunMetadata {
                    delivery_mode: delivery_mode.as_str().to_string(),
                    wire_format: "standard".to_string(),
                    single_anchor_used: false,
                    _mode_reason: mode_reason,
                    semantic_fallback_used: false,
                    outcome_memory_reuse_count,
                },
            )
        };
        let handle = self
            .store_context_handle(
                "get_working_set_context",
                seed_from_working_set_context(&report),
            )
            .await;

        self.serialize_workflow_with_context_handle(
            "get_working_set_context",
            report,
            &handle,
            "get_working_set_context",
            &metadata,
            &response_options,
        )
        .await
    }

    async fn tool_summarize_subsystem(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: query".to_string()))?;
        let mut files = parse_string_array(args, "files");
        let mut symbols = parse_string_array(args, "symbols");
        let requested_mode = parse_requested_bundle_mode(args);
        let response_options = parse_workflow_response_options(args);
        let memory_limit = if matches!(requested_mode, RequestedBundleMode::Full) {
            6
        } else {
            4
        };
        let embedding = self
            .embedding_engine
            .get()
            .and_then(|eng| eng.embed(query).ok());
        let mut memories = self
            .augment_memory_values_with_playbooks(
                query,
                &files,
                &symbols,
                self.load_relevant_memory_values(Some(query), &files, &symbols, memory_limit)
                    .await?,
                memory_limit,
            )
            .await?;

        let (mut compact_report, mut semantic_fallback_used) = {
            let engine = self.engine.lock().await;
            let project_rules = detect_project_rules(engine.graph());
            (
                summarize_subsystem(
                    engine.graph(),
                    query,
                    &files,
                    &symbols,
                    &memories,
                    &project_rules,
                    BundleMode::Compact,
                ),
                false,
            )
        };

        if should_try_subsystem_semantic_fallback(&compact_report, &files, &symbols) {
            if let Some(ref embedding) = embedding {
                let semantic_capsule = {
                    let mut engine = self.engine.lock().await;
                    engine.query(query, Some(embedding.as_slice()), false)
                };
                let candidate_files = merge_anchor_files_from_capsule(&files, &semantic_capsule);
                let candidate_symbols =
                    merge_anchor_symbols_from_capsule(&symbols, &semantic_capsule);

                if candidate_files != files || candidate_symbols != symbols {
                    let candidate_memories = self
                        .augment_memory_values_with_playbooks(
                            query,
                            &candidate_files,
                            &candidate_symbols,
                            self.load_relevant_memory_values(
                                Some(query),
                                &candidate_files,
                                &candidate_symbols,
                                memory_limit,
                            )
                            .await?,
                            memory_limit,
                        )
                        .await?;
                    let candidate_report = {
                        let engine = self.engine.lock().await;
                        let project_rules = detect_project_rules(engine.graph());
                        summarize_subsystem(
                            engine.graph(),
                            query,
                            &candidate_files,
                            &candidate_symbols,
                            &candidate_memories,
                            &project_rules,
                            BundleMode::Compact,
                        )
                    };

                    if subsystem_summary_quality(&candidate_report)
                        > subsystem_summary_quality(&compact_report)
                    {
                        files = candidate_files;
                        symbols = candidate_symbols;
                        memories = candidate_memories;
                        compact_report = candidate_report;
                        semantic_fallback_used = true;
                    }
                }
            }
        }

        let outcome_memory_reuse_count = count_outcome_memory_reuse(&memories);
        let (report, metadata) = {
            let engine = self.engine.lock().await;
            let project_rules = detect_project_rules(engine.graph());
            let (delivery_mode, mode_reason) =
                select_subsystem_summary_mode(requested_mode, &compact_report);
            let report = if matches!(delivery_mode, BundleMode::Full) {
                summarize_subsystem(
                    engine.graph(),
                    query,
                    &files,
                    &symbols,
                    &memories,
                    &project_rules,
                    BundleMode::Full,
                )
            } else {
                compact_report
            };

            (
                report,
                WorkflowRunMetadata {
                    delivery_mode: delivery_mode.as_str().to_string(),
                    wire_format: "standard".to_string(),
                    single_anchor_used: false,
                    _mode_reason: mode_reason,
                    semantic_fallback_used,
                    outcome_memory_reuse_count,
                },
            )
        };
        let handle = self
            .store_context_handle(
                "summarize_subsystem",
                seed_from_subsystem_summary(&report),
            )
            .await;

        let playbook_memory = self
            .auto_upsert_playbook_memory(
                format!(
                    "subsystem_playbook::{}",
                    stable_refresh_key(query, &files, &symbols)
                ),
                summarize_subsystem_memory_content(&report),
                report
                    .key_files
                    .iter()
                    .map(|item| item.file.clone())
                    .collect(),
                report
                    .key_symbols
                    .iter()
                    .map(|item| item.symbol.clone())
                    .collect(),
                Some(query.to_string()),
                true,
            )
            .await?;

        let mut value = serde_json::to_value(&report)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        attach_context_handle(&mut value, &handle, "summarize_subsystem");
        attach_playbook_memory(&mut value, playbook_memory);
        self.finalize_workflow_value("summarize_subsystem", value, &metadata, &response_options)
            .await
    }

    async fn tool_get_repo_playbook(&self, args: &Value) -> Result<Value, (i32, String)> {
        let requested_mode = parse_requested_bundle_mode(args);
        let response_options = parse_workflow_response_options(args);
        let memory_limit = if matches!(requested_mode, RequestedBundleMode::Full) {
            8
        } else {
            5
        };
        let memories = self.load_durable_memory_values(memory_limit).await?;
        let outcome_memory_reuse_count = count_outcome_memory_reuse(&memories);
        let (report, metadata) = {
            let engine = self.engine.lock().await;
            let project_rules = detect_project_rules(engine.graph());
            let compact_report =
                get_repo_playbook(engine.graph(), &memories, &project_rules, BundleMode::Compact);
            let (delivery_mode, mode_reason) =
                select_repo_playbook_mode(requested_mode, &compact_report);
            let report = if matches!(delivery_mode, BundleMode::Full) {
                get_repo_playbook(engine.graph(), &memories, &project_rules, BundleMode::Full)
            } else {
                compact_report
            };

            (
                report,
                WorkflowRunMetadata {
                    delivery_mode: delivery_mode.as_str().to_string(),
                    wire_format: "standard".to_string(),
                    single_anchor_used: false,
                    _mode_reason: mode_reason,
                    semantic_fallback_used: false,
                    outcome_memory_reuse_count,
                },
            )
        };
        let handle = self
            .store_context_handle("get_repo_playbook", seed_from_repo_playbook(&report))
            .await;

        let playbook_memory = self
            .auto_upsert_playbook_memory(
                "repo_playbook".to_string(),
                summarize_repo_playbook_memory_content(&report),
                report
                    .key_files
                    .iter()
                    .map(|item| item.file.clone())
                    .collect(),
                report
                    .notable_symbols
                    .iter()
                    .map(|item| item.symbol.clone())
                    .collect(),
                Some("repo playbook".to_string()),
                false,
            )
            .await?;

        let mut value = serde_json::to_value(&report)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        attach_context_handle(&mut value, &handle, "get_repo_playbook");
        attach_playbook_memory(&mut value, playbook_memory);
        self.finalize_workflow_value("get_repo_playbook", value, &metadata, &response_options)
            .await
    }

    async fn tool_get_docs_capsule(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: query".to_string()))?;
        let files = parse_string_array(args, "files");
        let symbols = parse_string_array(args, "symbols");
        let limit = (args["limit"].as_u64().unwrap_or(6) as usize).clamp(1, 20);

        let engine = self.engine.lock().await;
        let report = get_docs_capsule(engine.graph(), query, &files, &symbols, limit);
        let value = serde_json::to_value(&report)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        Ok(wrap_tool_result(value))
    }

    async fn tool_get_backlinks(&self, args: &Value) -> Result<Value, (i32, String)> {
        let target = args["target"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: target".to_string()))?;
        let kind = DocsTargetKind::from_str(args["kind"].as_str());
        let limit = (args["limit"].as_u64().unwrap_or(12) as usize).clamp(1, 50);

        let engine = self.engine.lock().await;
        let report = get_backlinks(engine.graph(), target, kind, limit).ok_or((
            -32602,
            format!("Unable to resolve backlinks target: {}", target),
        ))?;
        let value = serde_json::to_value(&report)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        Ok(wrap_tool_result(value))
    }

    async fn tool_get_outgoing_links(&self, args: &Value) -> Result<Value, (i32, String)> {
        let target = args["target"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: target".to_string()))?;
        let kind = DocsTargetKind::from_str(args["kind"].as_str());
        let limit = (args["limit"].as_u64().unwrap_or(12) as usize).clamp(1, 50);

        let engine = self.engine.lock().await;
        let report = get_outgoing_links(engine.graph(), target, kind, limit).ok_or((
            -32602,
            format!("Unable to resolve outgoing-links target: {}", target),
        ))?;
        let value = serde_json::to_value(&report)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        Ok(wrap_tool_result(value))
    }

    async fn tool_find_stale_docs(&self, args: &Value) -> Result<Value, (i32, String)> {
        let files = parse_string_array(args, "files");
        let symbols = parse_string_array(args, "symbols");
        let limit = (args["limit"].as_u64().unwrap_or(12) as usize).clamp(1, 50);

        let engine = self.engine.lock().await;
        let report = find_stale_docs(engine.graph(), &files, &symbols, limit);
        let value = serde_json::to_value(&report)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        Ok(wrap_tool_result(value))
    }

    async fn tool_diagnose_failure(&self, args: &Value) -> Result<Value, (i32, String)> {
        let input = args["input"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: input".to_string()))?;
        let kind = args["kind"].as_str();
        let requested_mode = parse_requested_bundle_mode(args);
        let response_options = parse_workflow_response_options(args);

        let (mut report, metadata_mode_reason) = {
            let engine = self.engine.lock().await;
            let project_rules = detect_project_rules(engine.graph());
            let compact_report =
                diagnose_failure(engine.graph(), input, kind, &project_rules, BundleMode::Compact);
            let (delivery_mode, mode_reason) =
                select_failure_diagnosis_mode(requested_mode, &compact_report);
            let report = if matches!(delivery_mode, BundleMode::Full) {
                diagnose_failure(engine.graph(), input, kind, &project_rules, BundleMode::Full)
            } else {
                compact_report
            };
            (report, (delivery_mode, mode_reason))
        };

        let memories = self
            .augment_memory_values_with_playbooks(
                input,
                &report.extracted_files,
                &report.extracted_symbols,
                self.load_relevant_memory_values(
                    Some(input),
                    &report.extracted_files,
                    &report.extracted_symbols,
                    4,
                )
                .await?,
                4,
            )
            .await?;
        let outcome_memory_reuse_count = count_outcome_memory_reuse(&memories);
        report.memory_highlights = report_memory_highlights(&memories, 1);
        report.overview = build_failure_overview_value(
            &report.overview,
            report.memory_highlights.first(),
        );
        let metadata = WorkflowRunMetadata {
            delivery_mode: metadata_mode_reason.0.as_str().to_string(),
            wire_format: "standard".to_string(),
            single_anchor_used: false,
            _mode_reason: metadata_mode_reason.1,
            semantic_fallback_used: false,
            outcome_memory_reuse_count,
        };
        let handle = self
            .store_context_handle("diagnose_failure", seed_from_failure_diagnosis(&report))
            .await;

        self.serialize_workflow_with_context_handle(
            "diagnose_failure",
            report,
            &handle,
            "diagnose_failure",
            &metadata,
            &response_options,
        )
        .await
    }

    async fn tool_record_workflow_outcome(&self, args: &Value) -> Result<Value, (i32, String)> {
        let task = args["task"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: task".to_string()))?;
        let status = args["status"].as_str().unwrap_or("success");
        let summary = args["summary"].as_str().map(|value| value.trim()).filter(|value| !value.is_empty());
        let mut files = parse_string_array(args, "files");
        let mut symbols = parse_string_array(args, "symbols");
        let mut tests = parse_string_array(args, "tests");
        let context_handle = args["context_handle"].as_str();

        let inherited_query = if let Some(handle) = context_handle {
            let cached = {
                let mut cache = self.context_cache.lock().await;
                cache.get(handle).ok_or((
                    -32602,
                    format!("Unknown or expired context handle: {}", handle),
                ))?
            };
            files.extend(cached.seed.files);
            symbols.extend(cached.seed.symbols);
            tests.extend(cached.seed.tests);
            cached.seed.query
        } else {
            None
        };

        files.retain(|file| is_queryable_workflow_file(file));
        tests.retain(|file| !file.trim().is_empty());
        dedupe_string_values(&mut files);
        dedupe_string_values(&mut symbols);
        dedupe_string_values(&mut tests);

        let refresh_key = format!("workflow_outcome::{}", stable_refresh_key(task, &files, &symbols));
        let source_query = summary
            .map(|value| value.to_string())
            .or(inherited_query)
            .or_else(|| Some(task.to_string()));
        let content = summarize_workflow_outcome_content(task, status, summary, &files, &symbols, &tests);
        let workspace_id = self.workspace_root.to_string_lossy().to_string();
        let branch = current_git_branch(&self.workspace_root);
        let scope = if branch.is_some() {
            MemoryScope::Branch
        } else {
            MemoryScope::Repo
        };

        let store = self.memory_store.lock().await;
        let existing = store
            .find_by_refresh_key(&refresh_key, Some(&workspace_id), branch.as_deref())
            .map_err(|e| (-32603, format!("Failed to find workflow outcome memory: {}", e)))?;

        let value = if let Some(existing) = existing {
            let refreshed = store
                .refresh_memory(
                    &existing.id,
                    Some(&content),
                    Some(MemoryType::Pattern),
                    Some(scope.clone()),
                    Some(&symbols),
                    Some(&files),
                    Some(&workspace_id),
                    branch.as_deref(),
                    Some(&refresh_key),
                    source_query.as_deref(),
                    Some(if status == "success" { 0.96 } else { 0.72 }),
                )
                .map_err(|e| (-32603, format!("Failed to refresh workflow outcome memory: {}", e)))?;
            json!({
                "status": "refreshed",
                "id": refreshed.id,
                "scope": refreshed.scope.as_str(),
                "refresh_key": refresh_key,
                "files": files,
                "symbols": symbols,
                "tests": tests,
            })
        } else {
            let id = store
                .store(Memory {
                    id: String::new(),
                    session_id: self.session_id.clone(),
                    content,
                    memory_type: MemoryType::Pattern,
                    scope: scope.clone(),
                    confidence: if status == "success" { 0.96 } else { 0.72 },
                    linked_symbols: symbols.clone(),
                    linked_files: files.clone(),
                    workspace_id: Some(workspace_id),
                    branch: branch.clone(),
                    refresh_key: Some(refresh_key.clone()),
                    source_query,
                    created_at: 0,
                    last_accessed: 0,
                    access_count: 0,
                    is_stale: false,
                    stale_reason: None,
                })
                .map_err(|e| (-32603, format!("Failed to store workflow outcome memory: {}", e)))?;
            json!({
                "status": "stored",
                "id": id,
                "scope": scope.as_str(),
                "refresh_key": refresh_key,
                "files": files,
                "symbols": symbols,
                "tests": tests,
            })
        };
        drop(store);
        self.record_auto_memory_write(1).await;
        self.record_outcome_pattern_write(1).await;

        Ok(wrap_tool_result(value))
    }

    async fn tool_expand_context(&self, args: &Value) -> Result<Value, (i32, String)> {
        let handle = args["handle"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: handle".to_string()))?;
        let focus = args["focus"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: focus".to_string()))?;
        let max_tokens = (args["max_tokens"].as_u64().unwrap_or(1200) as usize).clamp(200, 4000);

        let cached = {
            let mut cache = self.context_cache.lock().await;
            cache.get(handle).ok_or((
                -32602,
                format!("Unknown or expired context handle: {}", handle),
            ))?
        };

        let engine = self.engine.lock().await;
        let report = expand_context(engine.graph(), &cached.seed, focus, max_tokens);

        let mut value = serde_json::to_value(&report)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        attach_context_handle(&mut value, handle, &cached.origin);
        Ok(wrap_tool_result(value))
    }

    async fn store_context_handle(&self, origin: &str, seed: ExpandContextSeed) -> String {
        let mut cache = self.context_cache.lock().await;
        cache.insert(origin, seed)
    }

    async fn serialize_workflow_with_context_handle<T: serde::Serialize>(
        &self,
        tool_name: &str,
        report: T,
        handle: &str,
        origin: &str,
        metadata: &WorkflowRunMetadata,
        response_options: &WorkflowResponseOptions,
    ) -> Result<Value, (i32, String)> {
        let mut value = serde_json::to_value(&report)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        attach_context_handle(&mut value, handle, origin);
        self.finalize_workflow_value(tool_name, value, metadata, response_options)
            .await
    }

    async fn finalize_workflow_value(
        &self,
        tool_name: &str,
        mut value: Value,
        metadata: &WorkflowRunMetadata,
        response_options: &WorkflowResponseOptions,
    ) -> Result<Value, (i32, String)> {
        let pruning_profile = self.session_pruning_profile().await;
        let mut metadata = metadata.clone();
        let mut budget = select_workflow_budget(
            tool_name,
            &value,
            &metadata,
            response_options,
            pruning_profile,
        );
        apply_workflow_budget(tool_name, &mut value, budget, pruning_profile, &mut metadata);

        if let Some(max_tokens) = response_options.max_tokens {
            if approx_value_tokens(&value) > max_tokens {
                budget = WorkflowBudget::Tiny;
                apply_workflow_budget(
                    tool_name,
                    &mut value,
                    budget,
                    pruning_profile,
                    &mut metadata,
                );
                trim_value_for_token_budget(&mut value, max_tokens);
            }
        }

        let mut wire_format = select_workflow_wire_format(
            response_options,
            pruning_profile,
            budget,
            approx_value_tokens(&value),
        );
        if let Some(limit) = response_options.max_tokens {
            if approx_value_tokens(&value) > limit {
                wire_format = WorkflowWireFormat::Dense;
            }
        }

        metadata.wire_format = match wire_format {
            WorkflowWireFormat::Dense => "dense".to_string(),
            _ => "standard".to_string(),
        };
        attach_workflow_metadata(&mut value, &metadata);

        if matches!(wire_format, WorkflowWireFormat::Dense) {
            value = densify_workflow_value(value);
        }

        Ok(wrap_tool_result(value))
    }

    async fn session_pruning_profile(&self) -> SessionPruningProfile {
        let metrics = self.session_metrics.lock().await;
        derive_session_pruning_profile(&metrics.snapshot())
    }

    async fn load_relevant_memory_values(
        &self,
        query: Option<&str>,
        files: &[String],
        symbols: &[String],
        limit: usize,
    ) -> Result<Vec<Value>, (i32, String)> {
        let memory_query = build_memory_query(query, files, symbols);
        let store = self.memory_store.lock().await;

        let mut values: Vec<Value> = store
            .get_session_memories(&self.session_id, limit.min(3))
            .map_err(|e| (-32603, format!("Failed to load session memories: {}", e)))?
            .iter()
            .map(|memory| memory_to_value(memory, true))
            .collect();

        if values.len() < limit {
            let remaining = limit.saturating_sub(values.len());
            if let Some(ref keyword) = memory_query {
                let previous = store
                    .search_across_sessions(keyword, Some(&self.session_id), remaining)
                    .map_err(|e| (-32603, format!("Failed to search memories: {}", e)))?;
                values.extend(previous.iter().map(|memory| memory_to_value(memory, true)));
            }
        }

        dedupe_memory_values(&mut values);
        Ok(values)
    }

    async fn load_durable_memory_values(&self, limit: usize) -> Result<Vec<Value>, (i32, String)> {
        let workspace_id = self.workspace_root.to_string_lossy().to_string();
        let store = self.memory_store.lock().await;
        let mut memories = store
            .list_all()
            .map_err(|e| (-32603, format!("Failed to list memories: {}", e)))?;

        memories.retain(|memory| {
            !memory.is_stale
                && memory.scope != MemoryScope::Session
                && memory.workspace_id.as_deref() == Some(workspace_id.as_str())
        });
        memories.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.access_count.cmp(&a.access_count))
                .then_with(|| b.created_at.cmp(&a.created_at))
        });
        memories.truncate(limit.max(1));

        Ok(memories
            .iter()
            .map(|memory| memory_to_value(memory, true))
            .collect())
    }

    async fn augment_memory_values_with_playbooks(
        &self,
        query: &str,
        files: &[String],
        symbols: &[String],
        mut values: Vec<Value>,
        limit: usize,
    ) -> Result<Vec<Value>, (i32, String)> {
        let playbooks = self
            .load_playbook_memory_values(query, files, symbols)
            .await?;
        let outcomes = self
            .load_outcome_memory_values(query, files, symbols)
            .await?;
        values.splice(0..0, playbooks);
        values.splice(0..0, outcomes);
        dedupe_memory_values(&mut values);
        values.truncate(limit.max(1));
        Ok(values)
    }

    async fn load_playbook_memory_values(
        &self,
        query: &str,
        files: &[String],
        symbols: &[String],
    ) -> Result<Vec<Value>, (i32, String)> {
        let workspace_id = self.workspace_root.to_string_lossy().to_string();
        let branch = current_git_branch(&self.workspace_root);
        let subsystem_key = format!(
            "subsystem_playbook::{}",
            stable_refresh_key(query, files, symbols)
        );

        let store = self.memory_store.lock().await;
        let mut values = Vec::new();

        if let Some(memory) = store
            .find_by_refresh_key("repo_playbook", Some(&workspace_id), None)
            .map_err(|e| (-32603, format!("Failed to load repo playbook memory: {}", e)))?
        {
            values.push(memory_to_value(&memory, true));
        }

        if let Some(memory) = store
            .find_by_refresh_key(&subsystem_key, Some(&workspace_id), branch.as_deref())
            .map_err(|e| (-32603, format!("Failed to load subsystem playbook memory: {}", e)))?
        {
            values.push(memory_to_value(&memory, true));
        }

        Ok(values)
    }

    async fn load_outcome_memory_values(
        &self,
        query: &str,
        files: &[String],
        symbols: &[String],
    ) -> Result<Vec<Value>, (i32, String)> {
        let workspace_id = self.workspace_root.to_string_lossy().to_string();
        let branch = current_git_branch(&self.workspace_root);
        let refresh_key = format!("workflow_outcome::{}", stable_refresh_key(query, files, symbols));

        let store = self.memory_store.lock().await;
        let mut values = Vec::new();

        if let Some(memory) = store
            .find_by_refresh_key(&refresh_key, Some(&workspace_id), branch.as_deref())
            .map_err(|e| (-32603, format!("Failed to load workflow outcome memory: {}", e)))?
        {
            values.push(memory_to_value(&memory, true));
        }

        if branch.is_some() {
            if let Some(memory) = store
                .find_by_refresh_key(&refresh_key, Some(&workspace_id), None)
                .map_err(|e| (-32603, format!("Failed to load repo outcome memory: {}", e)))?
            {
                values.push(memory_to_value(&memory, true));
            }
        }

        if values.len() < 2 {
            if let Some(keyword) = build_memory_query(Some(query), files, symbols) {
                let mut searched = store
                    .search_across_sessions(&keyword, Some(&self.session_id), 2)
                    .map_err(|e| (-32603, format!("Failed to search outcome memories: {}", e)))?;
                searched.retain(|memory| {
                    memory.workspace_id.as_deref() == Some(workspace_id.as_str())
                        && memory
                            .refresh_key
                            .as_deref()
                            .map(|key| key.starts_with("workflow_outcome::"))
                            .unwrap_or(false)
                });
                values.extend(searched.iter().map(|memory| memory_to_value(memory, true)));
            }
        }

        dedupe_memory_values(&mut values);
        values.truncate(2);
        Ok(values)
    }

    async fn auto_upsert_playbook_memory(
        &self,
        refresh_key: String,
        content: String,
        linked_files: Vec<String>,
        linked_symbols: Vec<String>,
        source_query: Option<String>,
        prefer_branch_scope: bool,
    ) -> Result<Value, (i32, String)> {
        let workspace_id = self.workspace_root.to_string_lossy().to_string();
        let branch = current_git_branch(&self.workspace_root);
        let scope = if prefer_branch_scope && branch.is_some() {
            MemoryScope::Branch
        } else {
            MemoryScope::Repo
        };
        let scoped_branch = if scope == MemoryScope::Branch {
            branch.clone()
        } else {
            None
        };

        let store = self.memory_store.lock().await;
        let existing = store
            .find_by_refresh_key(
                &refresh_key,
                Some(&workspace_id),
                scoped_branch.as_deref(),
            )
            .map_err(|e| (-32603, format!("Failed to find playbook memory: {}", e)))?;

        let result = if let Some(existing) = existing {
            let refreshed = store
                .refresh_memory(
                    &existing.id,
                    Some(&content),
                    Some(MemoryType::Pattern),
                    Some(scope.clone()),
                    Some(&linked_symbols),
                    Some(&linked_files),
                    Some(&workspace_id),
                    scoped_branch.as_deref(),
                    Some(&refresh_key),
                    source_query.as_deref(),
                    Some(0.95),
                )
                .map_err(|e| (-32603, format!("Failed to refresh playbook memory: {}", e)))?;
            json!({
                "id": refreshed.id,
                "status": "refreshed",
                "scope": refreshed.scope.as_str(),
                "refresh_key": refresh_key
            })
        } else {
            let id = store
                .store(Memory {
                    id: String::new(),
                    session_id: self.session_id.clone(),
                    content,
                    memory_type: MemoryType::Pattern,
                    scope: scope.clone(),
                    confidence: 0.95,
                    linked_symbols,
                    linked_files,
                    workspace_id: Some(workspace_id),
                    branch: scoped_branch.clone(),
                    refresh_key: Some(refresh_key.clone()),
                    source_query,
                    created_at: 0,
                    last_accessed: 0,
                    access_count: 0,
                    is_stale: false,
                    stale_reason: None,
                })
                .map_err(|e| (-32603, format!("Failed to store playbook memory: {}", e)))?;
            json!({
                "id": id,
                "status": "stored",
                "scope": scope.as_str(),
                "refresh_key": refresh_key
            })
        };
        drop(store);
        self.record_auto_memory_write(1).await;

        Ok(result)
    }

    async fn record_auto_memory_write(&self, count: usize) {
        let mut metrics = self.session_metrics.lock().await;
        metrics.record_auto_memory_write(count);
    }

    async fn record_outcome_pattern_write(&self, count: usize) {
        let mut metrics = self.session_metrics.lock().await;
        metrics.record_outcome_pattern_write(count);
    }

    async fn record_tool_metrics(&self, tool_name: &str, value: &Value) {
        let (payload_bytes, approx_tokens, context_handle, context_origin, metadata) =
            extract_wrapped_tool_metrics(value);
        let mut metrics = self.session_metrics.lock().await;
        metrics.record_tool_call(
            tool_name,
            payload_bytes,
            approx_tokens,
            context_handle.as_deref(),
            context_origin.as_deref(),
            metadata,
        );
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

        let node = engine
            .graph()
            .all_nodes()
            .into_iter()
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
                        obj.insert(
                            "dep_list".to_string(),
                            json!(dependents
                                .iter()
                                .map(|(dep, edge)| json!({
                                    "s": dep.name, "f": dep.file, "e": edge.short_code()
                                }))
                                .collect::<Vec<_>>()),
                        );
                        obj.insert(
                            "deps_list".to_string(),
                            json!(dependencies
                                .iter()
                                .map(|(dep, edge)| json!({
                                    "s": dep.name, "f": dep.file, "e": edge.short_code()
                                }))
                                .collect::<Vec<_>>()),
                        );
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

        let node = engine
            .graph()
            .all_nodes()
            .into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let dependents = engine.graph().get_dependents(&n.id);
                let dep_values: Vec<Value> = dependents
                    .iter()
                    .map(|(dep, edge)| {
                        json!({
                            "s": dep.name,
                            "k": dep.kind.short_code(),
                            "f": dep.file,
                            "l": dep.line,
                            "e": edge.short_code()
                        })
                    })
                    .collect();

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

        let node = engine
            .graph()
            .all_nodes()
            .into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let dependencies = engine.graph().get_dependencies(&n.id);
                let dep_values: Vec<Value> = dependencies
                    .iter()
                    .map(|(dep, edge)| {
                        json!({
                            "s": dep.name,
                            "k": dep.kind.short_code(),
                            "f": dep.file,
                            "l": dep.line,
                            "e": edge.short_code()
                        })
                    })
                    .collect();

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

        let node = engine
            .graph()
            .all_nodes()
            .into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let affected = engine.graph().get_transitive_dependents(&n.id, hops);
                let affected_files: HashSet<&str> =
                    affected.iter().map(|a| a.file.as_str()).collect();

                let affected_values: Vec<Value> = affected
                    .iter()
                    .map(|a| {
                        json!({
                            "s": a.name,
                            "k": a.kind.short_code(),
                            "f": a.file,
                            "l": a.line
                        })
                    })
                    .collect();

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

        let mut results: Vec<Value> = engine
            .graph()
            .all_nodes()
            .into_iter()
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
        let symbols: Vec<Value> = file_nodes
            .iter()
            .map(|n| {
                let dep_count = engine.graph().get_dependents(&n.id).len();
                json!({
                    "symbol": n.name,
                    "kind": n.kind.short_code(),
                    "line": n.line,
                    "exported": n.is_exported,
                    "dependents": dep_count
                })
            })
            .collect();

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
        let scope =
            parse_memory_scope(args["scope"].as_str()).map_err(|message| (-32602, message))?;

        let linked_symbols = parse_string_array(args, "linked_symbols");
        let linked_files = parse_string_array(args, "linked_files");
        let workspace_id = args["workspace_id"]
            .as_str()
            .map(|value| value.to_string())
            .or_else(|| Some(self.workspace_root.to_string_lossy().to_string()));
        let branch = args["branch"].as_str().map(|value| value.to_string());
        let refresh_key = args["refresh_key"].as_str().map(|value| value.to_string());

        let memory = Memory {
            id: String::new(),
            session_id: self.session_id.clone(),
            content: content.to_string(),
            memory_type: memory_type.clone(),
            scope: scope.clone(),
            confidence: 1.0,
            linked_symbols: linked_symbols.clone(),
            linked_files: linked_files.clone(),
            workspace_id: workspace_id.clone(),
            branch: branch.clone(),
            refresh_key: refresh_key.clone(),
            source_query: None,
            created_at: 0,
            last_accessed: 0,
            access_count: 0,
            is_stale: false,
            stale_reason: None,
        };

        let store = self.memory_store.lock().await;
        let id = store
            .store(memory)
            .map_err(|e| (-32603, format!("Failed to store memory: {}", e)))?;

        Ok(wrap_tool_result(json!({
            "status": "stored",
            "id": id,
            "memory_type": memory_type.as_str(),
            "scope": scope.as_str(),
            "linked_symbols": linked_symbols,
            "linked_files": linked_files,
            "workspace_id": workspace_id,
            "branch": branch,
            "refresh_key": refresh_key
        })))
    }

    async fn tool_get_session_context(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"].as_str();
        let limit = (args["limit"].as_u64().unwrap_or(20) as usize).min(100);

        let store = self.memory_store.lock().await;

        // Current session memories (always included)
        let current = store
            .get_session_memories(&self.session_id, limit)
            .map_err(|e| (-32603, format!("Failed to get session memories: {}", e)))?;

        // Previous session memories: if query provided, search; otherwise get recent across sessions
        let remaining = limit.saturating_sub(current.len());
        let previous = if remaining > 0 {
            let keyword = query.unwrap_or("");
            if keyword.is_empty() {
                // Get recent memories from other sessions
                store
                    .search_across_sessions("", Some(&self.session_id), remaining)
                    .unwrap_or_default()
            } else {
                store
                    .search_across_sessions(keyword, Some(&self.session_id), remaining)
                    .unwrap_or_default()
            }
        } else {
            vec![]
        };

        let current_values: Vec<Value> =
            current.iter().map(|m| memory_to_value(m, false)).collect();
        let previous_values: Vec<Value> =
            previous.iter().map(|m| memory_to_value(m, true)).collect();

        Ok(wrap_tool_result(json!({
            "session_id": self.session_id,
            "current": current_values,
            "previous": previous_values,
            "counts": {
                "current": current.len(),
                "previous": previous.len()
            }
        })))
    }

    async fn tool_search_memory(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: query".to_string()))?;
        let limit = (args["limit"].as_u64().unwrap_or(10) as usize).min(100);

        let store = self.memory_store.lock().await;
        let memories = store
            .search_across_sessions(query, None, limit)
            .map_err(|e| (-32603, format!("Failed to search memories: {}", e)))?;

        let memory_values: Vec<Value> = memories.iter().map(|m| memory_to_value(m, true)).collect();

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
            store
                .get_session_memories(sid, limit)
                .map_err(|e| (-32603, format!("Failed to list observations: {}", e)))?
        } else {
            let all = store
                .list_all()
                .map_err(|e| (-32603, format!("Failed to list observations: {}", e)))?;
            all.into_iter().take(limit).collect()
        };

        let entries: Vec<Value> = memories.iter().map(|m| memory_to_value(m, true)).collect();

        Ok(wrap_tool_result(json!({
            "count": entries.len(),
            "memories": entries
        })))
    }

    async fn tool_list_stale_memories(&self, args: &Value) -> Result<Value, (i32, String)> {
        let query = args["query"].as_str();
        let limit = (args["limit"].as_u64().unwrap_or(50) as usize).min(200);

        let store = self.memory_store.lock().await;
        let memories = store
            .list_stale(query, limit)
            .map_err(|e| (-32603, format!("Failed to list stale memories: {}", e)))?;

        let entries: Vec<Value> = memories.iter().map(|m| memory_to_value(m, true)).collect();

        Ok(wrap_tool_result(json!({
            "count": entries.len(),
            "query": query,
            "memories": entries
        })))
    }

    async fn tool_promote_observation(&self, args: &Value) -> Result<Value, (i32, String)> {
        let id = args["id"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: id".to_string()))?;
        let scope =
            parse_memory_scope(args["scope"].as_str()).map_err(|message| (-32602, message))?;
        let linked_files = parse_optional_string_array(args, "linked_files");
        let workspace_id = args["workspace_id"]
            .as_str()
            .map(|value| value.to_string())
            .or_else(|| Some(self.workspace_root.to_string_lossy().to_string()));
        let branch = args["branch"].as_str().map(|value| value.to_string());
        let refresh_key = args["refresh_key"].as_str().map(|value| value.to_string());

        let store = self.memory_store.lock().await;
        store
            .promote_memory(
                id,
                scope.clone(),
                linked_files.as_deref(),
                workspace_id.as_deref(),
                branch.as_deref(),
                refresh_key.as_deref(),
            )
            .map_err(|e| (-32603, format!("Failed to promote observation: {}", e)))?;

        Ok(wrap_tool_result(json!({
            "status": "promoted",
            "id": id,
            "scope": scope.as_str(),
            "linked_files": linked_files,
            "workspace_id": workspace_id,
            "branch": branch,
            "refresh_key": refresh_key
        })))
    }

    async fn tool_refresh_memory(&self, args: &Value) -> Result<Value, (i32, String)> {
        let id = args["id"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: id".to_string()))?;
        let content = args["content"].as_str();
        let memory_type = args["memory_type"].as_str().map(MemoryType::from_str);
        let scope = if args.get("scope").is_some() {
            Some(parse_memory_scope(args["scope"].as_str()).map_err(|message| (-32602, message))?)
        } else {
            None
        };
        let linked_symbols = parse_optional_string_array(args, "linked_symbols");
        let linked_files = parse_optional_string_array(args, "linked_files");
        let workspace_id = args["workspace_id"].as_str();
        let branch = args["branch"].as_str();
        let refresh_key = args["refresh_key"].as_str();
        let source_query = args["source_query"].as_str();
        let confidence = args["confidence"].as_f64();

        if let Some(confidence) = confidence {
            if !(0.0..=1.0).contains(&confidence) {
                return Err((
                    -32602,
                    "Invalid confidence: expected a value between 0.0 and 1.0".to_string(),
                ));
            }
        }

        let has_updates = content.is_some()
            || memory_type.is_some()
            || scope.is_some()
            || linked_symbols.is_some()
            || linked_files.is_some()
            || workspace_id.is_some()
            || branch.is_some()
            || refresh_key.is_some()
            || source_query.is_some()
            || confidence.is_some();

        if !has_updates {
            return Err((
                -32602,
                "refresh_memory requires at least one field to refresh".to_string(),
            ));
        }

        let store = self.memory_store.lock().await;
        let memory = store
            .refresh_memory(
                id,
                content,
                memory_type,
                scope,
                linked_symbols.as_deref(),
                linked_files.as_deref(),
                workspace_id,
                branch,
                refresh_key,
                source_query,
                confidence,
            )
            .map_err(|e| (-32603, format!("Failed to refresh memory: {}", e)))?;

        Ok(wrap_tool_result(json!({
            "status": "refreshed",
            "memory": memory_to_value(&memory, true)
        })))
    }

    async fn tool_delete_observation(&self, args: &Value) -> Result<Value, (i32, String)> {
        let id = args["id"]
            .as_str()
            .ok_or((-32602, "Missing required parameter: id".to_string()))?;

        let store = self.memory_store.lock().await;
        store
            .invalidate(id)
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
        store
            .update_content(id, content)
            .map_err(|e| (-32603, format!("Failed to update observation: {}", e)))?;

        Ok(wrap_tool_result(json!({
            "status": "updated",
            "id": id
        })))
    }

    async fn tool_submit_lsp_edges(&self, args: &Value) -> Result<Value, (i32, String)> {
        let edges = args["edges"].as_array().ok_or((
            -32602,
            "Missing required parameter: edges (array)".to_string(),
        ))?;

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
            let from_id = graph
                .all_nodes()
                .iter()
                .find(|n| n.name == from_name && n.file == from_file)
                .map(|n| n.id.clone());
            let to_id = graph
                .all_nodes()
                .iter()
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
        let mut lang_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for node in &all_nodes {
            file_set.insert(node.file.clone());
            *lang_counts
                .entry(format!("{:?}", node.language))
                .or_insert(0) += 1;
        }
        let files: Vec<String> = file_set.into_iter().collect();

        // Detect project rules
        let detector = lattice_core::intelligence::RulesDetector::new();
        let rules = detector.detect_rules(&files);

        if format == "markdown" {
            let mut md = String::new();
            md.push_str(&format!("# Workspace Setup\n\n"));
            md.push_str(&format!(
                "**Files:** {} | **Symbols:** {} | **Edges:** {}\n\n",
                stats.file_count, stats.node_count, stats.edge_count
            ));
            md.push_str("## Languages\n\n");
            for (lang, count) in &lang_counts {
                md.push_str(&format!("- {}: {} symbols\n", lang, count));
            }
            md.push_str("\n## Detected Conventions\n\n");
            for rule in &rules {
                md.push_str(&format!(
                    "- {} (confidence: {:.0}%, {} occurrences)\n",
                    rule.description,
                    rule.confidence * 100.0,
                    rule.occurrences
                ));
            }
            Ok(wrap_tool_result(json!({ "markdown": md })))
        } else {
            let rule_values: Vec<Value> = rules
                .iter()
                .map(|r| {
                    json!({
                        "description": r.description,
                        "confidence": r.confidence,
                        "occurrences": r.occurrences,
                    })
                })
                .collect();

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

        let mut lang_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for node in &all_nodes {
            *lang_counts
                .entry(format!("{:?}", node.language))
                .or_insert(0) += 1;
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
            let roots: Vec<String> = self
                .workspace_roots
                .iter()
                .map(|r| r.to_string_lossy().to_string())
                .collect();
            obj.insert("workspaces".to_string(), json!(roots));
            obj.insert("multi_repo".to_string(), json!(true));

            if let Some(wm) = &self.workspace_manager {
                let wm = wm.lock().await;
                let repo_stats: Vec<Value> = wm
                    .repo_stats()
                    .iter()
                    .map(|s| {
                        json!({
                            "name": s.name,
                            "files": s.file_count,
                            "nodes": s.node_count,
                            "edges": s.edge_count
                        })
                    })
                    .collect();
                obj.insert("repos".to_string(), json!(repo_stats));
            }
        }

        Ok(wrap_tool_result(result))
    }

    async fn tool_get_session_metrics(&self, _args: &Value) -> Result<Value, (i32, String)> {
        let metrics = self.session_metrics.lock().await;
        let report = metrics.snapshot();
        serde_json::to_value(&report)
            .map(|value| wrap_tool_result(value))
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))
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
            (None, _) => {
                return Ok(wrap_tool_result(json!({
                    "error": format!("Source symbol '{}' not found", from_name)
                })))
            }
            (_, None) => {
                return Ok(wrap_tool_result(json!({
                    "error": format!("Target symbol '{}' not found", to_name)
                })))
            }
        };

        let paths = engine
            .graph()
            .find_call_paths(&from_node.id, &to_node.id, max_depth, 10);

        let path_values: Vec<Value> = paths
            .iter()
            .map(|path| {
                json!(path
                    .iter()
                    .map(|n| json!({
                        "s": n.name,
                        "f": n.file,
                        "l": n.line,
                        "k": n.kind.short_code()
                    }))
                    .collect::<Vec<_>>())
            })
            .collect();

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

        let symbols: Vec<Value> = file_nodes
            .iter()
            .map(|n| {
                let dependents = engine.graph().get_dependents(&n.id);
                let dependent_files: HashSet<&str> =
                    dependents.iter().map(|(d, _)| d.file.as_str()).collect();

                json!({
                    "name": n.name,
                    "line": n.line,
                    "dependentCount": dependents.len(),
                    "fileCount": dependent_files.len()
                })
            })
            .collect();

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
        let node = engine
            .graph()
            .all_nodes()
            .into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let dependents = engine.graph().get_dependents(&n.id);
                let cross_repo_count = 0usize; // Cross-repo is 0 for single workspace

                // Top 3 callers: dependents that call this symbol
                let top_callers: Vec<String> = dependents
                    .iter()
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
        let node = engine
            .graph()
            .all_nodes()
            .into_iter()
            .find(|n| n.name == name && n.file == file);

        match node {
            Some(n) => {
                let dependents = engine.graph().get_dependents(&n.id);
                let dep_values: Vec<Value> = dependents
                    .iter()
                    .map(|(dep, edge)| {
                        json!({
                            "name": dep.name,
                            "file": dep.file,
                            "line": dep.line,
                            "edge": format!("{:?}", edge)
                        })
                    })
                    .collect();

                Ok(json!(dep_values))
            }
            None => Err((-32602, format!("Symbol '{}' not found in '{}'", name, file))),
        }
    }

    /// Handle `lattice/clear_memory` or `lattice/clear` — clear all memories.
    async fn handle_clear_memory(&self) -> Result<Value, (i32, String)> {
        let store = self.memory_store.lock().await;
        let cleared = store
            .clear_all()
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
                        Err(_) => {
                            errors += 1;
                        }
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
            tracing::info!(
                "Reindex complete: {} files indexed, {} errors",
                files_indexed,
                errors
            );
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
    let matches_file =
        |n: &&lattice_core::graph::model::GraphNode| file_filter.map_or(true, |f| n.file == f);

    // 1. Exact match
    if let Some(node) = nodes.iter().find(|n| n.name == name && matches_file(n)) {
        return Some(node);
    }

    // 2. Suffix match: "is_blacklisted" matches "TokenBlacklist.is_blacklisted"
    let dot_suffix = format!(".{}", name);
    if let Some(node) = nodes
        .iter()
        .find(|n| n.name.ends_with(&dot_suffix) && matches_file(n))
    {
        return Some(node);
    }

    // 3. Case-insensitive exact match
    let name_lower = name.to_lowercase();
    if let Some(node) = nodes
        .iter()
        .find(|n| n.name.to_lowercase() == name_lower && matches_file(n))
    {
        return Some(node);
    }

    // 4. Case-insensitive suffix match
    let dot_suffix_lower = format!(".{}", name_lower);
    nodes
        .iter()
        .find(|n| n.name.to_lowercase().ends_with(&dot_suffix_lower) && matches_file(n))
        .copied()
}

fn parse_string_array(args: &Value, key: &str) -> Vec<String> {
    args[key]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_optional_string_array(args: &Value, key: &str) -> Option<Vec<String>> {
    if !args.get(key).map(|value| value.is_array()).unwrap_or(false) {
        return None;
    }

    Some(parse_string_array(args, key))
}

fn parse_memory_scope(value: Option<&str>) -> Result<MemoryScope, String> {
    match value.unwrap_or("session") {
        "session" => Ok(MemoryScope::Session),
        "branch" => Ok(MemoryScope::Branch),
        "repo" => Ok(MemoryScope::Repo),
        other => Err(format!(
            "Invalid scope '{}': expected 'session', 'branch', or 'repo'",
            other
        )),
    }
}

fn parse_requested_bundle_mode(args: &Value) -> RequestedBundleMode {
    match args["mode"].as_str().unwrap_or("auto") {
        "full" => RequestedBundleMode::Full,
        "compact" => RequestedBundleMode::Compact,
        _ => RequestedBundleMode::Auto,
    }
}

fn parse_workflow_response_options(args: &Value) -> WorkflowResponseOptions {
    let budget = match args["budget"].as_str() {
        Some("tiny") => WorkflowBudget::Tiny,
        Some("compact") => WorkflowBudget::Compact,
        Some("full") => WorkflowBudget::Full,
        _ => WorkflowBudget::Auto,
    };
    let wire_format = match args["wire_format"].as_str() {
        Some("dense") => WorkflowWireFormat::Dense,
        Some("standard") => WorkflowWireFormat::Standard,
        _ => WorkflowWireFormat::Auto,
    };
    let max_tokens = args["max_tokens"]
        .as_u64()
        .map(|value| (value as usize).clamp(80, 4000));

    WorkflowResponseOptions {
        budget,
        max_tokens,
        wire_format,
    }
}

fn select_task_bundle_mode(
    requested: RequestedBundleMode,
    compact: &TaskBundle,
) -> (BundleMode, String) {
    match requested {
        RequestedBundleMode::Compact => (BundleMode::Compact, "requested compact mode".to_string()),
        RequestedBundleMode::Full => (BundleMode::Full, "requested full mode".to_string()),
        RequestedBundleMode::Auto => {
            if let Some(reason) = task_bundle_widen_reason(compact) {
                (
                    BundleMode::Full,
                    format!("widened automatically because {}", reason),
                )
            } else {
                (
                    BundleMode::Compact,
                    "kept compact because likely edit anchors were already strong".to_string(),
                )
            }
        }
    }
}

fn select_diff_impact_mode(
    requested: RequestedBundleMode,
    compact: &DiffImpactReport,
) -> (BundleMode, String) {
    match requested {
        RequestedBundleMode::Compact => (BundleMode::Compact, "requested compact mode".to_string()),
        RequestedBundleMode::Full => (BundleMode::Full, "requested full mode".to_string()),
        RequestedBundleMode::Auto => {
            if let Some(reason) = diff_impact_widen_reason(compact) {
                (
                    BundleMode::Full,
                    format!("widened automatically because {}", reason),
                )
            } else {
                (
                    BundleMode::Compact,
                    "kept compact because the diff already mapped to concrete files and symbols"
                        .to_string(),
                )
            }
        }
    }
}

fn select_working_set_mode(
    requested: RequestedBundleMode,
    compact: &WorkingSetContext,
) -> (BundleMode, String) {
    match requested {
        RequestedBundleMode::Compact => (BundleMode::Compact, "requested compact mode".to_string()),
        RequestedBundleMode::Full => (BundleMode::Full, "requested full mode".to_string()),
        RequestedBundleMode::Auto => {
            if let Some(reason) = working_set_widen_reason(compact) {
                (
                    BundleMode::Full,
                    format!("widened automatically because {}", reason),
                )
            } else {
                (
                    BundleMode::Compact,
                    "kept compact because the working set already had enough local context"
                        .to_string(),
                )
            }
        }
    }
}

fn select_subsystem_summary_mode(
    requested: RequestedBundleMode,
    compact: &SubsystemSummary,
) -> (BundleMode, String) {
    match requested {
        RequestedBundleMode::Compact => (BundleMode::Compact, "requested compact mode".to_string()),
        RequestedBundleMode::Full => (BundleMode::Full, "requested full mode".to_string()),
        RequestedBundleMode::Auto => {
            if let Some(reason) = subsystem_summary_widen_reason(compact) {
                (
                    BundleMode::Full,
                    format!("widened automatically because {}", reason),
                )
            } else {
                (
                    BundleMode::Compact,
                    "kept compact because the subsystem map already had enough coverage"
                        .to_string(),
                )
            }
        }
    }
}

fn select_repo_playbook_mode(
    requested: RequestedBundleMode,
    compact: &RepoPlaybook,
) -> (BundleMode, String) {
    match requested {
        RequestedBundleMode::Compact => (BundleMode::Compact, "requested compact mode".to_string()),
        RequestedBundleMode::Full => (BundleMode::Full, "requested full mode".to_string()),
        RequestedBundleMode::Auto => {
            if let Some(reason) = repo_playbook_widen_reason(compact) {
                (
                    BundleMode::Full,
                    format!("widened automatically because {}", reason),
                )
            } else {
                (
                    BundleMode::Compact,
                    "kept compact because architecture and conventions were already well covered"
                        .to_string(),
                )
            }
        }
    }
}

fn select_failure_diagnosis_mode(
    requested: RequestedBundleMode,
    compact: &FailureDiagnosis,
) -> (BundleMode, String) {
    match requested {
        RequestedBundleMode::Compact => (BundleMode::Compact, "requested compact mode".to_string()),
        RequestedBundleMode::Full => (BundleMode::Full, "requested full mode".to_string()),
        RequestedBundleMode::Auto => {
            if let Some(reason) = failure_diagnosis_widen_reason(compact) {
                (
                    BundleMode::Full,
                    format!("widened automatically because {}", reason),
                )
            } else {
                (
                    BundleMode::Compact,
                    "kept compact because likely culprits and next steps were already clear"
                        .to_string(),
                )
            }
        }
    }
}

fn task_bundle_widen_reason(bundle: &TaskBundle) -> Option<&'static str> {
    let high_primary = bundle
        .primary_files
        .iter()
        .filter(|item| item.confidence_band == "high")
        .count();
    let high_symbols = bundle
        .symbols
        .iter()
        .filter(|item| item.confidence_band == "high")
        .count();

    if bundle.primary_files.is_empty() {
        Some("no primary edit files were identified")
    } else if high_primary == 0 && high_symbols == 0 {
        Some("the likely edit area is still low-confidence")
    } else if bundle.tests.is_empty() && bundle.primary_files.len() <= 1 && bundle.symbols.len() < 2 {
        Some("supporting symbols and tests were still sparse")
    } else {
        None
    }
}

fn diff_impact_widen_reason(report: &DiffImpactReport) -> Option<&'static str> {
    if report.changed_symbols.is_empty() && report.affected_symbols.len() < 2 {
        Some("the compact diff view did not resolve enough changed or affected symbols")
    } else if report.tests.is_empty() && report.risks.is_empty() && report.changed_files.len() <= 1 {
        Some("the compact diff view lacked downstream risk or test context")
    } else {
        None
    }
}

fn working_set_widen_reason(report: &WorkingSetContext) -> Option<&'static str> {
    if report.files.is_empty() {
        Some("the working set did not produce any anchored files")
    } else if report.files.len() == 1
        && report.active_symbols.len() + report.nearby_symbols.len() < 3
        && report.tests.is_empty()
    {
        Some("the working set was still too thin to avoid additional lookups")
    } else {
        None
    }
}

fn subsystem_summary_widen_reason(report: &SubsystemSummary) -> Option<&'static str> {
    if report.key_files.len() < 2 {
        Some("the compact summary surfaced too few key files")
    } else if report.key_symbols.is_empty() && report.tests.is_empty() && report.memories.is_empty() {
        Some("the compact summary lacked symbol, test, and memory coverage")
    } else {
        None
    }
}

fn repo_playbook_widen_reason(report: &RepoPlaybook) -> Option<&'static str> {
    if report.key_files.len() < 2 {
        Some("the compact playbook surfaced too few anchor files")
    } else if report.notable_symbols.is_empty() && report.conventions.len() < 2 {
        Some("the compact playbook lacked notable symbols and conventions")
    } else {
        None
    }
}

fn failure_diagnosis_widen_reason(report: &FailureDiagnosis) -> Option<&'static str> {
    let top_suspect_high = report
        .suspects
        .first()
        .map(|item| item.confidence_band == "high")
        .unwrap_or(false);

    if report.suspects.is_empty() {
        Some("the compact diagnosis did not identify concrete suspects")
    } else if !top_suspect_high && report.tests.is_empty() {
        Some("the diagnosis was not yet strong enough to anchor the next move")
    } else if report.likely_causes.is_empty() && report.related_symbols.len() < 2 {
        Some("the diagnosis lacked enough cause or nearby-context detail")
    } else {
        None
    }
}

fn should_try_prepare_change_semantic_fallback(
    capsule: &ContextCapsule,
    entry_files: &[String],
    entry_symbols: &[String],
) -> bool {
    if capsule.pivots.is_empty() {
        return true;
    }

    if capsule.stats.seed_count <= 1 && capsule.context.len() < 3 {
        return true;
    }

    if !entry_files.is_empty()
        && !capsule
            .pivots
            .iter()
            .any(|item| entry_files.iter().any(|file| file == &item.file))
        && !capsule
            .context
            .iter()
            .any(|item| entry_files.iter().any(|file| file == &item.file))
    {
        return true;
    }

    !entry_symbols.is_empty()
        && !capsule
            .pivots
            .iter()
            .any(|pivot| entry_symbols.iter().any(|symbol| symbol_matches_hint(&pivot.symbol, symbol)))
}

fn prepare_change_capsule_quality(
    capsule: &ContextCapsule,
    entry_files: &[String],
    entry_symbols: &[String],
) -> usize {
    let pivot_file_hits = capsule
        .pivots
        .iter()
        .filter(|pivot| entry_files.iter().any(|file| file == &pivot.file))
        .count();
    let pivot_symbol_hits = capsule
        .pivots
        .iter()
        .filter(|pivot| {
            entry_symbols
                .iter()
                .any(|symbol| symbol_matches_hint(&pivot.symbol, symbol))
        })
        .count();

    capsule.pivots.len() * 4
        + capsule.context.len() * 2
        + capsule.stats.seed_count.min(4)
        + pivot_file_hits * 4
        + pivot_symbol_hits * 3
}

fn should_try_subsystem_semantic_fallback(
    report: &SubsystemSummary,
    files: &[String],
    symbols: &[String],
) -> bool {
    report.key_files.len() < 2
        || (report.key_symbols.is_empty() && !files.is_empty())
        || (report.key_symbols.len() < 2 && !symbols.is_empty())
}

fn subsystem_summary_quality(report: &SubsystemSummary) -> usize {
    report.key_files.len() * 4
        + report.key_symbols.len() * 3
        + report.tests.len() * 2
        + report.memories.len()
        + report
            .key_files
            .iter()
            .filter(|item| item.confidence_band == "high")
            .count()
}

fn merge_anchor_files_from_capsule(existing: &[String], capsule: &ContextCapsule) -> Vec<String> {
    let mut files = existing.to_vec();
    files.extend(
        capsule
            .pivots
            .iter()
            .map(|pivot| pivot.file.clone())
            .chain(capsule.context.iter().map(|context| context.file.clone()))
            .filter(|file| is_queryable_workflow_file(file)),
    );
    dedupe_string_values(&mut files);
    files.truncate(4);
    files
}

fn merge_anchor_symbols_from_capsule(existing: &[String], capsule: &ContextCapsule) -> Vec<String> {
    let mut symbols = existing.to_vec();
    symbols.extend(
        capsule
            .pivots
            .iter()
            .map(|pivot| pivot.symbol.clone())
            .chain(capsule.context.iter().map(|context| context.symbol.clone())),
    );
    dedupe_string_values(&mut symbols);
    symbols.truncate(6);
    symbols
}

fn is_queryable_workflow_file(file: &str) -> bool {
    !file.trim().is_empty() && should_index_file(file)
}

fn dedupe_string_values(values: &mut Vec<String>) {
    values.sort();
    values.dedup();
}

fn symbol_matches_hint(candidate: &str, hint: &str) -> bool {
    candidate == hint || candidate.ends_with(&format!(".{}", hint))
}

fn count_outcome_memory_reuse(values: &[Value]) -> usize {
    values
        .iter()
        .filter(|value| {
            value.get("refresh_key")
                .and_then(|item| item.as_str())
                .map(|item| item.starts_with("workflow_outcome::"))
                .unwrap_or(false)
        })
        .count()
}

fn approx_value_tokens(value: &Value) -> usize {
    let bytes = serde_json::to_string(value)
        .map(|serialized| serialized.len())
        .unwrap_or_else(|_| value.to_string().len());
    (bytes / 4).max(1)
}

fn derive_session_pruning_profile(report: &SessionMetricsReport) -> SessionPruningProfile {
    SessionPruningProfile {
        prefer_tiny: report.workflow_tool_calls >= 6
            && report.follow_up_avoidance_rate >= 0.6
            && report.compact_to_expand_rate <= 0.35,
        prune_memory_highlights: report.workflow_tool_calls >= 6
            && report.outcome_memory_reuse_count == 0,
        prefer_dense: report.workflow_tool_calls >= 6
            && report.average_payload_tokens_per_tool >= 300,
    }
}

fn select_workflow_budget(
    tool_name: &str,
    value: &Value,
    metadata: &WorkflowRunMetadata,
    response_options: &WorkflowResponseOptions,
    pruning_profile: SessionPruningProfile,
) -> WorkflowBudget {
    let mut budget = match response_options.budget {
        WorkflowBudget::Tiny => WorkflowBudget::Tiny,
        WorkflowBudget::Compact => WorkflowBudget::Compact,
        WorkflowBudget::Full => WorkflowBudget::Full,
        WorkflowBudget::Auto => {
            if metadata.delivery_mode == "full" {
                WorkflowBudget::Full
            } else if workflow_is_high_confidence(tool_name, value) || pruning_profile.prefer_tiny {
                WorkflowBudget::Tiny
            } else {
                WorkflowBudget::Compact
            }
        }
    };

    if let Some(max_tokens) = response_options.max_tokens {
        if max_tokens <= 220 {
            budget = WorkflowBudget::Tiny;
        } else if max_tokens <= 500 && matches!(budget, WorkflowBudget::Full) {
            budget = WorkflowBudget::Compact;
        }
    }

    budget
}

fn select_workflow_wire_format(
    response_options: &WorkflowResponseOptions,
    pruning_profile: SessionPruningProfile,
    budget: WorkflowBudget,
    approx_tokens: usize,
) -> WorkflowWireFormat {
    match response_options.wire_format {
        WorkflowWireFormat::Dense => WorkflowWireFormat::Dense,
        WorkflowWireFormat::Standard => WorkflowWireFormat::Standard,
        WorkflowWireFormat::Auto => {
            if matches!(budget, WorkflowBudget::Tiny)
                && (pruning_profile.prefer_dense || approx_tokens > 260)
            {
                WorkflowWireFormat::Dense
            } else {
                WorkflowWireFormat::Standard
            }
        }
    }
}

fn apply_workflow_budget(
    tool_name: &str,
    value: &mut Value,
    budget: WorkflowBudget,
    pruning_profile: SessionPruningProfile,
    metadata: &mut WorkflowRunMetadata,
) {
    match budget {
        WorkflowBudget::Full => {}
        WorkflowBudget::Compact => {
            metadata.delivery_mode = "compact".to_string();
            apply_compact_workflow_pruning(value, pruning_profile);
        }
        WorkflowBudget::Tiny => {
            metadata.delivery_mode = "tiny".to_string();
            apply_compact_workflow_pruning(value, pruning_profile);
            let used_single_anchor = if workflow_is_high_confidence(tool_name, value) {
                apply_single_anchor_mode(tool_name, value, pruning_profile)
            } else {
                apply_tiny_workflow_pruning(tool_name, value, pruning_profile);
                false
            };
            metadata.single_anchor_used |= used_single_anchor;
        }
        WorkflowBudget::Auto => {}
    }
}

fn apply_compact_workflow_pruning(value: &mut Value, pruning_profile: SessionPruningProfile) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    object.remove("stats");
    object.remove("memories");
    object.remove("playbook_memory");
    truncate_array_field(object, "rationale", 0);
    truncate_array_field(object, "matched_rules", 1);
    truncate_array_field(object, "test_gaps", 1);

    if pruning_profile.prune_memory_highlights {
        truncate_array_field(object, "memory_highlights", 0);
        truncate_array_field(object, "durable_patterns", 0);
        truncate_array_field(object, "memories", 0);
    } else {
        truncate_array_field(object, "memory_highlights", 1);
        truncate_array_field(object, "durable_patterns", 1);
        shorten_memory_entries(object, "memory_highlights", 56);
        shorten_memory_entries(object, "durable_patterns", 56);
    }

    truncate_string_field(object, "overview", 128);
    truncate_array_strings_field(object, "likely_causes", 2, 72);
    truncate_array_strings_field(object, "next_steps", 2, 72);
    truncate_array_strings_field(object, "architecture", 2, 72);
    truncate_array_strings_field(object, "conventions", 2, 72);
}

fn apply_tiny_workflow_pruning(
    tool_name: &str,
    value: &mut Value,
    pruning_profile: SessionPruningProfile,
) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    truncate_string_field(object, "overview", 88);
    truncate_array_field(object, "memory_highlights", if pruning_profile.prune_memory_highlights { 0 } else { 1 });
    shorten_memory_entries(object, "memory_highlights", 44);
    object.remove("playbook_memory");

    match tool_name {
        "prepare_change" => {
            truncate_array_field(object, "primary_files", 1);
            truncate_array_field(object, "secondary_files", 0);
            truncate_array_field(object, "symbols", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "risks", 0);
        }
        "impact_from_diff" => {
            truncate_array_field(object, "changed_files", 1);
            truncate_array_field(object, "changed_symbols", 1);
            truncate_array_field(object, "affected_symbols", 1);
            truncate_array_field(object, "review_checklist", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "risks", 0);
        }
        "get_working_set_context" => {
            truncate_array_field(object, "files", 1);
            truncate_array_field(object, "active_symbols", 1);
            truncate_array_field(object, "nearby_symbols", 1);
            truncate_array_field(object, "tests", 1);
        }
        "summarize_subsystem" => {
            truncate_array_field(object, "key_files", 1);
            truncate_array_field(object, "key_symbols", 1);
            truncate_array_field(object, "tests", 1);
        }
        "get_repo_playbook" => {
            truncate_array_field(object, "architecture", 1);
            truncate_array_field(object, "conventions", 1);
            truncate_array_field(object, "key_files", 1);
            truncate_array_field(object, "notable_symbols", 1);
            truncate_array_field(object, "durable_patterns", 1);
            shorten_memory_entries(object, "durable_patterns", 44);
        }
        "diagnose_failure" => {
            truncate_array_field(object, "extracted_files", 1);
            truncate_array_field(object, "extracted_symbols", 1);
            truncate_array_field(object, "suspects", 1);
            truncate_array_field(object, "related_symbols", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "likely_causes", 1);
            truncate_array_field(object, "next_steps", 1);
        }
        _ => {}
    }

    ensure_suggested_expand(tool_name, object);
}

fn apply_single_anchor_mode(
    tool_name: &str,
    value: &mut Value,
    pruning_profile: SessionPruningProfile,
) -> bool {
    apply_tiny_workflow_pruning(tool_name, value, pruning_profile);

    let Some(object) = value.as_object_mut() else {
        return false;
    };

    match tool_name {
        "prepare_change" => {
            truncate_array_field(object, "primary_files", 1);
            truncate_array_field(object, "symbols", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "memory_highlights", 0);
        }
        "impact_from_diff" => {
            truncate_array_field(object, "changed_files", 1);
            truncate_array_field(object, "changed_symbols", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "review_checklist", 1);
            truncate_array_field(object, "affected_symbols", 0);
        }
        "get_working_set_context" => {
            truncate_array_field(object, "files", 1);
            truncate_array_field(object, "active_symbols", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "nearby_symbols", 0);
            truncate_array_field(object, "memory_highlights", 0);
        }
        "summarize_subsystem" => {
            truncate_array_field(object, "key_files", 1);
            truncate_array_field(object, "key_symbols", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "memories", 0);
        }
        "get_repo_playbook" => {
            truncate_array_field(object, "architecture", 1);
            truncate_array_field(object, "conventions", 1);
            truncate_array_field(object, "key_files", 1);
            truncate_array_field(object, "notable_symbols", 1);
            truncate_array_field(object, "durable_patterns", 0);
        }
        "diagnose_failure" => {
            truncate_array_field(object, "suspects", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "likely_causes", 1);
            truncate_array_field(object, "next_steps", 1);
            truncate_array_field(object, "related_symbols", 0);
            truncate_array_field(object, "memory_highlights", 0);
        }
        _ => return false,
    }

    ensure_suggested_expand(tool_name, object);
    true
}

fn workflow_is_high_confidence(tool_name: &str, value: &Value) -> bool {
    match tool_name {
        "prepare_change" => {
            first_confidence_band(value, "primary_files") == Some("high")
                || first_confidence_band(value, "symbols") == Some("high")
        }
        "impact_from_diff" => {
            first_confidence_band(value, "tests") == Some("high")
                || array_len(value, "changed_symbols") == 1
                || array_len(value, "changed_files") == 1
        }
        "get_working_set_context" => {
            first_confidence_band(value, "files") == Some("high")
                || first_confidence_band(value, "active_symbols") == Some("high")
        }
        "summarize_subsystem" => {
            first_confidence_band(value, "key_files") == Some("high")
                || first_confidence_band(value, "key_symbols") == Some("high")
        }
        "get_repo_playbook" => {
            first_confidence_band(value, "key_files") == Some("high")
                || first_confidence_band(value, "notable_symbols") == Some("high")
        }
        "diagnose_failure" => first_confidence_band(value, "suspects") == Some("high"),
        _ => false,
    }
}

fn ensure_suggested_expand(tool_name: &str, object: &mut serde_json::Map<String, Value>) {
    if object.contains_key("suggested_expand") {
        return;
    }

    let suggestion = match tool_name {
        "prepare_change" => first_symbol_focus(object, "symbols", "top change anchor")
            .or_else(|| first_file_focus(object, "primary_files", "top file")),
        "impact_from_diff" => first_symbol_focus(object, "changed_symbols", "changed symbol")
            .or_else(|| first_file_focus(object, "changed_files", "changed file")),
        "get_working_set_context" => first_symbol_focus(object, "active_symbols", "active symbol")
            .or_else(|| first_file_focus(object, "files", "top file")),
        "summarize_subsystem" => first_symbol_focus(object, "key_symbols", "key symbol")
            .or_else(|| first_file_focus(object, "key_files", "key file")),
        "get_repo_playbook" => first_symbol_focus(object, "notable_symbols", "notable symbol")
            .or_else(|| first_file_focus(object, "key_files", "key file")),
        "diagnose_failure" => first_symbol_focus(object, "suspects", "top suspect")
            .or_else(|| first_file_focus(object, "extracted_files", "failure file")),
        _ => None,
    };

    if let Some(suggested_expand) = suggestion {
        object.insert("suggested_expand".to_string(), suggested_expand);
    }
}

fn first_file_focus(
    object: &serde_json::Map<String, Value>,
    key: &str,
    reason: &str,
) -> Option<Value> {
    let first = object.get(key)?.as_array()?.first()?;
    let file = if first.is_string() {
        first.as_str()?.to_string()
    } else {
        first.get("file")?.as_str()?.to_string()
    };

    Some(json!({
        "focus": format!("file:{}", file),
        "reason": reason,
    }))
}

fn first_symbol_focus(
    object: &serde_json::Map<String, Value>,
    key: &str,
    reason: &str,
) -> Option<Value> {
    let symbol = object
        .get(key)?
        .as_array()?
        .first()?
        .get("symbol")?
        .as_str()?
        .to_string();
    Some(json!({
        "focus": format!("symbol:{}", symbol),
        "reason": reason,
    }))
}

fn trim_value_for_token_budget(value: &mut Value, max_tokens: usize) {
    if approx_value_tokens(value) <= max_tokens {
        return;
    }

    let Some(object) = value.as_object_mut() else {
        return;
    };

    object.remove("rationale");
    object.remove("matched_rules");
    object.remove("test_gaps");
    object.remove("playbook_memory");
    truncate_array_field(object, "memory_highlights", 0);
    truncate_array_field(object, "durable_patterns", 0);
    truncate_array_field(object, "secondary_files", 0);
    truncate_array_field(object, "related_symbols", 0);
    truncate_array_field(object, "affected_symbols", 1);
    truncate_array_field(object, "risks", 0);
    truncate_array_field(object, "review_checklist", 1);
    truncate_array_field(object, "tests", 1);
    truncate_array_field(object, "symbols", 1);
    truncate_array_field(object, "suspects", 1);
    truncate_array_field(object, "primary_files", 1);
    truncate_array_field(object, "changed_files", 1);
    truncate_array_field(object, "changed_symbols", 1);
    truncate_array_field(object, "files", 1);
    truncate_array_field(object, "key_files", 1);
    truncate_array_field(object, "key_symbols", 1);
    truncate_array_field(object, "architecture", 1);
    truncate_array_field(object, "conventions", 1);
    truncate_array_field(object, "next_steps", 1);
    truncate_array_field(object, "likely_causes", 1);
    truncate_string_field(object, "overview", 72);
}

fn densify_workflow_value(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(densify_workflow_value).collect()),
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (dense_key(&key).to_string(), densify_workflow_value(value)))
                .collect(),
        ),
        other => other,
    }
}

fn dense_key(key: &str) -> &str {
    match key {
        "overview" => "ov",
        "query" => "q",
        "intent" => "i",
        "primary_files" => "pf",
        "secondary_files" => "sf",
        "files" => "fs",
        "symbols" => "sy",
        "tests" => "ts",
        "test_gaps" => "tg",
        "matched_rules" => "mr",
        "memory_highlights" => "mh",
        "memories" => "mm",
        "risks" => "rk",
        "rationale" => "ra",
        "changed_files" => "cf",
        "changed_symbols" => "cs",
        "affected_symbols" => "af",
        "review_checklist" => "rc",
        "active_symbols" => "as",
        "nearby_symbols" => "ny",
        "key_files" => "kf",
        "key_symbols" => "ks",
        "notable_symbols" => "no",
        "architecture" => "ar",
        "conventions" => "cv",
        "durable_patterns" => "dp",
        "extracted_files" => "ef",
        "extracted_symbols" => "es",
        "suspects" => "su",
        "related_symbols" => "ry",
        "likely_causes" => "lc",
        "next_steps" => "nx",
        "suggested_expand" => "x",
        "context_handle" => "h",
        "context_origin" => "o",
        "delivery_mode" => "dm",
        "wire_format" => "wf",
        "single_anchor_used" => "sa",
        "semantic_fallback_used" => "se",
        "outcome_memory_reuse_count" => "or",
        "playbook_memory" => "pm",
        "focus" => "fo",
        "reason" => "r",
        "file" => "f",
        "symbol" => "s",
        "score" => "sc",
        "confidence" => "cf",
        "confidence_band" => "cb",
        "evidence" => "ev",
        "reasons" => "rs",
        "kind" => "k",
        "line" => "ln",
        "role" => "ro",
        "level" => "lv",
        "message" => "m",
        "impact_count" => "ic",
        "summary" => "sm",
        "why" => "w",
        "content" => "ct",
        "memory_type" => "mt",
        "scope" => "sp",
        "is_stale" => "st",
        "status" => "stt",
        "added_lines" => "al",
        "removed_lines" => "rl",
        "hunk_count" => "hc",
        "change_kind" => "ck",
        "via" => "v",
        _ => key,
    }
}

fn truncate_array_field(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
    limit: usize,
) {
    let remove = match object.get_mut(key) {
        Some(Value::Array(items)) => {
            if limit == 0 {
                true
            } else {
                items.truncate(limit);
                items.is_empty()
            }
        }
        _ => false,
    };

    if remove {
        object.remove(key);
    }
}

fn truncate_string_field(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
    limit: usize,
) {
    if let Some(Value::String(text)) = object.get_mut(key) {
        *text = truncate_text_value(text, limit);
    }
}

fn truncate_array_strings_field(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
    limit: usize,
    text_limit: usize,
) {
    let remove = match object.get_mut(key) {
        Some(Value::Array(items)) => {
            for item in items.iter_mut() {
                if let Some(text) = item.as_str() {
                    *item = Value::String(truncate_text_value(text, text_limit));
                }
            }
            if limit == 0 {
                true
            } else {
                items.truncate(limit);
                items.is_empty()
            }
        }
        _ => false,
    };

    if remove {
        object.remove(key);
    }
}

fn shorten_memory_entries(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
    text_limit: usize,
) {
    if let Some(Value::Array(items)) = object.get_mut(key) {
        for item in items.iter_mut() {
            if let Some(entry) = item.as_object_mut() {
                if let Some(content) = entry.get("content").and_then(|value| value.as_str()) {
                    entry.insert(
                        "content".to_string(),
                        Value::String(truncate_text_value(content, text_limit)),
                    );
                } else if let Some(content) = entry.get("ct").and_then(|value| value.as_str()) {
                    entry.insert(
                        "ct".to_string(),
                        Value::String(truncate_text_value(content, text_limit)),
                    );
                }
            }
        }
    }
}

fn truncate_text_value(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        value.to_string()
    } else {
        let cutoff = limit.saturating_sub(3);
        format!("{}...", &value[..cutoff])
    }
}

fn first_confidence_band<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?
        .as_array()?
        .first()?
        .get("confidence_band")?
        .as_str()
}

fn array_len(value: &Value, key: &str) -> usize {
    value.get(key)
        .and_then(|item| item.as_array())
        .map(|items| items.len())
        .unwrap_or(0)
}

fn attach_workflow_metadata(value: &mut Value, metadata: &WorkflowRunMetadata) {
    if let Some(object) = value.as_object_mut() {
        object.insert("delivery_mode".to_string(), json!(metadata.delivery_mode.as_str()));
        if metadata.wire_format != "standard" {
            object.insert("wire_format".to_string(), json!(metadata.wire_format.as_str()));
        }
        if metadata.single_anchor_used {
            object.insert("single_anchor_used".to_string(), json!(true));
        }
        if metadata.semantic_fallback_used {
            object.insert("semantic_fallback_used".to_string(), json!(true));
        }
        if metadata.outcome_memory_reuse_count > 0 {
            object.insert(
                "outcome_memory_reuse_count".to_string(),
                json!(metadata.outcome_memory_reuse_count),
            );
        }
    }
}

fn summarize_workflow_outcome_content(
    task: &str,
    status: &str,
    summary: Option<&str>,
    files: &[String],
    symbols: &[String],
    tests: &[String],
) -> String {
    let mut parts = vec![format!("Workflow outcome for '{}': {}.", task, status)];

    if let Some(summary) = summary {
        parts.push(format!("Summary: {}.", summary.trim_end_matches('.')));
    }
    if !files.is_empty() {
        parts.push(format!(
            "Files: {}.",
            files.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if !symbols.is_empty() {
        parts.push(format!(
            "Symbols: {}.",
            symbols
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !tests.is_empty() {
        parts.push(format!(
            "Tests: {}.",
            tests.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
        ));
    }

    parts.join(" ")
}

fn memory_to_value(memory: &Memory, include_session_id: bool) -> Value {
    let mut value = json!({
        "id": memory.id,
        "content": memory.content,
        "type": memory.memory_type.as_str(),
        "scope": memory.scope.as_str(),
        "confidence": memory.confidence,
        "linked_symbols": memory.linked_symbols,
        "linked_files": memory.linked_files,
        "workspace_id": memory.workspace_id,
        "branch": memory.branch,
        "refresh_key": memory.refresh_key,
        "source_query": memory.source_query,
        "created_at": memory.created_at,
        "last_accessed": memory.last_accessed,
        "access_count": memory.access_count,
        "is_stale": memory.is_stale,
        "stale_reason": memory.stale_reason,
    });

    if include_session_id {
        if let Some(object) = value.as_object_mut() {
            object.insert("session_id".to_string(), json!(memory.session_id));
        }
    }

    value
}

fn build_memory_query(query: Option<&str>, files: &[String], symbols: &[String]) -> Option<String> {
    let mut terms = Vec::new();

    if let Some(query) = query {
        terms.extend(extract_search_terms(query, 4));
    }

    if terms.is_empty() {
        for symbol in symbols.iter().take(2) {
            terms.extend(extract_search_terms(symbol, 1));
        }
    }

    if terms.is_empty() {
        for file in files.iter().take(2) {
            terms.extend(extract_search_terms(file, 1));
        }
    }

    if terms.is_empty() {
        None
    } else {
        terms.sort();
        terms.dedup();
        Some(terms.join(" "))
    }
}

fn extract_search_terms(value: &str, limit: usize) -> Vec<String> {
    let mut terms = Vec::new();
    for part in value.split(|c: char| !c.is_alphanumeric()) {
        if part.len() < 3 {
            continue;
        }
        let normalized = part.to_lowercase();
        if terms.iter().any(|existing| existing == &normalized) {
            continue;
        }
        terms.push(normalized);
        if terms.len() >= limit {
            break;
        }
    }
    terms
}

fn seed_from_task_bundle(bundle: &TaskBundle) -> ExpandContextSeed {
    let mut files: Vec<String> = bundle
        .primary_files
        .iter()
        .map(|item| item.file.clone())
        .chain(bundle.secondary_files.iter().map(|item| item.file.clone()))
        .collect();
    files.sort();
    files.dedup();

    let mut symbols: Vec<String> = bundle
        .symbols
        .iter()
        .map(|item| item.symbol.clone())
        .collect();
    symbols.sort();
    symbols.dedup();

    ExpandContextSeed {
        query: Some(bundle.query.clone()),
        files,
        symbols,
        tests: bundle.tests.iter().map(|item| item.file.clone()).collect(),
        memories: memory_seed_values(&bundle.memories, &bundle.memory_highlights),
    }
}

fn seed_from_diff_impact(report: &DiffImpactReport) -> ExpandContextSeed {
    let mut files: Vec<String> = report
        .changed_files
        .iter()
        .map(|item| item.file.clone())
        .collect();
    files.extend(report.affected_symbols.iter().map(|item| item.file.clone()));
    files.sort();
    files.dedup();

    let mut symbols: Vec<String> = report
        .changed_symbols
        .iter()
        .map(|item| item.symbol.clone())
        .collect();
    symbols.extend(
        report
            .affected_symbols
            .iter()
            .map(|item| item.symbol.clone()),
    );
    symbols.sort();
    symbols.dedup();

    ExpandContextSeed {
        query: None,
        files,
        symbols,
        tests: report.tests.iter().map(|item| item.file.clone()).collect(),
        memories: Vec::new(),
    }
}

fn seed_from_working_set_context(report: &WorkingSetContext) -> ExpandContextSeed {
    let mut files: Vec<String> = report.files.iter().map(|item| item.file.clone()).collect();
    files.sort();
    files.dedup();

    let mut symbols: Vec<String> = report
        .active_symbols
        .iter()
        .map(|item| item.symbol.clone())
        .collect();
    symbols.extend(report.nearby_symbols.iter().map(|item| item.symbol.clone()));
    symbols.sort();
    symbols.dedup();

    ExpandContextSeed {
        query: report.query.clone(),
        files,
        symbols,
        tests: report.tests.iter().map(|item| item.file.clone()).collect(),
        memories: memory_seed_values(&report.memories, &report.memory_highlights),
    }
}

fn seed_from_failure_diagnosis(report: &FailureDiagnosis) -> ExpandContextSeed {
    let mut files = report.extracted_files.clone();
    files.extend(report.suspects.iter().map(|item| item.file.clone()));
    files.sort();
    files.dedup();

    let mut symbols = report.extracted_symbols.clone();
    symbols.extend(report.suspects.iter().map(|item| item.symbol.clone()));
    symbols.extend(
        report
            .related_symbols
            .iter()
            .map(|item| item.symbol.clone()),
    );
    symbols.sort();
    symbols.dedup();

    ExpandContextSeed {
        query: Some(report.kind.clone()),
        files,
        symbols,
        tests: report.tests.iter().map(|item| item.file.clone()).collect(),
        memories: report
            .memory_highlights
            .iter()
            .map(|memory| {
                json!({
                    "content": memory.content,
                    "type": memory.memory_type,
                    "scope": memory.scope,
                    "is_stale": memory.is_stale
                })
            })
            .collect(),
    }
}

fn seed_from_subsystem_summary(report: &SubsystemSummary) -> ExpandContextSeed {
    ExpandContextSeed {
        query: Some(report.query.clone()),
        files: report.key_files.iter().map(|item| item.file.clone()).collect(),
        symbols: report
            .key_symbols
            .iter()
            .map(|item| item.symbol.clone())
            .collect(),
        tests: report.tests.iter().map(|item| item.file.clone()).collect(),
        memories: report
            .memories
            .iter()
            .map(|memory| {
                json!({
                    "content": memory.content,
                    "type": memory.memory_type,
                    "scope": memory.scope,
                    "is_stale": memory.is_stale
                })
            })
            .collect(),
    }
}

fn seed_from_repo_playbook(report: &RepoPlaybook) -> ExpandContextSeed {
    ExpandContextSeed {
        query: Some(report.overview.clone()),
        files: report.key_files.iter().map(|item| item.file.clone()).collect(),
        symbols: report
            .notable_symbols
            .iter()
            .map(|item| item.symbol.clone())
            .collect(),
        tests: Vec::new(),
        memories: report
            .durable_patterns
            .iter()
            .map(|memory| {
                json!({
                    "content": memory.content,
                    "type": memory.memory_type,
                    "scope": memory.scope,
                    "is_stale": memory.is_stale
                })
            })
            .collect(),
    }
}

fn attach_context_handle(value: &mut Value, handle: &str, origin: &str) {
    if let Some(object) = value.as_object_mut() {
        object.insert("context_handle".to_string(), json!(handle));
        object.insert("context_origin".to_string(), json!(origin));
    }
}

fn attach_playbook_memory(value: &mut Value, playbook_memory: Value) {
    if let Some(object) = value.as_object_mut() {
        object.insert("playbook_memory".to_string(), playbook_memory);
    }
}

fn extract_wrapped_tool_metrics(
    value: &Value,
) -> (usize, usize, Option<String>, Option<String>, ToolCallMetadata) {
    let text = value["content"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(|item| item.as_str())
        .unwrap_or("");
    let payload_bytes = text.len();
    let approx_tokens = payload_bytes / 4;
    let parsed = serde_json::from_str::<Value>(text).ok();

    let context_handle = parsed
        .as_ref()
        .and_then(|inner| inner.get("context_handle").or_else(|| inner.get("h")))
        .and_then(|item| item.as_str())
        .map(|item| item.to_string());
    let context_origin = parsed
        .as_ref()
        .and_then(|inner| inner.get("context_origin").or_else(|| inner.get("o")))
        .and_then(|item| item.as_str())
        .map(|item| item.to_string());
    let metadata = ToolCallMetadata {
        delivery_mode: parsed
            .as_ref()
            .and_then(|inner| inner.get("delivery_mode").or_else(|| inner.get("dm")))
            .and_then(|item| item.as_str())
            .map(|item| item.to_string()),
        wire_format: parsed
            .as_ref()
            .and_then(|inner| inner.get("wire_format").or_else(|| inner.get("wf")))
            .and_then(|item| item.as_str())
            .map(|item| item.to_string()),
        single_anchor_used: parsed
            .as_ref()
            .and_then(|inner| {
                inner
                    .get("single_anchor_used")
                    .or_else(|| inner.get("sa"))
            })
            .and_then(|item| item.as_bool())
            .unwrap_or(false),
        suggested_expand_focus: parsed
            .as_ref()
            .and_then(|inner| inner.get("suggested_expand").or_else(|| inner.get("x")))
            .and_then(|item| item.get("focus").or_else(|| item.get("fo")))
            .and_then(|item| item.as_str())
            .map(|item| item.to_string()),
        semantic_fallback_used: parsed
            .as_ref()
            .and_then(|inner| {
                inner
                    .get("semantic_fallback_used")
                    .or_else(|| inner.get("se"))
            })
            .and_then(|item| item.as_bool())
            .unwrap_or(false),
        outcome_memory_reuse_count: parsed
            .as_ref()
            .and_then(|inner| {
                inner
                    .get("outcome_memory_reuse_count")
                    .or_else(|| inner.get("or"))
            })
            .and_then(|item| item.as_u64())
            .unwrap_or(0) as usize,
    };

    (
        payload_bytes,
        approx_tokens,
        context_handle,
        context_origin,
        metadata,
    )
}

fn dedupe_memory_values(values: &mut Vec<Value>) {
    let mut seen = HashSet::new();
    values.retain(|value| {
        let key = value
            .get("id")
            .and_then(|item| item.as_str())
            .map(|item| item.to_string())
            .unwrap_or_else(|| value.to_string());
        seen.insert(key)
    });
}

fn report_memory_highlights(values: &[Value], limit: usize) -> Vec<MemoryHighlight> {
    values
        .iter()
        .filter_map(|value| {
            let content = value.get("content")?.as_str()?;
            Some(MemoryHighlight {
                content: truncate_memory_snippet(content, 120),
                memory_type: value
                    .get("type")
                    .or_else(|| value.get("memory_type"))
                    .and_then(|item| item.as_str())
                    .unwrap_or("observation")
                    .to_string(),
                scope: value
                    .get("scope")
                    .and_then(|item| item.as_str())
                    .unwrap_or("session")
                    .to_string(),
                is_stale: value
                    .get("is_stale")
                    .and_then(|item| item.as_bool())
                    .unwrap_or(false),
            })
        })
        .take(limit.max(1))
        .collect()
}

fn build_failure_overview_value(base: &str, memory: Option<&MemoryHighlight>) -> String {
    if let Some(memory) = memory {
        return format!(
            "{} Consider {}.",
            base.trim_end_matches('.'),
            memory_reference_phrase(memory)
        );
    }
    base.to_string()
}

fn memory_reference_phrase(memory: &MemoryHighlight) -> String {
    format!("prior {} {}", memory.scope, memory.memory_type)
}

fn memory_seed_values(memories: &[Value], highlights: &[MemoryHighlight]) -> Vec<Value> {
    if !memories.is_empty() {
        return memories.to_vec();
    }

    highlights
        .iter()
        .map(|memory| {
            json!({
                "content": memory.content,
                "type": memory.memory_type,
                "scope": memory.scope,
                "is_stale": memory.is_stale
            })
        })
        .collect()
}

fn truncate_memory_snippet(content: &str, limit: usize) -> String {
    let mut output = String::new();
    for ch in content.chars().take(limit) {
        output.push(ch);
    }
    if content.chars().count() > limit {
        output.push_str("...");
    }
    output
}

fn summarize_subsystem_memory_content(report: &SubsystemSummary) -> String {
    let file_list = report
        .key_files
        .iter()
        .map(|item| item.file.clone())
        .take(3)
        .collect::<Vec<_>>()
        .join(", ");
    let symbol_list = report
        .key_symbols
        .iter()
        .map(|item| item.symbol.clone())
        .take(3)
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        "Subsystem playbook for '{}': {} Key files: {}. Key symbols: {}.",
        report.query, report.overview, file_list, symbol_list
    )
}

fn summarize_repo_playbook_memory_content(report: &RepoPlaybook) -> String {
    let file_list = report
        .key_files
        .iter()
        .map(|item| item.file.clone())
        .take(3)
        .collect::<Vec<_>>()
        .join(", ");
    let conventions = report
        .conventions
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");

    format!(
        "Repo playbook: {} High-signal files: {}. Conventions: {}.",
        report.overview, file_list, conventions
    )
}

#[cfg(test)]
mod tests {
    use super::{
        build_failure_overview_value, count_outcome_memory_reuse, extract_wrapped_tool_metrics,
        memory_seed_values, report_memory_highlights, summarize_workflow_outcome_content,
        wrap_tool_result,
    };
    use serde_json::json;

    #[test]
    fn test_report_memory_highlights_truncates_content() {
        let long_content = "repo-playbook ".repeat(40);
        let highlights = report_memory_highlights(&[json!({
            "content": long_content,
            "type": "pattern",
            "scope": "repo",
            "is_stale": false
        })], 3);

        assert_eq!(highlights.len(), 1);
        assert!(highlights[0].content.len() <= 123);
        assert!(highlights[0].content.ends_with("..."));
    }

    #[test]
    fn test_build_failure_overview_value_uses_short_memory_reference() {
        let highlights = report_memory_highlights(&[json!({
            "content": "durable-note ".repeat(30),
            "type": "pattern",
            "scope": "repo",
            "is_stale": false
        })], 1);

        let overview = build_failure_overview_value("test diagnosis.", highlights.first());
        assert!(overview.contains("Consider prior repo pattern."));
        assert!(overview.len() < 80);
    }

    #[test]
    fn test_memory_seed_values_falls_back_to_highlights() {
        let seeded = memory_seed_values(
            &[],
            &[super::MemoryHighlight {
                content: "prior repo pattern".to_string(),
                memory_type: "pattern".to_string(),
                scope: "repo".to_string(),
                is_stale: false,
            }],
        );

        assert_eq!(seeded.len(), 1);
        assert_eq!(seeded[0]["content"], "prior repo pattern");
    }

    #[test]
    fn test_extract_wrapped_tool_metrics_reads_workflow_metadata() {
        let wrapped = wrap_tool_result(json!({
            "context_handle": "ctx-7",
            "context_origin": "prepare_change",
            "delivery_mode": "compact",
            "mode_reason": "kept compact",
            "semantic_fallback_used": true,
            "outcome_memory_reuse_count": 2,
            "suggested_expand": {
                "focus": "file:src/auth.ts",
                "reason": "top file"
            }
        }));

        let (_, _, handle, origin, metadata) = extract_wrapped_tool_metrics(&wrapped);
        assert_eq!(handle.as_deref(), Some("ctx-7"));
        assert_eq!(origin.as_deref(), Some("prepare_change"));
        assert_eq!(metadata.delivery_mode.as_deref(), Some("compact"));
        assert_eq!(metadata.wire_format, None);
        assert!(!metadata.single_anchor_used);
        assert_eq!(
            metadata.suggested_expand_focus.as_deref(),
            Some("file:src/auth.ts")
        );
        assert!(metadata.semantic_fallback_used);
        assert_eq!(metadata.outcome_memory_reuse_count, 2);
    }

    #[test]
    fn test_extract_wrapped_tool_metrics_reads_dense_workflow_metadata() {
        let wrapped = wrap_tool_result(json!({
            "h": "ctx-8",
            "o": "diagnose_failure",
            "dm": "tiny",
            "wf": "dense",
            "sa": true,
            "se": true,
            "or": 1,
            "x": {
                "fo": "symbol:_verify_org_access",
                "r": "top suspect"
            }
        }));

        let (_, _, handle, origin, metadata) = extract_wrapped_tool_metrics(&wrapped);
        assert_eq!(handle.as_deref(), Some("ctx-8"));
        assert_eq!(origin.as_deref(), Some("diagnose_failure"));
        assert_eq!(metadata.delivery_mode.as_deref(), Some("tiny"));
        assert_eq!(metadata.wire_format.as_deref(), Some("dense"));
        assert!(metadata.single_anchor_used);
        assert_eq!(
            metadata.suggested_expand_focus.as_deref(),
            Some("symbol:_verify_org_access")
        );
        assert!(metadata.semantic_fallback_used);
        assert_eq!(metadata.outcome_memory_reuse_count, 1);
    }

    #[test]
    fn test_count_outcome_memory_reuse_only_counts_workflow_outcomes() {
        let count = count_outcome_memory_reuse(&[
            json!({"refresh_key": "workflow_outcome::cert-tenant"}),
            json!({"refresh_key": "repo_playbook"}),
            json!({"refresh_key": "workflow_outcome::login-timeout"}),
        ]);
        assert_eq!(count, 2);
    }

    #[test]
    fn test_summarize_workflow_outcome_content_stays_compact() {
        let content = summarize_workflow_outcome_content(
            "fix certificate tenant isolation",
            "success",
            Some("narrowed access checks and updated tests"),
            &[
                "routers/certificates.py".to_string(),
                "models/certificates.py".to_string(),
            ],
            &["_verify_org_access".to_string()],
            &["tests/test_certificate_tenant_isolation.py".to_string()],
        );

        assert!(content.contains("fix certificate tenant isolation"));
        assert!(content.contains("routers/certificates.py"));
        assert!(content.len() < 260);
    }
}

fn stable_refresh_key(query: &str, files: &[String], symbols: &[String]) -> String {
    let mut terms = extract_search_terms(query, 4);
    for file in files.iter().take(2) {
        terms.extend(extract_search_terms(file, 1));
    }
    for symbol in symbols.iter().take(2) {
        terms.extend(extract_search_terms(symbol, 1));
    }
    terms.sort();
    terms.dedup();
    if terms.is_empty() {
        "general".to_string()
    } else {
        terms.join("-")
    }
}

fn current_git_branch(workspace_root: &std::path::Path) -> Option<String> {
    let git_path = workspace_root.join(".git");
    let head_path = if git_path.is_dir() {
        git_path.join("HEAD")
    } else if git_path.is_file() {
        let gitdir = std::fs::read_to_string(&git_path).ok()?;
        let relative = gitdir.trim().strip_prefix("gitdir:")?.trim();
        workspace_root.join(relative).join("HEAD")
    } else {
        return None;
    };

    let head = std::fs::read_to_string(head_path).ok()?;
    let trimmed = head.trim();
    if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
        Some(branch.to_string())
    } else {
        None
    }
}

fn detect_project_rules(
    graph: &lattice_core::graph::model::CodeGraph,
) -> Vec<lattice_core::intelligence::ProjectRule> {
    let mut files: Vec<String> = graph
        .all_nodes()
        .into_iter()
        .map(|node| node.file.clone())
        .collect();
    files.sort();
    files.dedup();
    RulesDetector::new().detect_rules(&files)
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
