# Lattice Project Instructions

## Always Use the Lattice MCP Tools

This project has a Lattice MCP server configured. **Always use Lattice tools for code exploration and context gathering before using file-based tools.** Lattice provides dependency graph analysis that understands code relationships across the entire codebase.

### Tool Priority

1. **Start with `get_context_capsule`** when exploring unfamiliar code or answering questions about the codebase. It returns ranked pivots (full source) and context (signatures) within a token budget.
2. **Use `get_impact_graph` before making changes** to understand what depends on the symbols you plan to change.
3. **Use `search_symbols`** instead of grep for finding functions/classes by name — it's faster and understands symbol boundaries.
4. **Use `get_skeleton`** to understand a file's role before reading it — shows all symbols with their dependent counts.
5. **Use `get_symbol`** for full details on a specific symbol including source code, signature, and relationships.
6. **Use `save_observation`** to persist insights, decisions, and patterns across sessions.
7. **Use `search_logic_flow`** to find call chains between two symbols.

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
- Tests: `cd daemon && cargo test --workspace` (89 tests)

## Deploy

After building, update both binary locations:

```
pkill -f lattice && sleep 2
cp daemon/target/release/lattice extension/bin/
cp daemon/target/release/lattice ~/.vscode/extensions/lattice.lattice-0.1.0/bin/
```
