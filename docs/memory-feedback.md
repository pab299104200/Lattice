# Memory feedback attribution

Memory retrieval attribution is journaled transactionally in the repository-owned
managed `memories.db`. A retrieval records bounded copies of its validated
`tool_called` and `memory_retrieved` event facts, repository and checkout
authority, and at most 256 canonical memory access rows. The journal is the
durable attribution authority after event-log compaction; it does not depend on
checkout-local graph state and does not import historical graph rows.

An empty recall result, including one emptied by lifecycle, scope, or
organization filtering, has no attributable delivery. It returns without an
attribution journal record, retrieval event, access rows, or attribution error;
ordinary tool-call telemetry remains independent.

An exact retrieval replay returns the original access IDs. Reusing a retrieval
ID with changed facts fails without partial writes. Terminal feedback accepts a
validated later `workflow_succeeded` or `workflow_failed` fact under the same
repository, checkout, and branch authority. Its stable claim is the disposition
plus sorted cited access IDs. An equal transport retry may carry a newer valid
terminal event and remains idempotent; the first terminal fact stays as audit
authority. A changed disposition or cited set fails the compare-and-set. The
same transaction marks cited `memory_accesses` as used only for an `applied`
claim; rejected or ignored claims mark every access unused.

Attribution and adoption metrics live in separate databases, so the journal
stores retrieval and per-access pending flags as an outbox. A metrics writer
reads compact facts through a typed, colon-safe cursor in batches of at most
256 metric operations and marks those flags only after its own commit. The
outbox is repository-wide because both the canonical memory journal and
adoption metrics are repository-owned; each fact retains its original checkout
and session authority for reporting. Repository retention sweeps prune
the journal in batches of at most 1,024 and inspect at most 100,000 retained
rows plus one batch for count enforcement. Ordinary expiry preserves pending
outbox rows. A separate, longer metric-pending horizon creates a durable
`metric_delivery_expired` receipt and reports the dead letter in the sweep;
memory purge creates a `memory_purged` receipt. These receipts prevent replay
from recreating attribution after dependent memory data has been deleted.
Candidate selection also has a fixed SQLite virtual-machine instruction budget.
If an unusually large ineligible outbox backlog exhausts it, maintenance fails
with an actionable error before making any journal mutation and remains safe to
retry after the outbox is drained.

Journal operations never update memory recall, access-recency, confidence, or
retention clocks. Organization-scoped memories are excluded from this local
feedback path.
