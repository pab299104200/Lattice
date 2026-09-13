# Memory trust dimensions

Memory trust has three independent dimensions:

- **Claim acceptance** is the memory lifecycle decision represented by
  `MemoryVerificationStatus`. Review, contradiction, supersession, expiry, and
  invalidation remain authoritative lifecycle states.
- **Evidence freshness** says whether referenced files, symbols, document
  sections, tests, and exact spans still resolve in the current checkout.
- **Behavioral validation** says whether a trusted runtime observed a bounded
  check pass or fail for the current revision or graph generation.

These dimensions must not be collapsed. Finding an existing file or test proves
only that the reference remains fresh. It does not prove that the assertion is
correct or that a test passed. A memory with no evidence, an unsupported
evidence kind, or a test without a current bound result stays behaviorally
unverified and cannot produce a `MarkVerified` proposal. A failed current test
invalidates the claim even while its test reference remains fresh.

Confidence and evidence strength are separate too. Confidence records the
claim's asserted likelihood; evidence strength measures the supporting evidence
that was actually stored. Workflow briefings preserve the stored evidence score
and never turn high confidence into strong evidence. A missing evidence score is
reported as unverified, and an unverified claim remains advisory even when its
confidence is high. Behavioral validation still requires the independently
bound runtime observation described below.

The verifier accepts `BehavioralValidationRecord` values from trusted runtime
code. Each record identifies the repository, checkout, evidence reference,
observed result, timestamp, revision, and graph generation. A result applies
only when all authority and content-generation fields match the checkout being
verified and its age is within the caller's explicit bound. Future and expired
results are ignored. Among applicable records the newest observation wins;
failure wins an equal-timestamp tie. Callers must obtain these records from
daemon-observed execution. Memory evidence contains no executable command, and
the verifier never runs commands recovered from memory.

Scope enforcement remains an independent gate. A scope leak invalidates the
memory, while a valid scope cannot turn missing or unsupported evidence into a
verified claim. Evidence drift, exact-span mismatch, contradiction,
supersession, expiry, and invalidation continue to take precedence over a fresh
reference or passing behavioral result.

`VerifierCore::with_behavioral_validations` is the integration boundary. The
runtime supplies observed records together with its current repository,
checkout, revision, graph generation, observation time, and maximum age.

The normalized session producer still does not satisfy that input contract. Its
categorical result has no dirty-source fingerprint or proof that the configured
program produced it, so producer and generic authenticated session events are
never adapted into behavioral records.

The explicit `recall` verification adapter does satisfy the contract when the
caller supplies `mode: "verify"` and `run_check`. It selects a repository
declaration by ID, runs it under daemon control, binds its evidence reference,
repository, checkout, revision, graph generation, and full source fingerprint,
and discards the result if those values change. Runtime and resource use are
bounded. This execution authority comes only from repository configuration and
the explicit request; memory content and client-supplied check outcomes are
never commands or validation records.

## Trusted check runner

The daemon exposes the bounded runner only through explicit verification. It is
not connected to hooks, background work, memory ingestion, or default and
queued verification. A caller must make an explicit run-check request, supply a
proven `WorkspaceIdentity`, select a check ID, and impose a maximum timeout.
Merely declaring a check in a repository never executes it.

Checks are declared in `.lattice/verification-checks.json`:

```json
{
  "schema_version": 2,
  "checks": [
    {
      "id": "unit",
      "label": "unit tests",
      "argv": ["/absolute/path/to/test-program", "--workspace"],
      "timeout_ms": 120000,
      "env": { "CI": "true" },
      "evidence_reference": "tests/unit.rs"
    }
  ]
}
```

Version 2 is the sole accepted schema. Earlier producer-only declarations are
reported invalid and must add the bounded execution fields; the daemon does not
keep a second parser or execution path. Optional `error` metadata retains the
producer's categorical error mapping, while `evidence_reference` is the only
declared binding from an explicit observation to memory evidence.

The file is opened beneath a pinned checkout descriptor without following
symlinks. It is limited to 64 KiB and 64 uniquely named checks. Argument and
environment counts and sizes are bounded. `argv` is passed directly to the
program: there is no command-string parser or shell. The environment is cleared
and only declared values are installed. Executables must be absolute or use an
explicit `./` checkout-relative path; shell executables and symlinked executable
files are rejected. The executable identity is included in the state binding
and checked again after execution.

That identity check does not make launch immutable. The OS starts the
executable by pathname after the initial hash, so a concurrent replacement can
run different bytes. A persistent change makes the post-execution identity
differ and discards the observation, but a transient change restored before the
post-check (an ABA replacement) is not detected. Repository declarations are
therefore execution authority over the selected path, and the runner must not
be treated as freezing checked code or as safe execution against an adversarial
workspace writer.

