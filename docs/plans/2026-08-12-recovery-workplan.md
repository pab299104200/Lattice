# Lattice Recovery Workplan — Orchestrated Execution Spec

**Date:** 2026-08-12
**Status:** Ready to execute
**Companion doc:** `docs/plans/2026-08-12-agent-adoption-recovery.md` (the assessment; read it first — it holds the verified evidence and file:line references behind every workload here).
**Audience:** An orchestrating agent (Sonnet/Luna or Opus/Terra class) that dispatches workloads to sub-agents, plus the sub-agents executing individual workloads.

## How to run this plan

- Each workload below is sized to run in a single focused session. Execute one workload per sub-agent; the orchestrator verifies the acceptance criteria before marking it done.
- **Model tiers** (assign the workload to the listed tier or higher):
  - **T1 — Sol/Fable**: open-ended design, architecture decisions, cross-cutting refactors where the approach itself must be invented.
  - **T2 — Opus/Terra**: complex implementation with a clear spec but many interacting parts.
  - **T3 — Sonnet/Luna**: well-specified implementation in a bounded area.
  - **T4 — Haiku/Luna**: mechanical work — config, docs, renames, straightforward test additions.
- Every workload: `cargo test --workspace` green before commit; one commit per workload, message `Wxx: <summary>`; update docs touched by the change in the same commit (repo rule: no doc drift).
- After daemon changes, rebuild `daemon/target/release/lattice` and restart daemon/proxy processes (`pkill -f lattice && sleep 2`) so live sessions use the fresh binary.
- Dependency graph: **Stream A first** (nothing else is observable until the channels work). Streams B, C, E are independent of each other after A. Stream D depends on A (working hooks) and benefits from B3/D5.

---

## Stream A — Restore the channels, make breakage loud

### A1. `lattice install` subcommand family — **T3 (Sonnet/Luna)**
Replace both bash installers (`integrations/claude-code/install.sh`, `integrations/codex/install.sh`) with Rust subcommands in `daemon/crates/lattice-daemon/src/cli.rs` (new module `cli/install.rs`):
- `lattice install mcp` — writes `.mcp.json` for the current machine (resolve the real binary path via `current_exe`; workspace roots from args or a config), replacing stale entries.
- `lattice install claude-code` / `lattice install codex` — merge hook config into `.claude/settings.json` / `.codex/hooks.json` idempotently, **reconciling** existing entries (update command/timeout/matcher in place — the Codex installer's behavior, not the Claude installer's append).
- `lattice install <target> --verify` — round-trip check: spawn the configured binary, `initialize` + `tools/list` (assert 8 tools); pipe a fixture payload through each installed hook and assert expected stdout with the daemon up. **Install fails loudly (nonzero) if verification fails.**
- Outer hook timeouts written by the installer must exceed the hooks' internal query timeouts; assert this in a unit test.
**Acceptance:** installer tests (port `integrations/codex/tests/install_test.sh` semantics to Rust or keep as shell tests against the new subcommand); running `lattice install mcp && claude mcp list` on this machine shows `lattice` ✔ connected; `--verify` fails when pointed at a broken config fixture.

