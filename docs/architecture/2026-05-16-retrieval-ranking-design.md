# Retrieval Ranking Design

This is the authoritative retrieval and ranking reference. It implements [## Retrieval Engine](../plans/2026-05-16-cognitive-workspace-fork-plan.md#retrieval-engine), [## Phase 4: Retrieval V1](../plans/2026-05-16-cognitive-workspace-fork-plan.md#phase-4-retrieval-v1), and the response contract in [## MCP Tool Contract Principles](../plans/2026-05-16-cognitive-workspace-fork-plan.md#mcp-tool-contract-principles).

## Pipeline

The retrieval pipeline is the ten-step sequence from [## Retrieval Engine](../plans/2026-05-16-cognitive-workspace-fork-plan.md#retrieval-engine):

1. Parse user task and classify intent.
2. Extract literal anchors: paths, symbols, errors, commands, APIs, config keys.
3. Resolve anchors into graph identities.
4. Retrieve graph candidates.
5. Retrieve memory candidates from typed streams.
6. Retrieve relevant event episodes.
7. Expand through bounded graph and memory neighborhoods.
8. Score candidates.
9. Deduplicate and compress.
10. Return a compact bundle with inclusion reasons and expansion handles.

The pipeline is shared by high-level workflow routes behind `context`, `prepare_change`, `impact`, and `diagnose`; see [MCP Tool Reference](./2026-06-11-mcp-tool-reference.md#public-mcp-tools).

## Candidate sources

Candidate sources are:

- exact path and symbol lookup
- code graph traversal
- doc backlinks and outgoing links
- full-text search
- embeddings
- event similarity
- memory links
- workflow similarity
- recent active working memory

Source retrieval must be bounded before ranking. Broad textual search remains appropriate for exact strings, but workflow tools should return summary-first bundles rather than forcing clients to dump source files.

Known-extension root files are first-class path anchors. Change-preparation routes resolve existing file tokens against the canonical workspace root, reject references outside that root, cap the inferred entry set, and promote those files ahead of generic lexical matches. Structural `search` normalizes punctuation and can require all query terms across a node name and file path, allowing document headings and root instruction files to be found without weakening exact-name ordering.

### Deterministic module digests

The indexer builds one deterministic module digest per indexed source file from graph
facts. Inputs are sorted, limits are fixed, and ties are
resolved by stable paths, lines, and symbol names. Digests are generated and persisted
with the committed graph/index epoch; query handling only selects from the immutable
cache and never generates or refreshes digest prose on the query hot path. Digest
prose carries exact `file:line` citations for its claims.

Warm graph loads hydrate the digest cache only when its epoch, generator/version
metadata, fingerprints, hashes, canonical payloads, and graph facts all validate. A
pre-digest or epoch-zero store may still serve graph-backed retrieval while indexing
rebuilds the cache. A cache with missing rows, malformed data, stale metadata, or facts
inconsistent with its graph is rejected with an actionable storage error rather than
partially hydrated or silently served. If no cached digest overlaps the requested subsystem,
the response falls back to structured prose from the already-ranked files and symbols,
with the same citation requirement; it does not synthesize a digest at query time.

## Ranking signals

The required ranking signals from [## Retrieval Engine](../plans/2026-05-16-cognitive-workspace-fork-plan.md#retrieval-engine) are:

| Signal | Meaning |
|---|---|
| task-type compatibility | Candidate matches fix, add, refactor, review, docs, failure, or planning intent. |
| graph proximity to anchors | Candidate is near resolved file, symbol, doc, test, event, or memory anchors. |
| exact identifier match | Candidate name, path, config key, command, API, or error matches literally. |
| semantic similarity | Candidate text or embedding is similar to the task. |
| verification status | Verified memory outranks unverified memory; invalid states are not trusted guidance. |
| freshness | Fresh evidence outranks stale or expired evidence. |
| scope | Session, branch, repo, user, and organization scope must be compatible with the task. |
| evidence strength | Direct source, test, or event evidence outranks weak inferred evidence. |
| contradiction/supersession state | Contradicted and superseded candidates are demoted or shown only with labels. |
| past usefulness | Memory or context that was later used successfully gets higher weight. |
| recent successful reuse | Recent workflow reuse improves confidence when scope still matches. |
| user preference compatibility | Candidate must not conflict with durable user preferences. |
| token cost | Lower-cost candidates can outrank equivalent high-cost candidates. |

Large-repository ranking computes graph-wide normalization values such as maximum modification time and caller count once per query, outside the candidate loop. Recomputing those values for every candidate is prohibited because it changes ranking from bounded graph work into repeated whole-graph traversal and causes hook/CLI latency to scale with candidate count.

## Diagnostic mode

Diagnostic mode exposes scores, signal contributions, excluded high-scoring candidates, uncertainty, payload hashes where relevant, and expansion targets. It exists for debugging ranking misses and review workflows, not for ordinary prompt injection.

Diagnostic responses must remain workspace-scoped and budgeted. They may include enough detail to explain why a memory, event, doc, or file was included or excluded.

## Compact mode

Compact mode includes only the context an assistant needs to act: overview, ranked pivots, relevant context, memory highlights, event episodes where relevant, risks, suggested expansion, and stable handles. It avoids full source bodies unless the workflow specifically returns a small bounded span.

Compact mode is the default for repeated assistant usage because token budget is an operational constraint.

## Inclusion reasons and expansion handles

Every returned candidate should explain why it is present: anchor match, graph proximity, verification state, freshness, evidence, workflow similarity, or prior usefulness. Candidates that need more detail should expose stable expansion handles for `expand_context`, memory detail views, or event trace pages.

Handles must expire safely and fail with actionable errors when missing, expired, ambiguous, or outside workspace scope.

## Forward compatibility with working memory

Phase 4 retrieval was designed not to depend on formal working-memory state, but its output schema must be forward-compatible with [## Working Memory](../plans/2026-05-16-cognitive-workspace-fork-plan.md#working-memory). Retrieval bundles should identify selected memories, excluded memories and reasons, budget decisions, unresolved questions, and checkpoint-friendly stable identities so Phase 5 can persist and inspect active context without rewriting retrieval.
