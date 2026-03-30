# Lattice Project Instructions

## Lattice Context Engine — Available Tools

Lattice provides a dependency graph and context engine for this codebase.
If you're unsure which tool to use, default to `prepare_change` for implementation tasks and `get_context_capsule` for understanding tasks.
If the task starts from a failing test, stack trace, or compiler error, start with `diagnose_failure` and use `prepare_change` after it narrows the likely culprit.

Use these tools when they're the best fit:

- `prepare_change` — first choice for "fix/add/refactor X" once you know the area to change and want likely edit files, tests, risks, and nearby memory in one result
- `get_context_capsule` — first choice for unfamiliar subsystems or broad questions (use `mode: "focused"` for targeted lookups)
- `get_skeleton` — use before opening a large file when you want structure without full source
- `impact_from_diff` — when reviewing a diff or local change and you want downstream impact plus tests
- `find_relevant_tests` — when deciding what tests to run for a file, symbol, or diff
- `diagnose_failure` — first choice when a fix starts from a stack trace, compiler error, or failing test and you need likely culprit symbols before planning the change
- `expand_context` — when a prior workflow call returned a handle and you only want the next delta, not a full bundle again
- `get_working_set_context` — only when you already have a few open files and want a compressed working set bundle instead of reading them one by one
- `get_impact_graph` — before refactoring to understand blast radius
- `search_symbols` — when looking for a symbol by name across the project
- `search_logic_flow` — to trace call chains between functions
- `save_observation` / `get_session_context` / `search_memory` — persist and recall insights across sessions
- `list_observations` — to review stored memories and clean up stale ones
- `list_stale_memories` — to find memories that likely need to be refreshed
- `promote_observation` / `refresh_memory` — to keep durable memory accurate instead of re-creating it
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
- Tests: `cd daemon && cargo test --workspace`

## Deploy

After building, update both binary locations:

```
pkill -f lattice && sleep 2
cp daemon/target/release/lattice extension/bin/
cp daemon/target/release/lattice ~/.vscode/extensions/lattice.lattice-0.1.0/bin/
```
