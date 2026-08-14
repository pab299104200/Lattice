# Trusted Local Hook Session-Binding Contract

**Date:** 2026-08-13

**Status:** Accepted security and lifecycle contract for recovery workplan D3

**Scope:** Authentication and authority for repository-local hook events and
session-digest capture across the stdio proxy, CLI, long-lived daemon, and Git
worktrees.

**Implementation status (2026-08-13):** D3a's authenticated loopback
transport is implemented for the ordinary stdio proxy, CLI, doctor, and hook
adapter paths: protected boot credentials, loopback/epoch/token checks,
versioned hello/ack, and connection metadata are live. The hook-session
capability state machine, protected client capability-file lifecycle, and the
SQLite binding/receipt/outbox are implemented and tested as payload-free
primitives. Installed Codex and Claude Code adapters now open/resume a binding
and queue sanitized edit facts or a `SessionEnd` close marker with bounded retry state.
The daemon hook routes reduce these facts into deterministic repository-store
session-digest memories on an authenticated close. No installed hook creates
an organization or cross-checkout memory.

This note narrows and completes the identity boundary in the
[Deterministic Session-Digest Capture Contract](./2026-08-13-session-digest-capture-contract.md).
That note remains authoritative for sanitization, deterministic extraction,
memory classes, consolidation, retention, and recall. This note supersedes its
assumption that an "authenticated local session" already exists and its
allowance for an integration to inspect a transcript. No D3 component opens,
reads, copies, hashes, stores, or forwards a transcript or transcript path.
The authority-free parser also rejects transport-supplied identity fields.

## Decision

D3 uses two distinct credentials:

1. A boot-scoped **transport token** authenticates a local Lattice proxy or CLI
   process to the daemon. It proves only that the process is operating inside
   the local user's Lattice trust boundary.
2. A daemon-minted **hook-session capability** authorizes events for one host
   session, integration, repository, and exact checkout. It is persisted
   independently of shard/runtime lifetime, presented on every hook event, and
   cannot widen its authority through request fields.

The future hook handler constructs `MemoryQueryAuthority` from the verified binding and
fresh Git state. The hook payload, CLI flags, environment variables,
`ProxyHello`, and JSON-RPC arguments are claims, never authority. Capture is a
dedicated authenticated request path that targets the binding's checkout
directly; it never uses logical-view primary-root fallback or path-based shard
inference.

The daemon-generated ID currently owned by `McpHandler` is a shard-runtime ID,
not a client or agent session ID. A shard handler can outlive a connection and
is reused by unrelated proxies. Implementation must rename or confine that ID
accordingly and must not use it for hook capture, session-scoped memory,
idempotency, metrics correlation, or context-handle ownership. Those operations
receive connection- or binding-scoped authority explicitly.

## Threat model and trust boundary

Lattice protects against:

- another local OS user connecting to the loopback daemon;
- an unauthenticated process forging `ProxyHello`, client/channel metadata,
  workspace roots, or a host session ID;
- an authenticated ordinary CLI request claiming to be a hook or attaching to
  a different active hook session;
- replay of an event after its binding is sealed, expired, or revoked;
- accidental cross-repository or sibling-worktree capture; and
- secrets or transcript material entering RPC, logs, state, memory, metrics,
  errors, or review proposals.

The operating-system user account is the local trust boundary. A malicious
process running as the same user and able to read that user's protected
Lattice runtime/state directories can impersonate that user. A localhost
bearer token cannot honestly distinguish such a process. This design does not
claim stronger host-application attestation than the host supplies.

The daemon listens only on a loopback address. It rejects a non-loopback peer
even when the peer presents a valid token. On platforms with peer-credentialed
Unix sockets or named pipes, the installation may use them in addition to the
token, but peer credentials do not replace the protocol credential or
session-capability checks.

## Transport authentication

### Credential and runtime files

On each daemon boot, the process creates a cryptographically random 256-bit
transport token and a random daemon epoch. It writes a versioned credential
record atomically under the user's private runtime directory, preferring
`$XDG_RUNTIME_DIR/lattice/` and otherwise using `~/.lattice/run/`. The directory
must be owned by the current user and mode `0700`; the credential file must be
owned by that user, be a regular non-symlink file, and mode `0600`. Unsafe
ownership, type, or permissions are fatal. The record is scoped to the exact
listener address and contains only protocol version, daemon epoch, token,
listener address, and daemon PID.

