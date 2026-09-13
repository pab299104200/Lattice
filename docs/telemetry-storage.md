# Indexed telemetry storage

Lattice stores adoption telemetry in the proven repository storage home's
`adoption_metrics.sqlite3`. Ordinary repositories use the primary checkout's
`.lattice`; linked worktrees share this destination. Bare and separate-Git-dir
repositories use their resolved common-directory home. Non-Git workspaces use
their own `.lattice`. Invalid Git identity fails visibly instead of creating a
standalone fallback. It uses SQLite WAL mode, full synchronous
commits, a unique index on stable metric IDs, and an index on event timestamps.
The database replaces the former runtime JSONL append ledger.

The store pins its home directory once and opens SQLite through the managed
VFS, including WAL, SHM, journals, and temporary files. Legacy import opens a
regular file relative to that same handle without following symlinks. Replacing
the directory pathname cannot redirect an existing instance into another home.
Telemetry remains best-effort: unavailable authority or storage is observable
without withholding a valid briefing or memory acknowledgement.

Writes use an immediate SQLite transaction. A stable metric ID is looked up
through the unique index before insertion: an exact retry returns the existing
result without another event, while reusing an ID with a different payload
fails with an adoption metric ID conflict. Separate store instances and
separate processes use SQLite locking, so they share this same guarantee.

Retention is ninety days. Each store write and metrics read performs one
indexed timestamp_secs < cutoff deletion inside its transaction. This keeps
storage bounded without replaying or rewriting the retained history.

Capture health uses a separate content-free daily aggregate. A capture event,
its idempotency receipt, and its UTC-day/outcome counter update in the same
transaction. Health reads query the fixed outcome vocabulary across the current
UTC day and previous 89 UTC days, so they inspect at most 630 aggregate rows and
never deserialize unrelated telemetry payloads. This calendar-day health
window is intentionally distinct from the operational ledger's rolling
90-times-24-hour report window.

Existing SQLite ledgers acquire the aggregate through a durable sequence
cursor. Each attempt examines at most 1,024 event rows, commits receipts,
counters, and its cursor together, and reports migration-in-progress instead of
returning a partial health result. New capture writes remain idempotent while
that migration advances. Oversized or malformed capture rows fail closed;
oversized unrelated event payloads are not loaded by capture health.

## Migration and recovery

If `adoption_metrics.jsonl` exists in that proven repository home when the SQLite database first
opens, Lattice imports at most 1,024 records or approximately 8 MiB per
attempt, with a 1 MiB per-record cap. Rows and a source-bound byte cursor commit
in the same transaction. A restart resumes that cursor without duplicating
unkeyed events. During migration, normal telemetry writes return an actionable
migration-in-progress error; later attempts advance the next page. Briefing and
memory delivery remain available independently. The completion marker is
committed with the last page. The JSONL file remains untouched and is not read
again after completion.

Historical linked-checkout telemetry files are preserved as audit artifacts;
they are neither imported into repository authority nor automatically deleted.
The repository-home ledger is the only automatic import source. This avoids
silently merging unproven checkout history or duplicating already imported
events. New feedback metrics and their idempotency keys share the repository
destination across linked worktrees, so a canonical pending outbox can recover
after the producing checkout's daemon exits.

Each page validates the source file identity, length, and modification time
before and after reading. A changed or missing source, malformed completed line,
or oversized record rolls back that page and preserves earlier imported pages.
Restore the original source before retrying an interrupted migration. These
metadata checks detect ordinary replacement/modification, not adversarial
restoration of identical filesystem metadata.

A valid final JSONL record without a newline is imported. An invalid
unterminated final record is treated as a torn write and ignored. A malformed
completed line, or a duplicate stable metric ID whose payload differs from a
row already present during a resumed import, rolls back the current page and leaves
the completion marker absent. Operators should preserve the files, correct or
move the malformed legacy ledger only after investigation, then retry. No tool
response should depend on telemetry persistence: callers report these storage
errors as best-effort observability failures. Hook capture and memory
presentation log a warning if telemetry cannot be written, while preserving an
otherwise valid authenticated receipt, replay binding, action claim, or
briefing.

The legacy ledger must not be edited as a way to add telemetry after migration:
the committed marker deliberately prevents a later full-file scan and preserves
bounded write work.

Expiry reclamation deletes at most 1,024 indexed rows per operation. Expired rows are excluded from ledger reads and exact-ID lookup while subsequent operations drain an idle backlog, so a returning hook does not pay for an unbounded range delete.
