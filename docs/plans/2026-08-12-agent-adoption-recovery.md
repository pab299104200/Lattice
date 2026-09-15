# Agent Adoption Recovery — Change Spec

**Date:** 2026-08-12
**Status:** Proposed recovery plan; dated assessment, not an implementation claim
**Audience:** Implementing agent. Self-contained; do not assume access to the conversation that produced it.
**Prior art:** `docs/plans/2026-06-11-agent-adoption-overhaul.md` (implemented as Phases 1–5, commits `b2ae740`…`73c64eb`). This spec explains why that effort did not stick and what to do differently.

> Findings in this document are an assessment snapshot from 2026-08-12. They
> describe what was observed or read at that time, including machine-local
> configuration and dated line references. They are not proof that the same
> condition still exists, nor proof that a plan item is complete. Current
> behavior must be established from the implementation, tests, and the commit
> history. Completed work should be recorded in a follow-up implementation note
> or commit message rather than changing these historical findings retroactively.

## Problem statement

Two months after the adoption overhaul shipped, `lattice metrics` shows effectively zero organic agent usage: no MCP calls to substantive verbs, no `claude-code | hook` rows at all, and 0% follow-through. Investigation on 2026-08-12 (including a feature-level comparison against Repowise, <https://docs.repowise.dev/>, an open-source system in the same category) found the causes fall into four tiers. All findings below were verified by execution or direct code reading.

### Tier 1 — Every channel into Claude Code was broken on this machine (2026-08-12 assessment)

1. **MCP never connects.** `.mcp.json` (gitignored, leftover from the Linux box) launches `/home/pete/cadres/lattice/daemon/target/release/lattice` — a path that does not exist on macOS. `claude mcp list` reports `✘ Failed to connect — ENOENT`. `lattice doctor` detects exactly this (`WARN stale binary path`), but nothing ever runs doctor.
2. **Claude Code hooks silently no-op on every invocation**, for two independent reasons in `integrations/claude-code/hooks/common.sh`:
   - `lattice_hook_call` places `--workspace <ws>` **before** the subcommand. `cli::is_cli_query_command()` (`daemon/crates/lattice-daemon/src/cli.rs:37-50`) only matches `args[1]`, so the call misses CLI dispatch entirely, falls through to daemon/stdio startup in `main.rs`, and exits 0 with empty stdout. Verified: `lattice --workspace <repo> status` → empty output, exit 0.
   - `lattice_detect_workspace` (common.sh:28-43) reimplements workspace detection in bash with a different marker set than `cli.rs::detect_workspace_root` (`cli.rs:597`), and can resolve to a parent directory outside any project (observed: `/Users/pete/Cadres`).
   - The readiness probe (`common.sh:50`) puts the flag **after** the subcommand — the only correctly formed call in the file — so the probe passes while every real call returns nothing. The hooks are validated by a code path the real calls never use.
3. **Codex hook timeouts are self-defeating.** `.codex/hooks.json` sets outer `timeout: 2` on every event while the hooks' internal query timeouts are 3.5 s; the installer writes 4/5 and `tests/install_test.sh` asserts 4/5. The checked-in file is stale.
4. **Failures are invisible by design.** Hooks exit 0 silently on any error. The CLI has no `--help` (falls through to daemon mode, exit 0, no output) and unknown subcommands are silently swallowed (`lattice frobnicate` → exit 0, empty, boots a daemon). Nine orphaned `--stdio` proxy processes have accumulated with no reaping.
5. **Instruction-surface drift.** `~/.claude/CLAUDE.md` still advertises the removed legacy tool names (`get_context_capsule`, `get_impact_graph`, `search_symbols`, …). Project `CLAUDE.md`, `AGENTS.md`, and both integration READMEs carry dead `/home/pete/cadres` paths; `integrations/claude-code/README.md:27` documents a probe default of 0.2 s (code: 3.0 s).

**Root cause, stated plainly:** Lattice's failure philosophy is "never bother anyone" — hooks eat errors, the CLI exits 0 on nonsense, doctor exists but is not in any loop. So when the environment changed (Linux → macOS migration), every integration died *silently* and stayed dead. The June overhaul fixed the tool surface but built no closed loop between "Lattice believes it is installed" and "an agent actually receives value." Without that loop, this spec's fixes will rot exactly the same way.

### Tier 2 — When it does work, the output is not worth the call

6. **The `context` overview is unreadable.** `build_subsystem_overview` (`daemon/crates/lattice-core/src/intelligence/agent.rs:6371`) concatenates the truncated query, file basenames, and symbol names: real observed output was `"how does indexing work: main and mcp. start build_workspace_runtime. test working memory tool tests. memory in-review branch ..."`. This is template word salad, not a summary. Repowise's equivalent (`get_answer`/`get_context`) serves prose from an auto-generated wiki with citations.
7. **The default render doubles the token cost.** `render: "hybrid"` (the default, `rpc/mcp.rs:6694-6696`) ships a markdown summary **and** the full JSON payload in the same response.
8. **Boilerplate on every response.** `attach_agent_retrieval_guidance` (`rpc/mcp.rs:7724`) injects a multi-sentence `agent_retrieval_contract` ("why not rg…") into every payload — self-promotion billed to the caller.
9. **The 5 s latency cap discards work.** `tokio::time::timeout` around dispatch (`rpc/mcp.rs:889-914`) drops the future and returns a stub `{"partial": true}` with no results — the opposite of the 2026-06-11 spec's "return ranked partial results."
10. **While indexing, Lattice tells agents to use rg.** `indexing_workflow_response` (`rpc/mcp.rs:6353-6380`) returns "retry shortly or use rg for exact literal lookup" — even though the daemon has already published a cached graph snapshot (`main.rs:242-248`) it could answer from. Lattice actively trains agents to build the grep habit during its own cold start.

### Tier 3 — Missing differentiated signal (the Repowise gap)

11. **No git intelligence.** `intelligence/mod.rs` computes "hotspots" and co-change from watcher-observed `edit_count` — zero on every fresh index, blind to all history. Repowise mines commit history (hotspots, co-change pairs, ownership, bus factor, bug-fix density) and credits it as a top adoption driver: behavioral signal that static analysis cannot produce.
12. **Language coverage is narrow and internally inconsistent.** Parsers exist for TS/JS, Python, Rust, Go, Java, Markdown (`parser/mod.rs:15-32`). But the cold-start scan (`runtime_support.rs:378-396`) collects `c/cpp/h/hpp/mdx`, which no parser handles (guaranteed warn-spam + wasted IO on C/C++ repos), and misses `mjs/cjs/pyi`, which the watcher *does* index — so index membership depends on which path saw a file first. Repowise: 18 languages.
13. **Partial-index state is invisible.** `BatchIndexReport.is_partial`/`failures` are computed (`indexer/mod.rs:32-48`) but only logged on the watcher path (`watcher.rs:336-343`); the `status` payload never mentions parse failures. An agent cannot learn that 200 files failed to parse — precisely when it most needs to distrust results.
14. **Memory retrieval is weaker than it looks.** FTS5 search orders by `created_at`, not bm25 (`memory/store.rs:1511-1549`); duplicate detection's "embedding" is a 64-dimension FNV hash bag-of-words (`embeddings/mod.rs:8-27`) applied at a 0.92 cosine threshold to decide supersession; real embeddings require an optional `model.onnx` nobody installs; `rebuild_fts()` runs on every store open (`store.rs:380`); migration errors are swallowed via `let _ =` (`store.rs:228-331`).

### Tier 4 — Ergonomics and measurement defects

15. **Adoption metrics do blocking full-file JSON rewrites** on every tool call under a `std::sync::Mutex` inside async (`adoption_metrics.rs:66-98`), with no day pruning.
16. **The follow-through metric cannot succeed.** It only credits when a `remember` call contains the literal string `"Session edited files:"` (`adoption_metrics.rs:154-165`) — emitted solely by the (dead) Stop hook. 0% follow-through is a measurement artifact, not just an adoption fact.
17. **Schema defects:** `recall.memory_id` and `status.anchor` are declared `{}` (untyped — strict MCP clients may reject); `impact.limit` schema default 8 vs handler default 12 (`mcp.rs:1022`); `context.mode` accepts undocumented `"playbook"`; hidden `_lattice_client`/`_lattice_channel` params are undeclared.
18. **Dead code hazards:** `query/watcher.rs` is an unwired stale copy of the watcher that cannot compile if included; the `indexer/lazy.rs` priority-queue design from the original plan was never wired in.
19. **Single-file saves rebuild the whole graph.** `rebuild_graph()` (`indexer/mod.rs:301-309`) is O(all files) per change batch; the 500 ms debounce and a concurrency-1 coordinator are the only backpressure. Degraded polling mode walks a far smaller exclusion list (`watcher.rs:188-191`) than the real one, stat-ing `dist/`, `.next/`, `coverage/` every 30 s.

## What Repowise gets right that Lattice should adopt

Confirmed from <https://docs.repowise.dev/> and the repo README (2026-08-12):

- **Answer-shaped output.** A documentation layer (auto-generated wiki per module, rebuilt on commit, deterministic without API keys) backs `get_answer`/`get_context` — agents get prose with citations, not ranked symbol-name lists.
- **Git intelligence as a first-class layer** — hotspots, ownership, co-change, bus factor from commit history; feeds ranking, impact, and proactive warnings.
- **Proactive hooks that enrich, with a learning loop.** SessionStart injects index freshness + relevance-ranked standing decisions; PostToolUse enriches reads/edits with hotspot and staleness notices; hooks record whether injected guidance was followed and adjust. Cold start < 500 ms, no network, fail-silent — but visibly verified at install time.
- **Task-shaped batch tools** — one `get_context` call takes multiple targets, cutting agent loops.
- **Measured claims.** A benchmark harness ("32/48 tasks cheaper at parity quality; −36% cost; −49% tool calls") makes token-efficiency a tested property, not a hope. Lattice has budget machinery but no end-to-end measurement of whether calls are *worth making*.
- **Command distillation** — compressing pytest/git output before the agent reads it (61–89% token savings). Orthogonal to indexing; high value per line of code.

Lattice's genuine advantages to preserve: Rust daemon speed (measured warm CLI latency here: ~0.09 s vs Repowise's Python stack), the memory/consolidation subsystem (proposal-audited mutations, scope enforcement, verification jobs — far deeper than Repowise's decision records), and the 8-verb surface (leaner than Repowise's 10 tools; the June consolidation was the right call).