Daemon startup remains single-flight under the existing address-scoped
advisory lock. The process holding that lock atomically replaces a stale
credential record before declaring the listener ready. A proxy never accepts a
credential whose address does not equal the address it will connect to.

Tokens are read directly by the Lattice binary. They do not appear in command
arguments, hook JSON, environment variables, process listings, lifecycle logs,
tracing fields, error strings, crash reports, or metrics. Comparisons are
constant-time. A daemon restart rotates the transport token and epoch, so every
existing TCP connection and boot-scoped connection grant becomes invalid.

### Authenticated hello

The current one-way `ProxyHello` becomes a versioned request/ack exchange. The
hello includes the protocol version, daemon epoch, transport token, a random
client-instance nonce, client kind, and requested roots/focus. The daemon
validates authentication before canonicalizing or loading any requested root.
It replies with a content-free acknowledgement containing protocol version,
daemon epoch, a random connection ID, and accepted connection features.

The transport token authenticates the connection, not its requested roots,
integration name, channel, or agent session. `LATTICE_CLIENT_NAME`,
`LATTICE_CLIENT_CHANNEL`, and equivalent request metadata remain display and
metrics labels only. Code must never branch authorization on them.

There is no unauthenticated compatibility hello. The coordinated daemon/proxy
protocol bump rejects old clients with an actionable version mismatch and
requires the client to reconnect through the matching binary. This repo is
prerelease; retaining an unauthenticated legacy path would preserve the defect.

## Hook-session binding

### Opening a binding

The installed `SessionStart` adapter accepts the host hook envelope locally and
extracts only:

- the integration adapter/version;
- the host's opaque session identifier;
- the hook event kind; and
- an optional host event identifier when the integration provides one.

It ignores and discards every other input field before constructing the Lattice
request. In particular, it never opens or forwards `transcript_path`, even if
the host includes one. A host session identifier is required for durable D3
capture. If an integration does not provide one, ordinary best-effort context
hooks may still run, but no session binding or automatic memory capture is
created.

The hook binary discovers the checkout from its actual process working
directory. It walks to the containing checkout, canonicalizes it, and resolves
`WorkspaceIdentity`. A payload `cwd`, repository ID, checkout ID, branch,
workspace flag, or configured multi-root list cannot override that result.
Session opening fails closed when the process directory is not inside one
unambiguous checkout.

Over an authenticated transport, `hook/session_open` sends the allowlisted host
identity and canonical checkout identity. The daemon independently resolves
the checkout and mints:

- a random binding ID;
- a random 256-bit hook-session capability;
- a daemon-owned internal session ID used by memory and metrics;
- repository identity from the Git common directory (or standalone root);
- checkout identity from the exact canonical checkout root;
- integration identity;
- start branch or detached revision;
- creation, last-seen, idle-expiry, and absolute-expiry times; and
- binding generation and state (`open`, `sealed`, `expired`, or `revoked`).

The host session identifier is an index input, not stored provenance. The
registry stores a keyed digest over integration identity, host session ID, and
checkout ID. Repeating `SessionStart` or resume for the same tuple returns the
same open binding and renews only its idle deadline. The daemon returns the raw
capability once to the local hook client and stores only its verifier hash.

### Client-side capability record

Because hook processes are short-lived, the hook binary stores the capability
in a versioned record under a user-private binding directory. The filename is a
keyed digest of integration, host session ID, and checkout identity; it does
not expose the host session ID. The record contains only binding ID,
capability, integration, checkout identity, daemon-independent expiry data, and
delivery bookkeeping. It contains no summary, path list, observation, prompt,
tool data, transcript field, or repository content.

