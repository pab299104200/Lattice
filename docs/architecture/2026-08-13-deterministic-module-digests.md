# Deterministic Module Digests

## Decision

Lattice builds one deterministic digest for each source directory represented in the
dependency graph. A directory is a useful module boundary because it is stable across
index runs, maps directly to citations, and does not depend on an opaque clustering
heuristic. Root-level files use `.` as their module key. Test-only directories are
retained because a query may explicitly ask how a subsystem is verified.

Each digest is structured from graph facts only:

- the files represented by the module;
- the module's symbol-kind mix;
- exported or otherwise central symbol anchors;
- typed relationships whose endpoints are both in the module; and
- exact `file:line` citations for every prose claim.

Generation is deterministic: inputs are sorted before selection, limits are fixed, and
ties use paths, line numbers, and symbol names. No model provider is consulted. An LLM
may eventually polish a copy of the deterministic prose off the query path, but that is
not part of the authoritative digest contract.

## Lifecycle and storage

`GraphStore::save_graph` is the index-epoch boundary. In the same transaction that
replaces graph nodes and edges, it increments `graph_metadata.index_epoch`, replaces
the `module_digests` rows, and records the new epoch on every digest. The table stores
the module key and a versioned JSON payload. A transaction failure preserves the prior
graph, epoch, and digest set together.

The saved graph receives the generated cache after the transaction commits. Warm graph
loads hydrate the cache from `module_digests`. Graph mutation invalidates the in-memory
cache, so a stale digest cannot be presented for a changed graph. This keeps digest
generation on the indexing/persistence path rather than the query hot path.

## Query behavior

Subsystem summaries select a cached digest by deterministic token overlap with the
query and already-ranked files. The digest prose is used only when it overlaps the
requested subsystem; otherwise the response builds structured prose from the ranked
files and symbols already computed for that query. Both forms use full sentences and
precise citations. The existing `key_files`, `key_symbols`, tests, rules, and memory
sections remain available for expansion and machine-readable clients.

## Failure and recovery

Malformed digest rows fail warm loading with an actionable storage error rather than
silently presenting untrusted prose. Derived graph corruption recovery removes the
graph database and therefore its derived digests; the next successful index rebuilds
both. Empty graphs persist an incremented epoch and an empty digest set.
