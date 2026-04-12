use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

use lattice_core::embeddings::EmbeddingEngine;
use lattice_core::graph::model::{CodeGraph, GraphNode};
use lattice_core::indexer::Indexer;
use lattice_core::intelligence::{
    diagnose_failure, expand_context, find_relevant_tests, find_stale_docs, get_backlinks,
    get_docs_capsule, get_outgoing_links, get_repo_playbook, get_working_set_context,
    impact_from_diff, plan_edit, prepare_change, summarize_subsystem, trace_scenario, BundleMode,
    DiffImpactReport, DocsTargetKind, ExpandContextSeed, FailureDiagnosis, MemoryHighlight,
    PlanEditBundle, RepoPlaybook, RulesDetector, ScenarioTraceBundle, SubsystemSummary, TaskBundle,
    WorkingSetContext,
};
use lattice_core::memory::model::MemoryStructuredFields;
use lattice_core::memory::{Memory, MemoryScope, MemoryStore, MemoryType};
use lattice_core::query::{ContextCapsule, QueryEngine};
use lattice_core::security::SecurityFilter;
use lattice_core::storage::{GraphStore, SharedVectorIndex};
use lattice_core::symbols::stable_file_handle;
use lattice_core::watcher::should_index_file;
use lattice_core::workspace::WorkspaceManager;

use super::context_cache::ContextHandleCache;
use super::server::RequestHandler;
use super::session_metrics::{SessionMetrics, SessionMetricsReport, ToolCallMetadata};