The directory and file receive the same ownership, non-symlink, and
`0700`/`0600` checks as transport credentials. Reads and replacements use
descriptor-relative, no-follow operations where available. The adapter refuses
an unsafe record instead of repairing or trusting it. Capability records are
deleted after a successful seal and a short retry grace, or when conclusively
expired/revoked. An explicit later `SessionStart` may retire the locally held
generation before that grace elapses only after its close was acknowledged and
no delivery remains pending; an unacknowledged close stays retryable.

### Presenting authority

Every `hook/event` and `hook/session_close` request is sent on an authenticated
transport and includes binding ID, hook-session capability, integration,
delivery ID, and sanitized schema-versioned event. The daemon verifies all of
the following before parsing any optional event text:

1. the binding exists and is open (or is the same close replay in grace);
2. the capability verifier matches;
3. the integration matches the binding;
4. the daemon-resolved current checkout exactly matches the binding checkout;
5. the checkout still resolves to the bound repository identity;
6. the request is within idle and absolute expiry; and
7. the delivery ID has not been used with different normalized content.

The endpoint does not accept repository, checkout, branch, session, scope, or
organization authority in the digest. If compatibility-shaped fields are
present, they must be ignored before normalization or rejected; they are never
compared and then trusted. `scope: organization` is structurally unavailable.

After verification, the daemon builds `MemoryQueryAuthority` from the binding's
repository ID, exact checkout ID, daemon-derived branch segment, internal
session ID, and no organization authority. It calls `MemoryStoreRouter` only
through the repository-store role. Neither the hook adapter nor the capture
handler receives an unscoped/admin store API.

## Branch and worktree safety

Repository identity and checkout identity deliberately differ. Linked
worktrees share the Git common-directory repository identity and repository
memory database, but each canonical worktree root is a distinct checkout
authority. A capability minted in one worktree is invalid in its sibling even
when both point at the same branch or commit.

Every event re-resolves the actual checkout rather than trusting the original
hello or cached current directory. Relative edited paths are normalized against
that exact root. Absolute paths, `..`, NUL, excluded/ignored paths, paths through
an escaping symlink, and paths in sibling worktrees are rejected. Existing
paths are canonicalized. For a deleted/missing leaf, the nearest existing
ancestor is canonicalized and must remain inside the checkout; unresolved
symlink components fail closed. Multi-root sessions never use "primary shard"
fallback for capture.

Branch is sampled by the daemon, not the hook. If the checkout changes branch
or enters/leaves detached HEAD during a host session, the binding remains tied
to the checkout but the daemon closes the current branch segment and opens a
new one. Facts and candidates are emitted per segment with the daemon-observed
branch or revision. A digest spanning branch segments is not mislabeled as one
branch-scoped memory.

Moving or recreating a worktree changes checkout identity and invalidates the
old binding. A later authenticated `SessionStart` creates a new generation; a
`SessionEnd` event alone cannot silently rebind. Per-turn `Stop` is not a terminal event and is not registered by Lattice.

## No-transcript data path

Receiving a host hook envelope does not authorize reading files named by that
envelope. Every adapter uses an event-specific allowlist and performs local,
bounded extraction before RPC:

- `SessionStart` extracts host session/event identity only;
- edit hooks extract normalized candidate paths only;
- structured check hooks extract a canonical check kind and typed outcome,
  never a command line, arguments, environment, output, or stack;
- structured error/resolution hooks extract a redacted category, stable local
  fingerprint, status, and optional bounded safe symptom only when the host
  supplies enough typed data; and
- `SessionEnd` carries only a content-free close marker. Host transcript,
  reason, cwd, and summary fields are not admitted.

Transcript mining is not a fallback. The adapter does not derive tests,
failures, resolutions, or a final summary from conversation text. When a host
does not expose safe structured facts, those fields are omitted. Edited-file
evidence may be reconciled with daemon watcher observations for the same exact
checkout and internal session, but neither source may infer a passing check or
resolved error.

Unknown input keys are discarded before serialization. Size checks occur
before allocation of unbounded values. Sanitization happens once in the
adapter and again at daemon admission. Daemon errors name only the failed
contract category; audit and observability retain counts and reason enums, not
rejected values.

## Replay, ordering, expiry, and close semantics

