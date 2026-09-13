# Embedding object storage

Lattice stores inference results as repository-shared immutable objects and keeps
checkout vector indexes as view-specific accelerators. Identical embedding input
from separate worktrees therefore resolves to one object while each checkout
retains its own file, symbol, offset, and vector-scope membership.

## Object identity

An object key is SHA-256 over a versioned, length-delimited encoding of the exact
embedding input and the complete inference identity:

- the SHA-256 digest of the pinned ONNX model artifact;
- the SHA-256 digest of the tokenizer artifact;
- output dimension;
- normalization algorithm version; and
- preprocessing version.

Model names and dimensions are insufficient identities. Two model artifacts with
the same output dimension produce different keys. The engine computes artifact
digests from the files it actually loads, rather than trusting their paths or
display names.

## Publication and recovery

`EmbeddingObjectCache::get_or_compute_batch` deduplicates identical inputs within
a batch. Concurrent callers sharing the repository cache join a single flight for
each object key. The flight owner computes all owned misses in one provider batch.
Objects are written to exclusive temporary files, synced, and renamed into their
content-addressed locations. Readers ignore incomplete temporary files.

Every read validates the format version, key, input digest, full inference
identity, dimension, and finite vector values. A corrupt immutable object is
discarded and recomputed. Provider, cache, or vector-publication errors are
reported as semantic failures; callers keep lexical and path retrieval available.

## Membership and collection

Checkout membership is committed transactionally to the repository object index
only after publishing the checkout vector generation. Rows map checkout/member
IDs to immutable object keys. Indexed reference counts preserve unaffected members
when changed-file updates arrive concurrently. A shared publication lease covers
inference through membership commit; exclusive GC defers while any publisher is
active. Checkout update locks serialize separate processes.

The repository lifecycle coordinator calls
`EmbeddingObjectCache::collect_garbage(max_bytes, limit)`. An indexed unreferenced
object query bounds work to at most 4096 entries per pass, without rebuilding the
union of checkout manifests. Active references remain even above the class budget.
An indexed temporary-publication journal lets exclusive GC remove interrupted
writes in bounded passes. Removal uses pinned directory descriptors and verifies
file identity. Missing files from interrupted deletion are reconciled on retry.

Checkout vector databases bind the complete model identity, including tokenizer
and preprocessing. Changing that identity transactionally retires previous vectors,
even if dimensions match, and rejects writes or queries from an older bound owner.

The object root is `embedding-objects` below the proven repository storage home.
Checkout-local vector databases remain under checkout cache namespaces. This
separation permits shared inference reuse without mixing divergent checkout
membership or search scope.

## Measurement

Cache statistics separately expose requested inputs, unique inputs, object hits,
computed objects, reused bytes, and written bytes. The deterministic ten-view
fixture proves 90 of 100 eligible requests are object hits, only ten vectors are
persisted, and reused object bytes are at least nine times first-write bytes. The
fixture measures storage reuse only; it makes no claim about model quality.

USearch persistence is a disposable accelerator over committed SQLite vectors.
The metadata binds the exact serialized index with SHA-256, its vector generation,
and dimension. An interrupted pair publication or malformed/digest-mismatched
payload triggers reconstruction from SQLite. Metadata reads are capped at 64 KiB;
index reads and publication are capped at 256 MiB. Oversized files return an
explicit resource error before allocation. Staging cleanup uses the identity of
its opened file descriptor and preserves a replacement it cannot prove it owns.
