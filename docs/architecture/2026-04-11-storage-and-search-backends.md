# Storage And Search Backends

## Summary

Lattice keeps SQLite as the durable source of truth for the graph and memories, but semantic retrieval no longer does brute-force cosine scans on every hot-path query. The daemon now uses a `VectorIndex` abstraction with:

- `VectorStore` as the compatibility backend: SQLite BLOB storage plus exact cosine search
- `UsearchVectorIndex` as the preferred backend: persisted USearch ANN graph under `.lattice/vectors.usearch`

Memory keyword search now uses SQLite FTS5 instead of ad hoc `LIKE` scans.

## Vector Indexing

- `QueryEngine` depends on `VectorIndex`, not on the SQLite store directly.
- SQLite remains the durable source of truth for vectors in `.lattice/vectors.db`.
- `vector_keys` stores stable numeric ANN keys for `(file, name, byte_offset)` triples.
- `vector_index_meta` stores the configured embedding dimension and a generation counter.
- `vectors.usearch.meta.json` records the persisted ANN generation so startup can detect drift and rebuild from SQLite when needed.

## Sync Model

- Cold start: the daemon opens the vector backend, warms from disk if generations match, otherwise rebuilds the ANN file from SQLite.
- Full graph embedding sync: initial indexing and explicit reindex rebuild the semantic index from the current graph snapshot.
- Incremental sync: watcher updates delete vectors for changed files, re-embed the current nodes for those files, then flush the ANN file.
- Failure mode: if USearch initialization or warm-up fails, Lattice falls back to the exact SQLite backend without changing MCP schemas.

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
