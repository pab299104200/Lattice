# Embedding Model Provisioning

## Contract

Semantic retrieval uses a pinned ONNX `all-MiniLM-L6-v2` bundle shared by all
workspaces for one user. `lattice install --with-embeddings` installs the model
and its required `tokenizer.json` at:

```text
~/.lattice/models/all-minilm-l6-v2-1110a243/
```

`LATTICE_EMBEDDING_MODEL_DIR` can select another complete bundle directory for
managed installations. The directory is an asset location, never workspace
state; `.lattice/` inside a checkout must not become a second model authority.

## Integrity and activation

The manifest pins source URLs to a specific upstream commit and gives each
asset a SHA-256 checksum. Provisioning downloads into a sibling staging
directory, verifies each file before activation, then renames the completed
directory into place. A valid existing bundle is revalidated and reused. An
incomplete or corrupt bundle is quarantined before replacement, so failed
downloads never become the active model.

At daemon startup Lattice resolves only a complete checksum-verified shared
bundle. Missing, unreadable, or corrupted assets result in no embedding engine;
the daemon logs the ordinary semantic-path absence/failure and continues with
lexical retrieval. No caller should infer that semantic recall was active just
because a model path existed.

The model bundle is deliberately distinct from ONNX Runtime. Before creating a
session, Lattice explicitly and fallibly loads the runtime from
`ORT_DYLIB_PATH`, when set, or from the platform default beside the executable.
Absent, incompatible, or session-construction failures are bounded in the log
and disable only semantic retrieval; they must never terminate the daemon or
invalidate an otherwise checksum-verified model bundle. Operators can install a
compatible runtime beside `lattice` or set `ORT_DYLIB_PATH` to its absolute
path. The status surface reports lexical fallback until an embedding engine is
available.

## Duplicate-memory safety

The old 64-dimensional FNV token fingerprint remains a lightweight utility but
is not duplicate or supersession evidence. Duplicate-memory consolidation now
requires overlap in typed anchors (`linked_files` or `linked_symbols`) and
records `typed_evidence_overlap` as its decision basis. This avoids treating a
hash collision or superficial token overlap as authority to supersede a memory.