/// MCP (Model Context Protocol) handler that routes JSON-RPC methods
/// to the appropriate tool implementations.
pub struct McpHandler {
    engine: Arc<Mutex<QueryEngine>>,
    indexer: Arc<Mutex<Indexer>>,
    memory_store: Arc<Mutex<MemoryStore>>,
    graph_store: Arc<Mutex<GraphStore>>,
    embedding_engine: Arc<OnceLock<Arc<EmbeddingEngine>>>,
    vector_index: Option<SharedVectorIndex>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowRenderMode {
    Json,
    Markdown,
    Hybrid,
}

#[derive(Debug, Clone)]
struct WorkflowResponseOptions {
    budget: WorkflowBudget,
    max_tokens: Option<usize>,
    wire_format: WorkflowWireFormat,
    render: WorkflowRenderMode,
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
        vector_index: Option<SharedVectorIndex>,
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
            vector_index,
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
                    "description": "First discovery tool when you do not yet know which files matter. Returns the most relevant source pivots plus nearby symbols, along with a reusable context_handle and suggested_expand target, so you can avoid broad file reads.",
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
                            },
                            "render": {
                                "type": "string",
                                "description": "Result rendering: 'hybrid' (default markdown summary + JSON payload), 'markdown', or 'json'",
                                "enum": ["json", "markdown", "hybrid"],
                                "default": "hybrid"
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "prepare_change",
                    "description": "First workflow tool for fix/add/refactor tasks once you know the area. Returns likely edit files, symbols, tests, risks, and reusable memory in one bundle.",
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
                            },
                            "render": {
                                "type": "string",
                                "description": "Workflow result rendering: 'hybrid' (default markdown summary + JSON payload), 'markdown', or 'json'",
                                "enum": ["json", "markdown", "hybrid"],
                                "default": "hybrid"
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "plan_edit",
                    "description": "Patch-oriented planning bundle that returns likely edit files, candidate spans, affected callers/dependencies, relevant docs, and recommended tests in one assistant-facing plan.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Natural language task such as 'fix login timeout' or 'add OAuth refresh'"
                            },
                            "entry_files": {
                                "type": "array",
                                "description": "Optional files to bias the edit plan toward",
                                "items": { "type": "string" }
                            },
                            "entry_symbols": {
                                "type": "array",
                                "description": "Optional symbols to bias the edit plan toward",
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
                            "render": {
                                "type": "string",
                                "description": "Workflow result rendering: 'hybrid' (default markdown summary + JSON payload), 'markdown', or 'json'",
                                "enum": ["json", "markdown", "hybrid"],
                                "default": "hybrid"
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "trace_scenario",
                    "description": "Scenario-focused debugging bundle that traces likely execution paths, guards, side effects, and failure branches from a behavior description.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "scenario": {
                                "type": "string",
                                "description": "Behavior description such as 'why does login fail after refresh'"
                            },
                            "entry_files": {
                                "type": "array",
                                "description": "Optional files to bias scenario tracing toward",
                                "items": { "type": "string" }
                            },
                            "entry_symbols": {
                                "type": "array",
                                "description": "Optional symbols to bias scenario tracing toward",
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
                            "render": {
                                "type": "string",
                                "description": "Workflow result rendering: 'hybrid' (default markdown summary + JSON payload), 'markdown', or 'json'",
                                "enum": ["json", "markdown", "hybrid"],
                                "default": "hybrid"
                            }
                        },
                        "required": ["scenario"]
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
                    "description": "First review tool for a local diff. Summarizes changed symbols, downstream impact, risks, review checklist, and relevant tests.",
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
                            "render": {
                                "type": "string",
                                "description": "Workflow result rendering: 'hybrid' (default markdown summary + JSON payload), 'markdown', or 'json'",
                                "enum": ["json", "markdown", "hybrid"],
                                "default": "hybrid"
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
                    "description": "Use when several files are already open or known, not as the first discovery call. Compresses the working set into one bundle of files, symbols, tests, and memory.",
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
                            },
                            "render": {
                                "type": "string",
                                "description": "Workflow result rendering: 'hybrid' (default markdown summary + JSON payload), 'markdown', or 'json'",
                                "enum": ["json", "markdown", "hybrid"],
                                "default": "hybrid"
                            }
                        },
                        "required": []
                    }
                },
                {
                    "name": "summarize_subsystem",
                    "description": "Summary-first map for an unfamiliar subsystem. Returns key files, symbols, tests, and durable memory without loading full source.",
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
                            },
                            "render": {
                                "type": "string",
                                "description": "Workflow result rendering: 'hybrid' (default markdown summary + JSON payload), 'markdown', or 'json'",
                                "enum": ["json", "markdown", "hybrid"],
                                "default": "hybrid"
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "get_repo_playbook",
                    "description": "Repo-wide startup summary of architecture, conventions, high-signal files, and durable patterns.",
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
                            },
                            "render": {
                                "type": "string",
                                "description": "Workflow result rendering: 'hybrid' (default markdown summary + JSON payload), 'markdown', or 'json'",
                                "enum": ["json", "markdown", "hybrid"],
                                "default": "hybrid"
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
                    "description": "First failure tool for compiler errors, failing tests, and stack traces. Turns raw failure text into suspects, related code, tests, and next steps.",
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
                            },
                            "render": {
                                "type": "string",
                                "description": "Workflow result rendering: 'hybrid' (default markdown summary + JSON payload), 'markdown', or 'json'",
                                "enum": ["json", "markdown", "hybrid"],
                                "default": "hybrid"
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
                    "description": "Follow-up to a result with a context_handle, including get_context_capsule and the workflow tools. Expands one suggested file, symbol, test, or memory target without repeating the broad search.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "handle": {
                                "type": "string",
                                "description": "A context handle returned by a prior result such as get_context_capsule, prepare_change, plan_edit, trace_scenario, or get_working_set_context"
                            },
                            "focus": {
                                "type": "string",
                                "description": "Target to expand, such as symbol_id:{...}, file_id:src/auth.ts, symbol:loginUser, file:src/auth.ts, test:tests/auth.test.ts, or memory:0"
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
            "plan_edit" => self.tool_plan_edit(arguments).await,
            "trace_scenario" => self.tool_trace_scenario(arguments).await,
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
        let render = parse_workflow_response_options(args).render;

        // Embed query text if embedding engine is available (graceful fallback to keyword)
        let embedding = self
            .embedding_engine
            .get()
            .and_then(|eng| eng.embed(query).ok());

        let mut engine = self.engine.lock().await;
        let capsule = engine.query(query, embedding.as_deref(), focused);
        let seed = seed_from_context_capsule(engine.graph(), &capsule);
        let suggested_expand = context_capsule_suggested_expand(&capsule, engine.graph());
        drop(engine);

        let handle = self.store_context_handle("get_context_capsule", seed).await;
        let mut value = serde_json::to_value(&capsule)
            .map_err(|e| (-32603, format!("Serialization error: {}", e)))?;
        attach_context_handle(&mut value, &handle, "get_context_capsule");
        attach_context_capsule_suggested_expand(&mut value, suggested_expand);

        Ok(wrap_workflow_tool_result(value, render))
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
                    if prepare_change_capsule_quality(
                        &semantic_capsule,
                        &entry_files,
                        &entry_symbols,
                    ) > prepare_change_capsule_quality(
                        &keyword_capsule,
                        &entry_files,
                        &entry_symbols,
                    ) {
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

    async fn tool_plan_edit(&self, args: &Value) -> Result<Value, (i32, String)> {
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
                    if prepare_change_capsule_quality(
                        &semantic_capsule,
                        &entry_files,
                        &entry_symbols,
                    ) > prepare_change_capsule_quality(
                        &keyword_capsule,
                        &entry_files,
                        &entry_symbols,
                    ) {
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
            let compact_bundle = plan_edit(
                engine.graph(),
                &capsule,
                &entry_files,
                &entry_symbols,
                &project_rules,
                BundleMode::Compact,
            );
            let (delivery_mode, mode_reason) =
                select_plan_edit_mode(requested_mode, &compact_bundle);
            let bundle = if matches!(delivery_mode, BundleMode::Full) {
                plan_edit(
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
            .store_context_handle("plan_edit", seed_from_plan_edit_bundle(&bundle))
            .await;

        self.serialize_workflow_with_context_handle(
            "plan_edit",
            bundle,
            &handle,
            "plan_edit",
            &metadata,
            &response_options,
        )
        .await
    }

    async fn tool_trace_scenario(&self, args: &Value) -> Result<Value, (i32, String)> {
        let scenario = args["scenario"]
            .as_str()
            .or_else(|| args["query"].as_str())
            .ok_or((-32602, "Missing required parameter: scenario".to_string()))?;
        let requested_mode = parse_requested_bundle_mode(args);
        let response_options = parse_workflow_response_options(args);
        let entry_files = parse_string_array(args, "entry_files");
        let entry_symbols = parse_string_array(args, "entry_symbols");

        let (bundle, metadata) = {
            let engine = self.engine.lock().await;
            let project_rules = detect_project_rules(engine.graph());
            let compact_bundle = trace_scenario(
                engine.graph(),
                scenario,
                &entry_files,
                &entry_symbols,
                &project_rules,
                BundleMode::Compact,
            );
            let (delivery_mode, mode_reason) =
                select_trace_scenario_mode(requested_mode, &compact_bundle);
            let bundle = if matches!(delivery_mode, BundleMode::Full) {
                trace_scenario(
                    engine.graph(),
                    scenario,
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
                    semantic_fallback_used: false,
                    outcome_memory_reuse_count: 0,
                },
            )
        };

        let handle = self
            .store_context_handle("trace_scenario", seed_from_trace_scenario_bundle(&bundle))
            .await;

        self.serialize_workflow_with_context_handle(
            "trace_scenario",
            bundle,
            &handle,
            "trace_scenario",
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
            .store_context_handle("summarize_subsystem", seed_from_subsystem_summary(&report))
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
            let compact_report = get_repo_playbook(
                engine.graph(),
                &memories,
                &project_rules,
                BundleMode::Compact,
            );
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
            let compact_report = diagnose_failure(
                engine.graph(),
                input,
                kind,
                &project_rules,
                BundleMode::Compact,
            );
            let (delivery_mode, mode_reason) =
                select_failure_diagnosis_mode(requested_mode, &compact_report);
            let report = if matches!(delivery_mode, BundleMode::Full) {
                diagnose_failure(
                    engine.graph(),
                    input,
                    kind,
                    &project_rules,
                    BundleMode::Full,
                )
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
        report.overview =
            build_failure_overview_value(&report.overview, report.memory_highlights.first());
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
        let summary = args["summary"]
            .as_str()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty());
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

        let refresh_key = format!(
            "workflow_outcome::{}",
            stable_refresh_key(task, &files, &symbols)
        );
        let source_query = summary
            .map(|value| value.to_string())
            .or(inherited_query)
            .or_else(|| Some(task.to_string()));
        let content =
            summarize_workflow_outcome_content(task, status, summary, &files, &symbols, &tests);
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
            .map_err(|e| {
                (
                    -32603,
                    format!("Failed to find workflow outcome memory: {}", e),
                )
            })?;

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
                .map_err(|e| {
                    (
                        -32603,
                        format!("Failed to refresh workflow outcome memory: {}", e),
                    )
                })?;
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
                .map_err(|e| {
                    (
                        -32603,
                        format!("Failed to store workflow outcome memory: {}", e),
                    )
                })?;
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
        apply_workflow_budget(
            tool_name,
            &mut value,
            budget,
            pruning_profile,
            &mut metadata,
        );

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

        Ok(wrap_workflow_tool_result(value, response_options.render))
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
        let branch = current_git_branch(&self.workspace_root);

        let current = store
            .get_session_memories(&self.session_id, limit.min(3))
            .map_err(|e| (-32603, format!("Failed to load session memories: {}", e)))?;
        let mut values = serialize_memory_values(&store, &current, true)?;

        if values.len() < limit {
            let remaining = limit.saturating_sub(values.len());
            if let Some(ref keyword) = memory_query {
                let previous = store
                    .search_across_sessions(keyword, Some(&self.session_id), remaining)
                    .map_err(|e| (-32603, format!("Failed to search memories: {}", e)))?;
                values.extend(serialize_memory_values(&store, &previous, true)?);
            }
        }

        sort_memory_values_for_recall(&mut values, branch.as_deref());
        dedupe_memory_values(&mut values);
        Ok(values)
    }

    async fn load_durable_memory_values(&self, limit: usize) -> Result<Vec<Value>, (i32, String)> {
        let workspace_id = self.workspace_root.to_string_lossy().to_string();
        let store = self.memory_store.lock().await;
        let branch = current_git_branch(&self.workspace_root);
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
        memories.truncate(limit.max(1).saturating_mul(8));

        let mut values = serialize_memory_values(&store, &memories, true)?;
        sort_memory_values_for_recall(&mut values, branch.as_deref());
        dedupe_memory_values(&mut values);
        values.truncate(limit.max(1));
        Ok(values)
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
        let branch = current_git_branch(&self.workspace_root);
        values.splice(0..0, playbooks);
        values.splice(0..0, outcomes);
        sort_memory_values_for_recall(&mut values, branch.as_deref());
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
            .map_err(|e| {
                (
                    -32603,
                    format!("Failed to load repo playbook memory: {}", e),
                )
            })?
        {
            values.push(serialize_memory_value(&store, &memory, true)?);
        }

        if let Some(memory) = store
            .find_by_refresh_key(&subsystem_key, Some(&workspace_id), branch.as_deref())
            .map_err(|e| {
                (
                    -32603,
                    format!("Failed to load subsystem playbook memory: {}", e),
                )
            })?
        {
            values.push(serialize_memory_value(&store, &memory, true)?);
        }

        sort_memory_values_for_recall(&mut values, branch.as_deref());
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
        let refresh_key = format!(
            "workflow_outcome::{}",
            stable_refresh_key(query, files, symbols)
        );

        let store = self.memory_store.lock().await;
        let mut values = Vec::new();

        if let Some(memory) = store
            .find_by_refresh_key(&refresh_key, Some(&workspace_id), branch.as_deref())
            .map_err(|e| {
                (
                    -32603,
                    format!("Failed to load workflow outcome memory: {}", e),
                )
            })?
        {
            values.push(serialize_memory_value(&store, &memory, true)?);
        }

        if branch.is_some() {
            if let Some(memory) = store
                .find_by_refresh_key(&refresh_key, Some(&workspace_id), None)
                .map_err(|e| (-32603, format!("Failed to load repo outcome memory: {}", e)))?
            {
                values.push(serialize_memory_value(&store, &memory, true)?);
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
                values.extend(serialize_memory_values(&store, &searched, true)?);
            }
        }

        sort_memory_values_for_recall(&mut values, branch.as_deref());
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
            .find_by_refresh_key(&refresh_key, Some(&workspace_id), scoped_branch.as_deref())
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

        let current_values = serialize_memory_values(&store, &current, false)?;
        let mut previous_values = serialize_memory_values(&store, &previous, true)?;
        sort_memory_values_for_recall(
            &mut previous_values,
            current_git_branch(&self.workspace_root).as_deref(),
        );

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

        let mut memory_values = serialize_memory_values(&store, &memories, true)?;
        sort_memory_values_for_recall(
            &mut memory_values,
            current_git_branch(&self.workspace_root).as_deref(),
        );

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

        let entries = serialize_memory_values(&store, &memories, true)?;

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

        let entries = serialize_memory_values(&store, &memories, true)?;

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
        let value = serialize_memory_value(&store, &memory, true)?;

        Ok(wrap_tool_result(json!({
            "status": "refreshed",
            "memory": value
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

        let mut added = 0usize;
        let mut skipped = 0usize;
        let mut skip_reasons: Vec<Value> = Vec::new();

        if self.workspace_manager.is_some() {
            let new_graph = {
                let mut engine = self.engine.lock().await;
                for edge in edges {
                    apply_lsp_edge_to_graph(
                        engine.graph_mut(),
                        edge,
                        &mut added,
                        &mut skipped,
                        &mut skip_reasons,
                    );
                }
                engine.graph().clone()
            };

            let graph_store = self.graph_store.lock().await;
            let _ = graph_store.save_graph(&new_graph);
        } else {
            let new_graph = {
                let mut indexer = self.indexer.lock().await;
                for edge in edges {
                    apply_lsp_edge_to_graph(
                        indexer.graph_mut(),
                        edge,
                        &mut added,
                        &mut skipped,
                        &mut skip_reasons,
                    );
                }
                indexer.graph().clone()
            };

            {
                let graph_store = self.graph_store.lock().await;
                let _ = graph_store.save_graph(&new_graph);
            }

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
        let workspace_roots = self.workspace_roots.clone();
        let indexer = Arc::clone(&self.indexer);
        let engine = Arc::clone(&self.engine);
        let graph_store = Arc::clone(&self.graph_store);
        let indexing = Arc::clone(&self.indexing);
        let workspace_manager = self.workspace_manager.clone();
        let embedding_engine = Arc::clone(&self.embedding_engine);
        let vector_index = self.vector_index.clone();

        indexing.store(true, Ordering::Relaxed);
        tokio::spawn(async move {
            let mut files_indexed = 0usize;
            let mut errors = 0usize;

            let new_graph = if let Some(workspace_manager) = workspace_manager {
                let mut manager = workspace_manager.lock().await;
                for root in &workspace_roots {
                    let repo_name = crate::repo_name_for_root(root);
                    if let Err(e) = manager.add_repo(repo_name.clone(), root.clone()) {
                        tracing::warn!("Failed to reset repo {} for reindex: {}", repo_name, e);
                        errors += 1;
                        continue;
                    }

                    let security_filter = SecurityFilter::new(root);
                    let mut entries = walk_directory_filtered(root, &security_filter);
                    crate::prioritize_indexable_paths(root, &mut entries);
                    for entry_path in &entries {
                        let rel_path = entry_path
                            .strip_prefix(root)
                            .unwrap_or(entry_path)
                            .to_string_lossy()
                            .replace('\\', "/");

                        if !should_index_file(&rel_path) || security_filter.is_excluded(&rel_path) {
                            continue;
                        }

                        match std::fs::read_to_string(entry_path) {
                            Ok(content) => {
                                if manager.index_file(&repo_name, &rel_path, &content).is_ok() {
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
                }
                manager.detect_cross_repo_edges();
                manager.unified_graph()
            } else {
                let security_filter = SecurityFilter::new(&workspace_root);
                let mut entries = walk_directory_filtered(&workspace_root, &security_filter);
                crate::prioritize_indexable_paths(&workspace_root, &mut entries);

                for chunk in entries.chunks(100) {
                    let mut batch = Vec::new();
                    for entry_path in chunk {
                        let rel_path = entry_path
                            .strip_prefix(&workspace_root)
                            .unwrap_or(entry_path)
                            .to_string_lossy()
                            .replace('\\', "/");

                        if !should_index_file(&rel_path) || security_filter.is_excluded(&rel_path) {
                            continue;
                        }

                        match std::fs::read_to_string(entry_path) {
                            Ok(content) => batch.push((rel_path, content)),
                            Err(_) => {
                                errors += 1;
                            }
                        }
                    }

                    if batch.is_empty() {
                        continue;
                    }

                    let snapshot = {
                        let mut idx = indexer.lock().await;
                        match idx.index_file_batch_contents(batch).await {
                            Ok(indexed) => {
                                files_indexed += indexed;
                            }
                            Err(_) => {
                                errors += 1;
                            }
                        }
                        idx.graph().clone()
                    };

                    {
                        let gs = graph_store.lock().await;
                        let _ = gs.save_graph(&snapshot);
                    }

                    let mut eng = engine.lock().await;
                    eng.update_graph(snapshot);
                }

                let idx = indexer.lock().await;
                idx.graph().clone()
            };

            {
                let gs = graph_store.lock().await;
                let _ = gs.save_graph(&new_graph);
            }

            let mut eng = engine.lock().await;
            eng.update_graph(new_graph);
            drop(eng);

            if let (Some(embedding_engine), Some(vector_index)) =
                (embedding_engine.get(), vector_index.as_ref())
            {
                let graph_snapshot = {
                    let eng = engine.lock().await;
                    eng.graph().clone()
                };
                match crate::vector_sync::sync_full_graph_embeddings(
                    &graph_snapshot,
                    embedding_engine.as_ref(),
                    vector_index.as_ref(),
                ) {
                    Ok(stats) => tracing::info!(
                        mode = stats.mode,
                        implementation = stats.implementation,
                        graph_nodes = stats.graph_nodes,
                        nodes_considered = stats.nodes_considered,
                        embedded_nodes = stats.embedded_nodes,
                        failed_nodes = stats.failed_nodes,
                        payload_chars_total = stats.payload_chars_total,
                        payload_chars_avg = stats.payload_chars_avg,
                        payload_chars_max = stats.payload_chars_max,
                        elapsed_ms = stats.elapsed_ms as u64,
                        throughput_nodes_per_sec = stats.throughput_nodes_per_sec(),
                        "Reindex semantic sync complete"
                    ),
                    Err(err) => {
                        tracing::warn!("Reindex graph updated but semantic sync failed: {}", err)
                    }
                }
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

fn apply_lsp_edge_to_graph(
    graph: &mut lattice_core::graph::CodeGraph,
    edge: &Value,
    added: &mut usize,
    skipped: &mut usize,
    skip_reasons: &mut Vec<Value>,
) {
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
            graph.add_edge(&fid, &tid, edge_kind);
            *added += 1;
        }
        (None, None) => {
            skip_reasons.push(json!({
                "from": from_name, "to": to_name,
                "reason": format!("both '{}::{}' and '{}::{}' not found in graph", from_file, from_name, to_file, to_name)
            }));
            *skipped += 1;
        }
        (None, Some(_)) => {
            skip_reasons.push(json!({
                "from": from_name, "to": to_name,
                "reason": format!("source '{}::{}' not found in graph", from_file, from_name)
            }));
            *skipped += 1;
        }
        (Some(_), None) => {
            skip_reasons.push(json!({
                "from": from_name, "to": to_name,
                "reason": format!("target '{}::{}' not found in graph", to_file, to_name)
            }));
            *skipped += 1;
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
                let is_indexing = self.indexing.load(Ordering::Relaxed);
                let engine = self.engine.lock().await;
                let stats = if engine.graph().stats().node_count == 0 && is_indexing {
                    if let Some(wm) = &self.workspace_manager {
                        if let Ok(wm) = wm.try_lock() {
                            let repo_stats = wm.repo_stats();
                            lattice_core::graph::GraphStats {
                                node_count: repo_stats.iter().map(|s| s.node_count).sum(),
                                edge_count: repo_stats.iter().map(|s| s.edge_count).sum(),
                                file_count: repo_stats.iter().map(|s| s.file_count).sum(),
                            }
                        } else if let Ok(idx) = self.indexer.try_lock() {
                            idx.graph().stats()
                        } else {
                            engine.graph().stats()
                        }
                    } else if let Ok(idx) = self.indexer.try_lock() {
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
    let render = match args["render"].as_str() {
        Some("json") => WorkflowRenderMode::Json,
        Some("markdown") => WorkflowRenderMode::Markdown,
        _ => WorkflowRenderMode::Hybrid,
    };
    let max_tokens = args["max_tokens"]
        .as_u64()
        .map(|value| (value as usize).clamp(80, 4000));

    WorkflowResponseOptions {
        budget,
        max_tokens,
        wire_format,
        render,
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

fn select_plan_edit_mode(
    requested: RequestedBundleMode,
    compact: &PlanEditBundle,
) -> (BundleMode, String) {
    match requested {
        RequestedBundleMode::Compact => (BundleMode::Compact, "requested compact mode".to_string()),
        RequestedBundleMode::Full => (BundleMode::Full, "requested full mode".to_string()),
        RequestedBundleMode::Auto => {
            if let Some(reason) = plan_edit_widen_reason(compact) {
                (
                    BundleMode::Full,
                    format!("widened automatically because {}", reason),
                )
            } else {
                (
                    BundleMode::Compact,
                    "kept compact because the edit plan already mapped to concrete patch anchors"
                        .to_string(),
                )
            }
        }
    }
}

fn select_trace_scenario_mode(
    requested: RequestedBundleMode,
    compact: &ScenarioTraceBundle,
) -> (BundleMode, String) {
    match requested {
        RequestedBundleMode::Compact => (BundleMode::Compact, "requested compact mode".to_string()),
        RequestedBundleMode::Full => (BundleMode::Full, "requested full mode".to_string()),
        RequestedBundleMode::Auto => {
            if let Some(reason) = trace_scenario_widen_reason(compact) {
                (
                    BundleMode::Full,
                    format!("widened automatically because {}", reason),
                )
            } else {
                (
                    BundleMode::Compact,
                    "kept compact because the scenario trace already had concrete execution anchors"
                        .to_string(),
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
    } else if bundle.tests.is_empty() && bundle.primary_files.len() <= 1 && bundle.symbols.len() < 2
    {
        Some("supporting symbols and tests were still sparse")
    } else {
        None
    }
}

fn plan_edit_widen_reason(bundle: &PlanEditBundle) -> Option<&'static str> {
    let high_edit_files = bundle
        .edit_files
        .iter()
        .filter(|item| item.confidence_band == "high")
        .count();
    let high_symbols = bundle
        .symbols
        .iter()
        .filter(|item| item.confidence_band == "high")
        .count();

    if bundle.edit_files.is_empty() {
        Some("no likely edit files were identified")
    } else if high_edit_files == 0 && high_symbols == 0 {
        Some("edit anchors were still low-confidence")
    } else if bundle.candidate_spans.is_empty()
        && bundle.affected_callers.is_empty()
        && bundle.affected_dependencies.is_empty()
    {
        Some("candidate spans and downstream impact were too sparse")
    } else if bundle.tests.is_empty()
        && bundle.relevant_docs.is_empty()
        && bundle.stale_doc_signals.is_empty()
    {
        Some("test and documentation guidance were still sparse")
    } else {
        None
    }
}

fn trace_scenario_widen_reason(bundle: &ScenarioTraceBundle) -> Option<&'static str> {
    let high_entrypoints = bundle
        .likely_entrypoints
        .iter()
        .filter(|item| item.confidence_band == "high")
        .count();
    let high_paths = bundle
        .execution_path
        .iter()
        .filter(|item| item.confidence_band == "high")
        .count();

    if bundle.likely_entrypoints.is_empty() {
        Some("no likely scenario entrypoints were identified")
    } else if high_entrypoints == 0 && high_paths == 0 {
        Some("entrypoint and path confidence were still low")
    } else if bundle.execution_path.is_empty() && bundle.plausible_paths.is_empty() {
        Some("execution path candidates were still sparse")
    } else if bundle.guards.is_empty()
        && bundle.side_effects.is_empty()
        && bundle.failure_branches.is_empty()
    {
        Some("guard, side-effect, and failure signals were still sparse")
    } else if bundle.tests.is_empty() && bundle.relevant_docs.is_empty() {
        Some("tests and docs guidance were still sparse")
    } else {
        None
    }
}

fn diff_impact_widen_reason(report: &DiffImpactReport) -> Option<&'static str> {
    if report.changed_symbols.is_empty() && report.affected_symbols.len() < 2 {
        Some("the compact diff view did not resolve enough changed or affected symbols")
    } else if report.tests.is_empty() && report.risks.is_empty() && report.changed_files.len() <= 1
    {
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
    } else if report.key_symbols.is_empty() && report.tests.is_empty() && report.memories.is_empty()
    {
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
        && !capsule.pivots.iter().any(|pivot| {
            entry_symbols
                .iter()
                .any(|symbol| symbol_matches_hint(&pivot.symbol, symbol))
        })
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
            value
                .get("refresh_key")
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
    truncate_array_field(
        object,
        "memory_highlights",
        if pruning_profile.prune_memory_highlights {
            0
        } else {
            1
        },
    );
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
        "plan_edit" => {
            truncate_array_field(object, "edit_files", 1);
            truncate_array_field(object, "supporting_files", 0);
            truncate_array_field(object, "symbols", 1);
            truncate_array_field(object, "candidate_spans", 1);
            truncate_array_field(object, "affected_callers", 1);
            truncate_array_field(object, "affected_dependencies", 0);
            truncate_array_field(object, "relevant_docs", 1);
            truncate_array_strings_field(object, "stale_doc_signals", 1, 72);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "risks", 0);
        }
        "trace_scenario" => {
            truncate_array_field(object, "likely_entrypoints", 1);
            truncate_array_field(object, "plausible_entrypoints", 0);
            truncate_array_field(object, "execution_path", 1);
            truncate_array_field(object, "plausible_paths", 0);
            truncate_array_field(object, "guards", 1);
            truncate_array_field(object, "side_effects", 1);
            truncate_array_field(object, "failure_branches", 1);
            truncate_array_field(object, "relevant_docs", 1);
            truncate_array_field(object, "tests", 1);
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
        "plan_edit" => {
            truncate_array_field(object, "edit_files", 1);
            truncate_array_field(object, "candidate_spans", 1);
            truncate_array_field(object, "symbols", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "affected_callers", 0);
            truncate_array_field(object, "affected_dependencies", 0);
            truncate_array_field(object, "relevant_docs", 0);
            truncate_array_strings_field(object, "stale_doc_signals", 0, 72);
            truncate_array_field(object, "memory_highlights", 0);
        }
        "trace_scenario" => {
            truncate_array_field(object, "likely_entrypoints", 1);
            truncate_array_field(object, "execution_path", 1);
            truncate_array_field(object, "guards", 1);
            truncate_array_field(object, "side_effects", 0);
            truncate_array_field(object, "failure_branches", 1);
            truncate_array_field(object, "tests", 1);
            truncate_array_field(object, "plausible_entrypoints", 0);
            truncate_array_field(object, "plausible_paths", 0);
            truncate_array_field(object, "relevant_docs", 0);
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
        "plan_edit" => {
            first_confidence_band(value, "edit_files") == Some("high")
                || first_confidence_band(value, "candidate_spans") == Some("high")
                || array_len(value, "candidate_spans") == 1
        }
        "trace_scenario" => {
            first_confidence_band(value, "likely_entrypoints") == Some("high")
                || first_confidence_band(value, "execution_path") == Some("high")
                || array_len(value, "execution_path") == 1
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
        "plan_edit" => first_symbol_focus(object, "candidate_spans", "top candidate edit span")
            .or_else(|| first_symbol_focus(object, "symbols", "top edit symbol"))
            .or_else(|| first_file_focus(object, "candidate_spans", "top candidate edit span"))
            .or_else(|| first_file_focus(object, "edit_files", "top edit file")),
        "trace_scenario" => first_symbol_focus(
            object,
            "likely_entrypoints",
            "top likely scenario entrypoint",
        )
        .or_else(|| first_trace_path_focus(object, "execution_path", "top execution path segment"))
        .or_else(|| first_symbol_focus(object, "guards", "top guard signal"))
        .or_else(|| first_symbol_focus(object, "failure_branches", "top failure branch"))
        .or_else(|| first_symbol_focus(object, "side_effects", "top side effect signal"))
        .or_else(|| first_file_focus(object, "likely_entrypoints", "top likely scenario file"))
        .or_else(|| first_trace_path_focus(object, "plausible_paths", "top plausible path")),
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
        "focus": stable_file_focus_value(&file),
        "reason": reason,
    }))
}

fn first_symbol_focus(
    object: &serde_json::Map<String, Value>,
    key: &str,
    reason: &str,
) -> Option<Value> {
    let first = object.get(key)?.as_array()?.first()?;
    let focus = first
        .get("symbol_handle")
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| item.to_string())
        .or_else(|| {
            first
                .get("symbol")
                .and_then(|item| item.as_str())
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| format!("symbol:{}", item))
        })?;

    Some(json!({
        "focus": focus,
        "reason": reason,
    }))
}

fn first_trace_path_focus(
    object: &serde_json::Map<String, Value>,
    key: &str,
    reason: &str,
) -> Option<Value> {
    let first = object.get(key)?.as_array()?.first()?;
    let focus = first
        .get("to_symbol_handle")
        .or_else(|| first.get("from_symbol_handle"))
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| item.to_string())
        .or_else(|| {
            first
                .get("to_symbol")
                .or_else(|| first.get("from_symbol"))
                .and_then(|item| item.as_str())
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| format!("symbol:{}", item))
        })
        .or_else(|| {
            first
                .get("to_file")
                .or_else(|| first.get("from_file"))
                .and_then(|item| item.as_str())
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(stable_file_focus_value)
        })?;

    Some(json!({
        "focus": focus,
        "reason": reason,
    }))
}

fn stable_file_focus_value(file: &str) -> String {
    let trimmed = file.trim();
    if trimmed.starts_with("file_id:") {
        return trimmed.to_string();
    }
    let normalized = file
        .strip_prefix("file:")
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .unwrap_or(trimmed)
        .trim();
    stable_file_handle(normalized)
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
    truncate_array_field(object, "edit_files", 1);
    truncate_array_field(object, "supporting_files", 0);
    truncate_array_field(object, "candidate_spans", 1);
    truncate_array_field(object, "affected_callers", 0);
    truncate_array_field(object, "affected_dependencies", 0);
    truncate_array_field(object, "likely_entrypoints", 1);
    truncate_array_field(object, "plausible_entrypoints", 0);
    truncate_array_field(object, "execution_path", 1);
    truncate_array_field(object, "plausible_paths", 0);
    truncate_array_field(object, "guards", 1);
    truncate_array_field(object, "side_effects", 0);
    truncate_array_field(object, "failure_branches", 1);
    truncate_array_field(object, "relevant_docs", 0);
    truncate_array_strings_field(object, "stale_doc_signals", 0, 72);
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
        Value::Array(items) => {
            Value::Array(items.into_iter().map(densify_workflow_value).collect())
        }
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
        "scenario" => "sn",
        "intent" => "i",
        "primary_files" => "pf",
        "secondary_files" => "sf",
        "edit_files" => "efi",
        "supporting_files" => "sfi",
        "likely_entrypoints" => "le",
        "plausible_entrypoints" => "pe",
        "execution_path" => "ep",
        "plausible_paths" => "pp",
        "guards" => "gd",
        "side_effects" => "sx",
        "failure_branches" => "fb",
        "candidate_spans" => "ps",
        "affected_callers" => "ac",
        "affected_dependencies" => "ad",
        "relevant_docs" => "rd",
        "stale_doc_signals" => "sd",
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
        "assertion_type" => "at",
        "verification_status" => "vs",
        "confidence_reason" => "cr",
        "supersedes_memory_id" => "smi",
        "superseded_by_memory_id" => "sbi",
        "contradicts_memory_ids" => "cms",
        "contradicted_by_memory_ids" => "cbi",
        "freshness_policy" => "fp",
        "freshness_policy_detail" => "fd",
        "provenance" => "pv",
        "evidence" => "ev",
        "source" => "src",
        "reference" => "rf",
        "captured_at" => "cat",
        "detail" => "dt",
        "note" => "nt",
        "reasons" => "rs",
        "kind" => "k",
        "line" => "ln",
        "line_span" => "ls",
        "start_line" => "sl",
        "end_line" => "el",
        "from_symbol" => "frs",
        "from_symbol_handle" => "frh",
        "from_kind" => "frk",
        "from_file" => "frf",
        "from_line" => "frl",
        "to_symbol" => "tos",
        "to_symbol_handle" => "toh",
        "to_kind" => "tok",
        "to_file" => "tof",
        "to_line" => "tol",
        "signal_type" => "sgt",
        "relationship" => "rp",
        "role" => "ro",
        "level" => "lv",
        "message" => "m",
        "matched_files" => "mf",
        "matched_symbols" => "ms",
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

fn truncate_array_field(object: &mut serde_json::Map<String, Value>, key: &str, limit: usize) {
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

fn truncate_string_field(object: &mut serde_json::Map<String, Value>, key: &str, limit: usize) {
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
    value
        .get(key)?
        .as_array()?
        .first()?
        .get("confidence_band")?
        .as_str()
}

fn array_len(value: &Value, key: &str) -> usize {
    value
        .get(key)
        .and_then(|item| item.as_array())
        .map(|items| items.len())
        .unwrap_or(0)
}

fn attach_workflow_metadata(value: &mut Value, metadata: &WorkflowRunMetadata) {
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "delivery_mode".to_string(),
            json!(metadata.delivery_mode.as_str()),
        );
        if metadata.wire_format != "standard" {
            object.insert(
                "wire_format".to_string(),
                json!(metadata.wire_format.as_str()),
            );
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

fn memory_to_value(
    memory: &Memory,
    structured_fields: Option<&MemoryStructuredFields>,
    include_session_id: bool,
) -> Value {
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

    if let Some(object) = value.as_object_mut() {
        if include_session_id {
            object.insert("session_id".to_string(), json!(memory.session_id));
        }

        if let Some(fields) = structured_fields {
            object.insert(
                "assertion_type".to_string(),
                json!(fields.assertion_type.as_str()),
            );
            object.insert(
                "verification_status".to_string(),
                json!(fields.verification_status.as_str()),
            );
            object.insert(
                "confidence_reason".to_string(),
                json!(fields.confidence_reason),
            );
            object.insert(
                "supersedes_memory_id".to_string(),
                json!(fields.supersedes_memory_id),
            );
            object.insert(
                "superseded_by_memory_id".to_string(),
                json!(fields.superseded_by_memory_id),
            );
            object.insert(
                "contradicts_memory_ids".to_string(),
                json!(fields.contradicts_memory_ids),
            );
            object.insert(
                "contradicted_by_memory_ids".to_string(),
                json!(fields.contradicted_by_memory_ids),
            );
            object.insert(
                "freshness_policy".to_string(),
                json!(fields.freshness_policy.as_str()),
            );
            object.insert(
                "freshness_policy_detail".to_string(),
                json!(fields.freshness_policy_detail),
            );
            object.insert("provenance".to_string(), json!(fields.provenance));
            object.insert("evidence".to_string(), json!(fields.evidence));
        }
    }

    value
}

fn serialize_memory_value(
    store: &MemoryStore,
    memory: &Memory,
    include_session_id: bool,
) -> Result<Value, (i32, String)> {
    let structured_fields = store.get_structured_fields(&memory.id).map_err(|e| {
        (
            -32603,
            format!("Failed to load structured memory fields: {}", e),
        )
    })?;
    Ok(memory_to_value(
        memory,
        structured_fields.as_ref(),
        include_session_id,
    ))
}

fn serialize_memory_values(
    store: &MemoryStore,
    memories: &[Memory],
    include_session_id: bool,
) -> Result<Vec<Value>, (i32, String)> {
    memories
        .iter()
        .map(|memory| serialize_memory_value(store, memory, include_session_id))
        .collect()
}

fn memory_verification_status_for_recall(value: &Value) -> &str {
    value
        .get("verification_status")
        .and_then(|item| item.as_str())
        .or_else(|| {
            if value
                .get("is_stale")
                .and_then(|item| item.as_bool())
                .unwrap_or(false)
            {
                Some("stale")
            } else {
                None
            }
        })
        .unwrap_or("unverified")
}

fn memory_assertion_type_for_recall(value: &Value) -> &str {
    value
        .get("assertion_type")
        .and_then(|item| item.as_str())
        .or_else(|| {
            value
                .get("refresh_key")
                .and_then(|item| item.as_str())
                .filter(|key| key.starts_with("workflow_outcome::"))
                .map(|_| "workflow_outcome")
        })
        .or_else(|| {
            value
                .get("type")
                .or_else(|| value.get("memory_type"))
                .and_then(|item| item.as_str())
        })
        .unwrap_or("observation")
}

fn memory_verification_rank(value: &Value) -> i32 {
    match memory_verification_status_for_recall(value) {
        "verified" => 6,
        "in_review" => 5,
        "unverified" => 4,
        "superseded" => 2,
        "contradicted" => 1,
        "stale" => 0,
        _ => 3,
    }
}

fn memory_scope_rank(value: &Value, preferred_branch: Option<&str>) -> i32 {
    match value
        .get("scope")
        .and_then(|item| item.as_str())
        .unwrap_or("session")
    {
        "branch" => {
            if preferred_branch.is_some()
                && value.get("branch").and_then(|item| item.as_str()) == preferred_branch
            {
                4
            } else {
                2
            }
        }
        "repo" => 3,
        "session" => 1,
        _ => 0,
    }
}

fn memory_assertion_rank(value: &Value) -> i32 {
    match memory_assertion_type_for_recall(value) {
        "workflow_outcome" => 4,
        "constraint" => 3,
        "decision" | "pattern" | "anti_pattern" => 2,
        "exploration" => 1,
        _ => 0,
    }
}

fn memory_is_weaker(value: &Value) -> bool {
    matches!(
        memory_verification_status_for_recall(value),
        "stale" | "contradicted" | "superseded"
    ) || value
        .get("superseded_by_memory_id")
        .map(|item| !item.is_null())
        .unwrap_or(false)
        || value
            .get("contradicted_by_memory_ids")
            .and_then(|item| item.as_array())
            .map(|items| !items.is_empty())
            .unwrap_or(false)
        || value
            .get("is_stale")
            .and_then(|item| item.as_bool())
            .unwrap_or(false)
}

fn memory_recall_priority(
    value: &Value,
    preferred_branch: Option<&str>,
) -> (i32, i32, i32, i32, i64, u64, u64) {
    let assertion_type = memory_assertion_type_for_recall(value);
    let is_workflow_outcome = assertion_type == "workflow_outcome"
        || value
            .get("refresh_key")
            .and_then(|item| item.as_str())
            .map(|key| key.starts_with("workflow_outcome::"))
            .unwrap_or(false);
    let confidence = (value
        .get("confidence")
        .and_then(|item| item.as_f64())
        .unwrap_or(0.0)
        * 1000.0)
        .round() as i64;

    (
        memory_verification_rank(value),
        if is_workflow_outcome { 1 } else { 0 },
        if is_workflow_outcome {
            memory_scope_rank(value, preferred_branch)
        } else {
            0
        },
        (memory_assertion_rank(value) * 2)
            + memory_scope_rank(value, preferred_branch)
            + if memory_is_weaker(value) { 0 } else { 1 },
        confidence,
        value
            .get("access_count")
            .and_then(|item| item.as_u64())
            .unwrap_or(0),
        value
            .get("created_at")
            .and_then(|item| item.as_u64())
            .unwrap_or(0),
    )
}

fn sort_memory_values_for_recall(values: &mut [Value], preferred_branch: Option<&str>) {
    values.sort_by(|left, right| {
        memory_recall_priority(right, preferred_branch)
            .cmp(&memory_recall_priority(left, preferred_branch))
    });
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
    let mut files = Vec::new();
    for file in bundle
        .primary_files
        .iter()
        .map(|item| item.file.as_str())
        .chain(bundle.secondary_files.iter().map(|item| item.file.as_str()))
    {
        push_seed_file(&mut files, file);
    }
    normalize_seed_values(&mut files);

    let mut symbols = Vec::new();
    for symbol in &bundle.symbols {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    normalize_seed_values(&mut symbols);

    ExpandContextSeed {
        query: Some(bundle.query.clone()),
        files,
        symbols,
        tests: bundle.tests.iter().map(|item| item.file.clone()).collect(),
        memories: memory_seed_values(&bundle.memories, &bundle.memory_highlights),
    }
}

fn seed_from_plan_edit_bundle(bundle: &PlanEditBundle) -> ExpandContextSeed {
    let mut files = Vec::new();
    for file in bundle
        .edit_files
        .iter()
        .map(|item| item.file.as_str())
        .chain(
            bundle
                .supporting_files
                .iter()
                .map(|item| item.file.as_str()),
        )
        .chain(bundle.candidate_spans.iter().map(|item| item.file.as_str()))
        .chain(
            bundle
                .affected_callers
                .iter()
                .map(|item| item.file.as_str()),
        )
        .chain(
            bundle
                .affected_dependencies
                .iter()
                .map(|item| item.file.as_str()),
        )
    {
        push_seed_file(&mut files, file);
    }
    normalize_seed_values(&mut files);

    let mut symbols = Vec::new();
    for symbol in &bundle.symbols {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    for span in &bundle.candidate_spans {
        push_seed_symbol(
            &mut symbols,
            span.symbol_handle.as_deref(),
            Some(span.symbol.as_str()),
        );
    }
    for impact in &bundle.affected_callers {
        push_seed_symbol(
            &mut symbols,
            impact.symbol_handle.as_deref(),
            Some(impact.symbol.as_str()),
        );
    }
    for impact in &bundle.affected_dependencies {
        push_seed_symbol(
            &mut symbols,
            impact.symbol_handle.as_deref(),
            Some(impact.symbol.as_str()),
        );
    }
    normalize_seed_values(&mut symbols);

    ExpandContextSeed {
        query: Some(bundle.query.clone()),
        files,
        symbols,
        tests: bundle.tests.iter().map(|item| item.file.clone()).collect(),
        memories: memory_seed_values(&bundle.memories, &bundle.memory_highlights),
    }
}

fn seed_from_trace_scenario_bundle(bundle: &ScenarioTraceBundle) -> ExpandContextSeed {
    let mut files = Vec::new();
    for file in bundle
        .likely_entrypoints
        .iter()
        .map(|item| item.file.as_str())
        .chain(
            bundle
                .plausible_entrypoints
                .iter()
                .map(|item| item.file.as_str()),
        )
        .chain(bundle.guards.iter().map(|item| item.file.as_str()))
        .chain(bundle.side_effects.iter().map(|item| item.file.as_str()))
        .chain(
            bundle
                .failure_branches
                .iter()
                .map(|item| item.file.as_str()),
        )
        .chain(bundle.relevant_docs.iter().map(|item| item.file.as_str()))
    {
        push_seed_file(&mut files, file);
    }
    for segment in &bundle.execution_path {
        push_seed_file(&mut files, &segment.from_file);
        push_seed_file(&mut files, &segment.to_file);
    }
    for segment in &bundle.plausible_paths {
        push_seed_file(&mut files, &segment.from_file);
        push_seed_file(&mut files, &segment.to_file);
    }
    normalize_seed_values(&mut files);

    let mut symbols = Vec::new();
    for symbol in bundle
        .likely_entrypoints
        .iter()
        .chain(bundle.plausible_entrypoints.iter())
    {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    for segment in bundle
        .execution_path
        .iter()
        .chain(bundle.plausible_paths.iter())
    {
        push_seed_symbol(
            &mut symbols,
            segment.from_symbol_handle.as_deref(),
            Some(segment.from_symbol.as_str()),
        );
        push_seed_symbol(
            &mut symbols,
            segment.to_symbol_handle.as_deref(),
            Some(segment.to_symbol.as_str()),
        );
    }
    for signal in bundle
        .guards
        .iter()
        .chain(bundle.side_effects.iter())
        .chain(bundle.failure_branches.iter())
    {
        push_seed_symbol(
            &mut symbols,
            signal.symbol_handle.as_deref(),
            Some(signal.symbol.as_str()),
        );
    }
    normalize_seed_values(&mut symbols);

    ExpandContextSeed {
        query: Some(bundle.scenario.clone()),
        files,
        symbols,
        tests: bundle.tests.iter().map(|item| item.file.clone()).collect(),
        memories: Vec::new(),
    }
}

fn seed_from_context_capsule(graph: &CodeGraph, capsule: &ContextCapsule) -> ExpandContextSeed {
    let all_nodes = graph.all_nodes();
    let mut files = Vec::new();
    let mut symbols = Vec::new();

    for item in &capsule.pivots {
        push_seed_file(&mut files, &item.file);
        let symbol_handle =
            resolve_capsule_symbol_handle(&all_nodes, &item.file, &item.symbol, item.line);
        push_seed_symbol(&mut symbols, symbol_handle.as_deref(), Some(&item.symbol));
    }
    for item in &capsule.context {
        push_seed_file(&mut files, &item.file);
        let symbol_handle =
            resolve_capsule_symbol_handle(&all_nodes, &item.file, &item.symbol, item.line);
        push_seed_symbol(&mut symbols, symbol_handle.as_deref(), Some(&item.symbol));
    }
    normalize_seed_values(&mut files);
    normalize_seed_values(&mut symbols);

    ExpandContextSeed {
        query: Some(capsule.query.clone()),
        files,
        symbols,
        tests: Vec::new(),
        memories: capsule.memories.clone(),
    }
}

fn seed_from_diff_impact(report: &DiffImpactReport) -> ExpandContextSeed {
    let mut files = Vec::new();
    for file in report
        .changed_files
        .iter()
        .map(|item| item.file.as_str())
        .chain(
            report
                .affected_symbols
                .iter()
                .map(|item| item.file.as_str()),
        )
    {
        push_seed_file(&mut files, file);
    }
    normalize_seed_values(&mut files);

    let mut symbols = Vec::new();
    for symbol in &report.changed_symbols {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    for symbol in &report.affected_symbols {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    normalize_seed_values(&mut symbols);

    ExpandContextSeed {
        query: None,
        files,
        symbols,
        tests: report.tests.iter().map(|item| item.file.clone()).collect(),
        memories: Vec::new(),
    }
}

fn seed_from_working_set_context(report: &WorkingSetContext) -> ExpandContextSeed {
    let mut files = Vec::new();
    for file in report.files.iter().map(|item| item.file.as_str()) {
        push_seed_file(&mut files, file);
    }
    normalize_seed_values(&mut files);

    let mut symbols = Vec::new();
    for symbol in &report.active_symbols {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    for symbol in &report.nearby_symbols {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    normalize_seed_values(&mut symbols);

    ExpandContextSeed {
        query: report.query.clone(),
        files,
        symbols,
        tests: report.tests.iter().map(|item| item.file.clone()).collect(),
        memories: memory_seed_values(&report.memories, &report.memory_highlights),
    }
}

fn seed_from_failure_diagnosis(report: &FailureDiagnosis) -> ExpandContextSeed {
    let mut files = Vec::new();
    for file in report
        .extracted_files
        .iter()
        .map(|item| item.as_str())
        .chain(report.suspects.iter().map(|item| item.file.as_str()))
    {
        push_seed_file(&mut files, file);
    }
    normalize_seed_values(&mut files);

    let mut symbols = Vec::new();
    for symbol in &report.extracted_symbols {
        push_seed_symbol(&mut symbols, None, Some(symbol));
    }
    for symbol in &report.suspects {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    for symbol in &report.related_symbols {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    normalize_seed_values(&mut symbols);

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
    let mut files = Vec::new();
    for file in report.key_files.iter().map(|item| item.file.as_str()) {
        push_seed_file(&mut files, file);
    }
    normalize_seed_values(&mut files);

    let mut symbols = Vec::new();
    for symbol in &report.key_symbols {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    normalize_seed_values(&mut symbols);

    ExpandContextSeed {
        query: Some(report.query.clone()),
        files,
        symbols,
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
    let mut files = Vec::new();
    for file in report.key_files.iter().map(|item| item.file.as_str()) {
        push_seed_file(&mut files, file);
    }
    normalize_seed_values(&mut files);

    let mut symbols = Vec::new();
    for symbol in &report.notable_symbols {
        push_seed_symbol(
            &mut symbols,
            symbol.symbol_handle.as_deref(),
            Some(symbol.symbol.as_str()),
        );
    }
    normalize_seed_values(&mut symbols);

    ExpandContextSeed {
        query: Some(report.overview.clone()),
        files,
        symbols,
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

fn attach_context_capsule_suggested_expand(value: &mut Value, suggested_expand: Option<Value>) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if object.contains_key("suggested_expand") {
        return;
    }
    if let Some(suggested_expand) = suggested_expand {
        object.insert("suggested_expand".to_string(), suggested_expand);
    }
}

fn context_capsule_suggested_expand(capsule: &ContextCapsule, graph: &CodeGraph) -> Option<Value> {
    let all_nodes = graph.all_nodes();

    if let Some(pivot) = capsule.pivots.first() {
        let focus =
            resolve_capsule_symbol_handle(&all_nodes, &pivot.file, &pivot.symbol, pivot.line)
                .unwrap_or_else(|| stable_file_focus_value(&pivot.file));
        return Some(json!({
            "focus": focus,
            "reason": "Expand the lead pivot to inspect nearby code and relationships."
        }));
    }

    if let Some(context) = capsule.context.first() {
        let focus =
            resolve_capsule_symbol_handle(&all_nodes, &context.file, &context.symbol, context.line)
                .unwrap_or_else(|| stable_file_focus_value(&context.file));
        return Some(json!({
            "focus": focus,
            "reason": "Expand the top supporting symbol to inspect nearby implementation details."
        }));
    }

    None
}

fn resolve_capsule_symbol_handle(
    nodes: &[&GraphNode],
    file: &str,
    symbol: &str,
    line: usize,
) -> Option<String> {
    let exact_line = nodes.iter().copied().find(|node| {
        is_queryable_workflow_file(&node.file)
            && node.file == file
            && node.name == symbol
            && node.line == line
    });
    if let Some(node) = exact_line {
        return Some(node.id.stable_handle());
    }

    let mut matches = nodes.iter().copied().filter(|node| {
        is_queryable_workflow_file(&node.file) && node.file == file && node.name == symbol
    });
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(first.id.stable_handle())
}

fn push_seed_file(files: &mut Vec<String>, file: &str) {
    let trimmed = file.trim();
    if trimmed.is_empty() {
        return;
    }
    files.push(stable_file_focus_value(trimmed));
    files.push(trimmed.to_string());
}

fn push_seed_symbol(
    symbols: &mut Vec<String>,
    symbol_handle: Option<&str>,
    symbol_name: Option<&str>,
) {
    if let Some(handle) = symbol_handle.map(str::trim).filter(|item| !item.is_empty()) {
        symbols.push(handle.to_string());
    }
    if let Some(name) = symbol_name.map(str::trim).filter(|item| !item.is_empty()) {
        symbols.push(name.to_string());
    }
}

fn normalize_seed_values(values: &mut Vec<String>) {
    values.retain(|item| !item.trim().is_empty());
    values.sort();
    values.dedup();
}

fn attach_playbook_memory(value: &mut Value, playbook_memory: Value) {
    if let Some(object) = value.as_object_mut() {
        object.insert("playbook_memory".to_string(), playbook_memory);
    }
}

fn extract_wrapped_tool_metrics(
    value: &Value,
) -> (
    usize,
    usize,
    Option<String>,
    Option<String>,
    ToolCallMetadata,
) {
    let texts: Vec<&str> = value["content"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(|inner| inner.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let payload_bytes = texts.iter().map(|text| text.len()).sum();
    let approx_tokens = payload_bytes / 4;
    let parsed = texts
        .iter()
        .find_map(|text| parse_wrapped_tool_payload(text));

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
            .and_then(|inner| inner.get("single_anchor_used").or_else(|| inner.get("sa")))
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

fn parse_wrapped_tool_payload(text: &str) -> Option<Value> {
    serde_json::from_str::<Value>(text)
        .ok()
        .or_else(|| parse_json_fenced_block(text))
        .or_else(|| parse_markdown_metrics_comment(text))
        .or_else(|| {
            text.split_once("\n\nStructured payload:\n")
                .and_then(|(_, payload)| serde_json::from_str::<Value>(payload).ok())
        })
}

fn parse_json_fenced_block(text: &str) -> Option<Value> {
    let (_, rest) = text.split_once("\n\n### Structured Payload\n```json\n")?;
    let payload = rest.strip_suffix("\n```")?;
    serde_json::from_str::<Value>(payload).ok()
}

fn parse_markdown_metrics_comment(text: &str) -> Option<Value> {
    let (_, payload) = text.rsplit_once("<!-- lattice-metrics: ")?;
    let payload = payload.strip_suffix(" -->")?;
    serde_json::from_str::<Value>(payload).ok()
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
                assertion_type: value
                    .get("assertion_type")
                    .and_then(|item| item.as_str())
                    .map(|item| item.to_string()),
                verification_status: value
                    .get("verification_status")
                    .and_then(|item| item.as_str())
                    .map(|item| item.to_string()),
                confidence_reason: value
                    .get("confidence_reason")
                    .and_then(|item| item.as_str())
                    .map(|item| item.to_string()),
                freshness_policy: value
                    .get("freshness_policy")
                    .and_then(|item| item.as_str())
                    .map(|item| item.to_string()),
                freshness_policy_detail: value
                    .get("freshness_policy_detail")
                    .and_then(|item| item.as_str())
                    .map(|item| item.to_string()),
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
        memory_seed_values, parse_wrapped_tool_payload, report_memory_highlights,
        seed_from_plan_edit_bundle, seed_from_task_bundle, seed_from_trace_scenario_bundle,
        stable_refresh_key, summarize_workflow_outcome_content, wrap_tool_result,
        wrap_workflow_tool_result, McpHandler, RequestHandler, WorkflowRenderMode,
    };
    use lattice_core::graph::CodeGraph;
    use lattice_core::indexer::Indexer;
    use lattice_core::intelligence::ExpandContextSeed;
    use lattice_core::intelligence::{
        EditSpanRecommendation, FileRecommendation, PlanEditBundle, ScenarioPathSegment,
        ScenarioSignal, ScenarioTraceBundle, SymbolRecommendation, TaskBundle,
    };
    use lattice_core::memory::model::{
        Memory, MemoryAssertionType, MemoryEvidence, MemoryFreshnessPolicy, MemoryProvenance,
        MemoryStructuredFields, MemoryVerificationStatus,
    };
    use lattice_core::memory::{MemoryScope, MemoryStore, MemoryType};
    use lattice_core::query::QueryEngine;
    use lattice_core::query::QueryIntent;
    use lattice_core::storage::GraphStore;
    use lattice_core::symbols::SymbolId;
    use lattice_core::symbols::{Language, SymbolKind};
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::Mutex;

    #[test]
    fn test_report_memory_highlights_truncates_content() {
        let long_content = "repo-playbook ".repeat(40);
        let highlights = report_memory_highlights(
            &[json!({
                "content": long_content,
                "type": "pattern",
                "scope": "repo",
                "is_stale": false
            })],
            3,
        );

        assert_eq!(highlights.len(), 1);
        assert!(highlights[0].content.len() <= 123);
        assert!(highlights[0].content.ends_with("..."));
    }

    #[test]
    fn test_build_failure_overview_value_uses_short_memory_reference() {
        let highlights = report_memory_highlights(
            &[json!({
                "content": "durable-note ".repeat(30),
                "type": "pattern",
                "scope": "repo",
                "is_stale": false
            })],
            1,
        );

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
                assertion_type: None,
                verification_status: None,
                confidence_reason: None,
                freshness_policy: None,
                freshness_policy_detail: None,
            }],
        );

        assert_eq!(seeded.len(), 1);
        assert_eq!(seeded[0]["content"], "prior repo pattern");
    }

    #[test]
    fn test_seed_from_task_bundle_includes_stable_and_legacy_handles() {
        let symbol_handle = SymbolId {
            file: "src/auth.ts".to_string(),
            name: "loginUser".to_string(),
            byte_offset: 41,
        }
        .stable_handle();
        let bundle = TaskBundle {
            query: "Fix login timeout".to_string(),
            intent: QueryIntent::FixBug,
            overview: "Likely edit: auth login".to_string(),
            suggested_expand: None,
            primary_files: vec![FileRecommendation {
                file: "src/auth.ts".to_string(),
                score: 9.1,
                confidence_band: "high".to_string(),
                evidence: Vec::new(),
                reasons: vec!["entry file".to_string()],
            }],
            secondary_files: Vec::new(),
            symbols: vec![SymbolRecommendation {
                symbol: "loginUser".to_string(),
                symbol_handle: Some(symbol_handle.clone()),
                kind: "fn".to_string(),
                file: "src/auth.ts".to_string(),
                line: 12,
                role: "pivot".to_string(),
                score: 8.4,
                confidence_band: "high".to_string(),
                evidence: Vec::new(),
            }],
            tests: Vec::new(),
            test_gaps: Vec::new(),
            matched_rules: Vec::new(),
            memories: Vec::new(),
            memory_highlights: Vec::new(),
            risks: Vec::new(),
            rationale: Vec::new(),
            stats: None,
        };

        let seed = seed_from_task_bundle(&bundle);
        assert!(
            seed.files.contains(&"src/auth.ts".to_string()),
            "expected legacy file seed for backward compatibility: {:?}",
            seed.files
        );
        assert!(
            seed.files.contains(&"file_id:src/auth.ts".to_string()),
            "expected stable file handle in seed: {:?}",
            seed.files
        );
        assert!(
            seed.symbols.contains(&"loginUser".to_string()),
            "expected legacy symbol seed for backward compatibility: {:?}",
            seed.symbols
        );
        assert!(
            seed.symbols.contains(&symbol_handle),
            "expected stable symbol handle in seed: {:?}",
            seed.symbols
        );
    }

    #[test]
    fn test_seed_from_plan_edit_bundle_includes_stable_and_legacy_handles() {
        let symbol_handle = SymbolId {
            file: "src/auth.ts".to_string(),
            name: "loginUser".to_string(),
            byte_offset: 41,
        }
        .stable_handle();
        let bundle = PlanEditBundle {
            query: "Fix login timeout".to_string(),
            intent: QueryIntent::FixBug,
            overview: "Patch the auth flow and verify callers.".to_string(),
            suggested_expand: None,
            edit_files: vec![FileRecommendation {
                file: "src/auth.ts".to_string(),
                score: 9.1,
                confidence_band: "high".to_string(),
                evidence: Vec::new(),
                reasons: vec!["entry file".to_string()],
            }],
            supporting_files: Vec::new(),
            symbols: vec![SymbolRecommendation {
                symbol: "loginUser".to_string(),
                symbol_handle: Some(symbol_handle.clone()),
                kind: "fn".to_string(),
                file: "src/auth.ts".to_string(),
                line: 12,
                role: "pivot".to_string(),
                score: 8.4,
                confidence_band: "high".to_string(),
                evidence: Vec::new(),
            }],
            candidate_spans: vec![EditSpanRecommendation {
                file: "src/auth.ts".to_string(),
                symbol: "loginUser".to_string(),
                symbol_handle: Some(symbol_handle.clone()),
                line_span: "12-32".to_string(),
                start_line: 12,
                end_line: 32,
                reason: "Primary auth branch".to_string(),
                confidence_band: "high".to_string(),
            }],
            affected_callers: Vec::new(),
            affected_dependencies: Vec::new(),
            relevant_docs: Vec::new(),
            stale_doc_signals: Vec::new(),
            tests: Vec::new(),
            test_gaps: Vec::new(),
            matched_rules: Vec::new(),
            memories: Vec::new(),
            memory_highlights: Vec::new(),
            risks: Vec::new(),
            rationale: Vec::new(),
            stats: None,
        };

        let seed = seed_from_plan_edit_bundle(&bundle);
        assert!(
            seed.files.contains(&"src/auth.ts".to_string()),
            "expected legacy file seed for backward compatibility: {:?}",
            seed.files
        );
        assert!(
            seed.files.contains(&"file_id:src/auth.ts".to_string()),
            "expected stable file handle in seed: {:?}",
            seed.files
        );
        assert!(
            seed.symbols.contains(&"loginUser".to_string()),
            "expected legacy symbol seed for backward compatibility: {:?}",
            seed.symbols
        );
        assert!(
            seed.symbols.contains(&symbol_handle),
            "expected stable symbol handle in seed: {:?}",
            seed.symbols
        );
    }

    #[test]
    fn test_seed_from_trace_scenario_bundle_includes_stable_and_legacy_handles() {
        let symbol_handle = SymbolId {
            file: "src/auth.ts".to_string(),
            name: "loginUser".to_string(),
            byte_offset: 41,
        }
        .stable_handle();
        let next_symbol_handle = SymbolId {
            file: "src/session.ts".to_string(),
            name: "refreshSession".to_string(),
            byte_offset: 88,
        }
        .stable_handle();
        let bundle = ScenarioTraceBundle {
            scenario: "why does login fail after refresh".to_string(),
            intent: QueryIntent::Explore,
            overview: "Trace login through refresh edge.".to_string(),
            suggested_expand: None,
            likely_entrypoints: vec![SymbolRecommendation {
                symbol: "loginUser".to_string(),
                symbol_handle: Some(symbol_handle.clone()),
                kind: "fn".to_string(),
                file: "src/auth.ts".to_string(),
                line: 12,
                role: "entrypoint".to_string(),
                score: 9.2,
                confidence_band: "high".to_string(),
                evidence: Vec::new(),
            }],
            plausible_entrypoints: Vec::new(),
            execution_path: vec![ScenarioPathSegment {
                from_symbol: "loginUser".to_string(),
                from_symbol_handle: Some(symbol_handle.clone()),
                from_kind: "fn".to_string(),
                from_file: "src/auth.ts".to_string(),
                from_line: 12,
                to_symbol: "refreshSession".to_string(),
                to_symbol_handle: Some(next_symbol_handle),
                to_kind: "fn".to_string(),
                to_file: "src/session.ts".to_string(),
                to_line: 44,
                relationship: "calls".to_string(),
                score: 8.8,
                confidence_band: "high".to_string(),
                rationale: vec!["edge".to_string()],
            }],
            plausible_paths: Vec::new(),
            guards: vec![ScenarioSignal {
                signal_type: "guard".to_string(),
                symbol: "loginUser".to_string(),
                symbol_handle: Some(symbol_handle.clone()),
                kind: "fn".to_string(),
                file: "src/auth.ts".to_string(),
                line: 15,
                summary: "validate credentials".to_string(),
                score: 7.2,
                confidence_band: "medium".to_string(),
            }],
            side_effects: Vec::new(),
            failure_branches: Vec::new(),
            relevant_docs: Vec::new(),
            tests: Vec::new(),
            test_gaps: Vec::new(),
            matched_rules: Vec::new(),
            rationale: Vec::new(),
            stats: None,
        };

        let seed = seed_from_trace_scenario_bundle(&bundle);
        assert!(
            seed.files.contains(&"src/auth.ts".to_string()),
            "expected legacy file seed for backward compatibility: {:?}",
            seed.files
        );
        assert!(
            seed.files.contains(&"file_id:src/auth.ts".to_string()),
            "expected stable file handle in seed: {:?}",
            seed.files
        );
        assert!(
            seed.symbols.contains(&"loginUser".to_string()),
            "expected legacy symbol seed for backward compatibility: {:?}",
            seed.symbols
        );
        assert!(
            seed.symbols.contains(&symbol_handle),
            "expected stable symbol handle in seed: {:?}",
            seed.symbols
        );
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
    fn test_wrap_workflow_tool_result_hybrid_prefixes_summary_before_json_payload() {
        let wrapped = wrap_workflow_tool_result(
            json!({
                "overview": "Likely edit: auth. focus loginUser.",
                "context_handle": "ctx-9",
                "context_origin": "prepare_change",
                "delivery_mode": "compact",
                "primary_files": [
                    { "file": "src/auth.ts" }
                ],
                "symbols": [
                    { "symbol": "loginUser" }
                ],
                "suggested_expand": {
                    "focus": "file:src/auth.ts",
                    "reason": "top file"
                }
            }),
            WorkflowRenderMode::Hybrid,
        );

        let text = wrapped["content"][0]["text"]
            .as_str()
            .expect("expected text payload");
        assert!(text.contains("### Summary"));
        assert!(text.contains("- Overview: Likely edit: auth. focus loginUser."));
        assert!(text.contains("- Top file: `src/auth.ts`"));
        assert!(text.contains("- Top symbol: `loginUser`"));
        assert!(text.contains("### Structured Payload"));
        assert!(text.contains("```json"));

        let (_, _, handle, origin, metadata) = extract_wrapped_tool_metrics(&wrapped);
        assert_eq!(handle.as_deref(), Some("ctx-9"));
        assert_eq!(origin.as_deref(), Some("prepare_change"));
        assert_eq!(metadata.delivery_mode.as_deref(), Some("compact"));
        assert_eq!(
            metadata.suggested_expand_focus.as_deref(),
            Some("file:src/auth.ts")
        );
    }

    #[test]
    fn test_wrap_workflow_tool_result_summarizes_context_capsule_payload() {
        let wrapped = wrap_workflow_tool_result(
            json!({
                "query": "how does auth login work",
                "intent": "Explore",
                "pivots": [
                    {
                        "file": "src/auth.ts",
                        "symbol": "loginUser",
                        "line": 12,
                        "kind": "fn",
                        "source": "fn loginUser() {}",
                        "score": 9.8,
                        "reason": "keyword"
                    }
                ],
                "context": [
                    {
                        "file": "src/session.ts",
                        "symbol": "validateSession",
                        "line": 44,
                        "kind": "fn",
                        "skeleton": "fn validateSession(...)",
                        "relationship": "dependency",
                        "score": 5.1
                    }
                ],
                "context_handle": "ctx-11",
                "context_origin": "get_context_capsule",
                "suggested_expand": {
                    "focus": "symbol:loginUser",
                    "reason": "Expand the lead pivot to inspect nearby code and relationships."
                }
            }),
            WorkflowRenderMode::Hybrid,
        );

        let text = wrapped["content"][0]["text"]
            .as_str()
            .expect("expected text payload");
        assert!(text.contains("### Summary"));
        assert!(text.contains("- Query: how does auth login work"));
        assert!(text.contains("- Top file: `src/auth.ts`"));
        assert!(text.contains("- Top symbol: `loginUser`"));
        assert!(text.contains("- Suggested expand: `symbol:loginUser`"));
        assert!(text.contains("### Structured Payload"));

        let (_, _, handle, origin, metadata) = extract_wrapped_tool_metrics(&wrapped);
        assert_eq!(handle.as_deref(), Some("ctx-11"));
        assert_eq!(origin.as_deref(), Some("get_context_capsule"));
        assert_eq!(
            metadata.suggested_expand_focus.as_deref(),
            Some("symbol:loginUser")
        );
    }

    #[test]
    fn test_wrap_workflow_tool_result_prefers_symbol_handle_in_suggested_expand() {
        let symbol_handle = SymbolId {
            file: "src/auth.ts".to_string(),
            name: "loginUser".to_string(),
            byte_offset: 41,
        }
        .stable_handle();
        let mut object = json!({
            "primary_files": [
                { "file": "src/auth.ts" }
            ],
            "symbols": [
                { "symbol": "loginUser", "symbol_handle": symbol_handle }
            ]
        })
        .as_object()
        .expect("expected object")
        .clone();
        super::ensure_suggested_expand("prepare_change", &mut object);

        let wrapped = wrap_tool_result(Value::Object(object));
        let (_, _, _, _, metadata) = extract_wrapped_tool_metrics(&wrapped);
        let expected = SymbolId {
            file: "src/auth.ts".to_string(),
            name: "loginUser".to_string(),
            byte_offset: 41,
        }
        .stable_handle();
        assert_eq!(
            metadata.suggested_expand_focus.as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn test_plan_edit_suggested_expand_prefers_candidate_span_handle() {
        let symbol_handle = SymbolId {
            file: "src/auth.ts".to_string(),
            name: "loginUser".to_string(),
            byte_offset: 41,
        }
        .stable_handle();
        let mut object = json!({
            "edit_files": [
                { "file": "src/auth.ts" }
            ],
            "candidate_spans": [
                { "file": "src/auth.ts", "symbol": "loginUser", "symbol_handle": symbol_handle }
            ]
        })
        .as_object()
        .expect("expected object")
        .clone();
        super::ensure_suggested_expand("plan_edit", &mut object);

        let wrapped = wrap_tool_result(Value::Object(object));
        let (_, _, _, _, metadata) = extract_wrapped_tool_metrics(&wrapped);
        let expected = SymbolId {
            file: "src/auth.ts".to_string(),
            name: "loginUser".to_string(),
            byte_offset: 41,
        }
        .stable_handle();
        assert_eq!(
            metadata.suggested_expand_focus.as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn test_trace_scenario_suggested_expand_uses_execution_path_handle() {
        let symbol_handle = SymbolId {
            file: "src/session.ts".to_string(),
            name: "refreshSession".to_string(),
            byte_offset: 88,
        }
        .stable_handle();
        let mut object = json!({
            "execution_path": [
                {
                    "from_symbol": "loginUser",
                    "from_file": "src/auth.ts",
                    "to_symbol": "refreshSession",
                    "to_symbol_handle": symbol_handle,
                    "to_file": "src/session.ts"
                }
            ]
        })
        .as_object()
        .expect("expected object")
        .clone();
        super::ensure_suggested_expand("trace_scenario", &mut object);

        let wrapped = wrap_tool_result(Value::Object(object));
        let (_, _, _, _, metadata) = extract_wrapped_tool_metrics(&wrapped);
        let expected = SymbolId {
            file: "src/session.ts".to_string(),
            name: "refreshSession".to_string(),
            byte_offset: 88,
        }
        .stable_handle();
        assert_eq!(
            metadata.suggested_expand_focus.as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn test_wrap_workflow_tool_result_uses_stable_file_handle_when_symbol_missing() {
        let mut object = json!({
            "primary_files": [
                { "file": "src/auth.ts" }
            ],
            "symbols": []
        })
        .as_object()
        .expect("expected object")
        .clone();
        super::ensure_suggested_expand("prepare_change", &mut object);

        let wrapped = wrap_tool_result(Value::Object(object));
        let (_, _, _, _, metadata) = extract_wrapped_tool_metrics(&wrapped);
        assert_eq!(
            metadata.suggested_expand_focus.as_deref(),
            Some("file_id:src/auth.ts")
        );
    }

    #[test]
    fn test_wrap_workflow_tool_result_markdown_keeps_hidden_metrics_comment() {
        let wrapped = wrap_workflow_tool_result(
            json!({
                "overview": "Likely edit: auth. focus loginUser.",
                "context_handle": "ctx-10",
                "context_origin": "prepare_change",
                "delivery_mode": "compact",
                "primary_files": [
                    { "file": "src/auth.ts" }
                ],
                "symbols": [
                    { "symbol": "loginUser" }
                ],
                "suggested_expand": {
                    "focus": "file:src/auth.ts",
                    "reason": "top file"
                }
            }),
            WorkflowRenderMode::Markdown,
        );

        let text = wrapped["content"][0]["text"]
            .as_str()
            .expect("expected text payload");
        assert!(text.contains("### Summary"));
        assert!(text.contains("- Overview: Likely edit: auth. focus loginUser."));
        assert!(!text.contains("### Structured Payload"));
        assert!(text.contains("<!-- lattice-metrics: "));

        let (_, _, handle, origin, metadata) = extract_wrapped_tool_metrics(&wrapped);
        assert_eq!(handle.as_deref(), Some("ctx-10"));
        assert_eq!(origin.as_deref(), Some("prepare_change"));
        assert_eq!(metadata.delivery_mode.as_deref(), Some("compact"));
        assert_eq!(
            metadata.suggested_expand_focus.as_deref(),
            Some("file:src/auth.ts")
        );
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

    fn unique_test_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nanos}"))
    }

    fn build_memory_test_handler(
        session_id: &str,
    ) -> (McpHandler, Arc<Mutex<MemoryStore>>, PathBuf) {
        let workspace_root = unique_test_path("lattice-mcp-memory");
        std::fs::create_dir_all(&workspace_root).expect("failed to create temp workspace");
        let context_cache_path = workspace_root.join("context_handles.json");
        let memory_store = Arc::new(Mutex::new(
            MemoryStore::open_in_memory().expect("memory store"),
        ));
        let handler = McpHandler::new(
            Arc::new(Mutex::new(QueryEngine::new(CodeGraph::new(), None, None))),
            Arc::new(Mutex::new(Indexer::new(workspace_root.clone()))),
            memory_store.clone(),
            Arc::new(Mutex::new(
                GraphStore::open_in_memory().expect("graph store"),
            )),
            Arc::new(OnceLock::new()),
            None,
            workspace_root.clone(),
            context_cache_path,
            session_id.to_string(),
            None,
            vec![workspace_root.clone()],
            Arc::new(AtomicBool::new(false)),
        );

        (handler, memory_store, workspace_root)
    }

    #[tokio::test]
    async fn test_augment_memory_values_with_playbooks_prefers_verified_workflow_outcome() {
        let (handler, memory_store, workspace_root) =
            build_memory_test_handler("session-memory-preference");
        let workspace_id = workspace_root.to_string_lossy().to_string();

        let observation_id = {
            let store = memory_store.lock().await;
            store
                .store(Memory {
                    id: String::new(),
                    session_id: "session-memory-preference".to_string(),
                    content: "Observed a login timeout while replaying refresh flow".to_string(),
                    memory_type: MemoryType::Observation,
                    scope: MemoryScope::Session,
                    confidence: 0.41,
                    linked_symbols: vec!["loginUser".to_string()],
                    linked_files: vec!["src/auth.ts".to_string()],
                    workspace_id: Some(workspace_id.clone()),
                    branch: None,
                    refresh_key: None,
                    source_query: Some("login timeout".to_string()),
                    created_at: 10,
                    last_accessed: 10,
                    access_count: 0,
                    is_stale: false,
                    stale_reason: None,
                })
                .expect("store observation")
        };

        let refresh_key = format!(
            "workflow_outcome::{}",
            stable_refresh_key("login timeout", &[], &[])
        );
        {
            let store = memory_store.lock().await;
            let outcome_id = store
                .store(Memory {
                    id: String::new(),
                    session_id: "session-prior".to_string(),
                    content: "Workflow outcome: login timeout fix validated in code and tests"
                        .to_string(),
                    memory_type: MemoryType::Pattern,
                    scope: MemoryScope::Repo,
                    confidence: 0.96,
                    linked_symbols: vec!["loginUser".to_string()],
                    linked_files: vec!["src/auth.ts".to_string()],
                    workspace_id: Some(workspace_id.clone()),
                    branch: None,
                    refresh_key: Some(refresh_key),
                    source_query: Some("verified from code and tests".to_string()),
                    created_at: 20,
                    last_accessed: 20,
                    access_count: 2,
                    is_stale: false,
                    stale_reason: None,
                })
                .expect("store outcome");
            let mut fields = store
                .get_structured_fields(&outcome_id)
                .expect("load outcome structured fields")
                .unwrap_or_default();
            fields.assertion_type = MemoryAssertionType::WorkflowOutcome;
            fields.verification_status = MemoryVerificationStatus::Verified;
            fields.confidence_reason = Some("Validated by the daemon workflow".to_string());
            store
                .update_structured_fields(&outcome_id, &fields)
                .expect("update outcome structured fields");
        }

        let observation_value = {
            let store = memory_store.lock().await;
            let current = store
                .get_session_memories("session-memory-preference", 5)
                .expect("load current session memories");
            let observation = current
                .iter()
                .find(|memory| memory.id == observation_id)
                .expect("expected stored observation");
            super::serialize_memory_value(&store, observation, true).expect("serialize observation")
        };

        let values = handler
            .augment_memory_values_with_playbooks(
                "login timeout",
                &[],
                &[],
                vec![observation_value],
                2,
            )
            .await
            .expect("augment memory values");

        assert_eq!(values.len(), 2);
        assert_eq!(
            values[0]
                .get("verification_status")
                .and_then(|value| value.as_str()),
            Some("verified")
        );
        assert_eq!(
            values[0]
                .get("assertion_type")
                .and_then(|value| value.as_str()),
            Some("workflow_outcome")
        );
        assert_eq!(
            values[1].get("id").and_then(|value| value.as_str()),
            Some(observation_id.as_str())
        );

        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn test_load_durable_memory_values_prefers_verified_workflow_outcome() {
        let (handler, memory_store, workspace_root) =
            build_memory_test_handler("session-durable-memory");
        let workspace_id = workspace_root.to_string_lossy().to_string();

        let observation_id = {
            let store = memory_store.lock().await;
            store
                .store(Memory {
                    id: String::new(),
                    session_id: "session-observation".to_string(),
                    content: "Repo note about login timeout mitigation".to_string(),
                    memory_type: MemoryType::Observation,
                    scope: MemoryScope::Repo,
                    confidence: 0.92,
                    linked_symbols: vec!["loginUser".to_string()],
                    linked_files: vec!["src/auth.ts".to_string()],
                    workspace_id: Some(workspace_id.clone()),
                    branch: None,
                    refresh_key: None,
                    source_query: Some("login timeout".to_string()),
                    created_at: 11,
                    last_accessed: 11,
                    access_count: 1,
                    is_stale: false,
                    stale_reason: None,
                })
                .expect("store durable observation")
        };

        {
            let store = memory_store.lock().await;
            let outcome_id = store
                .store(Memory {
                    id: String::new(),
                    session_id: "session-outcome".to_string(),
                    content: "Workflow outcome: login timeout fix verified in repo".to_string(),
                    memory_type: MemoryType::Pattern,
                    scope: MemoryScope::Repo,
                    confidence: 0.89,
                    linked_symbols: vec!["loginUser".to_string()],
                    linked_files: vec!["src/auth.ts".to_string()],
                    workspace_id: Some(workspace_id.clone()),
                    branch: None,
                    refresh_key: Some("workflow_outcome::login-timeout".to_string()),
                    source_query: Some("verified from code and tests".to_string()),
                    created_at: 12,
                    last_accessed: 12,
                    access_count: 0,
                    is_stale: false,
                    stale_reason: None,
                })
                .expect("store durable outcome");
            let mut fields = store
                .get_structured_fields(&outcome_id)
                .expect("load durable outcome structured fields")
                .unwrap_or_default();
            fields.assertion_type = MemoryAssertionType::WorkflowOutcome;
            fields.verification_status = MemoryVerificationStatus::Verified;
            store
                .update_structured_fields(&outcome_id, &fields)
                .expect("update durable outcome structured fields");
        }

        let values = handler
            .load_durable_memory_values(2)
            .await
            .expect("load durable memories");

        assert_eq!(values.len(), 2);
        assert_eq!(
            values[0]
                .get("assertion_type")
                .and_then(|value| value.as_str()),
            Some("workflow_outcome")
        );
        assert_eq!(
            values[1].get("id").and_then(|value| value.as_str()),
            Some(observation_id.as_str())
        );

        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn test_search_memory_surfaces_structured_weaker_metadata() {
        let (handler, memory_store, workspace_root) =
            build_memory_test_handler("session-memory-surface");
        let workspace_id = workspace_root.to_string_lossy().to_string();

        let (base_id, superseding_id, contradictor_id, stale_id) = {
            let store = memory_store.lock().await;

            let base_id = store
                .store(Memory {
                    id: String::new(),
                    session_id: "session-base".to_string(),
                    content: "Org isolation contract memory for daemon recall".to_string(),
                    memory_type: MemoryType::Observation,
                    scope: MemoryScope::Repo,
                    confidence: 0.78,
                    linked_symbols: vec!["OrgIsolation".to_string()],
                    linked_files: vec!["src/isolation.rs".to_string()],
                    workspace_id: Some(workspace_id.clone()),
                    branch: None,
                    refresh_key: None,
                    source_query: Some("org isolation".to_string()),
                    created_at: 30,
                    last_accessed: 30,
                    access_count: 1,
                    is_stale: false,
                    stale_reason: None,
                })
                .expect("store base memory");

            let superseding_id = store
                .store(Memory {
                    id: String::new(),
                    session_id: "session-superseding".to_string(),
                    content: "Org isolation contract was replaced by stricter repo guard"
                        .to_string(),
                    memory_type: MemoryType::Decision,
                    scope: MemoryScope::Repo,
                    confidence: 0.93,
                    linked_symbols: vec!["OrgIsolation".to_string()],
                    linked_files: vec!["src/isolation.rs".to_string()],
                    workspace_id: Some(workspace_id.clone()),
                    branch: None,
                    refresh_key: None,
                    source_query: Some("org isolation verified".to_string()),
                    created_at: 31,
                    last_accessed: 31,
                    access_count: 0,
                    is_stale: false,
                    stale_reason: None,
                })
                .expect("store superseding memory");

            let contradictor_id = store
                .store(Memory {
                    id: String::new(),
                    session_id: "session-contradictor".to_string(),
                    content: "Org isolation fallback path contradicts the older contract"
                        .to_string(),
                    memory_type: MemoryType::Decision,
                    scope: MemoryScope::Repo,
                    confidence: 0.87,
                    linked_symbols: vec!["OrgIsolation".to_string()],
                    linked_files: vec!["src/isolation.rs".to_string()],
                    workspace_id: Some(workspace_id.clone()),
                    branch: None,
                    refresh_key: None,
                    source_query: Some("org isolation verified".to_string()),
                    created_at: 32,
                    last_accessed: 32,
                    access_count: 0,
                    is_stale: false,
                    stale_reason: None,
                })
                .expect("store contradictor memory");

            let stale_id = store
                .store(Memory {
                    id: String::new(),
                    session_id: "session-stale".to_string(),
                    content: "Org isolation stale memory after contract change".to_string(),
                    memory_type: MemoryType::Observation,
                    scope: MemoryScope::Repo,
                    confidence: 0.65,
                    linked_symbols: vec!["OrgIsolation".to_string()],
                    linked_files: vec!["src/isolation.rs".to_string()],
                    workspace_id: Some(workspace_id.clone()),
                    branch: None,
                    refresh_key: None,
                    source_query: Some("org isolation".to_string()),
                    created_at: 33,
                    last_accessed: 33,
                    access_count: 0,
                    is_stale: false,
                    stale_reason: None,
                })
                .expect("store stale memory");

            let mut fields = store
                .get_structured_fields(&base_id)
                .expect("load base structured fields")
                .unwrap_or_else(MemoryStructuredFields::default);
            fields.confidence_reason =
                Some("Older observation retained for audit context".to_string());
            fields.freshness_policy = MemoryFreshnessPolicy::ManualReview;
            fields.freshness_policy_detail =
                Some("Re-review after org isolation contract edits".to_string());
            fields.provenance = vec![MemoryProvenance {
                source: "test".to_string(),
                reference: Some("search_memory".to_string()),
                captured_at: Some(40),
                note: Some("targeted daemon regression".to_string()),
            }];
            fields.evidence = vec![MemoryEvidence {
                kind: "file".to_string(),
                reference: Some("src/isolation.rs".to_string()),
                detail: Some("org isolation branch".to_string()),
                captured_at: Some(40),
            }];
            store
                .update_structured_fields(&base_id, &fields)
                .expect("update base structured fields");

            store
                .mark_memory_superseded(&base_id, &superseding_id)
                .expect("mark superseded");
            store
                .mark_memory_contradicted(&base_id, &contradictor_id)
                .expect("mark contradicted");
            store
                .mark_stale_by_symbol("OrgIsolation", "contract changed")
                .expect("mark stale");

            (base_id, superseding_id, contradictor_id, stale_id)
        };

        let response = RequestHandler::handle(
            &handler,
            "tools/call",
            json!({
                "name": "search_memory",
                "arguments": {
                    "query": "org isolation",
                    "limit": 10
                }
            }),
        )
        .await
        .expect("search_memory tools/call should succeed");

        let text = response["content"][0]["text"]
            .as_str()
            .expect("expected wrapped search_memory response text");
        let payload = parse_wrapped_tool_payload(text).expect("expected parseable wrapped payload");
        let memories = payload["memories"]
            .as_array()
            .expect("expected memories array");

        let base = memories
            .iter()
            .find(|memory| {
                memory.get("id").and_then(|value| value.as_str()) == Some(base_id.as_str())
            })
            .expect("expected contradicted memory");
        assert_eq!(
            base.get("verification_status")
                .and_then(|value| value.as_str()),
            Some("stale")
        );
        assert_eq!(
            base.get("superseded_by_memory_id")
                .and_then(|value| value.as_str()),
            Some(superseding_id.as_str())
        );
        assert!(base
            .get("contradicted_by_memory_ids")
            .and_then(|value| value.as_array())
            .is_some_and(|ids| ids
                .iter()
                .any(|id| id.as_str() == Some(contradictor_id.as_str()))));
        assert_eq!(
            base.get("freshness_policy")
                .and_then(|value| value.as_str()),
            Some("manual_review")
        );
        assert_eq!(
            base.get("freshness_policy_detail")
                .and_then(|value| value.as_str()),
            Some("Re-review after org isolation contract edits")
        );
        assert_eq!(
            base.get("confidence_reason")
                .and_then(|value| value.as_str()),
            Some("Older observation retained for audit context")
        );
        assert!(base
            .get("provenance")
            .and_then(|value| value.as_array())
            .is_some_and(|items| !items.is_empty()));
        assert!(base
            .get("evidence")
            .and_then(|value| value.as_array())
            .is_some_and(|items| !items.is_empty()));
        assert_eq!(
            base.get("type").and_then(|value| value.as_str()),
            Some("observation")
        );
        assert_eq!(
            base.get("scope").and_then(|value| value.as_str()),
            Some("repo")
        );
        assert_eq!(
            base.get("is_stale").and_then(|value| value.as_bool()),
            Some(true)
        );

        let stale = memories
            .iter()
            .find(|memory| {
                memory.get("id").and_then(|value| value.as_str()) == Some(stale_id.as_str())
            })
            .expect("expected stale memory");
        assert_eq!(
            stale
                .get("verification_status")
                .and_then(|value| value.as_str()),
            Some("stale")
        );
        assert_eq!(
            stale.get("stale_reason").and_then(|value| value.as_str()),
            Some("contract changed")
        );

        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn test_plan_edit_tool_path_returns_context_handle_and_origin() {
        let mut graph = CodeGraph::new();
        graph.add_node(
            SymbolId {
                file: "src/auth.ts".to_string(),
                name: "loginUser".to_string(),
                byte_offset: 41,
            },
            SymbolKind::Function,
            "loginUser".to_string(),
            "function loginUser(credentials) {}".to_string(),
            "function loginUser(credentials) {\n  return authenticate(credentials);\n}".to_string(),
            "src/auth.ts".to_string(),
            12,
            30,
            true,
            Language::TypeScript,
        );

        let workspace_root = unique_test_path("lattice-mcp-plan-edit");
        std::fs::create_dir_all(&workspace_root).expect("failed to create temp workspace");
        let context_cache_path = workspace_root.join("context_handles.json");

        let handler = McpHandler::new(
            Arc::new(Mutex::new(QueryEngine::new(graph, None, None))),
            Arc::new(Mutex::new(Indexer::new(workspace_root.clone()))),
            Arc::new(Mutex::new(
                MemoryStore::open_in_memory().expect("memory store"),
            )),
            Arc::new(Mutex::new(
                GraphStore::open_in_memory().expect("graph store"),
            )),
            Arc::new(OnceLock::new()),
            None,
            workspace_root.clone(),
            context_cache_path.clone(),
            "session-test-plan-edit".to_string(),
            None,
            vec![workspace_root.clone()],
            Arc::new(AtomicBool::new(false)),
        );

        let response = RequestHandler::handle(
            &handler,
            "tools/call",
            json!({
                "name": "plan_edit",
                "arguments": {
                    "query": "fix loginUser timeout",
                    "mode": "compact",
                    "render": "json"
                }
            }),
        )
        .await
        .expect("plan_edit tools/call should succeed");

        let text = response["content"][0]["text"]
            .as_str()
            .expect("expected wrapped plan_edit response text");
        let payload = parse_wrapped_tool_payload(text).expect("expected parseable wrapped payload");
        let origin = payload
            .get("context_origin")
            .or_else(|| payload.get("o"))
            .and_then(|value| value.as_str());
        assert_eq!(origin, Some("plan_edit"));
        let delivery_mode = payload
            .get("delivery_mode")
            .or_else(|| payload.get("dm"))
            .and_then(|value| value.as_str());
        assert!(
            matches!(delivery_mode, Some("compact" | "tiny")),
            "expected compact/tiny delivery mode for plan_edit payload, got {delivery_mode:?} in {payload:?}"
        );
        assert!(
            payload
                .get("context_handle")
                .or_else(|| payload.get("h"))
                .and_then(|value| value.as_str())
                .is_some_and(|value| !value.is_empty()),
            "expected non-empty context_handle in plan_edit payload: {payload:?}"
        );

        let _ = std::fs::remove_file(context_cache_path);
        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn test_trace_scenario_tool_path_returns_context_handle_and_origin() {
        let mut graph = CodeGraph::new();
        graph.add_node(
            SymbolId {
                file: "src/auth.ts".to_string(),
                name: "loginUser".to_string(),
                byte_offset: 41,
            },
            SymbolKind::Function,
            "loginUser".to_string(),
            "function loginUser(credentials) {}".to_string(),
            "function loginUser(credentials) {\n  return authenticate(credentials);\n}".to_string(),
            "src/auth.ts".to_string(),
            12,
            30,
            true,
            Language::TypeScript,
        );

        let workspace_root = unique_test_path("lattice-mcp-trace-scenario");
        std::fs::create_dir_all(&workspace_root).expect("failed to create temp workspace");
        let context_cache_path = workspace_root.join("context_handles.json");

        let handler = McpHandler::new(
            Arc::new(Mutex::new(QueryEngine::new(graph, None, None))),
            Arc::new(Mutex::new(Indexer::new(workspace_root.clone()))),
            Arc::new(Mutex::new(
                MemoryStore::open_in_memory().expect("memory store"),
            )),
            Arc::new(Mutex::new(
                GraphStore::open_in_memory().expect("graph store"),
            )),
            Arc::new(OnceLock::new()),
            None,
            workspace_root.clone(),
            context_cache_path.clone(),
            "session-test-trace-scenario".to_string(),
            None,
            vec![workspace_root.clone()],
            Arc::new(AtomicBool::new(false)),
        );

        let response = RequestHandler::handle(
            &handler,
            "tools/call",
            json!({
                "name": "trace_scenario",
                "arguments": {
                    "scenario": "why does loginUser fail after refresh",
                    "mode": "compact",
                    "render": "json"
                }
            }),
        )
        .await
        .expect("trace_scenario tools/call should succeed");

        let text = response["content"][0]["text"]
            .as_str()
            .expect("expected wrapped trace_scenario response text");
        let payload = parse_wrapped_tool_payload(text).expect("expected parseable wrapped payload");
        let origin = payload
            .get("context_origin")
            .or_else(|| payload.get("o"))
            .and_then(|value| value.as_str());
        assert_eq!(origin, Some("trace_scenario"));
        let delivery_mode = payload
            .get("delivery_mode")
            .or_else(|| payload.get("dm"))
            .and_then(|value| value.as_str());
        assert!(
            matches!(delivery_mode, Some("compact" | "tiny")),
            "expected compact/tiny delivery mode for trace_scenario payload, got {delivery_mode:?} in {payload:?}"
        );
        assert!(
            payload
                .get("context_handle")
                .or_else(|| payload.get("h"))
                .and_then(|value| value.as_str())
                .is_some_and(|value| !value.is_empty()),
            "expected non-empty context_handle in trace_scenario payload: {payload:?}"
        );

        let _ = std::fs::remove_file(context_cache_path);
        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn test_expand_context_tool_path_supports_stable_and_legacy_focus() {
        let mut graph = CodeGraph::new();
        let cert_symbol_id = SymbolId {
            file: "routers/certificates.py".to_string(),
            name: "_verify_org_access".to_string(),
            byte_offset: 66,
        };
        graph.add_node(
            cert_symbol_id.clone(),
            SymbolKind::Function,
            "_verify_org_access".to_string(),
            "def _verify_org_access(org_id):".to_string(),
            "def _verify_org_access(org_id):\n    raise HTTPException(status_code=403)".to_string(),
            "routers/certificates.py".to_string(),
            66,
            72,
            false,
            Language::Python,
        );
        graph.add_node(
            SymbolId {
                file: "routers/compliance_mgmt/_shared.py".to_string(),
                name: "_verify_org_access".to_string(),
                byte_offset: 14,
            },
            SymbolKind::Function,
            "_verify_org_access".to_string(),
            "def _verify_org_access(perms, org_id):".to_string(),
            "def _verify_org_access(perms, org_id):\n    return perms.validate(org_id)".to_string(),
            "routers/compliance_mgmt/_shared.py".to_string(),
            14,
            19,
            false,
            Language::Python,
        );

        let workspace_root = unique_test_path("lattice-mcp-expand-context");
        std::fs::create_dir_all(&workspace_root).expect("failed to create temp workspace");
        let context_cache_path = workspace_root.join("context_handles.json");

        let handler = McpHandler::new(
            Arc::new(Mutex::new(QueryEngine::new(graph, None, None))),
            Arc::new(Mutex::new(Indexer::new(workspace_root.clone()))),
            Arc::new(Mutex::new(
                MemoryStore::open_in_memory().expect("memory store"),
            )),
            Arc::new(Mutex::new(
                GraphStore::open_in_memory().expect("graph store"),
            )),
            Arc::new(OnceLock::new()),
            None,
            workspace_root.clone(),
            context_cache_path.clone(),
            "session-test-expand-stable".to_string(),
            None,
            vec![workspace_root.clone()],
            Arc::new(AtomicBool::new(false)),
        );

        let stable_focus = cert_symbol_id.stable_handle();
        let seed = ExpandContextSeed {
            query: Some("Fix certificate access checks".to_string()),
            files: vec![
                "file_id:routers/certificates.py".to_string(),
                "routers/certificates.py".to_string(),
                "file_id:routers/compliance_mgmt/_shared.py".to_string(),
                "routers/compliance_mgmt/_shared.py".to_string(),
            ],
            symbols: vec![stable_focus.clone(), "_verify_org_access".to_string()],
            tests: Vec::new(),
            memories: Vec::new(),
        };

        let handle = handler.store_context_handle("prepare_change", seed).await;

        let stable_response = RequestHandler::handle(
            &handler,
            "tools/call",
            json!({
                "name": "expand_context",
                "arguments": {
                    "handle": handle,
                    "focus": stable_focus
                }
            }),
        )
        .await
        .expect("stable focus expand_context call should succeed");
        let stable_text = stable_response["content"][0]["text"]
            .as_str()
            .expect("expected wrapped stable response text");
        let stable_payload =
            parse_wrapped_tool_payload(stable_text).expect("expected parseable wrapped payload");
        assert_eq!(
            stable_payload["context_origin"].as_str(),
            Some("prepare_change")
        );
        assert_eq!(stable_payload["focus_type"].as_str(), Some("symbol"));
        let stable_first = stable_payload["symbols"]
            .as_array()
            .and_then(|items| items.first())
            .expect("expected expanded symbol context for stable focus");
        assert_eq!(stable_first["symbol"].as_str(), Some("_verify_org_access"));
        assert_eq!(
            stable_first["file"].as_str(),
            Some("routers/certificates.py")
        );

        let legacy_response = RequestHandler::handle(
            &handler,
            "tools/call",
            json!({
                "name": "expand_context",
                "arguments": {
                    "handle": stable_payload["context_handle"].as_str().expect("context handle"),
                    "focus": "symbol:_verify_org_access"
                }
            }),
        )
        .await
        .expect("legacy focus expand_context call should succeed");
        let legacy_text = legacy_response["content"][0]["text"]
            .as_str()
            .expect("expected wrapped legacy response text");
        let legacy_payload =
            parse_wrapped_tool_payload(legacy_text).expect("expected parseable wrapped payload");
        assert_eq!(
            legacy_payload["context_origin"].as_str(),
            Some("prepare_change")
        );
        assert_eq!(legacy_payload["focus_type"].as_str(), Some("symbol"));
        let legacy_first = legacy_payload["symbols"]
            .as_array()
            .and_then(|items| items.first())
            .expect("expected expanded symbol context for legacy focus");
        assert_eq!(legacy_first["symbol"].as_str(), Some("_verify_org_access"));
        let legacy_file = legacy_first["file"]
            .as_str()
            .expect("legacy symbol should include file");
        assert!(
            legacy_file == "routers/certificates.py"
                || legacy_file == "routers/compliance_mgmt/_shared.py",
            "legacy focus should resolve to one of duplicate symbol files, got {legacy_file}"
        );

        let _ = std::fs::remove_file(context_cache_path);
        let _ = std::fs::remove_dir_all(workspace_root);
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
    let text = serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
    wrap_text_result(text)
}

fn wrap_workflow_tool_result(value: Value, render: WorkflowRenderMode) -> Value {
    let serialized = serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
    let summary = build_tool_result_summary(&value)
        .unwrap_or_else(|| "- Structured workflow result ready.".to_string());

    match render {
        WorkflowRenderMode::Json => wrap_text_result(serialized),
        WorkflowRenderMode::Markdown => {
            let mut text = format!("### Summary\n{}", summary);
            if let Some(comment) = build_workflow_metrics_comment(&value) {
                text.push_str("\n\n");
                text.push_str(&comment);
            }
            wrap_text_result(text)
        }
        WorkflowRenderMode::Hybrid => wrap_text_result(format!(
            "### Summary\n{summary}\n\n### Structured Payload\n```json\n{serialized}\n```"
        )),
    }
}

fn wrap_text_result(text: String) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": text
        }]
    })
}

fn build_workflow_metrics_comment(value: &Value) -> Option<String> {
    let metadata = build_workflow_metrics_payload(value)?;
    let serialized = serde_json::to_string(&metadata).ok()?;
    Some(format!("<!-- lattice-metrics: {} -->", serialized))
}

fn build_workflow_metrics_payload(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    let mut metadata = serde_json::Map::new();

    copy_object_alias_value(
        object,
        &mut metadata,
        "context_handle",
        &["context_handle", "h"],
    );
    copy_object_alias_value(
        object,
        &mut metadata,
        "context_origin",
        &["context_origin", "o"],
    );
    copy_object_alias_value(
        object,
        &mut metadata,
        "delivery_mode",
        &["delivery_mode", "dm"],
    );
    copy_object_alias_value(object, &mut metadata, "wire_format", &["wire_format", "wf"]);
    copy_object_alias_value(
        object,
        &mut metadata,
        "single_anchor_used",
        &["single_anchor_used", "sa"],
    );
    copy_object_alias_value(
        object,
        &mut metadata,
        "semantic_fallback_used",
        &["semantic_fallback_used", "se"],
    );
    copy_object_alias_value(
        object,
        &mut metadata,
        "outcome_memory_reuse_count",
        &["outcome_memory_reuse_count", "or"],
    );

    if let Some(suggested_expand) =
        object_get(object, &["suggested_expand", "x"]).and_then(|item| item.as_object())
    {
        let mut suggested = serde_json::Map::new();
        copy_object_alias_value(suggested_expand, &mut suggested, "focus", &["focus", "fo"]);
        copy_object_alias_value(suggested_expand, &mut suggested, "reason", &["reason", "r"]);
        if !suggested.is_empty() {
            metadata.insert("suggested_expand".to_string(), Value::Object(suggested));
        }
    }

    if metadata.is_empty() {
        None
    } else {
        Some(Value::Object(metadata))
    }
}

fn copy_object_alias_value(
    source: &serde_json::Map<String, Value>,
    target: &mut serde_json::Map<String, Value>,
    key: &str,
    aliases: &[&str],
) {
    if let Some(value) = object_get(source, aliases) {
        target.insert(key.to_string(), value.clone());
    }
}

fn object_get<'a>(object: &'a serde_json::Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| object.get(*key))
}

fn build_tool_result_summary(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    let mut lines = Vec::new();
    let overview = object_get(object, &["overview", "ov"])
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|item| !item.is_empty());

    if overview.is_none() {
        if let Some(scenario) = object_get(object, &["scenario", "sn"])
            .and_then(|item| item.as_str())
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            lines.push(format!("- Scenario: {}", truncate_text_value(scenario, 96)));
        } else if let Some(query) = object_get(object, &["query", "q"])
            .and_then(|item| item.as_str())
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            lines.push(format!("- Query: {}", truncate_text_value(query, 96)));
        }
    }

    if let Some(overview) = overview {
        lines.push(format!("- Overview: {}", overview));
    }

    if let Some(file) = first_result_file(object) {
        lines.push(format!("- Top file: `{}`", file));
    }

    if let Some(symbol) = first_result_symbol(object) {
        lines.push(format!("- Top symbol: `{}`", symbol));
    }

    if let Some(step) = object_get(object, &["next_steps", "nx"])
        .and_then(|item| item.as_array())
        .and_then(|items| items.first())
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        lines.push(format!("- Next step: {}", step));
    }

    if let Some(suggested_expand) =
        object_get(object, &["suggested_expand", "x"]).and_then(|item| item.as_object())
    {
        let focus = suggested_expand
            .get("focus")
            .or_else(|| suggested_expand.get("fo"))
            .and_then(|item| item.as_str())
            .map(str::trim)
            .filter(|item| !item.is_empty());
        let reason = suggested_expand
            .get("reason")
            .or_else(|| suggested_expand.get("r"))
            .and_then(|item| item.as_str())
            .map(str::trim)
            .filter(|item| !item.is_empty());

        if let Some(focus) = focus {
            match reason {
                Some(reason) => lines.push(format!("- Suggested expand: `{}` ({})", focus, reason)),
                None => lines.push(format!("- Suggested expand: `{}`", focus)),
            }
        }
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn first_result_file(object: &serde_json::Map<String, Value>) -> Option<String> {
    [
        "pivots",
        "context",
        "primary_files",
        "pf",
        "edit_files",
        "efi",
        "supporting_files",
        "sfi",
        "likely_entrypoints",
        "le",
        "plausible_entrypoints",
        "pe",
        "execution_path",
        "ep",
        "plausible_paths",
        "pp",
        "guards",
        "gd",
        "side_effects",
        "sx",
        "failure_branches",
        "fb",
        "candidate_spans",
        "ps",
        "affected_callers",
        "ac",
        "affected_dependencies",
        "ad",
        "relevant_docs",
        "rd",
        "changed_files",
        "cf",
        "files",
        "fs",
        "key_files",
        "kf",
        "tests",
        "ts",
        "extracted_files",
        "ef",
        "changed_symbols",
        "cs",
    ]
    .iter()
    .find_map(|key| {
        object
            .get(*key)
            .and_then(|item| item.as_array())
            .and_then(|items| items.first())
            .and_then(first_item_file)
    })
}

fn first_result_symbol(object: &serde_json::Map<String, Value>) -> Option<String> {
    [
        "pivots",
        "context",
        "symbols",
        "sy",
        "likely_entrypoints",
        "le",
        "plausible_entrypoints",
        "pe",
        "execution_path",
        "ep",
        "plausible_paths",
        "pp",
        "guards",
        "gd",
        "side_effects",
        "sx",
        "failure_branches",
        "fb",
        "candidate_spans",
        "ps",
        "affected_callers",
        "ac",
        "affected_dependencies",
        "ad",
        "active_symbols",
        "as",
        "key_symbols",
        "ks",
        "suspects",
        "su",
        "related_symbols",
        "ry",
        "changed_symbols",
        "cs",
        "notable_symbols",
        "no",
    ]
    .iter()
    .find_map(|key| {
        object
            .get(*key)
            .and_then(|item| item.as_array())
            .and_then(|items| items.first())
            .and_then(first_item_symbol)
    })
}

fn first_item_file(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    value
        .get("file")
        .or_else(|| value.get("from_file"))
        .or_else(|| value.get("to_file"))
        .or_else(|| value.get("frf"))
        .or_else(|| value.get("tof"))
        .or_else(|| value.get("f"))
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToString::to_string)
}

fn first_item_symbol(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    value
        .get("symbol")
        .or_else(|| value.get("from_symbol"))
        .or_else(|| value.get("to_symbol"))
        .or_else(|| value.get("frs"))
        .or_else(|| value.get("tos"))
        .or_else(|| value.get("s"))
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToString::to_string)
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
