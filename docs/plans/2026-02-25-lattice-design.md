# Lattice — Design Document

**Date:** 2026-02-25
**Status:** Approved

## Problem

AI coding agents read entire files, dump them into context, and burn through tokens fast. On a 50-file project, a single question can consume 30k+ tokens of irrelevant code. Responses suffer. Costs go up.

Cloud-based context engines solve this — but they require your code to leave the building.

## Solution

Lattice builds a live dependency graph of your codebase using advanced static analysis — entirely on your machine. When an AI agent asks a question, Lattice returns only the pivot nodes that matter — the exact functions, classes, and types relevant to the task — plus compact skeletons of surrounding context.

**Target: 65-70% fewer tokens, faster responses, dramatically more accurate answers. No accounts. No API keys. No code leaving your laptop.**

---

## Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Name | Lattice | Evokes interconnected structure |
| Languages | Multi-language via Tree-sitter | TS, Python, Go, Rust, Java, etc. Grammar-pluggable |
| Integration | MCP server + VS Code extension UI | MCP for AI agents, VS Code for human-facing features |
| Runtime | Rust sidecar daemon + TypeScript extension | Concurrency, performance, no extension host blocking |
| Memory store | SQLite + sqlite-vec | Single file, vector search, no external dependencies |
| Graph store | In-memory petgraph + SQLite persistence | Fast traversal, survives restarts |
| Embeddings | Local ONNX model (all-MiniLM-L6-v2, ~23MB) | Zero network calls, fast inference |
| Scale target | 50k+ files | Incremental + lazy indexing |
| Query engine | Graph traversal + semantic ranking | Balances accuracy with speed |
| Memory capture | Automatic + manual | Auto-capture patterns, agent can also store explicitly |

---

## 1. System Architecture

Two-process architecture:

```
┌─────────────────────────────────────┐
│         VS Code Extension           │
│         (TypeScript)                │
│                                     │
│  ┌──────────┐  ┌─────────────────┐  │
│  │ Sidebar  │  │ CodeLens/Hover  │  │
│  │ Panel    │  │ Providers       │  │
│  └────┬─────┘  └───────┬─────────┘  │
│       │                │            │
│  ┌────┴────────────────┴─────────┐  │
│  │    Extension Host Controller  │  │
│  │    (lifecycle, IPC, config)   │  │
│  └────────────┬──────────────────┘  │
└───────────────┼──────────────────────┘
                │ stdio JSON-RPC
┌───────────────┼──────────────────────┐
│  Lattice Daemon (Rust)               │
│               │                      │
│  ┌────────────┴──────────────────┐   │
│  │      Request Router           │   │
│  │  (MCP + internal commands)    │   │
│  └──┬──────┬──────┬──────┬───┘   │
│     │      │      │      │       │
│  ┌──┴──┐┌──┴──┐┌──┴──┐┌──┴───┐  │
│  │Index││Query││Mem- ││Watch │  │
│  │  er ││Eng. ││ory  ││  er  │  │
│  └──┬──┘└──┬──┘└──┬──┘└──┬───┘  │
│     │      │      │      │       │
│  ┌──┴──────┴──────┴──────┴───┐   │
│  │   Storage Layer           │   │
│  │  petgraph | SQLite | ONNX │   │
│  └───────────────────────────┘   │
└──────────────────────────────────┘
```

### Lattice Daemon (Rust)

Single Rust binary, spawned by the VS Code extension on activation:

- **Indexer** — Tree-sitter parsing, AST extraction, symbol resolution, incremental updates
- **Query Engine** — Context Capsule generation (semantic search + graph traversal + ranking)
- **Memory** — Session memory CRUD, observation storage, stale-knowledge detection
- **Watcher** — File system events → incremental re-index of changed files
- **Storage** — petgraph (in-memory graph), SQLite + sqlite-vec (persistence + vectors), ONNX Runtime (embeddings)

### VS Code Extension (TypeScript)

Lightweight thin client:

- Manages daemon lifecycle (spawn, health check, restart)
- Provides UI: sidebar panel, CodeLens, hover info, status bar
- Routes MCP tool calls from AI agents to the daemon
- No heavy computation

### Communication

stdio JSON-RPC between extension and daemon. The daemon also serves as the MCP server directly (MCP uses stdio JSON-RPC natively), so AI agents can connect to it without going through the extension.

