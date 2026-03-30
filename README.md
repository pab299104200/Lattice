# Lattice

Local AI context engine for VS Code and MCP-enabled coding assistants.

Lattice indexes your codebase and repo Markdown into a dependency graph, then serves ranked context, workflow bundles, docs navigation, compact summaries, and persistent memory to assistants like Codex and Claude Code. Instead of sending whole files or relying on broad search, it returns the files, symbols, docs sections, tests, and prior decisions that are most likely to matter.

## Why Lattice

- `get_skeleton` and `get_context_capsule` cut discovery cost before an assistant starts reading source
- Workflow tools like `prepare_change`, `impact_from_diff`, and `diagnose_failure` collapse multi-step coding tasks into one or two calls
- Markdown docs, runbooks, and decisions are first-class graph nodes with backlinks, outgoing links, and code mentions
- `find_stale_docs` helps catch docs that likely drifted after code or runbook changes
- `expand_context` reuses a prior handle and returns only the next delta
- Memory is persistent, scoped, refreshable, and stale-aware
- Compact workflow shaping now defaults to small assistant-friendly responses instead of large generic payloads

## Current Measured Results

Current guidance from internal evaluation:

- Broad codebase exploration: about `60-80%` fewer tokens
- Deep discovery tasks: about `35-55%` fewer tokens
- Mixed coding workloads: about `40%` fewer tokens
- Current synthetic workflow benchmark: about `1305B` average payload, about `327` estimated tokens, `100%` top-3 hit, `100%` target hit, about `2.6` calls saved, and `100%` stale-memory precision

That synthetic workflow benchmark reflects the current compact workflow stack, including:

- auto `tiny` / `compact` / `full` shaping
- high-confidence single-anchor responses
- optional dense wire format
- restart-persistent `context_handle` reuse

## What You Get In VS Code

After installing Lattice from the VS Code marketplace (`SPOGRMM.lattice-mcp`), you get:

- a Lattice activity-bar view with daemon status and index statistics
- a **Knowledge Freshness** panel with changed-file context and likely stale docs
- an **Agent Efficiency** panel with session token/efficiency metrics
- a **Docs Graph** workbench that follows the active editor, can pin a target, and lets you walk backlinks and outgoing links visually
- Markdown CodeLens actions for `Open Docs Graph`, `Backlinks`, and `Outgoing`
- maintenance actions in the sidebar: `Re-index Workspace`, `Clear Memory`, and `Open Docs Graph`
- Command Palette actions for workflow tools like `Prepare Change`, `Analyze Diff Impact`, `Diagnose Failure`, and `Expand Context`
- Command Palette actions for docs workflows like `Get Docs Capsule`, `Show Backlinks`, `Show Outgoing Links`, `Find Stale Docs`, and `Open Docs Graph`
- a bundled daemon that starts automatically for the current workspace

## Which Tool First?

If you are not sure which tool to call, use this order:

1. `diagnose_failure`
   First choice when the task starts from a failing test, stack trace, compiler error, or runtime failure.
2. `prepare_change`
   First choice for implementation work once the problem area is known: fix a bug, add a feature, refactor a path, or plan an edit.
3. `get_context_capsule`
   First choice for understanding work: unfamiliar subsystems, broad architecture questions, or "how does X work?"
4. `get_skeleton`
   Use before opening a large file when you want structure without full source.
5. `summarize_subsystem`
   Use when you want a summary-first map of a subsystem instead of a broad capsule.
6. `impact_from_diff`
   Use when you already have a diff or local edits and want downstream impact plus tests.
7. `find_relevant_tests`
   Use when test selection is the main question.
8. `expand_context`
   Use after a workflow tool returns a `context_handle` and you only want the next focused delta.
9. `get_docs_capsule`
   Use when the question is really about how the repo explains a subsystem: runbooks, decisions, design docs, or implementation notes.

Lower-priority helpers:

- `get_working_set_context` is best when the assistant already has a few open files and wants them compressed into one bundle
- `get_repo_playbook` is best for quickly refreshing repo-wide conventions and architecture patterns
- `get_backlinks` and `get_outgoing_links` are best for walking the local docs graph around a known symbol, file, document, or section
- `find_stale_docs` is best after a diff or active edit when you want to see what docs may now be out of date
- memory hygiene tools matter most in long-running or repeated assistant sessions where observations and outcomes are actually being written

Practical rule:

- `diagnose_failure` decides where the failure is coming from
- `prepare_change` decides what to edit
- `get_context_capsule` decides how the code works
- `get_skeleton` decides whether a file is worth opening

## VS Code Commands

### Workflow Commands

- `Lattice: Prepare Change`
- `Lattice: Analyze Diff Impact`
- `Lattice: Get Working Set Context`
- `Lattice: Diagnose Failure`
- `Lattice: Expand Context`