## The plan

Ordering principle: nothing in Tiers 2–4 matters while Tier 1 keeps every channel dead, and none of it *stays* fixed without a verification loop. So: **A) restore and self-verify the channels → B) make responses worth reading → C) add the missing signal → D) make adoption measurable.** Per the no-prerelease-legacy-debt policy (`CLAUDE.md` → "Execution Philosophy"), superseded scripts/configs are replaced, not shimmed.

### Phase A — Restore the channels and make breakage loud (do first)

**A1. One installer, generated configs, doctor-gated.**
- Add `lattice install <claude-code|codex|mcp>` subcommands (Rust, in `cli.rs` — replacing both bash `install.sh` scripts) that: resolve the real binary path on *this* machine, write/merge `.mcp.json` and hook config idempotently (reconcile like the Codex installer, not append like the Claude one), and finish by running the relevant doctor checks — **install fails loudly if the freshly written config doesn't round-trip** (spawn, `initialize`, `tools/list` == 8; run each hook against a fixture payload and assert non-empty output with the daemon up).
- Regenerate `.mcp.json` for `/Users/pete/Cadres/*` and remove the stale Linux one. Kill the nine orphaned `--stdio` proxies; add idle-exit to the proxy (terminate when stdin closes or after N minutes without a client — verify `proxy.rs` handles EOF today; the leak says it doesn't).

**A2. Fix the Claude Code hooks — by deleting the bash duplication.**
- Replace `lattice_hook_call`'s flag ordering; better, drop `--workspace` entirely (the CLI's `detect_workspace_root`, `cli.rs:597`, is strictly better than the bash reimplementation — delete `lattice_detect_workspace`).
- Unify `integrations/claude-code/hooks` and `integrations/codex/hooks` on one shared `common.sh` generation (the Codex one is newer and correct); the packages differ only in output envelope (Claude JSON `additionalContext` vs bare markdown) and payload field extraction.
- The readiness probe must use the **same call shape** as real calls. Add a hook self-test mode (`lattice install claude-code --verify`) that pipes recorded fixture payloads through each hook and asserts expected stdout.
- Fix `.codex/hooks.json` by rerunning the (new) installer; outer timeouts must exceed inner query timeouts, asserted in the installer test.