---

## 2. Dependency Graph Model

### Node Types

| Node Type | Examples | Stored Data |
|---|---|---|
| `Function` | `loginUser()`, `def validate()` | name, signature, params, return type, full body, file:line |
| `Class` | `class AuthService`, `struct User` | name, methods (as child nodes), fields, file:line |
| `Interface/Trait` | `interface Authenticator` | name, method signatures, file:line |
| `Type/Enum` | `type Role = 'admin' \| 'user'` | name, definition, file:line |
| `Module` | file-level node | exports, imports, file path |
| `Variable/Const` | `const MAX_RETRIES = 3` | name, type, value (if const), file:line |

### Edge Types

| Edge Type | Meaning | Example |
|---|---|---|
| `calls` | A invokes B | `loginUser() → validatePassword()` |
| `imports` | A imports B | `auth.ts → jwt.ts` |
| `implements` | A implements B | `JWTAuth implements Authenticator` |
| `extends` | A extends B | `AdminUser extends User` |
| `type_ref` | A references type B | `fn login(creds: Credentials)` |
| `contains` | A is parent of B | `AuthService.validateToken()` |
| `co_changes` | A and B frequently change together | learned from git/edit patterns |

### Node Metadata

Each node also stores:

- Embedding vector (384 dims from MiniLM) for semantic search
- Edit frequency (hotspot score)
- Last modified timestamp
- Dependents count (how many other nodes reference this one)
- Cross-repo flag (for multi-repo workspaces)

### Skeleton Generation

When a node is classified as "context" (not a pivot), Lattice strips the body and returns only:

```
fn validatePassword(plain: &str, hashed: &str) -> Result<bool, AuthError>
  // 12 lines, 3 callers, last modified 2d ago
```

This is what delivers the 65-70% token reduction — full code only for pivots, signatures for everything else.

---

## 3. Context Capsule Query Engine

The core algorithm. When an AI agent calls the `query_context` MCP tool:

**Input:** Natural language query + optional filters (file paths, languages, scope)

### Pipeline

```
Query: "How does authentication work?"
           │
           ▼
┌─────────────────────────┐
│ 1. INTENT DETECTION     │
│                         │
│ Classify query intent:  │
│ • explore  → broad      │
│ • fix_bug  → error paths│
│ • refactor → blast radius│
│ • add_feature → module  │
│   boundaries            │
└───────────┬─────────────┘
            ▼
┌─────────────────────────┐
│ 2. SEMANTIC SEARCH      │
│                         │
│ Embed query → vector    │
│ Search sqlite-vec for   │
│ top-K similar nodes     │
│ (K=10 for explore,      │
│  K=5 for fix_bug)       │
└───────────┬─────────────┘
            ▼
┌─────────────────────────┐
│ 3. GRAPH TRAVERSAL      │
│                         │
│ From each semantic hit, │
│ traverse outward N hops │
│ (N=2 for explore,       │
│  N=3 for fix_bug)       │
│                         │
│ Collect all reached     │
│ nodes into candidate set│
└───────────┬─────────────┘
            ▼
┌─────────────────────────┐
│ 4. RANKING              │
│                         │
│ Score each candidate:   │
│                         │
│ score =                 │
│   semantic_sim  * 0.4   │
│ + graph_centrality * 0.3│
│ + edit_recency  * 0.2   │
│ + caller_count  * 0.1   │
│                         │
│ Adjust weights by intent│
│ fix_bug: recency → 0.4  │
│ refactor: centrality→0.4│
└───────────┬─────────────┘
            ▼
┌─────────────────────────┐
│ 5. BUDGET ALLOCATION    │
│                         │
│ Token budget: 3000      │
│ (adaptive: expands on   │
│  repeated queries)      │
│                         │
│ Classify nodes:         │
│ • Pivot (>0.7): full src│
│ • Context (0.3-0.7):    │
│   skeleton only         │
│ • Excluded (<0.3): drop │
│                         │
│ Pack into budget:       │
│ pivots first, then      │
│ skeletons until budget  │
│ exhausted               │
└───────────┬─────────────┘
            ▼
┌─────────────────────────┐
│ 6. CAPSULE ASSEMBLY     │
│                         │
│ Combine pivots,         │
│ skeletons, memories,    │
│ and metadata into       │
│ final capsule           │
└─────────────────────────┘
```

