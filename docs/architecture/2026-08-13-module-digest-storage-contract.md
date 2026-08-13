# Module Digest Storage Contract

## Status and reconciliation

This note is the implementation contract for the cached half of recovery workplan B1.
The first half is already present: `build_subsystem_overview` builds full sentences from
ranked graph files, symbols, relationships, and test targets, and formats source
locations as `` `file:line` ``
(`daemon/crates/lattice-core/src/intelligence/agent.rs:6385-6657`). The persistence
half is not present: `graph.db` currently contains nodes, edges, file-index entries,
and parsed files only (`daemon/crates/lattice-core/src/storage/schema.rs:1-46`), while a
`QueryEngine` snapshot carries only its graph and unrelated query services
(`daemon/crates/lattice-core/src/query/engine.rs:173-232`).

This contract adds the missing persisted cache without replacing the useful live-graph
fallback. It refines and supersedes the **directory granularity**, **lifecycle and
storage**, and **query behavior** sections of
`docs/architecture/2026-08-13-deterministic-module-digests.md`. In particular, a
source module is one indexed workspace-relative file, not a directory or a community
cluster. The earlier note's graph-fact-only generation, off-hot-path requirement, and
citation rules remain binding.

The authoritative cache is deterministic. Optional model-polished text is outside this
contract: it must not replace the deterministic payload, affect selection, or be needed
to answer a query. This keeps no-key operation, repeatable snapshots, and citation
provenance identical.

## Source-module boundary

Lattice generates exactly one digest for every distinct normalized `GraphNode.file` in
the committed graph. This is the **source module key** (`module_path`). It is a UTF-8,
workspace-relative, forward-slash path with `.` components removed. Absolute paths,
`..`, empty keys, and paths outside the workspace are rejected before persistence.

This definition deliberately includes code, tests, generated parser-visible source,
and indexed documentation. Their role is recorded as `code`, `test`, or
`documentation`; it does not alter identity. A root-level file remains its filename.
Files with no graph node have no digest because the graph has no citable fact from
which to build one. Directory aggregation would mix unrelated files and invalidate too
broadly, while graph community clustering would make keys sensitive to traversal and
tie-breaking changes.

Module enumeration and payload generation use only a committed `CodeGraph` and fixed
generator constants. No filesystem reads, clocks, random values, hash-map iteration
order, memory records, query text, model calls, or vector similarity may influence the
payload. Inputs are canonicalized as follows:

1. modules sort by normalized path;
2. nodes within a module sort by `(line, byte_offset, kind, name, signature)`;
3. incident edges sort by `(kind, from path, from line, from name, to path, to line,
   to name)`; and
4. every bounded selection takes the first entries after its documented ranking and
   stable path/line/name tie-breakers.

The versioned generator chooses a bounded entry anchor, representative symbol kinds,
exported anchors, document headings, and typed intra- or cross-module relationships.
The entry anchor prefers an exported declaration, then higher graph degree, then lower
line/offset/name. Representative and relationship limits are constants covered by the
generator version. Aggregate kind counts may be retained as structured data, but
rendered prose describes only facts for which the payload carries concrete evidence
locations.

## Canonical payload

`payload_json` is compact canonical JSON: object keys have a fixed serializer order,
arrays are already sorted, numbers are integers, and no optional field is serialized as
`null`. Its version-1 logical shape is:

```json
{
  "schema_version": 1,
  "module_path": "daemon/crates/lattice-core/src/indexer/mod.rs",
  "module_role": "code",
  "language": "Rust",
  "entry_anchor": {
    "name": "Indexer",
    "kind": "struct",
    "line": 19
  },
  "symbol_kinds": ["function", "method", "struct"],
  "exported_anchors": [
    {"name": "Indexer", "kind": "struct", "line": 19}
  ],
  "headings": [],
  "relationships": [
    {
      "kind": "calls",
      "from": {"path": "daemon/crates/lattice-core/src/indexer/mod.rs", "name": "index_file", "line": 280},
      "to": {"path": "daemon/crates/lattice-core/src/parser/mod.rs", "name": "parse_file", "line": 42}
    }
  ],
  "facts": [
    {
      "fact_id": "entry:Indexer:19",
      "sentence": "Start with `Indexer` at `daemon/crates/lattice-core/src/indexer/mod.rs:19`, the module's exported graph anchor.",
      "citations": [
        {"path": "daemon/crates/lattice-core/src/indexer/mod.rs", "line": 19}
      ]
    }
  ],
  "search_terms": ["index", "indexer", "rust"]
}
```

Every `facts[].sentence` is a complete sentence and every factual clause is supported
by at least one entry in that fact's non-empty `citations` array. A cross-module
relationship fact cites both endpoints. Citation paths must equal the module path or an
edge endpoint present in the same committed graph; citation lines are one-based and
must equal an indexed node line. `search_terms` are normalized, deduplicated tokens
derived only from the module path, language, selected symbol names/kinds, headings, and
relationship endpoints.

