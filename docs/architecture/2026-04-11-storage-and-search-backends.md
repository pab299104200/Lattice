# Storage And Search Backends

## Summary

Lattice keeps SQLite as the durable source of truth for the graph and memories, but semantic retrieval no longer does brute-force cosine scans on every hot-path query. The daemon now uses a `VectorIndex` abstraction with scoped retrieval and a richer semantic indexing pipeline:

- `VectorStore` as the compatibility backend: SQLite BLOB storage plus exact cosine search
- `UsearchVectorIndex` as the preferred backend: persisted USearch ANN graph under `.lattice/vectors.usearch`
- `VectorScope` to distinguish symbol vectors from file-summary vectors while preserving a compatibility default search path

Memory keyword search now uses SQLite FTS5 instead of ad hoc `LIKE` scans.

## Vector Indexing

- `QueryEngine` depends on `VectorIndex`, not on the SQLite store directly.
- SQLite remains the durable source of truth for vectors in `.lattice/vectors.db`.
- `vector_keys` stores stable numeric ANN keys for `(file, name, byte_offset)` triples.
- `vector_index_meta` stores the configured embedding dimension and a generation counter.
- `vectors.usearch.meta.json` records the persisted ANN generation so startup can detect drift and rebuild from SQLite when needed.
- Default `search(...)` stays symbol-scoped for compatibility, while `search_in_scope(...)` can target `Symbol`, `FileSummary`, or `All`.
- The first multi-granularity slice adds deterministic per-file summary vectors alongside symbol vectors.

## Sync Model

- Cold start: the daemon opens the vector backend, warms from disk if generations match, otherwise rebuilds the ANN file from SQLite.
- Full graph embedding sync: initial indexing and explicit reindex rebuild the semantic index from the current graph snapshot.
- Incremental sync: watcher updates delete vectors for changed files, re-embed the current nodes for those files, then flush the ANN file.
- Embedding payloads now include richer symbol-body anchors such as compact summaries, comments/docstrings, error strings, config keys, and routes before they are written to the vector store.
- Failure mode: if USearch initialization or warm-up fails, Lattice falls back to the exact SQLite backend without changing MCP schemas.

## Retrieval Pipeline

- The query engine now searches both symbol vectors and file-summary vectors before final ranking.
- Semantic candidates are re-ranked with deterministic graph, identifier, and query-intent signals before final delivery.
- Identifier-heavy queries still prefer keyword anchors when they are available, so the richer semantic path does not override exact code-first matches.

## Rollout Observability

- Vector sync now logs structured batch stats for mode, backend, graph size, payload size, elapsed time, and throughput.
- Watcher-triggered syncs log both vector-sync elapsed time and end-to-end watcher overhead.
- USearch flushes log index and metadata byte deltas so operators can track on-disk growth during rollout.

## Memory Search

- `memories.db` remains the source of truth for rows and metadata.
- `memories_fts` stores normalized search text for content, linked symbols, and linked files.
- Existing databases are migrated on open:
  - missing schema columns are added
  - FTS5 is created if absent
  - FTS content is rebuilt from the current non-invalidated memory rows
- Store, refresh, promote, invalidate, prune, and clear operations keep FTS rows in sync.

## Contract Notes

- MCP/tool response shapes do not change.
- Runtime storage gains `.lattice/vectors.usearch` and `.lattice/vectors.usearch.meta.json`.
- Keyword search semantics stay AND-based, ordered by recency, but are now implemented through FTS5 rather than wildcard table scans.
- Symbol-only search remains the compatibility default; file-summary vectors are available through the scope-aware retrieval path.