### Key Behaviors

- **Adaptive budget** — repeated queries to the same area automatically expand the token budget
- **Intent-aware weighting** — "fix bug" boosts error-adjacent code and recent changes; "refactor" boosts high-centrality nodes; "add feature" boosts module boundaries
- **Explainability** — every included node has a `why_included` field

---

## 4. Session Memory

### Memory Types

| Memory Type | Example | Stored Fields |
|---|---|---|
| `observation` | "User prefers JWT over sessions" | content, confidence, source_query, linked_nodes[], timestamp |
| `decision` | "Chose PostgreSQL over MySQL for X reason" | content, rationale, alternatives[], linked_nodes[], timestamp |
| `exploration` | "Agent explored auth flow, found 3 entry points" | content, nodes_visited[], findings[], timestamp |
| `pattern` | "login + session always change together" | content, node_pairs[], frequency, first_seen, last_seen |
| `anti_pattern` | "Agent added retry logic then removed it" | content, nodes_affected[], detection_reason |

### Memory Lifecycle

1. **Capture** — Automatic extraction from agent sessions + explicit `store_memory` MCP tool
2. **Link** — Each memory is linked to graph nodes it relates to
3. **Embed** — Memory content is embedded for semantic retrieval
4. **Retrieve** — Relevant memories included in Context Capsules alongside code
5. **Decay** — When linked code changes, memories flagged as `potentially_stale` with the triggering diff
6. **Prune** — Low-confidence memories not retrieved in N sessions are archived

### Stale Knowledge Detection

When a node is modified, any linked memory is flagged:

```
WARNING STALE: "validatePassword uses bcrypt with cost=12"
  Reason: validatePassword() modified 2h ago
  Diff: +argon2 replaced bcrypt
```

---

## 5. Passive Intelligence (Change Tracking)

The file watcher re-parses changed files and diffs the AST to detect structural changes (not just "file changed").

### Derived Insights

1. **Co-change patterns** — Functions modified together 3+ times → `co_changes` edge added with weight
2. **Hotspot detection** — Frequently modified functions get boosted ranking scores
3. **Anti-pattern detection:**
   - Code added then removed in same session → "dead-end exploration" flag
   - Same function modified 5+ times in one session → "thrashing" flag
   - Surfaced as observations in memory
4. **Stale memory triggers** — Any structural change checks linked memories
5. **Project rules** — Recurring patterns (e.g., "every new API endpoint needs a test file") detected and stored as project-level rules

All zero-configuration. Watcher runs continuously, diffs computed incrementally, insights accumulate.

---

## 6. MCP Tool Surface

| MCP Tool | Purpose | Returns |
|---|---|---|
| `query_context` | Natural language query → Context Capsule | Pivots (full code) + context (skeletons) + memories |
| `get_symbol` | Look up a specific symbol by name | Full source, dependents, callers, file location |
| `get_dependents` | "What depends on this function?" | Callers/importers with file locations |
| `get_dependencies` | "What does this function depend on?" | Callees/imports |
| `blast_radius` | "If I change this, what breaks?" | Transitively dependent nodes, ranked by impact |
| `store_memory` | Save an observation or decision | Confirmation + memory ID |
| `recall_memories` | Retrieve relevant memories | Ranked memories with staleness flags |
| `search_symbols` | Find symbols by name or semantic query | Matched symbols with scores |
| `get_file_context` | Get a file's role in the graph | Exports, importers, hotspot score, co-change partners |
| `get_project_rules` | Get auto-detected conventions | Rules with confidence scores |

### Context Capsule Format

```json
{
  "query": "How does authentication work?",
  "intent": "explore",
  "pivots": [
    {
      "symbol": "loginUser",
      "kind": "function",
      "file": "src/auth/login.ts",
      "line": 42,
      "source": "async function loginUser(creds: Credentials): Promise<Session> {\n  ...\n}",
      "why": "semantic_match: 0.91, centrality: high"
    }
  ],
  "context": [
    {
      "symbol": "hashPassword",
      "kind": "function",
      "file": "src/auth/crypto.ts",
      "line": 15,
      "skeleton": "fn hashPassword(plain: string): Promise<string>  // 8 lines, 2 callers",
      "relationship": "called_by: loginUser"
    }
  ],
  "memories": [
    {
      "content": "Auth system migrated from bcrypt to argon2 last session",
      "type": "observation",
      "stale": false
    }
  ],
  "stats": {
    "tokens_used": 2400,
    "tokens_saved": 15600,
    "nodes_evaluated": 47,
    "nodes_included": 11
  }
}
```

