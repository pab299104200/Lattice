# Workspace boundary policy

Lattice uses one source boundary for initial indexing, incremental reindexing,
watcher updates, and memory evidence reads. A source path must be repository
relative, contain only normal path components, use a supported source extension,
and pass the root and nested `.gitignore`, `.lattice_ignore`, and
`.latticeignore` rules. Default secret patterns and generated or dependency
directories are excluded. Ordinary `lib` and `lib64` source directories are not
excluded.

Traversal never follows symbolic links. On Unix, file reads open the workspace
root and then each relative path component with `openat` and `O_NOFOLLOW`;
intermediate components must be directories and the final descriptor must be a
regular file. The byte limit is checked on the opened descriptor and again
while reading. This prevents a path rename from redirecting a read through a
symbolic link between validation and open. The non-Unix implementation checks
each component for symbolic links, canonical containment, regular-file type,
and size, but the platform filesystem API does not provide the same
descriptor-relative traversal guarantee; this is an explicit coverage gap.

Ignore evaluation happens for direct watcher and evidence reads as well as
recursive scans. A path rejected by the boundary is never parsed or used as
verification evidence. Deletions are recorded only when opening an otherwise
eligible source returns `NotFound`; policy rejection and transient read failure
do not masquerade as deletion.

Native watcher coverage is established before startup indexing scans source.
The watcher keeps a metadata-only inventory of eligible paths. When a backend
reports only a directory, root, or rename notification, Lattice compares the
reported subtree with that inventory and sends only new, metadata-changed, and
deleted paths to the content indexing pipeline. Unchanged source bytes are not
read by this reconciliation. Registration inventories metadata for all eligible
paths once; a coalesced notification inventories metadata for its reported
subtree, so work is linear in that subtree's entry count while content reads
remain proportional to its detected changes. Native paths are mapped through canonical
workspace containment before they are restored to repository-relative form;
this handles equivalent platform path spellings without weakening the source
boundary. The same metadata comparison runs periodically while native watching
is healthy, so a backend event lost during registration or runtime is recovered
without rereading unchanged source content. Inventory errors preserve the last
committed baseline, report degraded watcher health, and never infer deletion
from a partial traversal.

Traversal, metadata, open, UTF-8, and read failures produce index failures and
mark the batch partial. An incomplete traversal does not infer that unseen
manifest entries were deleted and does not discard their prior parsed state.
This preserves the last known graph while exposing the coverage gap through
the existing index-health report. Parse failures and boundary/read failures are
distinguished by `IndexFailureKind`.

The boundary is intentionally limited to workspace source content. Git
administrative reads use their separate checkout-identity policy, and graph
persistence and retrieval authorization remain separate controls.
