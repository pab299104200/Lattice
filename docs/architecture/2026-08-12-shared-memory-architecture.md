# Shared Memory Architecture

**Date:** 2026-08-12

**Status:** Accepted design for recovery workplan D2

**Scope:** Local, cross-repository organization memory; repository memory remains repository-local

## Decision summary

Lattice will use two physically separate SQLite memory stores and a scope-enforcing router:

- each repository owns one repository store at the canonical repository's shared `.lattice/memories.db`;
- the local daemon owns one shared store at `~/.lattice/shared/memories.db` by default;
- session, branch, and repository memories exist only in the repository store;
- organization memories exist only in the shared store;
- every recall path queries the stores independently under explicit authority, then performs one bounded ranked merge;
- organization access is derived from trusted daemon configuration, not from an arbitrary organization identifier in a tool request;
- a repository verification does not transfer to another repository. Cross-repository results are advisory and effectively unverified until verified in the querying repository or against organization-global evidence;
- promotion is explicit or proposal-driven. Consolidation never silently widens scope;
- stable record identity is separate from assertion identity. Record IDs locate an immutable history item; assertion keys group equivalent or conflicting claims across repositories.

This chooses a shared organization store over federating all repository stores. Federation would require discovering, opening, migrating, querying, and recovering an unbounded set of repositories for every recall. It also makes organization knowledge disappear when a repository moves or is unavailable. A single local shared store gives bounded query cost, one schema authority, and an auditable promotion boundary.

The shared store is local state for one OS-user daemon. It is not a network database, a multi-host synchronization protocol, or an authorization service.

## Goals and non-goals

The design must:

- make explicitly shared knowledge available in every configured repository without leaking branch, session, or repository memory;
- preserve the repository and revision that supplied evidence;
- expose trust differences instead of presenting a result verified in repository A as verified in repository B;
- deduplicate repeated observations while preserving independent provenance;
- surface disagreement through the existing contradiction and supersession workflow;
- remain safe under concurrent daemon shards and Git worktrees;
- keep query fan-out and response growth bounded;
- be diagnosable and recoverable after partial writes, SQLite contention, or unavailable source repositories.

It does not:

- automatically upload memory or share it between machines;
- infer organization membership from a Git remote, directory name, repository file, or request argument;
- make organization memory globally trusted;
- copy repository memories into every repository store;
- allow query-time scans of all known repository databases;
- make LLM output an authoritative promotion, deduplication, or conflict decision.

## Terms and identities

The implementation needs four identities with different jobs:

| Identity | Meaning | Used for scope? |
|---|---|---:|
| `RepositoryId` | Canonical identity shared by the main checkout and all of its Git worktrees | yes |
| `CheckoutId` | Stable identity of one worktree/checkout of a repository | no; evidence freshness only |
| `OrganizationId` | Explicit organization configured for the daemon/user environment | yes |
| `MemoryId` | Stable identity of one stored memory record under its owning repository or organization authority | lookup and links |

Workplan C1 is the source of truth for `RepositoryId` and `CheckoutId`. D2 must not derive repository scope from the current worktree path. Until C1 is complete, D2 may use injected identities in tests but must not ship path-based fallback behavior.

`MemoryId` must identify the authority that owns the record rather than the workspace that happens to query it. D2 should replace the current workspace-only assumption with:

```text
MemoryAuthority = Repository(RepositoryId) | Organization(OrganizationId)
MemoryId        = (MemoryAuthority, ULID)
```

The encoded form must include the authority kind. A shared memory returned in repository B therefore retains its organization authority; it is never reconstructed as a repository-B identity. Expansion handles and memory links carry the same authority-qualified ID. Because Lattice is prerelease, D2 should update the identity model, callers, tests, and docs together instead of adding a second legacy alias.

### Assertion identity

Record IDs do not deduplicate knowledge. Each durable memory also receives an `assertion_key`:

```text
sha256(
  schema_version,
  organization_id,
  memory_class,
  assertion_type,
  normalized_subject,
  normalized_predicate,
  normalized_applicability
)
```

The assertion key identifies a claim slot, not the claimed value. A separate `claim_fingerprint` hashes the normalized value/content. Structured writers should supply subject, predicate, applicability, and value directly. For existing free-text writers, a normalized `refresh_key` supplies the assertion slot when present and normalized content supplies the claim fingerprint. Without a refresh key or structured fields, normalized content is used for both; this safely finds exact duplicates, while the existing bounded duplicate/contradiction detectors must propose semantic matches. Normalization is versioned, Unicode-normalized, whitespace/case normalized, and strips repository-specific evidence locations. The normalizer version is stored with both keys.