Each hook invocation allocates a monotonically increasing local sequence and a
random delivery ID under a lock on the capability record. It writes only the
sanitized pending envelope before sending. Concurrent hook processes therefore
cannot reuse a sequence. If delivery fails or times out, a later hook may retry
that same pending delivery before sending its new event. The queue is bounded
by count, bytes, and age; overflow drops the oldest non-close event and records
only an aggregate local diagnostic. It never contains raw host input.

The daemon keeps a unique `(binding_id, delivery_id)` receipt with normalized
payload hash and result:

- same ID and same hash returns the original result without another mutation;
- same ID and different hash is a replay violation and revokes the binding;
- a previously unseen event for a sealed, expired, or revoked binding is
  rejected; and
- deterministic memory idempotency remains a second line of defense beneath
  delivery idempotency.

Sequence gaps are observable but do not invent missing facts. Out-of-order
events may be admitted within a small bounded reorder window and are reduced in
sequence order. Once a close is reducible, it seals the binding transactionally
with its admitted events and extracted candidates. A duplicate close returns
the stored close result during retry grace. A new event after close cannot
reopen or amend the session.

Expiry uses daemon monotonic time while running and persisted wall-clock
deadlines conservatively across restart. Configuration defines idle, absolute,
close-retry, receipt, and pending-event limits with tested finite defaults and
hard maxima. Event traffic may renew idle expiry but never absolute expiry.
`SessionStart`/resume may renew an open binding within its absolute lifetime;
no other event can. Expiry seals no inferred digest: admitted facts remain
auditable, but automatic memory capture requires an authenticated close unless
an explicitly documented crash-recovery policy later defines otherwise.

## Daemon, proxy, and shard lifecycle

The implemented `HookSessionRegistry` is a small versioned SQLite state
registry under the user's private Lattice state; it stores authority
fingerprints, capability verifiers, delivery hashes/order/receipts, and a
payload-free outbox marker. It has no event envelope, transcript, summary,
path, or command field. Hook-session bindings and receipts live there—not in
an in-memory `McpHandler`, TCP connection, logical view, or evictable shard.
Registry admission, the repository-local capture journal, and deterministic
memory extraction are coordinated so crash recovery uses receipt/idempotency
replay rather than guessing whether a write happened.

A TCP disconnect destroys its connection grant but not an open hook binding.
Proxy idle exit, shard eviction, index rebuild, daemon idle exit, and daemon
restart likewise do not transfer or broaden a binding. After restart, a new
transport token authenticates the hook binary; the existing unexpired
hook-session capability then resumes the persisted binding. Any in-flight
delivery is retried with its original delivery ID.

The daemon prunes expired bindings, capability verifiers, and receipts after
their configured audit/idempotency windows. It never prunes a binding merely
because the owning shard is cold. Registry corruption or an unavailable
registry disables hook capture with a typed, content-free health failure;
ordinary MCP workflows may continue. The daemon does not reconstruct a binding
from a claimed host session ID. A fresh authenticated `SessionStart` is the
only recovery path that can mint a new binding.

Binary upgrade is an explicit protocol transition. The daemon reports its
protocol/state schema before accepting roots, migrates the binding registry
transactionally, and refuses capture if migration cannot complete. Deployment
stops the old daemon and proxies after installing the matching binary, as the
repository deployment contract already requires.

## Public and internal surfaces

The following authenticated hook routes are daemon-internal integration
surfaces, not MCP tools: `hook/session_open`, `hook/event`, and
`hook/session_close`. They are callable only by the hook-adapter client kind;
ordinary CLI/MCP clients receive method-not-found. The installed adapters
present the capability, and the daemon connects parsing, registry
admission/outbox completion, authority binding, capture-journal reduction, and
repository-store writes end to end.

The assistant-facing public verbs remain `context`, `prepare_change`, `impact`,
`diagnose`, `search`, `remember`, `recall`, and `status`. Hook open/event/close
are an integration transport surface, not MCP tools and not a new unscoped
memory API.

The installed hook entry point is the private `__hook-adapter`; it loads and
presents a valid binding capability and sends normalized events over the hook
transport. There is no public `remember --kind session-digest` command.
Supplying `session_id`, `_lattice_*` metadata, `--workspace`, or a
session-shaped JSON document does not create authority. Direct ordinary
`remember --kind outcome` remains an explicit user write and is not labeled,
ranked, or audited as automatic session capture.