`input_fingerprint` is SHA-256 over the canonical, length-delimited graph facts that the
generator can use for this module, including all of its nodes and all incident edges.
It is intentionally not the raw file content hash: a body-only edit that leaves every
digest input unchanged does not need new prose. `payload_sha256` is SHA-256 over the
exact UTF-8 bytes stored in `payload_json`. The two hashes distinguish correct cache
reuse from payload corruption.

## `graph.db` schema

The graph schema adds these tables and index:

```sql
CREATE TABLE IF NOT EXISTS graph_metadata (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    index_epoch INTEGER NOT NULL CHECK (index_epoch >= 0),
    digest_schema_version INTEGER NOT NULL CHECK (digest_schema_version > 0)
);

INSERT OR IGNORE INTO graph_metadata
    (singleton, index_epoch, digest_schema_version)
VALUES
    (1, 0, 1);

CREATE TABLE IF NOT EXISTS module_digests (
    module_path TEXT PRIMARY KEY,
    index_epoch INTEGER NOT NULL CHECK (index_epoch > 0),
    generator_version INTEGER NOT NULL CHECK (generator_version > 0),
    input_fingerprint TEXT NOT NULL CHECK (length(input_fingerprint) = 64),
    payload_sha256 TEXT NOT NULL CHECK (length(payload_sha256) = 64),
    payload_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_module_digests_epoch
    ON module_digests(index_epoch, module_path);
```

`graph_metadata` has exactly one row. `index_epoch = 0` means no complete graph
generation has been committed. All digest rows for a usable generation must have the
single metadata epoch; mixed epochs are invalid. The payload repeats `module_path` and
its schema version so hydration can detect a row-key mismatch or a decoder/version
mismatch instead of trusting SQLite columns alone.

This is derived workspace data and follows `graph.db` recovery policy. Schema creation
is additive, but an older database containing graph rows with epoch zero is not
silently declared digest-complete. It is exposed as `digest_cache_missing`, schedules a
normal index publication, and uses the existing cited live-graph overview until that
publication succeeds. It never synthesizes digests during warm load or a query.

## Commit and invalidation protocol

Digest generation is part of index publication, not request handling.
`GraphStore::save_graph` is replaced by a generation-oriented operation that returns
the epoch and hydrated cache it committed. The protocol is:

1. From the exact immutable graph candidate, enumerate modules, compute canonical
   inputs and fingerprints, and generate or reuse payloads before opening the SQLite
   transaction. Reuse is allowed only when `module_path`, `generator_version`,
   `input_fingerprint`, decoded payload version/path, and `payload_sha256` all match.
2. Begin one transaction; replace nodes and edges; set `next_epoch = index_epoch + 1`
   with checked overflow; delete digest rows for vanished modules; and upsert one row
   for every current module with `next_epoch`. Reused payload bytes remain identical,
   but their row epoch advances with the graph generation.
3. Update `graph_metadata` last and commit. Any graph insert, digest generation,
   serialization, hash validation, metadata, or commit failure rolls back the graph,
   digest rows, and epoch together.
4. Return an immutable `IndexSnapshot { epoch, graph, module_digests }` built from the
   same validated payloads. Only after success may the daemon publish it to queries.
   Persistence failure retains the previously published snapshot and reports an
   actionable indexing failure; it must not publish the new graph alone.

The module cache is invalidated when any digest-visible node or incident edge changes,
when a module appears or disappears, or when the generator/payload schema version
changes. An incident cross-module edge invalidates both endpoint modules because either
digest may describe the relationship. A generator-version bump regenerates all rows.
An empty graph still commits a new positive epoch and an empty cache.

Computing fingerprints requires one bounded pass over nodes and edges, followed by
sorting within each module. It must not perform one full-graph traversal per module or
one SQLite query per module. Payload and fact limits keep cache size linear in module
count and prevent unbounded query output.

## Hydration and query snapshot contract

Warm load becomes `GraphStore::load_index_snapshot`, loading graph metadata, graph,
and all current digest rows in one SQLite read transaction. It validates the singleton
metadata row, epoch equality, normalized unique paths, expected module set, generator
and payload versions, both hashes, canonical JSON, and every citation against the
loaded graph. Hydration is all-or-nothing: one bad row rejects the entire digest cache,
reports the row and reason, and schedules reindexing. The healthy graph may still serve
the current cited live-graph fallback; no subset of untrusted digest rows is served.

The hydrated `ModuleDigestCache` contains:

- the committed epoch;
- an immutable path-to-digest map; and
- an immutable token-to-sorted-module-postings map built once during hydration or
  publication.

`QueryEngine` owns an `Arc<IndexSnapshot>` rather than a graph `Arc` plus an
independently replaceable cache. Constructors, warm startup, watcher publication,
manual reindex, tests, and workspace-manager paths must all publish the pair through a
single `publish_index_snapshot` method. Cloning a query engine clones this one snapshot,
so an in-flight query sees one epoch even when a later index is published. The current
graph-only `update_graph` and `update_graph_arc` mutation surface is removed or made
test-private; leaving it public would permit stale-digest reads by construction.