Before and after the process, the runner re-resolves repository and checkout
authority and hashes the configuration, executable identity, HEAD, the bounded
Git index, and every source path and byte admitted by the shared workspace
policy. Traversal, source count, individual source size, index size, output,
and runtime all have hard limits. Crossing a limit is an actionable error and
never produces a partial fingerprint. A changed authority, revision, index,
configuration, or source fingerprint discards the observation. Passing and
failing exits are both observations; launch errors and timeouts are not.

The observation carries the full SHA-256 source fingerprint in addition to the
repository ID, checkout ID, revision, caller-supplied graph generation, check
ID, optional evidence reference, result, and observation time. An integration
must persist and compare that full fingerprint before adapting an observation
to `BehavioralValidationRecord`. Folding it into the record's numeric graph
generation would not collision-resistently bind dirty or untracked bytes.
The runner exposes a content-only recheck helper for this purpose. It reloads
the same declaration, revalidates workspace authority and graph generation,
and recomputes the fingerprint without executing any command or reading memory.

On Unix the check runs in a fresh process group and a timeout kills and reaps
the group. Windows creates the check suspended, assigns a non-breakaway Job
Object with kill-on-close, and resumes only after assignment succeeds. The job
contains descendants through timeout, output failure, and primary exit. Native
Windows runtime tests remain pending; isolated Windows cross-compilation passed.


## Auditable lesson revision

Use the existing public `remember` verb with `kind: "evolution"` to propose,
apply, or reject a change to repository memory. Creating a proposal captures the
current source state; supersession binds a hash of the replacement state without
copying its content into another proposal payload. Neither proposal creation nor
feedback counts as memory recall.

For an outdated decision, first capture the replacement through durable
`remember`, then propose its explicit relationship:

```json
{"kind":"evolution","action":"propose","memory_id":"OLD_ID","superseded_by_memory_id":"NEW_ID","reason":"The current repository contract replaced the prior decision."}
```

The CLI uses the same proposal validation:

```sh
lattice remember --kind evolution --action propose --memory-id OLD_ID \
  --superseded-by-memory-id NEW_ID --reason "Current contract changed" --json
lattice remember --kind evolution --action apply --proposal-id PROPOSAL_ID --json
```

Inspect the returned prior/proposed states before deciding. Apply with
`{"kind":"evolution","action":"apply","proposal_id":"PROPOSAL_ID"}` or
reject with the same shape and `action: "reject"`. An equal decision retry is
idempotent; switching an already final decision fails. Use
`status(scope: "conflicts", anchor: "OLD_ID", render_mode: "full")` to inspect
the scoped replacement-to-predecessor `supersedes` edge. Default retrieval
excludes the superseded predecessor. This interface does not create arbitrary
contradiction edges.

Both memories must have proven repository authority and be applicable to the
current checkout. Missing, deleted, unbound, foreign, or changed source and
replacement states fail closed. Applying a stale proposal cannot overwrite a
newer memory state. Changing claim content downgrades behavioral verification;
it does not carry a prior passing check onto the new assertion.

Automatic and session proposal producers use the same canonical memory store
and explicit repository, checkout, and branch authority. Proposal creation
checks target, replacement, and source applicability before persisting review
content. Episode creates and refreshes carry complete canonical prior/proposed
snapshots; episode provenance stays in evidence. Separate scheduling storage
cannot authorize a memory decision or bypass the canonical review limit.

The decision commits memory changes, proposal/job state, and a pending audit
event in one memory-database transaction. The event is published after that
commit, with a stable identity for safe retry. The response's `audit_event`
reports this proposal as `published` or `pending`; `has_more_pending` is a
bounded backlog indicator, not an exact count. Publication failure explicitly
reports that the decision already committed and its audit delivery is pending.
Retry the same proposal decision; do not create a different proposal merely
because event publication failed. Restart/request recovery drains committed
pending events. Delivery retires the outbox payload.

New consolidation audit events carry a proposal reference, immutable applied or
reverted transition, and state hash, without copying full lesson snapshots.
This upgrades the event database to schema version 4 and emits event envelope
version 3. Earlier binaries reject the upgraded event database rather than
misinterpreting an omitted snapshot as an empty-memory transition.
Replay requires the canonical proposal database and its deletion receipts; an
event alone cannot authorize restoring knowledge. Historical events without the
explicit transition remain audit-only. Replay derives a disposable view rather
than clearing or replacing canonical memories. Reversal is a separate explicit
canonical decision with its own transaction and audit event. Pending reversals
must follow their corresponding apply event even when publication is retried.
Historical event snapshot bodies are handled by the existing event compaction
policy; the new reference-only format is not a claim that old audit files were
deleted.

The September 13 implementation is undergoing final failure/replay integration
verification; the execution tracker records the acceptance evidence and any
remaining gate. Do not infer that earlier storage or delivery smoke results
cover this newly exposed revision workflow.
