# Deterministic Session-Digest Capture Contract

**Date:** 2026-08-13

**Status:** Accepted design for recovery workplan D3

**Scope:** Ambient, repository-local capture of bounded session outcomes. The [Shared Memory Architecture](./2026-08-12-shared-memory-architecture.md) remains authoritative for routing, identity, store roles, and promotion.

## Decision

Lattice captures a small structured `session-digest` when an agent session stops. The hook sends a sanitized envelope containing repository-relative edited paths, a bounded final summary, and typed outcome observations. The daemon deterministically extracts `WorkflowOutcome` and `FailurePattern` memories with evidence.

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

The bundled Stop hook invokes an explicit public entry point:

```text
lattice remember --kind session-digest --input <sanitized-json>
```

Stdin is preferred so payloads do not appear in process listings. The command accepts a versioned JSON envelope, not a transcript path. An integration may inspect its own transcript path solely to make the envelope, but must not pass that path or transcript onward to the CLI, daemon, event log, or memory store.

Version 1 has this logical shape; exact public field names are set with the CLI schema implementation:

```json
{
  "schema_version": 1,
  "session_id": "opaque-session-id",
  "repository_id": "claimed-repository-id",
  "checkout_id": "optional-checkout-id",
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

The hook is not an authority for repository, checkout, branch, session, scope, or organization. The daemon derives these from the authenticated local session, working directory, and `MemoryQueryAuthority`; claimed identities must match or the capture is rejected. This endpoint always produces repository-local session capture. It cannot accept `scope: organization`.

## Admission, normalization, and privacy

Identity violations fail closed. Individual malformed optional observations are dropped while safe observations can still be captured. The daemon retains only normalized allowed fields; audit entries record category/counts, never rejected content.

| Field | Allowed form | Limit and handling |
|---|---|---|
| `session_id` | opaque ID | Required, 1–128 ASCII-safe bytes; provenance-only, not searchable content. |
| `edited_paths` | normalized repository-relative paths | At most 128 unique paths, 512 bytes each. Reject absolute paths, `..`, NUL, excluded/ignored paths, and paths outside the resolved checkout. Sort and deduplicate. |
| `final_summary` | plain text | Optional. Normalize, redact, then cap at 2,000 bytes; omit if empty or unsafe. |
| `observations` | typed allowlist | At most 64 records, 1,024 bytes each; drop unknown kinds. |
| check label | display check name | At most 256 bytes after redaction. Never store shell arguments, cwd, environment, or output. |
| error summary | short symptom | At most 512 bytes after redaction. Never store raw stacks, request bodies, headers, or paths outside the repository. |
| timestamps | RFC3339 UTC | Reject invalid values; clamp far-future values to daemon receive time. |

Before limiting text, the sanitizer removes recognized credential patterns (tokens, private keys, connection strings, and password assignments), secret-bearing environment values, home-directory paths, and external path-like substrings. If safe redaction is uncertain, it drops the entire text field. No original value, replacement value, transcript location, or secret appears in logs, errors, metrics, proposals, or audit events.

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

Capture uses `MemoryStoreRouter::remember` with daemon-derived repository, checkout, branch, and session authority. It receives no direct store handle or admin/unscoped query capability. Router/store role checks reject capture into the organization store.

Every record preserves integration/version, schema version, sanitized-payload hash, extractor version, opaque session ID, receipt time, repository/check-out identity, branch, repository-relative evidence paths, and typed observation evidence. It preserves no raw command/output/transcript.

Captured records are advisory observations, not fresh verification of the current checkout. Default applicability remains the captured repository and branch/session. `recall --mode task` can return compact same-repository digests with provenance, but session/branch data must not cross repositories, unrelated checkouts, expansion handles, or the shared-memory tier.

## Opt-in LLM consolidation

A background consolidation job may be queued only when all conditions hold:

1. the operator enables `memory.session_digest_llm_consolidation`;
2. daemon-owned configuration has a supported provider and usable key;
3. the bounded queue and job budget allow admission; and
4. selected sanitized digests are in the same repository authority and retention window.

The job reads only admitted digests and typed facts—not transcripts or rejected fields. It can submit repository-scoped `Decision`/`Constraint` proposals via the existing proposal/audit pipeline. Proposals retain source IDs, evidence summary, prompt/response hashes, provider/model metadata, budget outcome, and uncertainty. They cannot auto-apply, overwrite, widen scope, or create organization memories.

Absent opt-in, provider/key, or queue capacity means no LLM call. The daemon records only a content-free skip reason; deterministic capture remains valid. Malformed model output likewise makes no memory mutation.

## Failure handling, retention, and observability

The Stop hook uses a short deadline and exits zero after best-effort diagnostics without printing payload content. Internal outcomes distinguish `captured`, `partially_captured`, `rejected`, `daemon_unavailable`, `store_unavailable`, and `llm_skipped`. Writes are transactional: crashes before commit leave no record; same-key retries are safe. Partial capture reports committed/rejected counts and never claims full success. Store unavailability is neither a successful no-op nor an empty session.

Status, doctor, and metrics expose bounded aggregate counters by integration, schema/extractor version, outcome/rejection class, and queue state. They must not expose summaries, paths, error text, reconstructable fingerprints, or session IDs.

Session digests require an explicit repository-local age-and-count retention policy before release. Pruning/deletion transactionally removes dependent capture evidence, links, and queued jobs. It must not delete review proposals or durable records derived from the digest; those keep content-free provenance of the deleted opaque source ID and deletion time. Operator deletion by session ID uses the same path and audits no content.

## Required verification

Implementation must prove all of the following through direct daemon/CLI tests and the real bundled Stop-hook package:

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
