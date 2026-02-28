# Lattice

Local AI context engine for VS Code. Lattice builds a dependency graph of your codebase using tree-sitter parsing and serves semantically ranked context to LLM coding assistants via MCP (Model Context Protocol).

Instead of sending entire files or relying on text search, Lattice gives your AI assistant precisely the functions, classes, and relationships it needs — typically using 65-70% fewer tokens.

## How It Works

1. **Indexes your codebase** — tree-sitter parses every source file into symbols (functions, classes, interfaces) and edges (calls, imports, extends)
2. **Builds a dependency graph** — petgraph stores the full call graph with centrality scores, cross-directory relationships, and IDF-weighted keyword indices
3. **Serves Context Capsules** — when an LLM asks "how does authentication work?", the query engine returns the most relevant pivot symbols (full source) and context symbols (signatures only), within a token budget
4. **Remembers across sessions** — observations, decisions, and patterns persist in SQLite and surface automatically when relevant

## Supported Languages

Python, TypeScript, JavaScript, Rust, Go, Java

## Architecture

```
VS Code Extension (TypeScript)
    │
    │  JSON-RPC over stdio
    ▼
Lattice Daemon (Rust)
    ├── tree-sitter parser (6 languages)
    ├── petgraph dependency graph
    ├── query engine (keyword + graph scoring)
    ├── memory store (SQLite)
    └── file watcher (incremental re-indexing)
```

- `extension/` — VS Code extension, sidebar UI, daemon lifecycle management
- `daemon/crates/lattice-core/` — Core library: parser, graph, query engine, indexer, storage
- `daemon/crates/lattice-daemon/` — Binary entry point, JSON-RPC server, MCP tool handlers

## Prerequisites

- Rust 1.75+ (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
- Node.js 18+ and npm
- VS Code 1.85+

## Build

```bash
# Daemon
cd daemon && cargo build --release

# Extension
cd extension && npm install && npm run compile
```

## Install

### VS Code Extension (development)

1. Build both daemon and extension (see above)
2. Copy or symlink the extension directory into VS Code's extensions folder:

```bash
# Create the extension directory
mkdir -p ~/.vscode/extensions/lattice.lattice-0.1.0

# Copy extension files
cp -r extension/out extension/package.json extension/resources \
      ~/.vscode/extensions/lattice.lattice-0.1.0/

# Copy the daemon binary
mkdir -p ~/.vscode/extensions/lattice.lattice-0.1.0/bin
cp daemon/target/release/lattice ~/.vscode/extensions/lattice.lattice-0.1.0/bin/
```

3. Reload VS Code. The Lattice icon appears in the activity bar.

### Updating the Binary

The daemon process holds the binary open. To update:

```bash
pkill -f lattice
sleep 2
cp daemon/target/release/lattice ~/.vscode/extensions/lattice.lattice-0.1.0/bin/
# Also update the repo copy if you have one
cp daemon/target/release/lattice extension/bin/
```

If you get "Text file busy", use rename-then-move:

```bash
mv ~/.vscode/extensions/lattice.lattice-0.1.0/bin/lattice \
   ~/.vscode/extensions/lattice.lattice-0.1.0/bin/lattice.old
cp daemon/target/release/lattice ~/.vscode/extensions/lattice.lattice-0.1.0/bin/
rm ~/.vscode/extensions/lattice.lattice-0.1.0/bin/lattice.old
```

## MCP Server Setup

Lattice exposes its tools via MCP. To connect it to Claude Code, Codex, or any MCP-compatible client, add to your project's `.mcp.json`:

```json
{
  "mcpServers": {
    "lattice": {
      "type": "stdio",
      "command": "/path/to/lattice",
      "args": ["--stdio", "--workspace", "/path/to/your/project"]
    }
  }
}
```

## MCP Tools

| Tool | Description |
|------|-------------|
| `get_context_capsule` | Query the codebase — returns ranked pivots (full source) and context (signatures) within a token budget |
| `get_symbol` | Full details on a specific symbol: source, signature, dependents, dependencies |
| `get_dependents` | All symbols that depend on a given symbol (incoming edges) |
| `get_dependencies` | All symbols a given symbol depends on (outgoing edges) |
| `get_impact_graph` | Transitive dependents up to N hops — what breaks if this changes |
| `search_symbols` | Find symbols by name pattern across the code graph |
| `get_skeleton` | File structure overview — symbols, kinds, and dependent counts |
| `search_logic_flow` | Find call chains between two symbols |
| `save_observation` | Store a decision, pattern, or insight for future sessions |
| `get_session_context` | Retrieve memories from current + previous sessions |
| `search_memory` | Search across all session memories |
| `submit_lsp_edges` | Enrich the graph with LSP call hierarchy edges |
| `workspace_setup` | Project conventions, language breakdown, recommended config |
| `index_status` | Indexing progress, graph stats, language breakdown |
| `get_project_rules` | Auto-detected project conventions and patterns |

## Query Engine

The query engine (v31) combines keyword matching with graph-based scoring to find relevant symbols. Key mechanisms:

- **IDF-weighted keyword scoring** — per-word inverse document frequency with a 30% floor
- **Graph traversal** — follows call/import edges from seed hits, with cross-directory decay
- **Hub dampening** — log-compressed centrality prevents infrastructure functions from dominating
- **Keyword coherence gate** — graph-traversed nodes must share at least one query word
- **Negative keyword signal** — symbols with 2+ strong name parts absent from the query get capped (prevents wrong-subsystem matches)
- **Word-boundary matching** — `split_identifier` prevents "dispatch" from matching "patch"
- **Intent detection** — adjusts budget and scoring weights for Explore/FixBug/Refactor/AddFeature queries

Benchmarked at 96.5% average precision.

## Running Tests

```bash
cd daemon && cargo test --workspace
# 89 tests: 85 core + 4 daemon
```

## LLM Memory Instructions

Add the following to your LLM assistant's project memory (`CLAUDE.md`, `AGENTS.md`, Codex instructions, or equivalent) when working on codebases with Lattice enabled:

```markdown
### Lattice Context Engine — Available Tools

Lattice provides a dependency graph and context engine for this codebase.
Use these tools when they're the best fit:

- `get_context_capsule` — when exploring unfamiliar code or broad questions (use `mode: "focused"` for targeted lookups)
- `get_impact_graph` — before refactoring to understand blast radius
- `search_symbols` — when looking for a symbol by name across the project
- `get_skeleton` — for a quick overview of a large file's structure
- `search_logic_flow` — to trace call chains between functions
- `save_observation` / `get_session_context` / `search_memory` — persist and recall insights across sessions
- `list_observations` — to review stored memories and clean up stale ones
- `delete_observation` — to remove obsolete or incorrect memories

For targeted edits to known files, Read/Grep/Edit are fine.
Lattice adds the most value when you don't already know where to look.
```

## License

MIT
