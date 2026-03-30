# Lattice Unified Engineering Knowledge Graph Design

**Date:** 2026-03-30
**Status:** Proposed
**Audience:** Lattice maintainers building assistant-first code, docs, and memory retrieval
**Related Docs:** `docs/plans/2026-02-25-lattice-design.md`, `docs/plans/2026-03-13-agent-workflow-roadmap.md`

## Goal

Expand Lattice from a strong local code context engine into the best local engineering knowledge graph for assistants.

The system should unify:

- code structure and execution relationships
- Markdown docs, ADRs, runbooks, scorecards, and design notes
- durable assistant memory and workflow outcomes
- freshness and provenance between code and documentation

This is not an "Obsidian inside VS Code" project. The goal is to beat note-first tools at engineering retrieval by making code, docs, and memory part of one local graph that assistants can query efficiently.

## Product Thesis

The current Lattice stack is strong when the answer lives in source code. It is materially weaker when the answer is split across:

- design docs in `docs/`
- repo instructions in `README.md`, `AGENTS.md`, `CLAUDE.md`
- runbooks and scorecards
- prior decisions and assistant memory

That gap forces assistants back into broad grep, file dumping, and repeated rediscovery.

Best in class means Lattice should:

1. Match the baseline note-graph capabilities that users expect from local Markdown tools:
   backlinks, outgoing links, local graph, unresolved links, orphan detection, aliases, section-level addressing.
2. Surpass those tools on engineering work:
   code-to-doc links, stale doc detection, decision retrieval, runbook retrieval, and compact assistant-first bundles.
3. Preserve Lattice's existing strengths:
   local-only processing, compact responses, workflow-oriented tools, and low token overhead.

## Non-Goals

This design explicitly does not try to make Lattice:

- a general personal knowledge management app
- a cloud sync platform
- a collaborative editor
- a canvas-first visualization product
- a replacement for direct file editing, grep, or source control tools

Human-facing graph UX matters, but only insofar as it improves trust, curation, and assistant usefulness.

## Competitive Standard

To be credible as best in class, Lattice should be able to do all of the following well:

- answer "how does this subsystem work?" with both code pivots and the right docs
- answer "what design decision explains this implementation?" with traceable evidence
- answer "what docs are now stale because this code changed?" with ranked, explainable results
- show backlinks and outgoing links for the current file, symbol, or document section
- identify unresolved links, orphan docs, and dead-end docs
- let assistants write decisions and runbooks in structured, human-readable form
- keep durable memory tied to files, symbols, branches, and freshness signals

## Design Principles

1. Assistant-first, not dashboard-first.
2. One graph, not parallel code and docs systems.
3. Freshness and provenance are product features, not maintenance details.
4. Compact default responses, with deliberate expansion paths.
5. Human-readable artifacts matter, but transactional storage still needs to be reliable.
6. Trust requires inspectable evidence for every strong link or stale warning.

## Unified Graph Model

Lattice should evolve from a code dependency graph into a unified engineering graph with four logical layers:

1. Code graph
   Functions, methods, types, files, tests, dependencies, and impact edges.
2. Document graph
   Markdown documents, sections, aliases, tags, links, backlinks, unresolved references.
3. Knowledge graph
   Decisions, runbooks, scorecards, tasks, observations, patterns, outcomes.
4. Freshness graph
   Evidence, provenance, staleness, and supersession relationships between code, docs, and memories.

These layers should share one storage and retrieval framework so ranking can move naturally between them.

## Node Types

### Existing Nodes To Keep

- `Function`
- `Method`
- `Class`
- `Struct`
- `Trait`
- `Interface`
- `TypeAlias`
- `Enum`
- `Variable`
- `Constant`
- `Module`

### New First-Class Nodes

| Node Type | Description | Pivot Eligibility |
|---|---|---|
| `Document` | Whole Markdown file with metadata and summary | context anchor |
| `Section` | Heading-scoped chunk of a document | primary doc pivot |
| `Alias` | Alternate names for a document or section | lookup only |
| `Tag` | Markdown tag or controlled taxonomy tag | lookup and filter |
| `Decision` | ADR-style rationale item | pivot |
| `Runbook` | Operational procedure | pivot |
| `Scorecard` | Feature/system assessment | pivot |
| `Task` | Open action item or checklist item | context and workflow |
| `Memory` | Durable assistant observation/pattern/decision | pivot or context |
| `Outcome` | Recorded successful workflow or fix outcome | pivot or context |

### Section Granularity Rules

- `Document` nodes carry file-level metadata, summaries, and coarse graph position.
- `Section` nodes are the main retrieval unit for Markdown content.
- Only sections should usually be returned as high-confidence doc pivots.
- Very small files may collapse to a single synthetic section when that improves retrieval quality.

## Edge Types

### Existing Edges To Keep

- `calls`
- `imports`
- `implements`
- `extends`
- `type_ref`
- `contains`
- `co_changes`

### New Doc And Knowledge Edges