The key has deliberately limited authority:

- same assertion key and same claim fingerprint: duplicate candidate; merge provenance only through an auditable proposal;
- same assertion key and different claim fingerprint: contradiction candidate, never a duplicate;
- different keys with high lexical or embedding similarity: possible duplicate proposal, never automatic collapse;
- changed normalization version: recompute in a migration and retain the prior key in migration evidence.

This lets the same fact observed in two repositories converge without treating semantically similar prose as proof of equivalence.

## Storage ownership and layout

### Repository store

The repository store remains `.lattice/memories.db`, but C1 resolves that path from the Git common repository identity. The main checkout and all worktrees use the same repository memory store. It contains only:

- `session` memories;
- `branch` memories;
- `repo` memories;
- their structured fields, evidence, links, access history, proposals, verification jobs, and audit events.

Branch visibility uses `RepositoryId` plus branch name. `CheckoutId` is recorded on evidence and verification results so two worktrees with different content do not share a false current-state verdict.

### Shared store

The shared store defaults to `~/.lattice/shared/memories.db`. Tests and managed deployments may inject a different absolute path. The parent directory is created with user-only permissions where the platform supports them, and the database and sidecars must not be made group/world readable by Lattice.

It contains only `organization` memories and their associated structured fields, evidence, links, proposals, verification-context results, access history, and scope-filter audit events. Multiple organizations may share the physical database, but every organization row and dependent record is partitioned by `OrganizationId` and every query binds that ID.

The daemon registry owns one shared-store instance per canonical database path. Shards receive a reference to that instance rather than opening independent in-process owners. Separate processes can still open the database safely under the SQLite rules below.

### Store roles are enforced

`MemoryStore` needs an explicit role:

```text
Repository { repository_id }
Shared { organization_set }
```

Open, write, query, proposal, verification, and link operations validate the role. A repository store rejects organization writes; a shared store rejects session, branch, and repository writes. The database records its role and schema version in metadata so opening the wrong file fails loudly. Store-role violations emit an audit event and return a domain error; they are not filtered into apparent success.

Physical separation is a safety boundary, not merely an optimization. It makes an unscoped scan of one database incapable of crossing from repository-private data into organization data.

## Organization authority configuration

The active organization set comes from daemon startup configuration controlled by the local operator. D2 adds `[memory] organization_id` and optional `shared_store_path` to the user-level `~/.lattice/config.toml`; `LATTICE_ORGANIZATION_ID` and `LATTICE_SHARED_MEMORY_PATH` are explicit process-level overrides for managed launches and tests. A configured shared path must be absolute. Repository-controlled files, Git remotes, and request payloads cannot grant organization access.

A handler receives an immutable `MemoryQueryAuthority`:

```text
repository_id
checkout_id
branch
session_id
allowed_organization_ids
```

For the initial implementation, one active organization per handler is sufficient. A `remember` request with `scope: organization` routes to that configured organization. If no organization is configured, it fails with an actionable error. If a lower-level/raw tool includes an `organization_id`, it must match the configured authority; it cannot widen it.

## Scope enforcement

All assistant-facing memory operations go through `MemoryStoreRouter`. The router exposes scoped operations such as `remember`, `recall`, `get`, `verify`, `link`, `propose`, and `list_conflicts`; it does not expose an assistant-facing unscoped query.

Recall executes two independent reads:

1. Query the repository store with explicit session, branch, and repository authority. Organization rows are impossible in this store and are also rejected by the role check.
2. If organization authority is configured, query the shared store with exactly the configured `OrganizationId`. Repository, branch, and session rows are impossible in this store and are also rejected by the role check.

Each read is bounded before merge. The shared store must not be opened or queried when the handler has no organization authority.

`query_unscoped_admin` remains restricted to migrations, integrity checks, and explicit operator diagnostics. Its type should require an internal admin capability so ordinary retrieval code cannot call it accidentally. The store must audit its use with operation name and store role.

The Retrieval V1 leak identified in the recovery assessment is already closed by commit `eb50349`: `retrieval_v1/candidates.rs` now constructs a `ScopeFilter` and calls `MemoryStore::query` instead of `query_unscoped_admin`. D2 must preserve that regression test and add the same negative cases for the shared tier. Retrieval candidates must be built from the returned authority-qualified `MemoryId`; they must never relabel a record with the querying workspace.