No request reads SQLite and no request invokes the digest generator. Query selection
uses the already-ranked subsystem result:

1. select cached modules matching explicit file anchors, in caller order;
2. then select modules matching ranked `key_files`, in rank order;
3. if no direct module matched, score token-posting candidates by exact distinct-token
   overlap with normalized query terms, descending, with `module_path` as the tie-break;
4. take at most two modules in compact mode or four in full mode; and
5. within each module, emit facts in stored order, deduplicating identical `fact_id`
   plus citation tuples.

The overview keeps the current query-specific, graph-ranked start sentence and then
uses cached fact sentences for selected modules. If hydration is unavailable or no
digest matches, `build_subsystem_overview` remains the cited live-graph fallback. That
fallback is not persisted or labeled as a digest. Cache hit/miss, selected module
paths, epoch, fallback reason, and validation failure are recorded in structured logs
and session metrics, not added as response boilerplate.

## Output citation contract

The public `SubsystemSummary.overview` remains a string; B1 does not require a second
wire representation. Cached prose is copied from validated `facts[].sentence`, not
reconstructed by parsing an opaque paragraph. This preserves these renderer-neutral
rules:

- every factual sentence contains at least one inline `` `workspace/path:line` ``
  citation also present in its structured fact;
- cross-module relationship sentences cite both endpoints;
- citations use normalized workspace-relative paths and one-based indexed node lines,
  never absolute paths, directory-only references, or line ranges;
- claims without a valid citation are omitted rather than weakened to a file-only
  assertion; and
- JSON, Markdown, compact, and full render paths receive the same cited overview before
  their ordinary response shaping.

The existing `key_files`, `key_symbols`, `tests`, rules, memories, rationale, expansion
handle, and token accounting remain query-derived. A digest supplements the overview;
it does not replace ranked machine-readable evidence or allow a stale module into those
sections.

## Failure behavior

- A generation or persistence error leaves the prior graph-and-digest snapshot live and
  marks the attempted index publication failed.
- A warm-load cache validation error names the module path, epoch, and failed invariant,
  rejects all cached digests, and triggers reindex; the graph-backed cited fallback
  remains available.
- A missing cache on an upgraded database is distinguishable from corruption and does
  not delete the graph database.
- Confirmed SQLite corruption continues through `open_recovering`; removal of the
  derived `graph.db` removes its digests as well, and the next index rebuilds both.
- An empty, valid cache at a positive epoch is not an error for an empty graph. An empty
  cache for a non-empty graph is invalid.
- Query-time cache misses do not write, retry, or regenerate. They use the live-graph
  fallback and increment a bounded diagnostic counter.

## Verification contract

Implementation is complete only with exact-output and lifecycle coverage. Checked-in
snapshot fixtures must use a small multi-file graph containing root-level code, nested
code, a test module, indexed Markdown headings, exports, and a cross-module edge.

Required tests are:

1. **Generator snapshots:** exact canonical JSON and rendered fact sentences for every
   fixture module, including exact inline citations.
2. **Determinism:** randomized node/edge insertion order produces byte-identical
   payloads, fingerprints, search terms, selected facts, and overview snapshots.
3. **Granularity:** there is exactly one row per normalized graph file; root, nested,
   test, and documentation modules do not collapse into directory rows.
4. **Invalidation:** a digest-visible node change regenerates its module; a cross-module
   edge change regenerates both endpoints; deletion removes its row; an irrelevant body
   change reuses identical payload bytes; and a generator-version bump regenerates all.
5. **Atomic storage:** injected failures during node, edge, digest, metadata, and commit
   stages preserve the prior graph, digest set, and epoch. A successful save advances
   all rows to exactly one epoch.
6. **Hydration:** restart round-trips byte-identical payloads; missing, mixed-epoch,
   malformed, non-canonical, wrong-path, unknown-version, bad-hash, and bad-citation
   rows reject the complete cache with a typed actionable error. Empty graph/cache
   round-trips successfully.
7. **Snapshot isolation:** a retained query snapshot continues to return the old graph
   and old digest epoch after a new snapshot publishes; a new query sees both new
   values. No test can update only one side.
8. **Query snapshots:** compact and full subsystem overviews are exact snapshots for
   direct-file, ranked-file, token-overlap, and fallback cases. A citation validator
   checks every factual sentence and both endpoints of relationship facts.
9. **No hot-path generation:** a generator invocation counter or panic-on-call test
   proves repeated cold and warm queries only read the in-memory cache. Query execution
   performs no `graph.db` reads or writes.
10. **End-to-end restart:** after one index publication, the same fixed `context` query
    before and after daemon restart selects the same modules and returns the same cited
    overview. The test also records `approx_tokens` so B1's before/after measurement is
    reviewable rather than asserted from prose.

The strongest acceptance fixture remains `lattice context "how does indexing work"`
against this repository: its overview must identify a useful indexing entry point,
explain the selected modules and at least one graph relationship in full sentences,
and attach valid `file:line` citations to every factual sentence without generating a
digest on the request path.
