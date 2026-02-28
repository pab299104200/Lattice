# Lattice Project Instructions

## Lattice Context Engine — Available Tools

Lattice provides a dependency graph and context engine for this codebase.
Use these tools when they're the best fit:

- `get_context_capsule` — when exploring unfamiliar code or broad questions (use `mode: "focused"` for targeted lookups)
- `get_impact_graph` — before refactoring to understand blast radius
- `search_symbols` — when looking for a symbol by name across the project
- `get_skeleton` — for a quick overview of a large file's structure
- `search_logic_flow` — to trace call chains between functions
- `save_observation` / `get_session_context` / `search_memory` — persist and recall insights across sessions
- `list_observations` — to review stored memories and clean up stale ones
- `update_observation` — to edit an existing observation's content in-place
- `delete_observation` — to remove obsolete or incorrect memories

For targeted edits to known files, Read/Grep/Edit are fine.
Lattice adds the most value when you don't already know where to look.

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