Scope-filter audit records for the shared tier include the attempted repository, checkout, organization, operation, memory authority, and denial reason. They must not copy memory content into logs.

## Write and promotion flows

### Explicit organization memory

`remember` accepts `scope: organization`. The server supplies the configured `OrganizationId` and writes only to the shared store. The record starts `unverified` (or `in_review` when organization policy requires review), carries its origin repository and checkout when one exists, and preserves evidence exactly as qualified below. Explicit scope is consent to share; it is not proof that the claim is true.

An organization write requires:

- configured organization authority;
- memory class and assertion type;
- confidence and confidence reason;
- a deterministic assertion key;
- provenance identifying the authoring session/operator;
- validity conditions and invalidation triggers for decisions, constraints, architecture invariants, and other high-impact classes;
- at least one evidence or provenance record, unless the class is explicitly a preference authored by the operator.

### Proposal-driven promotion

Repository memory is never mutated in place into organization memory. Promotion creates a new organization record linked with `derived_from` to every source repository memory. Source memories remain repository-scoped audit evidence.

Consolidation may suggest promotion only when:

- equivalent assertion keys occur in at least two distinct canonical `RepositoryId` values;
- the sources are not stale, contradicted, superseded, expired, or invalidated;
- the sources have independent provenance rather than copies from the same prior organization memory;
- their applicability does not name only one repository;
- their normalized claims agree.

The result is a review proposal containing the target organization, assertion key, source memory identities, evidence summary, proposed content, proposed validity conditions, and any trust limitations. Applying the proposal writes the organization record and link graph transactionally in the shared store. Rejection is retained for audit. No background or LLM job writes organization memory directly.

If two repositories disagree for the same assertion key, consolidation creates a contradiction proposal instead of a promotion proposal.

## Evidence and cross-repository trust

Organization evidence must be repository-qualified. A file, symbol, document, test, or Git reference includes:

- origin `RepositoryId`;
- origin `CheckoutId` when captured from a worktree;
- repository-relative artifact identity;
- source Git ref and commit OID when available;
- content hash and exact span when available;
- capture time and evidence kind.

Absolute checkout paths are diagnostic metadata only and are never the durable identity. Moving a checkout or opening another worktree therefore does not rewrite evidence.

The stored lifecycle status and the effective status in a query context are distinct:

- `origin_verification_status` describes the last verification against the evidence's own repository or organization-global source;
- `effective_verification_status` describes whether the claim is verified for the querying repository/checkout;
- `verification_contexts` stores verdicts keyed by `(memory_id, repository_id, checkout_state)` without overwriting another repository's verdict.

A memory verified only against repository A's graph is `unverified` and `advisory` when recalled in repository B. It may still show A's verification as provenance, but the response must say `cross_repo: true`, name the origin repository, and state that repository-local evidence has not been verified here. Repository B can create its own verification-context result without changing A's result.

When the origin repository is not loaded or no longer exists, verification returns `source_unavailable`. It does not erase the memory, pretend the evidence is current, or mark every consumer repository stale. The result remains advisory with an actionable recheck hint. Organization-global evidence, such as an explicitly configured organization policy document, can be verified independently of any repository, but it must use a distinct evidence authority rather than a repository path.

Contradicted, superseded, expired, and invalidated organization memories never appear as normal trusted guidance. Diagnostic and conflict modes may return them with explicit status and links.

## Merged recall and ranking

Every public `recall` mode—search, task, and verify lookup—uses the router. Code-context workflows that include memory highlights use the same merged retrieval primitive so behavior cannot drift between public recall and proactive delivery.

For a requested limit `N`, D2 queries at most a bounded oversample from each eligible store, initially `min(max(2N, 8), 64)`. It normalizes each candidate to one scoring record, deduplicates by authority-qualified record ID, groups exact assertion-key duplicates, applies trust gates, sorts, and truncates to `N`.

The ordering is deterministic:

1. query/anchor relevance;
2. eligible lifecycle and effective verification tier;
3. scope specificity, with current repository memory ahead of organization memory on an otherwise equal score;
4. confidence and recorded usefulness;
5. recency;
6. authority-qualified memory ID as a stable final tie-breaker.

Scope is a tie-breaker, not permission to outrank a materially more relevant result. Stale or contradicted records cannot regain trusted placement through recency or confidence.