| Edge Type | Meaning |
|---|---|
| `links_to` | Markdown link or wiki-link target |
| `mentions` | Unstructured textual mention with confidence |
| `aliases` | Alias points to document or section |
| `tagged_with` | Document or section has a tag |
| `documents` | Document or section explains a file, symbol, subsystem, or workflow |
| `decides` | Decision applies to a subsystem, file, or symbol |
| `runbook_for` | Runbook applies to a subsystem, service, or failure mode |
| `validated_by` | Code path, test, or outcome validates a doc claim |
| `stale_against` | Node is likely stale relative to changed evidence |
| `supersedes` | Newer decision or runbook replaces older guidance |
| `derived_from` | Memory or outcome was distilled from prior tool calls or docs |

## Markdown Parsing And Indexing

### Supported Content

Initial first-class targets:

- `README.md`
- `AGENTS.md`
- `CLAUDE.md`
- `docs/**/*.md`
- ADRs, runbooks, scorecards, architecture docs, operational notes

Optional later targets:

- issue exports
- changelog-style docs
- mirrored decision logs

### Parsing Responsibilities

Add a dedicated Markdown parser that extracts:

- frontmatter fields
- title and heading tree
- section boundaries
- wiki-links and standard Markdown links
- heading anchor targets
- aliases from frontmatter
- tags
- checklists and open tasks
- fenced code blocks and inline code spans
- explicit file paths and symbol-like references

### Link Resolution Rules

Resolution priority should be deterministic and explainable:

1. explicit relative path
2. explicit heading anchor
3. exact file path match
4. exact wiki-link target by basename
5. alias match from frontmatter
6. section title match within candidate documents

Unresolved links should be stored as unresolved edges, not silently dropped.

### Code Reference Extraction

Markdown should not be treated as only note-to-note links. The parser should also extract likely code references from:

- inline code spans like ``AuthService`` or `prepare_change`
- explicit file paths
- symbol-like identifiers
- fenced code blocks that reference current repo symbols or paths
- frontmatter fields such as `lattice.files`, `lattice.symbols`, `lattice.subsystem`

The system should distinguish:

- explicit evidence
- inferred evidence

Only explicit evidence should create high-confidence edges without qualification.

## Freshness And Provenance

Freshness is the capability that will most clearly separate Lattice from generic note graph tools.

Every doc, memory, and outcome node should be able to carry:

- linked files
- linked symbols
- linked tests
- refresh key
- evidence source
- evidence confidence
- last verified timestamp
- stale state and stale reason

### Staleness Triggers

A doc or memory should be marked stale or suspect when:

- a linked file changes
- a linked symbol changes
- a linked test disappears or begins failing repeatedly
- a decision is superseded
- a linked path becomes unresolved

### Staleness Levels

- `fresh`
- `suspect`
- `stale`
- `superseded`

`suspect` is important. The product should avoid binary stale labeling when the evidence is weak.

## Storage Strategy

SQLite should remain the canonical structured store for transactional metadata, memory records, outcomes, and persisted graph state.

Markdown should become a first-class indexed source, not the only source of truth.

Recommended storage model:

- canonical graph state in SQLite plus in-memory graph
- canonical assistant memory and outcome records in SQLite
- optional Markdown mirrors for durable repo-scope decisions, runbooks, and selected memories

This keeps retrieval fast and updates safe while still giving humans inspectable artifacts.

## Retrieval Model

The query engine should evolve from "find the right code symbols" to "find the right answer-bearing evidence set."

### Retrieval Inputs

Queries may be anchored by:

- natural language
- file paths
- symbols
- document paths
- prior context handles
- failure text
- diffs

### Candidate Pools

For each query, build candidates from:

- code nodes
- document and section nodes
- memory and outcome nodes
- freshness warnings relevant to the anchor

### Ranking Signals

Use weighted ranking across:

- lexical relevance
- semantic similarity
- graph proximity
- explicit evidence strength
- freshness state
- node quality
- node type prior
- workflow intent

#### Node Quality Signals

For Markdown sections:

- heading specificity
- link density
- outbound evidence count
- freshness
- section length suitability
- proximity to title and top-level structure

For decisions and runbooks:

- explicit linked subsystem or symbol coverage
- recency
- supersession state
- validation by outcomes or tests

### Intent-Aware Retrieval

Add doc-aware intent handling:

- `ExploreArchitecture`
- `ExplainDecision`
- `OperationalHowTo`
- `StaleKnowledgeCheck`
- `ChangePlanning`
- `FailureDiagnosis`

Example behavior:

- architecture questions boost design docs and stable decisions
- operational questions boost runbooks and recent outcomes
- change planning boosts code pivots plus nearby docs and stale warnings
- failure diagnosis boosts runbooks, recent outcomes, and impacted symbols

## Tool Surface

### Existing Tools To Extend

Extend these tools to optionally include doc and knowledge evidence:

- `get_context_capsule`
- `prepare_change`
- `diagnose_failure`
- `get_working_set_context`
- `summarize_subsystem`
- `get_repo_playbook`
- `expand_context`

### New MCP Tools