**A3. Kill silent failure in the CLI.**
- `--help`/`-h`/no-args → real usage text, exit 0. Unknown subcommand → error to stderr, exit 64. Daemon/stdio mode only via explicit `--daemon`/`--stdio`. This removes the fall-through that made `lattice --workspace X status` a silent no-op — the class of bug, not just the instance.

**A4. Put doctor in the loop.**
- SessionStart hook: after the recall/rules calls, if the daemon is unreachable or doctor's config scan finds stale paths/duplicate registrations, inject **one line** (e.g. `lattice: daemon unreachable — run 'lattice doctor'`) instead of pure silence. Once per session, bounded, never blocking. Silent-when-broken is how we got here; one quiet line is the fix that keeps hooks polite *and* observable.
- Extend `doctor` to detect the hook-package faults it missed: flag-order/call-shape verification (execute a real hook with fixture input), outer-vs-inner timeout consistency, orphaned proxy count.

**A5. Fix every instruction surface in the same change.**
- `~/.claude/CLAUDE.md` (global): replace the legacy tool list with the 8 verbs — this file is currently teaching every session to call tools that don't exist.
- Project `CLAUDE.md`, `AGENTS.md`, `integrations/*/README.md`: correct paths (no `/home/pete`), correct defaults, reference `lattice install` + `lattice doctor` as the only setup path.

