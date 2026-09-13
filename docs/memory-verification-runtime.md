# Explicit Memory Verification Runtime

Memory verification does not execute commands by default. `recall` with
`mode: "verify"` re-evaluates structural evidence and may consume a previously
recorded trusted-check observation, but it runs an external process only when
the caller also supplies `run_check` with a check ID declared in
`.lattice/verification-checks.json`.

Before execution, the daemon resolves the current repository and checkout
identity, loads the target memory through the active scope filter, and requires
the declared check's `evidence_reference` to be linked to that memory. An
organization-owned memory is rejected because a checkout process does not own
the organization memory authority. The daemon releases memory, graph, and
index locks before starting the bounded check process.

The trusted runner binds the result to the declared configuration and
executable, repository and checkout IDs, HEAD and Git index, graph generation,
and a full SHA-256 fingerprint of eligible workspace source bytes. It compares
the authority and fingerprint before and after execution. The MCP adapter then
recomputes the fingerprint before persistence and once more before supplying
the observation to the verifier. The executable is launched by pathname after
its initial hash. A concurrent replacement may therefore execute before the
post-run identity check. A persistent change is rejected, but a transient
change restored before that check (an ABA replacement) is not detected. The
binding does not freeze checked code or certify execution against a concurrent
adversarial writer. A declared check path must be treated as
repository-authorized executable content, not as an isolation boundary.

Admission also records a canonical digest of the target memory content,
validity rules, links, and evidence from the exact loaded claim and structured
fields supplied to the verifier; it does not bind a later database reread to an
earlier in-memory claim. The final transaction requires current storage to
match that digest.
Reusing a memory ID while changing its claim or evidence cannot attach an
in-flight result or an older verifier result to the replacement row.

Observations are stored in the repository memory database with the exact
target memory, check ID, evidence reference, repository, checkout, revision,
graph generation, full source fingerprint, semantic target-memory digest,
outcome, timestamp, and exit code. Writes
fail when the target is absent, belongs to another repository, or no longer
matches the semantic digest captured before the check ran. This comparison and
the observation insert share an immediate SQLite write transaction, so another
database connection cannot replace the target between validation and insert.
At most 64
observations are retained per memory, newest first, and deleting the memory
cascades to its observations. A failed persistence operation fails the request;
it cannot produce a verified result.

Default and queued verification never run a stored command. They consider only
persisted observations whose full authority and source fingerprint still match
the current checkout. An explicit run is persisted and then evaluated together
with all other current observations; it never bypasses a newer failure. Stale,
mismatched, corrupt, or removed check declarations provide
no behavioral validation. When multiple current observations apply, verifier
ordering is deterministic and a later failure dominates an older pass.
The adapter snapshots the persisted observation set before filesystem
revalidation and reloads it under the final memory-store lock. If another
request publishes an observation in that interval, verification fails closed
and asks the caller to retry with the new complete set.

Structured verification metadata, recall status columns, staleness, timestamp,
and graph generation commit in one immediate SQLite write transaction. Inside
that transaction the store reloads the target digest and the exact observation
set and compares both with the adapter's bound snapshot before writing. This
fences independent daemon and consolidation connections as well as concurrent
requests using the primary connection. If a target, observation, status, or
evidence write conflicts or fails, the whole verification update rolls back
and the request reports the persistence failure.

Public verification evaluates evidence without creating a second consolidation
proposal in a temporary runtime database. Its existing bound status commit is
the sole canonical mutation. Background proposal producers use the canonical
MemoryStore with explicit repository, checkout and branch authority; a separate
queue database cannot authorize a memory decision.

Queued verification selects only memories applicable to the active checkout.
Its final status transaction rechecks checkout and branch applicability as
well as the bound target and observations, so concurrent scope changes fail
without applying verification. Scope-only reads without a checkout continue
to exclude checkout-bound memories; an explicit checkout uses the same scope
checks with exact checkout applicability.