| Tool | Purpose |
|---|---|
| `get_docs_capsule` | Return ranked document and section pivots for a query |
| `get_backlinks` | Return inbound links and mentions for a doc, section, file, or symbol |
| `get_outgoing_links` | Return outgoing links and code references for a doc or section |
| `find_unresolved_links` | Return unresolved wiki-links, Markdown links, and stale targets |
| `find_orphan_docs` | Return docs with no meaningful inbound links or graph role |
| `find_stale_docs` | Return docs likely stale against recent code or decision changes |
| `explain_symbol_with_docs` | Join a code symbol with linked docs, decisions, runbooks, and memory |
| `write_decision` | Create or update a structured decision artifact and graph links |
| `write_runbook` | Create or update a structured runbook artifact and graph links |
| `resolve_doc_links` | Suggest target matches for unresolved links and aliases |

### Output Shape

Responses should stay compact and assistant-oriented:

- top pivots
- why each item was included
- freshness and confidence markers
- explicit evidence vs inferred evidence
- suggested expansion targets

The product should never default to dumping whole documents.

## VS Code Experience

Human-facing UI should support trust and curation without turning into a large separate product.

### High-Value Surfaces

- local graph for current file, symbol, or document section
- backlinks and outgoing links panel
- stale docs queue
- unresolved links queue
- "explain with docs" command for active symbol or file
- structured composers for decision and runbook artifacts

### Lower Priority

- broad global graph visualizations
- decorative dashboards
- large standalone note management surfaces

## Implementation Touchpoints

Likely core changes:

- `daemon/crates/lattice-core/src/symbols.rs`
  add `Language::Markdown` and new node kinds or generalized node taxonomy
- `daemon/crates/lattice-core/src/parser/mod.rs`
  route Markdown parsing
- `daemon/crates/lattice-core/src/parser/markdown.rs`
  new parser
- `daemon/crates/lattice-core/src/watcher/mod.rs`
  allow Markdown indexing by policy, not as generic code
- `daemon/crates/lattice-core/src/graph/model.rs`
  expand node and edge types plus freshness metadata
- `daemon/crates/lattice-core/src/graph/builder.rs`
  build doc and knowledge edges
- `daemon/crates/lattice-core/src/query/engine.rs`
  rank doc pivots, decisions, and freshness evidence
- `daemon/crates/lattice-core/src/intelligence/agent.rs`
  compose mixed code and docs bundles
- `daemon/crates/lattice-core/src/memory/*`
  add provenance and optional Markdown mirroring
- `daemon/crates/lattice-daemon/src/rpc/mcp.rs`
  expose new tools and extend existing schemas
- `extension/src/*`
  add backlinks, stale docs, local graph, and structured authoring commands

## Suggested Build Order

### Phase 0: Graph Foundations

- add Markdown language support
- add new node and edge taxonomy
- add parser and graph storage tests

### Phase 1: Document Graph

- index Markdown docs and sections
- support links, aliases, tags, unresolved links
- add docs capsule and backlinks/outgoing links tools

### Phase 2: Code-to-Docs Evidence

- link docs to files and symbols
- rank mixed code and docs responses
- add explain-symbol-with-docs workflow

### Phase 3: Freshness

- mark docs and knowledge artifacts fresh, suspect, stale, or superseded
- add stale docs tooling and UI queues

### Phase 4: Structured Authoring

- add write-decision and write-runbook tools
- optionally mirror durable artifacts to Markdown templates

### Phase 5: Trust Surfaces

- add local graph, backlinks, unresolved links, and stale docs panels
- add inspection affordances for evidence and confidence

## Benchmarks And Success Metrics

Success should be measured with dedicated doc-aware benchmarks, not only code retrieval metrics.

Recommended metrics:

- top-3 hit rate for architecture questions
- top-3 hit rate for operational/runbook questions
- stale-doc precision
- stale-doc recall
- average payload size for mixed code plus docs queries
- tool calls saved on architecture and change-planning tasks
- time-to-first-correct-answer on benchmark tasks
- unresolved link reduction over time
- percentage of durable memories with traceable evidence

## Risks

### Risk: Noisy Code-To-Doc Linking

Symbol-like strings in Markdown can generate false links.

Mitigation:

- separate explicit and inferred evidence
- expose confidence and evidence source
- require stronger thresholds for stale warnings than for search suggestions

### Risk: Graph Size Explosion

Section-level indexing and derived edges can grow quickly.

Mitigation:

- make sections the main retrieval unit, not every paragraph
- gate inferred edges
- keep compact summaries cached

### Risk: Doc Noise Dominates Retrieval

Large or low-signal docs can drown better evidence.

Mitigation:

- strong node type priors
- freshness weighting
- section quality scoring
- doc family weighting by intent

### Risk: Human Trust Breaks On False Stale Labels

If stale labeling is too noisy, users will ignore it.

Mitigation:

- use `suspect` as an intermediate state
- attach exact evidence for each stale mark
- let users refresh or dismiss with explicit reasons

## Decision

Lattice should pursue a unified engineering knowledge graph, not a simple Markdown feature and not an Obsidian clone.

The winning move is:

- first-class Markdown retrieval
- code-to-doc provenance
- freshness-aware decisions and runbooks
- assistant-native mixed evidence bundles

If executed well, this becomes a stronger category than local note graphs because it answers engineering questions, not just note navigation.
