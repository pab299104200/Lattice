# Lesson capture boundary

Lattice keeps navigation products in the derived context-handle cache. Subsystem
summaries and repository playbooks are tied to the graph generation recorded on
their handle; a changed generation makes the handle stale. They are not
automatically written into durable memory.

Session capture records generic edits, failed checks, skipped checks, and prose
as bounded episode evidence. None of those facts independently becomes a
reusable lesson.

A durable automatic lesson requires one bounded, authority-bound digest to
contain all of the following:

- the same failure fingerprint observed and later resolved;
- a sanitized recorded cause;
- a correction summary;
- an observed check with outcome passed.

The extracted record is a branch-scoped failure-pattern assertion with the
repository, checkout, branch, revision, capture segment, edited paths, failure
fingerprint, correction hash, and passing-check evidence. Extraction is
deterministic and idempotent through the existing session-digest delivery and
candidate keys. A failed or skipped check never supplies successful validation,
and no automatic capture promotes a lesson to broader scope. Digests older than
seven days at receipt are rejected before extraction, so delayed replay cannot
create a current lesson from stale episode evidence.

When a hook presents an existing lesson, it returns a separate memory delivery
receipt. The next authenticated hook request may acknowledge that exact
receipt. Presentation and a failed hook response do not renew retention; only
the matching acknowledgement does.

The 30-day/count-bounded session-capture policy retires transport delivery and
candidate-commit rows. It does not delete the lessons created by a capture,
whether or not they have been recalled. Those memories follow the repository
memory lifecycle: retention-stale after 90 days without acknowledged recall
and payload purge after 180 days by default. A short-lived, content-free
capture tombstone blocks accepted-window replay after transport retirement and
is itself removed once that replay window has elapsed. Each maintenance pass
shares one 256-row budget across capture transports and expired tombstones. It
reserves up to half for expired tombstones, preventing sustained capture
backlog from starving replay-fence cleanup, then uses any unused capacity for
either class. Repeated passes make deterministic progress. Explicit
repository-scoped operator deletion remains a deliberate knowledge deletion;
it removes the selected capture's source lessons and retains content-free
provenance needed by derived records and audit history.

Opt-in model consolidation selects committed, non-invalidated source lessons
within the worker's exact repository, checkout, and observed branch before
applying its candidate limit or contacting a provider. Session-only memories
are excluded. Typed capture evidence must agree with that authority; mismatched
evidence fails before provider invocation. Proposal creation and application
recheck source authority transactionally, so a concurrent source change cannot
turn a previously eligible fact into an authorized proposal.

Each model run processes exactly one leased capture. Source facts from another
capture cannot replace an older pending capture merely because they are newer.
The repository worker examines indexed pages of at most 256 captures (at most
two pages when wrapping its scan), preserving pending work across branch
switches. Its cursor is a scheduling accelerator, never a completion record.

All proposals from one capture and its content-free completion receipt commit
in one canonical memory transaction. A crash before commit leaves no partial
proposal batch; a crash after commit cannot cause another provider call for
that capture. Queue capacity and budget skips remain pending with bounded
backoff. A terminal provider rejection records completion without capturing
provider error content. Completion receipts retire with the source capture
transport through a foreign key cascade. Historical runtime watermark fields
are no longer consulted as proof of processing.
