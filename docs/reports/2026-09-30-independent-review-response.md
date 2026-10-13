# Response to the independent "NOT READY" review

An independent review flagged three blockers and two softer themes. Each claim was verified against the actual code before any change. Four were real and are now fixed (each RED-first, then verified); one blocker was already correctly handled and needed no change. Full `cargo test --workspace -- --test-threads=4` is green (1310 core + 391 daemon lib + 685 daemon bin + 8 CLI, 0 failures); `cargo fmt --check` clean; release build clean.

Toolchain: cargo 1.99 under `~/.cargo` (`. "$HOME/.cargo/env"`). Discipline: prove the failure with a RED test first, then fix; verify with cargo test and fmt.

---

## Blocker #1 — Untrusted input can crash the daemon (CONFIRMED, fixed)

**Finding (accurate).** Every language's reference walker `collect_call_identifiers` (Rust, Python, TypeScript/JavaScript, Go, Java), plus Python's `collect_string_model_refs`, recursed once per AST level with no depth guard. Lattice parses arbitrary repository content, so a crafted file (a few KB of deeply-nested calls) overflowed the thread stack and aborted the process.

**Fix.** Added `MAX_REFERENCE_WALK_DEPTH = 512` in `parser/mod.rs` and threaded a `depth` argument through every reference walker; each returns early at the cap. Reference extraction is best-effort (the symbol graph is unaffected), so truncating the deepest edges is a safe, non-fatal degradation. The cap is far above any realistic nesting and far below the stack budget.

**Proof.** New test `parser::tests::deeply_nested_input_does_not_overflow_the_stack` parses an 8000-deep nested expression for all six languages on a deliberately small (512 KiB) stack. With the cap temporarily removed (`usize::MAX`), it aborted with `SIGABRT: stack overflow`. With the cap restored it passes.

Files: `parser/mod.rs`, `parser/rust_lang.rs`, `parser/python.rs`, `parser/typescript.rs`, `parser/go_lang.rs`, `parser/java.rs`, `parser/tests.rs`.

---

## Blocker #2 — A crash can silently corrupt the search index (CONFIRMED, fixed)

**Finding (accurate).** The vector index `flush()` had two write paths. The managed/publication path used the durable `replace_managed` helper (temp → fsync → atomic rename → dir fsync). The non-publication `else` branch used plain `fs::write` to the live index and sidecar — no temp, no rename, no fsync — so a crash mid-write left a torn index or an index/sidecar pair out of sync.

**Fix.** Added `atomic_write_plain` (the plain-path counterpart to `replace_managed`): write to a sibling temp file, `sync_all`, `rename` over the destination, then fsync the parent directory. The flush `else` branch now routes both writes through it. SQLite remains the source of truth, and the load path already validates `index_sha256` and rebuilds on mismatch, so an index/sidecar skew is additionally self-healing.

**Proof.** New test `atomic_write_plain_replaces_by_rename_not_in_place_truncation`: a reader that opened the destination before the write still reads the complete old bytes afterward (possible only with rename-replace, impossible with in-place truncation), and no `.tmp` residue remains. With the helper temporarily reverted to a plain `fs::write`, the reader saw the new bytes and the test failed; restored, it passes. Plus `plain_path_flush_round_trips_through_the_atomic_write`.

File: `storage/usearch_index.rs`.

---

## Blocker #3 — Model download trusts the network (NOT ACCURATE — already handled, no change)

**Finding (incorrect for this code).** The review stated the ONNX model is fetched with no checksum. In fact `embeddings/mod.rs` pins `MODEL_SHA256`/`TOKENIZER_SHA256` and uses them: `install_embedding_model_at` calls `verify_asset` (streaming SHA-256 vs the pinned constant) immediately after each `curl` download, re-validates the whole bundle before the atomic activation rename, and `verified_shared_embedding_model_path` re-checks the checksums on every load — returning `None` (lexical fallback) on any mismatch. The constants are live, not dead. `curl` itself does not checksum, but the surrounding code does, before the model is ever used.

**Action.** None. Verified correct and left unchanged rather than "fixing" a non-issue.

---

## Soft theme #1 — Lifecycle log swallows every write error (CONFIRMED, fixed)

**Finding (accurate).** `lifecycle_log.rs::log_event` discarded its I/O results (`let _ = writeln!(...)`, `let Ok(file) = open(...) else { return }`), so a failed audit write dropped silently — the start/crash trail could go blank with no signal.

**Fix.** Extracted the write into `append_event_line(path, line) -> io::Result<()>` so failures are now a value, not a discarded result. `log_event` surfaces the first failure of each kind via `tracing::warn`, latched by a `WRITE_FAILURE_WARNED` `AtomicBool` so a persistent fault (full disk, read-only mount) warns once instead of flooding the real log. It remains best-effort and never crashes the daemon.

**Proof.** `append_event_line_surfaces_write_failures` (happy path lands the line; an unwritable path returns `Err` instead of swallowing) and `write_failure_warning_latches_after_the_first`. RED is structural: the old code discarded the result, so a failure was unobservable by construction.

File: `lifecycle_log.rs`.

---

## Soft theme #2 — Embedding subprocess has no read timeout (CONFIRMED, fixed)

**Finding (accurate).** The R2 subprocess provider read the child's response with a blocking `BufReader::read_line` and no deadline, under the `child` mutex. A wedged worker (alive but never replying) blocked that call — and every other caller waiting on the mutex — forever, instead of failing over to lexical.

**Fix.** The child's stdout is now drained by a dedicated reader thread that forwards lines over an `mpsc` channel; the embed call uses `recv_timeout` (default 30s, overridable via `LATTICE_EMBEDDING_SUBPROCESS_TIMEOUT_SECS`). On timeout the child is killed and dropped (next call respawns) and an error is returned, which the existing semantic circuit breaker turns into lexical-only retrieval.

**Proof.** `a_wedged_worker_times_out_instead_of_hanging`: a fake worker reads the request then sleeps forever; with a 500ms timeout the embed call returns `Err` promptly. The test runs on a watchdog thread with a 10s `recv_timeout`, so a regression that reintroduced an unbounded read would fail the test by timing out rather than hang the suite. Plus `response_timeout_env_override_is_respected`.

File: `embedding_subprocess.rs`.

---

## Verification summary

- `cargo test --workspace -- --test-threads=4`: green — 1310 + 391 + 685 + 8, 0 failures.
- `cargo fmt --check`: clean.
- `cargo build --release`: clean (~46s).
- New/changed tests: 1 parser overflow test; 2 usearch atomic-write tests; 3 lifecycle-log tests; 2 subprocess timeout/override tests (+ the existing R2 suite still green). All RED-proven where a RED state could be demonstrated without leaking a blocked thread.