### A2. Fix and unify the hook packages — **T3 (Sonnet/Luna)**
In `integrations/`:
- Fix `claude-code/hooks/common.sh:54-60`: subcommand **before** `--workspace`, or drop `--workspace` entirely (preferred — the CLI's `detect_workspace_root` at `cli.rs:597` is strictly better than the bash `lattice_detect_workspace`; delete the bash function).
- Unify on one shared hook library (the Codex `common.sh` is the newer, correct generation): concurrent SessionStart calls, `lattice_extract_prompt`/`lattice_extract_files`, probe timeout 0.5s. The two packages should differ only in output envelope (Claude JSON `additionalContext` vs bare markdown) and payload extraction.
- The readiness probe must use the **same call shape** as real calls (the current probe is the only correctly-formed call in the Claude package — it validates a code path the real calls never use).
- Extend `integrations/codex/tests/hooks_test.sh` to cover the Claude package, including a regression test for the flag-order bug (assert the exact argv the hook builds).
**Acceptance:** with the daemon up, running each Claude hook with a recorded fixture payload prints non-empty context JSON; `lattice metrics` gains `claude-code | hook` rows after one live session.

### A3. CLI silent-failure removal — **T4 (Haiku/Luna)**
In `cli.rs` / `main.rs`:
- `--help`/`-h`/bare `lattice` → usage text listing all subcommands, exit 0.
- Unknown subcommand → error on stderr, exit 64. Daemon/stdio modes reachable **only** via explicit `--daemon`/`--stdio`. This kills the fall-through class of bug (e.g. `lattice --workspace X status` silently booting a daemon).
**Acceptance:** tests for `lattice`, `lattice --help`, `lattice frobnicate` (exit 64, message), and `lattice --workspace <ws> status` (now an error pointing at correct syntax, not a silent daemon).

### A4. Doctor in the loop — **T3 (Sonnet/Luna)**
- Extend `doctor.rs`: execute one real hook with fixture input and assert non-empty output (call-shape check); verify hook-config outer timeouts exceed inner ones; count orphaned `lattice --stdio` processes and warn.
- SessionStart hook: when the daemon is unreachable or the config scan finds stale paths, inject **one line** (`lattice: daemon unreachable — run 'lattice doctor'`) instead of pure silence. Once per session, bounded; never blocks.
**Acceptance:** doctor test fixtures for each new check; manual run of a session with the daemon stopped shows the one-line notice and nothing else.

### A5. Config + docs cleanup for this machine — **T4 (Haiku/Luna)**
- Regenerate `.mcp.json` via A1 for `/Users/pete/Cadres/*` roots; delete the stale `/home/pete` version. Kill the orphaned `--stdio` proxies.
- Rerun the codex installer so `.codex/hooks.json` timeouts match (currently 2s vs installer's 4/5).
- Fix every instruction surface: `~/.claude/CLAUDE.md` (replace the legacy tool list — `get_context_capsule` etc. — with the 8 verbs), project `CLAUDE.md`, `AGENTS.md`, both integration READMEs (dead `/home/pete` paths, wrong probe default 0.2 vs 3.0, "tokens" vs chars).
**Acceptance:** `grep -r "home/pete" --include="*.md" --include="*.json"` finds only historical plan docs; `lattice doctor` passes clean.

### A6. Proxy lifecycle — **T3 (Sonnet/Luna)**
`proxy.rs`: exit when stdin reaches EOF (the client is gone) and after a configurable idle timeout without traffic. Investigate why nine proxies accumulated (verify EOF handling with a test that closes stdin and asserts exit).
**Acceptance:** integration test: spawn proxy, close stdin, process exits ≤ 2s; no proxy survives its client.

---

## Stream B — Make responses worth the tokens

### B1. Real summaries + cached module digests — **T1 (Sol/Fable)**
The core quality problem. Replace `build_subsystem_overview` and sibling template builders (`daemon/crates/lattice-core/src/intelligence/agent.rs:6371-6850`) — current output is truncated-name concatenation ("word salad", verified in the assessment doc).
- Build structured prose from data the graph has: what each key file *is* (symbol-kind mix, exports, markdown headings), how key symbols relate (edge kinds between them), where to start and why. Full sentences; `file:line` on every claim.
- Add a cached **module digest** layer (Repowise's wiki idea, adapted): on index-epoch change, generate per-subsystem digests — deterministic template first, optional LLM polish only when a key is configured; cached in `graph.db`; never generated on the query hot path. `context` answers quote the digest with citations.
- Design deliverable first (half a page in the PR description): digest granularity (directory? community-cluster?), invalidation trigger, storage schema. Then implement.
**Acceptance:** `lattice context "how does indexing work"` on this repo returns prose a reviewer can answer the question from without opening files; snapshot tests for digest generation; before/after `approx_tokens` recorded.

### B2. Render defaults and boilerplate — **T3 (Sonnet/Luna)**
- Default `render` → `markdown` only (kill the hybrid double-payload, `rpc/mcp.rs:6694-6696, 12007`); JSON on request.
- Shrink `agent_retrieval_contract` (`mcp.rs:7724`) to a single `next_action` line.
- Remove the `<!-- lattice-metrics: … -->` HTML comment from responses; record those fields server-side in session metrics instead.
**Acceptance:** render-mode tests updated; default-mode response for a fixed query measurably smaller (record numbers).

### B3. Honest partials + answer-during-indexing — **T2 (Opus/Terra)**
- Replace the future-dropping 5s `tokio::time::timeout` (`mcp.rs:889-914`) with cooperative deadline checks at ranking-stage boundaries; on expiry return ranked-so-far results with `partial: true` through the normal renderer (handle + budget metadata included).
- While a shard is indexing, answer from the already-published cached snapshot (`main.rs:242-248` publishes it) with a one-line freshness banner. Delete the "use rg" copy in `indexing_workflow_response` (`mcp.rs:6353-6380`).
**Acceptance:** test forcing a slow retrieval asserts partial results with metadata; test during simulated indexing asserts real results + banner, no "use rg".

### B4. Schema hygiene — **T4 (Haiku/Luna)**
Type `recall.memory_id` and `status.anchor` (currently `{}`); align `impact.limit` schema default with the handler (8 vs 12, `mcp.rs:1022`); document or remove `context.mode:"playbook"`; declare `_lattice_client`/`_lattice_channel` or move to transport metadata. Add a test asserting schema defaults == handler defaults programmatically.
**Acceptance:** `mcp_schema_tests` extended; strict-schema validation passes.

---

## Stream C — Git worktrees: stop the reindex storms

**Verified mechanism (2026-08-12):** two compounding faults.
1. Worktrees share refs with the main checkout. `is_git_state_path` (`daemon/crates/lattice-daemon/src/watcher.rs:547-560`) matches `.git/refs/heads/*`, and `should_invalidate_workspace` (`watcher.rs:535-545`) escalates **any** git-state path to a full workspace invalidation epoch. So every commit or branch update **in any worktree** full-invalidates the main workspace — and each invalidation pays `rebuild_graph()` (`indexer/mod.rs:301-309`), which is O(all files).
2. A worktree's `.git` is a pointer file, so `detect_workspace_root` (`cli.rs:597`) treats each worktree as a brand-new workspace → full cold index into a fresh `<worktree>/.lattice/` — including a **separate `memories.db`**, silently fragmenting cross-session memory (directly undermining Stream D).

### C1. Worktree-aware workspace identity — **T2 (Opus/Terra)**
- Resolve a `.git` pointer file to the main repository (`git rev-parse --git-common-dir` semantics, via `git2` or manual parse of the `gitdir:` line).
- A worktree workspace shares the main repo's `.lattice/` stores: **memories.db is always the common one** (memory is per-repo knowledge, not per-checkout). The graph store is keyed per checkout content but reuses the shared parsed-file cache — cache hits are content-hash based (`stable_content_hash`, `runtime_support.rs:315-322`), and worktree files are overwhelmingly identical to the main checkout, so a worktree cold start becomes mostly cache reads.
- `workspace_id` for memory scoping resolves to the main repo identity regardless of which worktree the session runs in.
- Design note required in `docs/architecture/` (per repo docs rule): worktree identity resolution, store sharing, and concurrency (two daemon shards writing one memories.db — SQLite WAL handles it, but state the locking assumptions).
**Acceptance:** integration test: create a real worktree of a fixture repo; assert it resolves to the main repo identity, reuses ≥90% of parsed cache on cold start, and reads/writes the shared memories.db.

### C2. Precise git-state invalidation — **T3 (Sonnet/Luna)**
- On a git-state event, don't full-invalidate unconditionally. Read HEAD's target: full invalidation only when **this checkout's** checked-out ref or HEAD commit actually changed (branch switch, reset, rebase). A ref update for a branch this checkout doesn't have checked out (i.e. a sibling worktree's commit) → no-op.
- Ignore `.git/worktrees/**` churn entirely (sibling worktrees' HEAD/index files).
- Keep the ≥20-paths batch threshold for real checkout storms — those changed files genuinely need (incremental) reindexing; the point is to stop *epoch invalidation*, not file updates.
**Acceptance:** unit tests: sibling-worktree commit → no invalidation; own branch switch → invalidation; fixture-based watcher test asserting reindex counts.

### C3. Incremental graph maintenance — **T1 (Sol/Fable)** *(measure first; largest single win if it lands)*
`rebuild_graph()` rebuilds the petgraph from **every** parsed file on any change batch. Design and implement scoped graph updates: remove the changed files' nodes/edges, re-add from their new parses, and re-resolve only cross-file edges that touch the changed files (the resolver already knows edge endpoints). Before building: add a benchmark measuring rebuild time on this repo (609 files) and a synthetic 5k-file tree; only proceed if the numbers justify the complexity — and record them either way.
**Acceptance:** benchmark in `daemon/crates/lattice-core/benches/`; correctness proven by an equivalence test (incremental result == full rebuild result on randomized change sequences); watcher path uses the incremental update.

---

## Stream D — Shared knowledge: make memory the reason agents come back

**Vision:** Lattice's moat is that it has seen *all* work across *all* sessions and repos. An agent should reach for `recall` because it reliably surfaces decisions, gotchas, and outcomes that no amount of grepping reproduces — and should feed `remember` because stored knowledge demonstrably resurfaces. Much of the machinery exists (scopes incl. `Organization` in `memory/model.rs:105-110`, consolidation with auditable proposals, verification jobs, trust statuses); what's missing is the **shared tier, retrieval quality, automatic capture, and proactive delivery**.

### D1. Shared-memory architecture design — **T1 (Sol/Fable)**
**Status:** Complete — see `docs/architecture/2026-08-12-shared-memory-architecture.md`.

Design doc (`docs/architecture/`) deciding:
- **Storage:** one shared org store (e.g. `~/.lattice/shared/memories.db`) vs per-repo stores + federated query. Recommend evaluating the shared store: `Organization`-scoped memories live there; `Repo`/`Branch`/`Session` stay in the repo store; `recall` merges both, ranked. C1's worktree identity work feeds this (one repo = one store).
- **Scope promotion:** when does a repo memory become org knowledge? (explicit `remember --scope org`; plus consolidation proposals that *suggest* promotion when the same fact is observed in ≥2 repos.)
- **Trust across repos:** a memory verified against repo A's graph is `unverified` in repo B; define how cross-repo memories carry evidence (repo-qualified file refs) and how verification degrades gracefully.
- **Conflict handling** across repos reuses the existing supersede/contradict machinery; define identity keys so the same fact from two repos dedups.
- Note: `retrieval_v1/candidates.rs:235` uses `query_unscoped_admin` in `retrieve_fts` — audit and close this scope-leak risk as part of the design.
**Acceptance:** design doc reviewed (orchestrator reads it against this checklist); D2 spec extracted from it.

### D2. Implement shared store + merged recall — **T2 (Opus/Terra)**
Implement the accepted D1 architecture as one complete contract:
- Depend on C1's canonical repository/worktree identity. Introduce authority-qualified memory IDs so shared records retain organization ownership and are never relabeled as the querying workspace.
- Add trusted daemon organization configuration, `MemoryQueryAuthority`, explicit repository/shared store roles, and a `MemoryStoreRouter`. Organization authority cannot be widened by request arguments.
- Keep session/branch/repo rows only in the canonical repository store and organization rows only in `~/.lattice/shared/memories.db`; migrate any existing organization rows idempotently and remove dual authority.
- Route `remember(scope: organization)` to the shared store. Automatic/outcome writes stay repository-local; consolidation promotion remains proposal-driven and requires equivalent observations from at least two distinct repositories.
- Route every `recall` mode and workflow memory lookup through bounded repository + shared queries, then one deterministic merge. Relevance leads; effective trust and lifecycle gate results; current-repository scope wins an otherwise equal tie; confidence/usefulness, recency, and stable ID finish the ordering.
- Persist deterministic, versioned assertion keys and claim fingerprints. Same-key/same-fingerprint records create a duplicate/provenance proposal; same-key/different-fingerprint records create a contradiction proposal. Never settle organization conflicts with last-write-wins.
- Qualify evidence by origin repository, checkout, revision, artifact identity, hash, and span. Preserve origin verification, but return organization memory in another repository as `cross_repo`, effectively `unverified`, and advisory until verified for that repository.
- Extend scope-filter auditing, metrics, doctor/status health, public MCP docs, and partial-result reporting to the shared tier. Preserve the `eb50349` Retrieval V1 scoped-query regression and make admin-unscoped reads structurally unavailable to assistant retrieval.

**Hard dependencies:** D1; C1 repository/worktree identity; `eb50349` scoped candidate retrieval; A3 explicit CLI behavior. A1/A2/A4 are required for end-to-end hook acceptance but do not block store integration tests. D3/D4/D6 depend on D2; B3/D5 must consume D2's router rather than add separate memory reads.

**Acceptance:** two fixture repos sharing one org store prove cross-repo recall and effective trust downgrade; exhaustive negative tests prove repo/branch/session data cannot cross repositories through any recall, workflow, expansion, status, proposal, or conflict path; forged/no-org authority tests; deterministic rank-tie test; duplicate-versus-contradiction tests; shared-worktree/distinct-checkout verification test; concurrent WAL test; shared-tier partial-failure test; crash-point migration idempotence test; `verification/scope_leak_tests.rs` and the structural no-admin-query guard green.

### D3. Automatic capture from sessions — **T2 (Opus/Terra)**
Agents won't reliably call `remember`; capture must be ambient (Repowise mines transcripts for corrections — same principle, deterministic-first):
- Stop hook: send the session's edited files + final summary (available in the Stop payload transcript path) to a new `lattice remember --kind session-digest` that runs **deterministic extraction**: files touched, tests run and their outcomes, error messages that appeared and were resolved. Store as `WorkflowOutcome`/`FailurePattern` class memories with evidence.
- Consolidation already supports LLM jobs off the hot path (`consolidation/llm/`); add an opt-in background job that reads recent session digests and proposes durable `Decision`/`Constraint` memories (through the existing proposal/audit pipeline — never direct writes).
**Acceptance:** after a scripted fixture session, `recall --mode task` returns the digest; proposals appear in the review queue; no LLM call happens without a configured key.

### D4. Proactive delivery — **T3 (Sonnet/Luna)**
Make stored knowledge arrive without being asked (this is what makes agents *notice* memory exists):
- SessionStart hook: include top-k memories relevance-ranked against the session's working set (dirty files, branch) — not just task memory. Budget-capped, threshold-gated (emit nothing over noise).
- UserPromptSubmit: memory hits ranked alongside code context in the injection.
- PostToolUse on Edit|Write: if the edited file has linked `Decision`/`Constraint`/`AntiPattern` memories, inject a one-line warning with the memory reference.
**Acceptance:** fixture test: seed a Constraint memory linked to file X; editing X in a hooked session injects the warning; metrics record the injection.

### D5. Semantic recall — **T3 (Sonnet/Luna)**
- Order FTS results by bm25 rank, not `created_at` (`memory/store.rs:1511-1549, 1949-2034`).
- `lattice install --with-embeddings`: download a small quantized ONNX embedding model to `~/.lattice/models/` (shared across repos), enabling the already-built vector path (`retrieval_v1/candidates.rs:249-256`, currently dead without `model.onnx`).
- Until real embeddings are present, gate memory supersession on typed-evidence overlap alone — the 64-dim FNV hash fingerprint (`embeddings/mod.rs:8-27`) must not decide supersession by itself.
- Make `rebuild_fts()` incremental (dirty-flag or triggers) instead of every store open (`store.rs:380`); replace the `let _ =` migration swallowing (`store.rs:228-331`) with a versioned migrations table.
**Acceptance:** rank-ordering test; recall quality spot-check with and without embeddings on this repo's real memories; store open time measured before/after.

### D6. Memory value metrics — **T4 (Haiku/Luna)**
Extend adoption metrics: memory retrievals that were subsequently cited/used (the `memory_accesses.was_used` column exists, `store.rs:200-207` — wire it), injections shown vs acted on, store growth per week, staleness ratio. Add a `lattice metrics --memory` view.
**Acceptance:** metrics visible after a hooked session; test coverage for the new counters.

---

## Stream E — Differentiated signal + honest measurement

### E1. Git intelligence layer — **T2 (Opus/Terra)**
New `lattice-core` module mining the last 500 commits (`git2`): file/symbol hotspots, co-change pairs, ownership/bus-factor, bug-fix density. Persist in `graph.db` keyed by commit; refresh on git-state events (already watched). Wire into: retrieval ranking, `impact` output (rank dependents by hotspot; **list co-change partners missing from the current diff**), and the PostToolUse hook (top-decile hotspot warning). Delete the watcher-`edit_count` pseudo-hotspots in `intelligence/mod.rs` (superseded; no-legacy-debt).
**Acceptance:** miner unit tests on a fixture repo; `impact` on a hot file in this repo shows history-backed annotations.

### E2. One indexing predicate — **T4 (Haiku/Luna)**
Single `should_index_file` shared by cold-start scan and watcher. Fix `runtime_support.rs:378-396`: stop collecting `c/cpp/h/hpp/mdx` (no parser → warn-spam); include `mjs/cjs/pyi`. Align the degraded-polling exclusion list (`watcher.rs:188-191`) with `EXCLUDED_DIRS`.
**Acceptance:** test asserting scan set == watcher set; no parse warnings on a fixture containing `.c` files.

### E3. Partial-index truth — **T3 (Sonnet/Luna)**
Thread `BatchIndexReport` (`indexer/mod.rs:32-48`) into shard state; `status{scope:"index"}` gains `parse_failures`, `failed_files` (top N), `is_partial`; doctor warns on nonzero.
**Acceptance:** test injecting parse failures asserts they appear in `status` and doctor output.

### E4. Metrics that can't lie — **T3 (Sonnet/Luna)**
- Follow-through: correlate `context`/`impact`-suggested files with subsequent watcher-observed edits in the session window, replacing the `"Session edited files:"` string match (`adoption_metrics.rs:154-165`).
- Storage: append-only JSONL or a `graph.db` table with compaction, replacing the full-file rewrite-per-call under a blocking mutex (`adoption_metrics.rs:66-98`); prune >90 days.
**Acceptance:** simulated session test shows nonzero follow-through; no full-file rewrite on the hot path (verify by inspection + a write-count test).

### E5. Worth-it benchmark — **T2 (Opus/Terra)**
Scripted harness (under `tools/` or `daemon/benches/`): fixed task set on this repo (find-the-implementation, blast-radius, diagnose-a-failure, recall-a-decision), run agent-style with and without Lattice, recording tokens, tool calls, and answer-cites-right-files. This turns B1's quality bar and D4's injection value into regression tests.
**Acceptance:** harness runs end-to-end; baseline numbers committed to the PR/plan.

### E6. Dead code removal — **T4 (Haiku/Luna)**
Delete `daemon/crates/lattice-core/src/query/watcher.rs` (unwired, cannot compile if included). Either wire `indexer/lazy.rs` into cold start or delete it (the static sort in `runtime_support.rs:405-442` is what actually runs — if keeping the sort, delete lazy.rs per no-legacy-debt).
**Acceptance:** `cargo test --workspace` green; no orphan modules remain (grep for undeclared `mod` files).

---

## Suggested execution order

| Wave | Workloads | Notes |
|---|---|---|
| 1 | A1, A2, A3 | Channels back. A3 unblocks A2's cleanest fix. |
| 2 | A4, A5, A6, B4, E2, E6 | Small parallel cleanups once channels work. |
| 3 | C1, C2, B2, E3 | Worktree fix + response cost. |
| 4 | B1, B3, D1 | The quality core; D1 is design-only. |
| 5 | D2, D3, E1, E4 | Shared memory + git signal. |
| 6 | D4, D5, D6, C3, E5 | Proactive delivery, semantic recall, incremental graph (measured), benchmark. |

## Success criteria

1. A fresh Claude Code session on this machine: MCP connected (8 tools), hooks injecting, and `lattice metrics` showing `claude-code` rows in both `hook` and `mcp` channels within a normal working week.
2. Committing in a worktree causes **zero** full reindex of the main workspace; a new worktree cold-starts mostly from cache and shares the repo's memory store (measured in C1/C2 tests).
3. A memory saved in one repo/session resurfaces — unprompted, via hooks — in a later session where it's relevant, and `lattice metrics --memory` proves retrieval and follow-through are nonzero.
4. `context` output passes the readability bar; default-call token cost is down vs the hybrid baseline; `impact` carries history-backed signal grep cannot produce.

## Proxy lifecycle correction — September 14, 2026

The idle-exit proposal above is superseded. Tool inactivity does not mean the MCP client has disconnected. Proxies remain alive until client stdin closes; there is no idle timeout setting. See [the lifecycle contract](../architecture/2026-09-14-mcp-proxy-lifetime.md).
