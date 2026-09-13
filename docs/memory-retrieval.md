# Memory retrieval

Memory recall is bound to the daemon's repository, checkout, branch, session,
and configured organization authority before a result budget is applied. A
repository cannot use an MCP request to widen that authority.

SQLite maintains normalized path, symbol, and failure membership indexes from
the durable memory record. They are derived indexes only: `memories` remains
the authority for content, lifecycle, scope, and verification state. Recall
uses exact scoped path, symbol, and failure candidates plus bounded sanitized
lexical groups. Exact memberships rank first. Lexical candidates rank by the
number of distinct query groups they cover before confidence and recency, so a
lesson matching a natural-language prompt is not displaced by high-confidence
noise sharing one common word. Scope, lifecycle, navigation, and retention
filters run in SQL before this ranking and before the result budget; lexical
candidate generation remains bounded to 64 query terms.

Workflow recall combines bounded task terms with resolved file and symbol
anchors even when the task already contains usable words. Exact linked-file or
linked-symbol membership therefore remains available to `context` and
`prepare_change` across checkouts of the same repository. The graph query layer
does not open or search memory: repository, checkout, branch, lifecycle, and
organization authority are applied once through the daemon memory router before
typed results enter a workflow bundle.

The 64-term limit applies after invalid or empty prompt tokens are removed and
duplicate sanitized groups are collapsed in first-occurrence order, so repeated
or punctuation noise cannot displace later usable terms. Queries are limited to
32 KiB of UTF-8 and each whitespace-delimited input term to 512 UTF-8 bytes;
oversized input is rejected before SQL execution. A nonempty prompt with no
usable exact or lexical group returns an empty result rather than falling back
to an unfiltered memory listing. Candidate execution also installs a per-query
SQLite progress handler. The default allowance is 5,000,000 approximate virtual
machine instructions, sampled every 1,000 instructions, and can be configured
with `LATTICE_MEMORY_RECALL_VM_INSTRUCTIONS` from 1 through 1,000,000,000. The
sampling granularity means interruption can occur up to 999 VM instructions
after the configured approximate boundary.
Invalid configuration fails with an actionable error. Exceeding the allowance
interrupts the query without returning partial candidates; an RAII guard removes
the handler on every exit so subsequent queries on the connection are unaffected.

Automatic briefings exclude invalidated, contradicted, expired, superseded,
and retention-stale records. Retention-stale records require an explicit
inspection request and are labeled as stale; they cannot replace current
evidence. Organization memories from another repository are advisory and the
briefing carries that uncertainty and a focused validation action.

Delivery is recorded as an attempted binding of the final payload hash and
memory set. Repository-only search, merged MCP search, workflow memory
highlights, explicit stale inspection, and hook presentation all return the
same receipt shape: `authority`, `delivery_id`, `payload_hash`, and
`ack_required`. Workflow and hook receipts contain only entries that survived
their final render budget. Dense JSON receipts bind both the shortened lead
highlight and any full memory representation that remains in the structured
payload. Default Markdown includes at most one complete memory of at most 512
UTF-8 bytes, with its ID and trust label; larger memories are omitted with an
explicit `recall` expansion instruction. If the attempted-delivery record cannot be
persisted, that memory content is withheld; a workflow may still return its
lexical and graph result with a degradation diagnostic. A receipt only renews retention after an authenticated
acknowledgement with the same repository, session, delivery id, and payload
hash. A duplicate acknowledgement is an idempotent replay; an unknown,
mismatched, or expired receipt is rejected. A queued, truncated, or failed
transport attempt is never treated as delivery.

Explicit `recall` with `include_retention_stale: true` is the lifecycle
inspection path for retention-stale records. It applies the current repository
and branch authority before its limit, labels returned retention state, and
issues the same acknowledgement-bound receipt when content is returned.
Ordinary search and task recall use the same indexed candidate query as
automatic briefings, including checkout applicability and semantic lifecycle
exclusions; neither scans or reranks the full memory ledger. Task recall with
only an opaque task ID returns no durable candidates until the task has a
statement, intent, or file/directory focus. A later call may reuse a previously
stored nonempty task statement. Exact-term diagnostics describe only the
bounded returned candidate set (`exact_term_counts_complete: false`) and never
claim that an unmatched identifier is absent from all durable memory.

Path, symbol, and failure memberships are backfilled once in a transactional migration and then maintained by write triggers. Authority, retention state, and exclusion of historical `repo_playbook` / `subsystem_playbook::` navigation records apply before the candidate limit. The exact-path regression includes 10,000 newer irrelevant, stale, and wrong-branch rows.

Conflict status is a separate explicit inspection path; it never contributes a
memory to recall or renews retention. Memory, normalized file, symbol, and doc
section anchors resolve through derived membership indexes under the same scope
and checkout applicability authority. Contradiction and supersession edges from both the link table and
structured assertion fields are deduplicated, sorted deterministically, counted
exactly, and paged in SQLite. Semantic-stale, contradicted, and superseded rows
remain visible here so an operator can understand and resolve them. Invalidated
or out-of-scope anchors and endpoints are rejected rather than silently omitted.

Conflict pages default to 25 records and accept at most 4,096. Cursor arithmetic
and conversion to SQLite integers are checked before execution. The complete
anchor, authority, edge, count, and page pipeline runs under a SQLite progress
handler, with a default allowance of 5,000,000 approximate virtual-machine
instructions. `LATTICE_MEMORY_CONFLICT_VM_INSTRUCTIONS` may set an allowance
from 1 through 1,000,000,000. Exceeding it returns an actionable error and no
partial count or page; the handler is removed on every exit.

Historical linked-doc memberships migrate in durable pages of 256 memory rows.
Startup advances one page, and a doc conflict inspection advances another page
before returning an explicit migration-in-progress error when more remain. New
and updated records are indexed immediately by triggers. Conflict anchor,
endpoint validation, exact count, and page reads share one SQLite snapshot, so
concurrent memory writes cannot make a page disagree with its reported total.

Memory feedback uses stable repository authority for both event and memory
identities. Retrieval validates the exact tool-call and memory-retrieved event
rows through their indexed IDs, including branch, session, and monotonic event
order; unrelated event volume cannot hide a valid retrieval. The validated
facts and pending access rows are then stored transactionally in the
repository-owned memory database. A later explicit workflow outcome may arrive
from a restarted daemon session: it validates the new terminal event against
the same repository and checkout, while the immutable original retrieval facts
come from the journal and remain usable after event compaction. Operational
metrics are a separate idempotent outbox. Each request retries at most 256
metric operations through a typed rotating cursor, and only marks an outbox row
after the metric store accepts its stable ID. Historical checkout-local attribution files
are retained as unauthoritative audit artifacts and are never imported
automatically.

Durable memory IDs may use the historical ULID form or the current UUID-like
local form. Repository-qualified IDs are reduced to local IDs only when their
repository authority exactly matches the active handler; foreign repository
and organization IDs are not attributed to the local repository.