### Docs And Knowledge Commands

- `Lattice: Get Docs Capsule`
- `Lattice: Show Backlinks`
- `Lattice: Show Outgoing Links`
- `Lattice: Find Stale Docs`
- `Lattice: Open Docs Graph`

### Workspace Commands

- `Lattice: Re-index Workspace`
- `Lattice: Show Status`
- `Lattice: Show Dependents`
- `Lattice: Clear Memory`

## MCP Server Setup

Add Lattice to your project's `.mcp.json`:

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

### Summary-First Discovery And Planning

- `get_skeleton`
  Fast file map: symbols, kinds, and structure before loading source.
- `get_context_capsule`
  Broad discovery tool for unfamiliar subsystems or architectural questions.
- `summarize_subsystem`
  Summary-first subsystem map: key files, key symbols, tests, and memories in a compact bundle.
- `get_repo_playbook`
  Repo-wide architecture and convention summary for fast session startup.
- `prepare_change`
  Change-oriented bundle: likely edit files, symbols, tests, memory, and risks.
- `impact_from_diff`
  Diff review bundle: changed symbols, affected code, review checklist, and tests.
- `diagnose_failure`
  Failure triage bundle: likely culprit symbols, tests, likely causes, and next steps.
- `expand_context`
  Focused delta expansion from a prior `context_handle`.

### Tests, Working Set, And Refactoring

- `find_relevant_tests`
  Rank tests from files, symbols, or diff text.
- `get_working_set_context`
  Compress active files, nearby symbols, tests, and memory into one bundle.
- `get_symbol`
  Full symbol details: source, signature, dependencies, and dependents.
- `get_dependencies`
  Outbound dependencies for a symbol.
- `get_dependents`
  Inbound dependents for a symbol.
- `get_impact_graph`
  Transitive blast radius before refactoring.
- `search_symbols`
  Symbol lookup by name pattern.
- `search_logic_flow`
  Call-chain tracing between two functions or symbols.
- `submit_lsp_edges`
  Add high-confidence LSP edges to enrich the graph.

### Docs, Decisions, And Runbooks

- `get_docs_capsule`
  Return the most relevant Markdown documents and sections for a natural-language query, plus related code symbols mentioned from those docs.
- `get_backlinks`
  Return inbound Markdown references to a symbol, file, document, or section.
- `get_outgoing_links`
  Return outgoing Markdown links and code mentions from a document, section, or file target.
- `find_stale_docs`
  Flag docs and sections that likely need review because they mention changed symbols, changed files, or changed docs.

### Memory, Outcomes, And Long-Running Context

- `save_observation`
  Store a decision, pattern, or note for later reuse.
- `get_session_context`
  Recall current-session plus relevant previous-session memory.
- `search_memory`
  Search stored memory across sessions.
- `list_observations`
  Review stored memories.
- `list_stale_memories`
  Find memories that likely need refresh.
- `promote_observation`
  Promote a memory from session scope into branch or repo scope.
- `refresh_memory`
  Refresh a memory in place with new evidence while preserving identity.
- `update_observation`
  Edit stored memory content in place.
- `delete_observation`
  Remove obsolete or incorrect memory.
- `record_workflow_outcome`
  Persist successful workflow outcomes so future sessions can reuse real solutions.

### Observability And Workspace Understanding

- `index_status`
  Current indexing progress and graph stats.
- `get_session_metrics`
  Session-level efficiency metrics: token usage, delivery mix, follow-up avoidance, handle reuse, and outcome-memory reuse.
- `get_project_rules`
  Auto-detected repo conventions and recurring patterns.
- `workspace_setup`
  Workspace conventions, language breakdown, and recommended setup.

## Workflow Response Controls

Workflow tools support assistant-oriented response shaping:

- `mode`
  Existing high-level mode selection (`auto`, `compact`, `full`)
- `budget`
  Output budget control: `tiny`, `compact`, or `full`
- `max_tokens`
  Approximate hard cap for returned payload size
- `wire_format`
  `standard` or `dense`

What this means in practice:

- Lattice can auto-select `tiny`, `compact`, or `full` depending on confidence and signal quality
- high-confidence results may collapse to a single anchor plus `suggested_expand`
- workflow responses can include a `context_handle` and a `suggested_expand` target
- `expand_context` handles persist across daemon restarts
- `get_session_metrics` exposes how often tiny/dense/single-anchor paths are actually being used

## Docs Graph Workflow

Use this flow when you want to navigate repo docs the way people use Obsidian-style vault graphs, but grounded in code and assistant workflows:

1. Open a Markdown file or place the cursor on a symbol.
2. Use `Lattice: Open Docs Graph` or the Markdown CodeLens `Lattice: Open Docs Graph`.
3. In the graph panel, follow the active editor automatically or pin the current target.
4. Click `Focus` on incoming or outgoing nodes to walk the local graph.
5. Click `Open` to jump straight to the linked file or section.