## Observability and failure behavior

Hook commands remain best-effort for the host and exit zero after a short
deadline. Lattice records truthful content-free outcomes such as
`binding_opened`, `binding_missing`, `transport_auth_failed`,
`checkout_mismatch`, `replay_rejected`, `capture_committed`,
`capture_partially_committed`, `registry_unavailable`, and
`daemon_unavailable`. Best-effort means the host session is not blocked; it
does not mean a failed capture is counted as success. A successful close is
the only path that seals the binding and commits deterministic session-digest
candidates; no close is inferred from process exit, silence, a later check, or
transcript text.

Status and doctor expose protocol version, token-file safety, registry schema
and health, counts of open/expired bindings, oldest pending age, replay
rejections, and capture outcomes. They expose no transport token, capability,
host or internal session ID, keyed session digest, edited path, summary,
observation, payload hash, or reconstructable fingerprint. Logs apply the same
rule.

## Delivered D3a invariants

The shipped path now enforces the following invariants at the adapter and
daemon boundaries:

- authenticated hello/ack and protected transport credentials are required;
  unauthenticated compatibility hello is rejected;
- binding authority is derived from the exact checkout and repository role,
  while the handler runtime ID remains distinct from hook-session identity;
- `hook/session_open`, `hook/event`, and `hook/session_close` are accepted only
  from the hook-adapter client kind and are routed before ordinary MCP tools;
- capability files, pending deliveries, registry receipts, and the capture
  journal are protected, bounded, replay-safe, and payload-limited;
- the SessionEnd flow seals only on an authenticated close and writes deterministic
  repository-local candidates through `MemoryStoreRouter`; it has no shared or
  organization-store fallback; and
- explicit `remember` records are independent of automatic capture and are not
  relabeled as session digests.

Any future schema or host integration must preserve these invariants and add
tests before changing the allowlist, retention, authority, or LLM behavior.

## Required verification

Implementation is not accepted until direct unit, integration, adversarial,
and real-package tests prove:

- missing, malformed, stale-epoch, wrong-address, and wrong transport tokens
  are rejected before root loading, while secrets never enter errors or logs;
- unsafe runtime/state ownership, modes, symlinks, and token/capability file
  replacement fail closed;
- a forged host session ID, client/channel label, workspace root, integration,
  or ordinary CLI call cannot acquire or use another binding;
- repeated SessionStart/resume is idempotent, concurrent hook invocations get
  unique ordering, same-delivery retry is idempotent, changed replay revokes,
  duplicate close returns its recorded result, and post-close writes fail;
- idle/absolute expiry, revocation, transport-token rotation, proxy disconnect,
  shard eviction, daemon restart, registry migration, and registry corruption
  produce the specified outcomes;
- primary and linked-worktree fixtures share repository identity but cannot use
  each other's capabilities or paths; moved worktrees, escaping symlinks,
  deleted paths, ambiguous roots, and multi-root primary fallback are covered;
- a branch-switch fixture produces separately scoped branch segments with no
  caller-supplied branch authority;
- a fixture transcript path points to a sentinel file containing secrets and
  filesystem instrumentation proves no D3 process opens it; the path and
  sentinel content are absent from RPC captures, databases, pending files,
  memory, proposals, logs, metrics, status, and errors;
- checks and resolutions appear only from allowed structured events, never
  from summaries, transcript text, later success, or silence;
- capture uses only the repository store and cannot reach organization memory,
  another repository, a sibling checkout, unscoped retrieval, or consolidation
  through forged fields or handles;
- crash points before receipt, after receipt, during memory commit, and after
  close converge to one deterministic result on retry; and
- the installed Codex and Claude Code packages complete a scripted
  SessionStart/structured-edit/SessionEnd session, produce task-recallable bounded
  memories, remain successful when capture is unavailable, and make no LLM
  call by default.

The release acceptance artifact must include a wire-level assertion that no
raw host hook envelope crosses the adapter boundary and a structural test that
the capture handler has no admin/unscoped store dependency.
