# Deterministic Session-Digest Capture Contract

**Date:** 2026-08-13

**Status:** Accepted design for recovery workplan D3

**Scope:** Ambient, repository-local capture of bounded session outcomes. The [Shared Memory Architecture](./2026-08-12-shared-memory-architecture.md) remains authoritative for routing, identity, store roles, and promotion.

**Implementation status (2026-08-13):** The authority-free v1 parser,
normalizer, sanitizer, deterministic extractor, authenticated D3a transport,
protected client state, payload-free registry, capture journal, and
repository-store admission are implemented and tested. Installed adapters
open/resume a protected binding, queue sanitized edit facts or a close marker,
retry delivery by ID, and reduce a verified close into deterministic
repository-local session-digest memories. There is no automatic LLM step: the
separate consolidation workflow remains explicit and proposal-only.

## Decision

Lattice captures a small structured `session-digest` when an agent session
stops. D3a currently sends only the structured facts that the installed hosts
expose safely: an opaque session identifier at start, repository-relative
edited paths from an allowlisted edit event, and a close marker at Stop. A
future host adapter may add typed checks, typed errors, and a dedicated
final-summary field only through an explicit schema change. Trusted
session/repository/checkout authority is bound outside the payload. The daemon
deterministically extracts and writes repository-local `WorkflowOutcome` and
`FailurePattern` memories with evidence in one idempotent batch.

The system never stores or forwards a raw transcript, transcript path, prompt, tool input/output, command line, terminal output, environment, editor buffer, or diff as session memory. The stop hook is best-effort and fast: capture failure does not fail or delay agent shutdown. The daemon enforces all policy, including for direct or malformed CLI callers.

Optional LLM consolidation is background-only, explicitly opt-in, and creates review proposals for `Decision` or `Constraint` memories. It never writes those memories directly.

## Goals and non-goals

Capture preserves facts agents routinely omit from `remember`:

- edited files;
- checks run and their final outcome;
- errors observed and explicitly resolved in the same session; and
- a short final account of the work.

It must be deterministic, bounded, scope-safe, replay-safe, auditable, and usable via `recall --mode task` without an LLM call.

It does not archive agent conversations, act as telemetry, infer secrets, promote a repository observation to organization memory, or infer resolution from a later successful command. Missing final summaries are normal and do not make a session failure.

## Payload boundary

The bundled Stop hook invokes the private `__hook-adapter` entry point. This is
an implemented best-effort transport adapter, not a user-facing memory-write
command: it opens/resumes a capability binding, queues a sanitized close
marker, and the authenticated daemon route reduces the complete bound journal
into repository-local memories. There is no public
`remember --kind session-digest` admission command. The parser accepts content
only; it rejects identity fields as unknown input. The capture handler binds
parsed content to the capability before persistence and never treats a
caller-provided `session_id`, repository, checkout, branch, or scope as
authority.

Stdin is preferred so payloads do not appear in process listings. The D3a
adapter accepts a versioned content envelope, not a transcript path. An
integration must not inspect, open, hash, or forward a transcript path to make
the envelope; hosts that do not expose safe structured facts omit them.

The future digest schema has this logical shape; it is not the payload emitted
by today's Stop hook and exact public field names require a versioned adapter
change:

```json
{
  "schema_version": 1,
  "branch": "optional-branch-name",
  "ended_at": "RFC3339 timestamp",
  "edited_paths": ["daemon/crates/lattice-core/src/memory.rs"],
  "final_summary": "Implemented scoped memory recall; focused tests pass.",
  "observations": [
    {"kind": "check", "label": "cargo test -p lattice-core", "outcome": "passed"},
    {"kind": "error", "category": "compiler", "fingerprint": "sha256:...", "status": "resolved", "summary": "unresolved import in memory router"}
  ]
}
```

The D3a hook is not an authority for repository, checkout, branch, session, scope,
or organization. The parser is intentionally authority-free: it performs
bounded lexical/path normalization and sanitization only. The capture handler
derives repository and exact-checkout authority from the verified
hook capability and construct `MemoryQueryAuthority` there; the payload cannot
select or override it. This endpoint always produces repository-local session
capture. It cannot accept `scope: organization`.

### D3a adapter facts

The live adapters have a narrower contract than the future digest shape above:

- `SessionStart` admits only the host's opaque `session_id` for binding lookup.
- `PostToolUse` admits `tool_name` when it is one of `apply_patch`, `Edit`,
  `Write`, or `NotebookEdit`, plus the dedicated `file_path` (or the typed
  `tool_input.file_path` fallback). It emits only a normalized repository-
  relative `edited_path` fact; command arguments, tool input/output, and
  terminal text are discarded.
- `Stop` admits the host `session_id` and emits only a close marker. The
  installed adapters do not admit `edited_files`, a prompt, a transcript path,
  or a final summary from the host envelope.

Every adapter requires bounded JSON, drops unknown keys before transport, and
never opens a path named by the envelope. Repository, checkout, branch, and
scope are derived by the daemon from the authenticated capability and current
working tree. A host that cannot provide a safe structured field causes that
fact to be omitted, not reconstructed from transcript or raw tool data.

## Admission, normalization, and privacy

Identity violations fail closed. Individual malformed optional observations are dropped while safe observations can still be captured. The daemon retains only normalized allowed fields; audit entries record category/counts, never rejected content.

| Field | Allowed form | Limit and handling |
|---|---|---|
| bound session authority | daemon/capability-derived opaque ID | Required at persistence time, 1–128 ASCII-safe bytes; provenance-only, not parser input or searchable content. |
| `edited_paths` | normalized repository-relative paths | At most 128 unique paths, 512 bytes each. The parser rejects absolute paths, `..`, NUL, separators/drive forms, and excluded components; exact checkout containment, ignored-file policy, symlink resolution, and existence checks belong to the authenticated daemon handler. Sort and deduplicate. |
| `final_summary` | plain text | Optional. Normalize, reject secret/path/shell/control material, then cap at 2,000 bytes; omit if empty or unsafe. |
| `observations` | typed allowlist | At most 64 records, 1,024 bytes each; drop unknown kinds. |
| check label | display check name | At most 256 bytes after redaction. Never store shell arguments, cwd, environment, or output. |
| error summary | short symptom | At most 512 bytes after redaction. Never store raw stacks, request bodies, headers, or paths outside the repository. |
| timestamps | RFC3339 UTC | Reject invalid values; clamp far-future values to daemon receive time. |

Before limiting text, the sanitizer rejects text containing recognized credential
patterns (tokens, private keys, connection strings, and password assignments),
secret-bearing environment values, home-directory paths, and external
path-like substrings. It also rejects shell/control constructs. If safe
handling is uncertain, it drops the entire text field; it does not attempt
replacement redaction. No original value, replacement value, transcript
location, or secret appears in logs, errors, metrics, proposals, or audit
events.

This is a deliberately narrow input contract, not a claim that arbitrary prose can safely be stored. Raw diagnostics use a separate, explicit local workflow.

## Deterministic extraction

Extraction is pure over the normalized envelope, authority, extractor version, and matching records. It does not read files/transcripts, run commands, call a network service, or call an LLM. Equal inputs produce equal candidates and idempotency keys.

| Source | Candidate | Memory class | Evidence |
|---|---|---|---|
| edited paths | session change record | `WorkflowOutcome` | sorted repository-relative paths, session ID, capture time, repository/check-out identity |
| typed check | check outcome | `WorkflowOutcome` | normalized label, outcome, time, session ID |
| error with `status: resolved` | resolved failure | `FailurePattern` | category, stable fingerprint, sanitized symptom, status, session ID |
| final summary and one typed outcome | digest narrative | `WorkflowOutcome` | summary hash and linked extracted facts |

Final-summary prose alone cannot establish changed files, passing tests, a decision, a constraint, or a resolved error. A resolved error requires an explicit same-session resolution event with the identical normalized error fingerprint, or a constrained integration-generated resolution marker tied to that fingerprint. A later successful check, silence, or restart is not resolution. Unresolved errors are not ordinary `FailurePattern` memories.

Each candidate has extractor-versioned assertion and claim fingerprints. Its idempotency key includes repository ID, session ID, candidate kind, normalized evidence fingerprint, and extractor version. Retries merge provenance rather than duplicate records; existing proposal/contradiction machinery handles semantic matches across sessions.

## Storage, scope, and recall