**Acceptance:** fresh Claude Code session in this repo shows `lattice` connected with 8 tools; `lattice metrics` gains `claude-code | hook` rows after one working session; `bash tests/hooks_test.sh` extended to cover the Claude package and the call-shape regression; `lattice frobnicate` exits nonzero with a message; doctor passes clean.

### Phase B — Make the response worth the tokens

**B1. Real summaries.** Replace `build_subsystem_overview` and siblings (`agent.rs:6371-6850`) with structured prose built from data the graph actually has: what each key file *is* (kind mix, exports, doc headings from the markdown parser), how the key symbols relate (edge kinds between them), and where to start — full sentences, no truncated-basename chains. Add an optional cached **module digest** layer (Repowise's wiki idea): on index epoch change, generate per-subsystem digests — deterministic template first; LLM-polished only if a key is configured, cached in `graph.db`, never on the hot path. `context` answers quote the digest with `file:line` citations.

**B2. Stop paying twice.** Default `render` → `markdown` (summary + compact findings, `file:line` on every item); JSON only on request. Reduce `agent_retrieval_contract` to a single `next_action` line. Delete the metrics HTML comment from markdown output (it's for the metrics pipeline; record server-side instead).

**B3. Honest partials.** Replace the future-dropping 5 s cap with cooperative deadline checks at ranking-stage boundaries: on expiry, return ranked-so-far results with `partial: true` through the normal renderer (with handle + budget metadata). While a shard is indexing, **answer from the published cached snapshot** with a one-line freshness banner; never say "use rg."

**B4. Schema hygiene.** Type `recall.memory_id` and `status.anchor`; align `impact.limit` default; document `"playbook"` or remove it; declare `_lattice_client`/`_lattice_channel` or move them to transport metadata. Update `mcp_schema_tests` to assert schemas ↔ handler defaults programmatically.

**Acceptance:** `lattice context "how does indexing work"` on this repo returns readable prose citing real files (manual bar: a reviewer can answer the question from the summary alone); default-mode response for the same query shrinks vs today's hybrid (record before/after `approx_tokens` in the PR); timeout path test asserts partial results with metadata.

### Phase C — Build the signal that earns adoption

**C1. Git intelligence layer.** New `lattice-core` module mining the last N commits (default 500, `git2`): file/symbol hotspot scores, co-change pairs, ownership + bus factor, bug-fix density (fix-shaped subjects touching the file). Persist in `graph.db` keyed by commit; refresh incrementally on git-state watcher events (already watched: `watcher.rs:547-560`). Feed it into: retrieval ranking (`retrieval_v1`), `impact` (rank dependents by hotspot; list co-change partners the diff is missing — Repowise's most concretely useful pre-merge signal), and the PostToolUse hook (one-line warning when editing a top-decile hotspot). Replace the watcher-`edit_count` pseudo-hotspots in `intelligence/mod.rs` — they are superseded (no-legacy-debt).

**C2. One indexing predicate.** Single `should_index_file` shared by cold-start scan and watcher (fix `runtime_support.rs:378-396`): stop collecting `c/cpp/h/hpp/mdx` until parsers exist; include `mjs/cjs/pyi`. Then close the biggest coverage gaps by need across the Cadres repos (C/C++ and the web-stack languages actually present; each is a contained `parser/*.rs` following the six existing implementations).

**C3. Surface partial-index truth.** Thread `BatchIndexReport` into shard state; `status{scope:"index"}` gains `parse_failures`, `failed_files` (top N), `is_partial`; doctor warns on nonzero. Align the degraded-polling exclusion list with `EXCLUDED_DIRS`.

**C4. Memory retrieval fixes.** Order FTS by bm25; make `rebuild_fts()` incremental (triggers or dirty-flag) instead of every-open; either ship a small quantized embedding model via `lattice install --with-embeddings` or gate supersession decisions on typed-evidence overlap alone (the 64-dim hash fingerprint is not a safe basis for superseding memories); stop swallowing migration errors (versioned migrations table).

**Acceptance:** `impact` on a hot file in this repo shows hotspot/co-change annotations sourced from real history; unit tests for the git miner on a fixture repo; `status` shows injected parse failures in a test; memory search test asserts rank ordering.

### Phase D — Measure honestly, then iterate

**D1. Real follow-through.** Correlate `context`/`impact`-suggested files with subsequent watcher-observed edits within a session window (the event pipeline already sees edits), replacing the `"Session edited files:"` string match. Keep the Stop-hook summary as an additional signal, not the definition.

**D2. Metrics storage.** Append-only JSONL (or a table in `graph.db`) with periodic compaction, replacing the full-file rewrite-per-call; prune beyond 90 days.

**D3. A worth-it benchmark.** Scripted harness (can live under `daemon/benches/` or `tools/`): a fixed task set on this repo (find-the-implementation, blast-radius, diagnose-a-failure), run agent-style with and without Lattice, recording tokens + tool calls + whether the answer cites the right files. This is the Repowise discipline that keeps output quality honest — Phase B's summary bar becomes a regression test instead of a one-time review.

**D4. Delete dead code.** `query/watcher.rs` (unwired, cannot compile if included); either wire `indexer/lazy.rs` into cold start or delete it — the static priority sort (`runtime_support.rs:405-442`) is what actually runs.

**Acceptance:** `lattice metrics` shows nonzero follow-through from a real session; benchmark results recorded in the PR; `cargo test --workspace` green.

## Out of scope

- Multi-repo/workspace federation, dashboards, PR bots (Repowise features with no current Cadres consumer).
- Rewriting retrieval_v1 ranking internals beyond wiring in git signals.
- Command distillation — genuinely attractive (Repowise measures 61–89% savings) but orthogonal; consider as its own spec once adoption is nonzero.

## Success criteria

1. A fresh Claude Code session on this machine: `lattice` MCP connected (8 tools), SessionStart context injected, and `lattice metrics` shows `claude-code` rows in both `hook` and `mcp` channels within a normal working week — measured, not anecdotal.
2. Breakage is loud: a stale path, dead daemon, malformed hook call, or partial index is visible in doctor, in `status`, and (one line) in-session — verified by tests that force each failure.
3. `context` output passes the readability bar and the token cost of a default call is lower than today's hybrid render.
4. `impact` carries history-backed hotspot/co-change signal no grep can produce — the differentiated value the 2026-06-11 spec promised, now actually present.

## Proxy lifecycle correction — September 14, 2026

The idle-exit proposal above is superseded. Tool inactivity does not mean the MCP client has disconnected. Proxies remain alive until client stdin closes; there is no idle timeout setting. See [the lifecycle contract](../architecture/2026-09-14-mcp-proxy-lifetime.md).