Use these shortcuts when you do not need the full panel:

- `Lattice: Get Docs Capsule` for a natural-language docs query like "how does auth login work?"
- `Lattice: Show Backlinks` to see which docs mention the current symbol or section
- `Lattice: Show Outgoing Links` to inspect what a document points to
- `Lattice: Find Stale Docs` after a staged diff, working tree change, or active edit

What Lattice indexes in the docs graph:

- Markdown files such as `README.md`, `docs/**`, runbooks, ADRs, scorecards, and repo notes
- document nodes and section nodes
- Markdown links and wiki-links
- inline code references from docs into code symbols

## How Lattice Works

1. **Indexes your codebase and Markdown**
   tree-sitter parses source into symbols and relationships, Markdown parsing extracts document sections and links, and a file watcher keeps the graph updated incrementally.
2. **Builds a unified dependency graph**
   petgraph stores calls, imports, inheritance, doc links, code mentions from docs, and related edges.
3. **Ranks context**
   keyword scoring, graph traversal, hub dampening, and query-intent heuristics identify high-signal symbols and docs.
4. **Shapes workflow and docs bundles**
   task, diff, failure, working-set, docs, and summary tools build compact assistant-facing payloads on top of the graph.
5. **Remembers across sessions**
   SQLite-backed memories, playbooks, outcomes, stale-memory tracking, and stale-doc workflows preserve useful context without blindly replaying old notes.

The workflow layer is built on top of the core discovery primitives. `get_skeleton` and `get_context_capsule` remain foundational.

## Supported Languages

Python, TypeScript, JavaScript, Rust, Go, Java, Markdown

## Query Engine Notes

The retrieval core combines keyword matching with graph scoring. Important behaviors:

- IDF-weighted keyword scoring
- graph traversal from seed hits
- hub dampening to avoid infrastructure functions dominating
- keyword coherence gating
- negative keyword signal for wrong-subsystem suppression
- word-boundary matching
- intent detection for Explore / FixBug / Refactor / AddFeature queries

The newer workflow tools layer task reasoning and response shaping on top of that retrieval core.

## Current Validation

Latest verification:

```bash
cd daemon
cargo test --workspace
cargo test workflow_bench_scorecard -- --ignored --nocapture
cd ../extension
./node_modules/.bin/tsc --noEmit -p ./
```

Current test baseline:

- `123` passing core tests
- `17` passing daemon tests
- extension TypeScript build passes with `tsc --noEmit`

## Assistant Memory Instructions

Add this to your assistant's project memory (`CLAUDE.md`, `AGENTS.md`, Codex instructions, or equivalent):

```markdown
### Lattice Context Engine — Available Tools

Lattice provides a dependency graph and context engine for this codebase.
If you're unsure which tool to use, default to `prepare_change` for implementation tasks and `get_context_capsule` for understanding tasks.
If the task starts from a failing test, stack trace, or compiler error, start with `diagnose_failure` and use `prepare_change` after it narrows the likely culprit.

Use these tools when they're the best fit:

- `prepare_change` — first choice for "fix/add/refactor X" once you know the area to change
- `get_context_capsule` — first choice for unfamiliar subsystems or broad questions
- `get_docs_capsule` — first choice for "what docs or runbooks explain this?" questions
- `get_skeleton` — use before opening a large file
- `summarize_subsystem` — use for a summary-first subsystem map
- `get_repo_playbook` — use to refresh repo-wide conventions and architecture
- `impact_from_diff` — use when reviewing a diff or local change
- `find_relevant_tests` — use when deciding what tests to run
- `diagnose_failure` — first choice when a fix starts from a failing test or error
- `expand_context` — use when a prior workflow call returned a handle and you want the next delta
- `get_backlinks` / `get_outgoing_links` — walk the local docs graph around a known section, file, or symbol
- `find_stale_docs` — check docs after code changes or before a release
- `get_working_set_context` — only when batching several already-known open files is cheaper than reading them one by one
- `get_impact_graph` — before refactoring to understand blast radius
- `search_symbols` — when looking for a symbol by name
- `search_logic_flow` — to trace call chains between functions
- `save_observation` / `get_session_context` / `search_memory` — persist and recall insights across sessions
- `list_observations` / `list_stale_memories` / `promote_observation` / `refresh_memory` — keep durable memory accurate
- `update_observation` / `delete_observation` — maintain existing memories
- `record_workflow_outcome` — store successful outcomes so later sessions can reuse them

For targeted edits to known files, direct Read/Grep/Edit are still fine.
Lattice adds the most value when you do not already know where to look.
```

## License

MIT