Each result includes `source_tier`, `memory_id`, `assertion_key`, `origin_repository_id`, `origin_checkout_id` when applicable, `cross_repo`, origin and effective verification statuses, trust reason, evidence links, and conflict/supersession state. Store origin is carried through expansion handles so a follow-up lookup goes directly to the correct store.

If the shared store is unavailable, recall returns repository results plus a typed `partial` diagnostic naming the failed tier. It must not report a complete organization search. If the repository store fails, an organization-only result is likewise partial and must not masquerade as complete task history.

## Deduplication and conflicts

The existing contradiction, supersession, duplicate-detection, proposal, and review machinery remains the mutation authority. D2 extends it to authority-qualified IDs and shared-store transactions.

Cross-store links are represented in the shared store as qualified external references. The shared store never writes foreign keys into a repository database. Repository deletion therefore cannot corrupt shared-store integrity; verification can mark the external source unavailable.

Conflict handling follows these rules:

- exact assertion key plus identical claim: duplicate proposal to combine provenance;
- exact assertion key plus incompatible claim: contradiction proposal linking both records;
- newer evidence that explicitly replaces an older claim: supersession proposal;
- repository-specific exceptions: distinct applicability keys, linked with `refines` or `specializes`, not contradictions;
- unresolved conflict: all affected records are advisory and the conflict is visible in recall/status.

Organization conflicts are never settled by last-write-wins. Applying a proposal records actor, reason, prior state, source identities, and resulting links.

## SQLite concurrency, durability, and recovery

Both stores use the current `MemoryStore` SQLite settings: WAL journal mode, a five-second busy timeout, passive auto-checkpointing at 100 pages, and a bounded journal size. Those settings support concurrent readers and serialize writers, but they do not make SQLite a distributed database.

Operational assumptions:

- database, WAL, and SHM files are on one local filesystem and one host;
- the normal topology is one long-lived daemon with multiple shards and lightweight proxies;
- write transactions are short and contain no graph scans, network calls, or LLM calls;
- migrations acquire an exclusive schema/version transaction before requests are served;
- `SQLITE_BUSY` after the bounded timeout is an actionable partial failure, not an infinite retry;
- cancellation never interrupts a transaction between record creation and its required structured fields/audit link;
- checkpoint failure is logged with database path, store role, and SQLite code, while preserving the WAL for recovery.

WAL allows many readers but only one writer at a time. A process-local shared-store owner reduces avoidable contention; SQLite remains the inter-process correctness boundary if two daemon processes briefly overlap. Network filesystems and multi-host access are unsupported. `lattice doctor` should report journal mode, schema version, integrity result, pending WAL size, store role, and path/permission problems for both stores.

Backup and recovery must treat `memories.db`, `memories.db-wal`, and `memories.db-shm` as one live set or use SQLite's online backup API. Copying only the main file while the daemon is writing is not a valid backup. The shared store is durable authority and must never be deleted as reconstructable cache state.

## Existing organization rows and migration

D2 must remove the current ambiguous state in which an organization-scoped row can be written into a repository store.

At startup, before serving memory requests, an idempotent migration scans the repository store for organization rows. With matching configured organization authority, it copies each complete record and dependent structured data into the shared store using the same ULID and a new organization authority-qualified identity, records migration provenance, verifies the copied checksum, and removes the repository copy. A migration ledger makes restart after any step safe and prevents duplicate records. Without matching organization authority, startup reports a blocked migration and organization retrieval remains disabled; it must not silently expose or discard the row.

After migration, store-role validation rejects recurrence. There is no runtime dual-read compatibility path.

## Observability and security

Metrics and structured logs must distinguish repository and shared tiers without including memory content:

- query count, candidate count, returned count, latency, and partial failures by tier;
- scope denials by attempted authority and reason;
- shared-store busy timeouts, migration failures, WAL/checkpoint health, and integrity failures;
- cross-repository results surfaced and later used;
- promotion proposals created, applied, rejected, and blocked by conflict;
- effective trust downgrades by reason;
- assertion-key duplicate and contradiction candidates.

The shared store expands the blast radius of a local disclosure, so evidence and logs must avoid secrets and full command output. Workspace exclusion/security filters still apply before evidence is captured. A repository cannot grant itself access to an organization by editing tracked files. Organization memory must not carry unredacted absolute paths from another repository into assistant output.

## D2 implementation contract

