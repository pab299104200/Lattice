# Lattice Project Instructions

## Always Use the Lattice MCP Tools

This project has a Lattice MCP server configured. **Always use Lattice tools for code exploration and context gathering before using file-based tools.** Lattice provides dependency graph analysis that understands code relationships across the entire codebase.

### Tool Usage Guidelines

1. **Start with `query_context`** when exploring unfamiliar code or answering questions about the codebase. It returns a Context Capsule with the most relevant symbols and their relationships.

2. **Use `blast_radius` before making changes** to understand the full impact. It shows all transitive dependents up to N hops, so you know what might break.

3. **Use `get_dependents` / `get_dependencies`** to understand how symbols relate to each other — who calls what, who imports what.

4. **Use `search_symbols`** to find symbols by name pattern across the entire code graph (faster and more semantic than grep for symbol lookups).

5. **Use `get_file_context`** to understand all symbols defined in a file along with their dependent counts — gives you an instant overview of a file's role.

6. **Use `get_symbol`** to get full details on a specific symbol including its source code, signature, dependents, and dependencies.

7. **Use `store_memory` / `recall_memories`** to persist insights, decisions, and patterns across the conversation. Store observations about the codebase architecture, decisions made, and anti-patterns discovered.

8. **Use `get_project_rules`** to discover project conventions and patterns automatically detected from the codebase.

### Workflow

- Before reading files manually, check if Lattice has the context you need via `query_context` or `search_symbols`
- Before editing code, run `blast_radius` on the symbols you plan to change
- After making architectural decisions, store them with `store_memory` for future reference

## Project Structure

- `extension/` — VS Code extension (TypeScript), communicates with daemon over JSON-RPC stdio
- `daemon/` — Rust backend with tree-sitter parsing, petgraph dependency graph, SQLite storage
- `daemon/crates/lattice-core/` — Core library (parser, graph, query engine, indexer, storage)
- `daemon/crates/lattice-daemon/` — Binary entry point + RPC server

## Build

- Daemon: `cd daemon && cargo build --release`
- Extension: `cd extension && npm install && npm run compile`