---

## 7. VS Code UI

### Sidebar Panel

- Daemon status indicator (running/starting/stopped)
- Index statistics (files indexed, nodes in graph, last index time)
- Per-repo cards in multi-repo workspaces (name, file count, last indexed)
- Action buttons: re-index, clear memory, open settings

### CodeLens

Inline annotations above exported symbols:

```
[Lattice: 12 dependents across 5 files]
export function validateToken(token: string): TokenPayload {
```

Only shown on exported/public symbols. Clicking opens a peek view of dependents.

### Hover Info

On exported declarations:

```
┌─────────────────────────────────┐
│ Lattice Impact                  │
│ Dependents: 12 (3 cross-repo)  │
│ Top callers: AuthMiddleware,    │
│   loginUser, refreshSession     │
│ Hotspot: 3/5                    │
│ Last modified: 2h ago           │
└─────────────────────────────────┘
```

### Status Bar

Left-aligned: `Lattice: ✓ 4,231 nodes` | `Lattice: ⟳ indexing 34%` | `Lattice: ✗ daemon stopped`

---

## 8. Multi-Repo Workspaces

- Multiple workspace folders → each indexed as a separate subgraph
- Cross-repo edges detected via: import path resolution, shared type names, package dependency declarations
- Queries span all repos by default; scopeable with `repo:name` filter
- Each repo gets its own SQLite database; workspace-level metadata DB tracks cross-repo edges

---

## 9. Indexing Strategy

### Phase 1: Initial Index (cold start)

- Scan workspace for all files (respecting `.lattice_ignore`)
- Priority queue: open files first, then recently modified, then the rest
- Parse files in parallel (Rust tokio tasks, N = CPU cores)
- Build graph incrementally as files are parsed
- Generate embeddings in batches (ONNX batch inference)
- Persist to SQLite on completion

### Phase 2: Incremental Updates (steady state)

- File watcher detects save/create/delete
- Re-parse only the changed file
- Diff old AST vs new AST
- Update affected graph nodes and edges
- Re-embed only changed/new nodes
- Persist delta to SQLite

### Phase 3: Lazy Expansion (for very large repos)

- On cold start, index only: open files, same-directory files, direct imports (1 hop)
- As user navigates, expand index to touched files
- Background thread indexes remaining files
- Full index eventually achieved without blocking startup

### Performance Targets

| Operation | Target |
|---|---|
| Cold start (1k files) | < 5 seconds |
| Cold start (50k files, lazy) | Usable in < 5s, full index in background |
| Incremental update (single file) | < 200ms |
| Query response | < 500ms |

---

## 10. Security

- **100% local processing** — zero network calls, zero cloud dependencies
- **Binary verification** — SHA-256 checksums verified on extension activation
- **`.lattice_ignore`** — custom exclusion file (`.gitignore` syntax)
- **Default exclusions** — `*.env`, `*credentials*`, `*.pem`, `*.key`, `id_rsa*`, `*.pfx`
- **Content filtering** — strings matching `password=`, `secret=`, API key patterns redacted from indexed content

---

## 11. Technology Stack

### Rust Daemon

| Crate | Purpose |
|---|---|
| `tree-sitter` + language grammars | Multi-language parsing |
| `petgraph` | In-memory dependency graph |
| `rusqlite` + `sqlite-vec` | Persistent storage + vector search |
| `ort` (ONNX Runtime) | Local embedding inference |
| `tokio` | Async runtime, concurrent queries |
| `notify` | Cross-platform file watching |
| `serde` / `serde_json` | JSON-RPC serialization |
| `tower` / custom | MCP server implementation |

### VS Code Extension (TypeScript)

| Package | Purpose |
|---|---|
| `vscode` API | Extension host, UI providers |
| `vscode-languageclient` (optional) | If LSP features needed |
| JSON-RPC over stdio | Communication with daemon |

### Build & Distribution

- Rust cross-compilation via GitHub Actions (Windows x64, macOS x64/arm64, Linux x64)
- Platform-specific `.vsix` packages with pre-built binaries
- SHA-256 checksums published alongside releases
