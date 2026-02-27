# Lattice Project Instructions

## Always Use the Lattice MCP Tools

This project has a Lattice MCP server configured. **Always use Lattice tools for code exploration and context gathering before using file-based tools.** Lattice provides dependency graph analysis that understands code relationships across the entire codebase.

### Tool Usage Guidelines

1. **Start with `get_context_capsule`** when exploring unfamiliar code or answering questions about the codebase. It returns a Context Capsule with the most relevant symbols and their relationships.

2. **Use `get_impact_graph` before making changes** to understand the full impact. It shows all transitive dependents up to N hops, so you know what might break.

3. **Use `get_dependents` / `get_dependencies`** to understand how symbols relate to each other — who calls what, who imports what.

4. **Use `search_symbols`** to find symbols by name pattern across the entire code graph (faster and more semantic than grep for symbol lookups).

5. **Use `get_skeleton`** to understand all symbols defined in a file along with their dependent counts — gives you an instant overview of a file's role.

6. **Use `get_symbol`** to get full details on a specific symbol including its source code, signature, dependents, and dependencies.

7. **Use `save_observation`** to persist insights, decisions, and patterns. Use `get_session_context` to retrieve current + previous session memories, and `search_memory` to search across all sessions.

8. **Use `search_logic_flow`** to find execution paths between two symbols — traces call chains from source to target.

9. **Use `submit_lsp_edges`** to enrich the graph with high-confidence call hierarchy edges from LSP.

10. **Use `workspace_setup`** to get project conventions, language breakdown, and recommended configuration.

11. **Use `index_status`** to check indexing progress, graph stats, and language breakdown.

12. **Use `get_project_rules`** to discover project conventions and patterns automatically detected from the codebase.

### Workflow

- Before reading files manually, check if Lattice has the context you need via `get_context_capsule` or `search_symbols`
- Before editing code, run `get_impact_graph` on the symbols you plan to change
- After making architectural decisions, store them with `save_observation` for future reference

## Project Structure

- `extension/` — VS Code extension (TypeScript), communicates with daemon over JSON-RPC stdio
- `daemon/` — Rust backend with tree-sitter parsing, petgraph dependency graph, SQLite storage
- `daemon/crates/lattice-core/` — Core library (parser, graph, query engine, indexer, storage)
- `daemon/crates/lattice-daemon/` — Binary entry point + RPC server

## Build

- Daemon: `cd daemon && cargo build --release`
- Extension: `cd extension && npm install && npm run compile`