The capture handler uses `MemoryStoreRouter::remember` with
daemon-derived repository, checkout, branch, and session authority. The
current parser and extractor do not receive a store handle and perform no
persistence. The handler receives no admin/unscoped query
capability; router/store role checks must reject capture into the organization
store.

Every record preserves integration/version, schema version, sanitized-payload hash, extractor version, opaque session ID, receipt time, repository/check-out identity, branch, repository-relative evidence paths, and typed observation evidence. It preserves no raw command/output/transcript.

Captured records are advisory observations, not fresh verification of the current checkout. Default applicability remains the captured repository and branch/session. `recall --mode task` can return compact same-repository digests with provenance; session/branch data must not cross repositories, unrelated checkouts, expansion handles, or the shared-memory tier.

## Opt-in LLM consolidation

A background consolidation job may be queued only when all conditions hold:

1. the operator enables `memory.session_digest_llm_consolidation`;
2. daemon-owned configuration has a supported provider and usable key;
3. the bounded queue and job budget allow admission; and
4. selected sanitized digests are in the same repository authority and retention window.

The job reads only admitted digests and typed facts—not transcripts or rejected fields. It can submit repository-scoped `Decision`/`Constraint` proposals via the existing proposal/audit pipeline. Proposals retain source IDs, evidence summary, prompt/response hashes, provider/model metadata, budget outcome, and uncertainty. They cannot auto-apply, overwrite, widen scope, or create organization memories.

Absent opt-in, provider/key, or queue capacity means no LLM call. The daemon records only a content-free skip reason; deterministic capture remains valid. Malformed model output likewise makes no memory mutation.

## Failure handling, retention, and observability

The Stop hook has a short deadline and exits zero after best-effort diagnostics
without printing payload content. Today, a missing daemon, missing binding, or
unavailable registry prevents delivery; it is not reported as a successful
memory write. Capture admission is transactional, and internal outcomes must
distinguish `captured`, `partially_captured`, `rejected`, `daemon_unavailable`,
`store_unavailable`, and `llm_skipped`. Writes must be transactional: crashes
before commit leave no record; same-key retries are safe. Partial capture must
report committed/rejected counts and never claim full success.

Status, doctor, and metrics expose bounded aggregate counters by integration, schema/extractor version, outcome/rejection class, and queue state. They must not expose summaries, paths, error text, reconstructable fingerprints, or session IDs.

The implemented D3a client state already has finite protected retention: at
most 256 pending deliveries/256 KiB, a 24-hour pending age, and a five-minute
close retry grace. Acknowledged, expired, or conclusively closed capability
records and pending deliveries are pruned without logging their content. The
registry primitive likewise retains only opaque authority/receipt metadata and
must prune expired bindings and receipts after its configured
audit/idempotency windows; it stores no digest content. Session-digest records
use an explicit repository-local age-and-count retention policy. That policy
must transactionally remove
dependent evidence, links, and queued jobs, while preserving review proposals
or durable records derived from a digest with content-free provenance and
deletion time. The current daemon capture journal and registry receipt window
is seven days. Operator deletion by repository-scoped session ID uses the same
transactional path and audits no content; it cannot widen authority to an
organization store or another checkout.

## Required verification

The implementation and bundled Stop-hook package must prove all of the following through direct daemon/CLI tests:

- a scripted fixture session yields task-recallable `WorkflowOutcome` and `FailurePattern` records with typed evidence;
- delivery retry is idempotent and output ordering is deterministic;
- malformed JSON, oversized fields, path traversal/absolute/ignored paths, mismatched authority, and unknown observations do not persist unsafe input;
- secret-bearing summaries, check labels, and errors are redacted or dropped, with no raw transcript path/content in memory, logs, metrics, proposals, or errors;
- prose cannot assert a passing test or resolution without typed corroboration;
- resolved errors require a matching same-session resolution marker;
- captured session/branch data cannot reach another repository or the shared store, including through handles and consolidation;
- daemon/store failure leaves the Stop hook successful but reports a truthful observable capture failure;
- no LLM request occurs without explicit enabled configuration and a usable key; enabled jobs create review proposals only; and
- retention/deletion cleans capture dependencies without deleting derived durable memory or required audit history.

The acceptance path is a scripted fixture session, `recall --mode task`, review-queue inspection, and a verified absence of LLM calls under default configuration.