D2 is complete only when all of the following land together:

1. Add canonical organization configuration and `MemoryQueryAuthority`; fail organization operations when authority is absent or mismatched.
2. Add authority-qualified memory identity and update encoding, decoding, links, expansion handles, retrieval candidates, serialization, docs, and tests.
3. Add `MemoryStoreRole` and `MemoryStoreRouter`; enforce scope/store compatibility on all read and write paths.
4. Open one shared store from the daemon registry and inject it into repository handlers. Keep repository-store ownership worktree-aware through C1.
5. Route `remember(scope: organization)` to the shared store and reject organization scope for outcome/automatic writes unless an explicit reviewed promotion path requests it.
6. Route every `recall` mode and workflow memory lookup through bounded two-tier retrieval and deterministic merge ranking.
7. Add assertion-key persistence, deterministic normalization, provenance grouping, and contradiction-versus-duplicate classification.
8. Add repository-qualified evidence plus per-repository verification contexts and cross-repository trust annotations.
9. Extend scope-filter audit events and operational metrics with store tier and organization authority.
10. Add the idempotent migration for existing organization rows; remove dual authority after migration.
11. Extend doctor/status diagnostics for shared-store configuration, permissions, schema, WAL, integrity, and partial availability.
12. Update the public MCP reference and README for organization remember/recall behavior, configuration, trust labels, and failure modes.

Required verification:

- two repository fixtures sharing one organization store: an organization memory written in A appears in B;
- the same memory in B is marked cross-repository, effectively unverified, and advisory while preserving A's origin verification;
- repository, branch, and session memories from A never appear in B through search, task, verify, workflow highlights, expansion, status, proposal, or conflict APIs;
- no organization authority means no shared-store query and an organization write fails;
- a forged request organization ID cannot widen configured authority;
- repository memory wins an otherwise equal ranking tie with organization memory;
- stale/contradicted organization memory cannot surface as trusted;
- identical assertion keys create one duplicate proposal; incompatible claims create a contradiction proposal;
- two worktrees of one repository share repository memory but retain distinct checkout verification contexts;
- concurrent readers/writers across repository shards produce no lost writes, torn structured records, or unbounded busy retry;
- shared-store failure yields bounded, correctly labeled partial recall;
- migration is idempotent across injected crash points and leaves one authoritative organization record;
- `query_unscoped_admin` is unreachable from assistant retrieval paths, enforced by a structural test and scope-leak regression tests.

## Dependencies and delivery order

Hard dependencies:

- D1 (this decision) is accepted;
- commit `eb50349` remains present and its scoped Retrieval V1 regression stays green;
- C1 provides canonical `RepositoryId`, shared worktree repository-store paths, and distinct `CheckoutId` semantics;
- A3's explicit CLI behavior remains in place so shared-store configuration/doctor failures cannot fall through into daemon mode.

A1/A2/A4 are needed before end-to-end hook acceptance but do not block core D2 store tests. B3 and D5 are not D2 dependencies; they must consume the router after D2 rather than inventing another memory query path. D3, D4, and D6 depend on D2. C2 and C3 are independent except that their checkout events may later trigger verification-context refresh.

Recommended delivery order inside D2 is identity and configuration, store roles/router, daemon ownership, write routing, merged recall, trust/evidence, migration, observability, then full integration and scope-leak tests. These are implementation checkpoints, not separate compatibility phases: the public behavior ships only when the complete D2 contract is green.

## Rejected alternatives

### Federate all repository stores on every recall

Rejected because discovery is unbounded, moved or offline repositories make results nondeterministic, concurrent schema versions become a query concern, and repository-private scope becomes easier to leak. Federation is appropriate for explicit multi-repository graph views, not for durable organization authority.

### Copy organization memory into every repository store

Rejected because copies drift, conflict resolution becomes last-sync-wins, revocation is unreliable, and every new repository needs a bulk import. The shared record must have one authority.

### Keep organization rows in both stores and merge by content

Rejected because two stores become runtime authorities and partial migration produces duplicates or inconsistent trust. D2 performs an idempotent migration and then enforces store roles.

### Trust verification from any source repository

Rejected because a fact that holds in repository A can be false or inapplicable in repository B. Verification is context-qualified and cross-repository results remain advisory until independently supported.

### Infer organization from Git hosting metadata

Rejected because remotes are mutable, forks cross trust boundaries, and directory layout is not authorization. Organization authority must be explicit operator configuration.
