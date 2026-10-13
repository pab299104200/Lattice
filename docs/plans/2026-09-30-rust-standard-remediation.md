# Rust standard remediation plan

Standard authority: [Coding Standard — Rust](../Templates/Coding%20Standard%20—%20Rust.md). Shared baseline: [coding.md](../Templates/coding.md). Evidence: audit of the `daemon/` workspace performed 2026-09-30 (382 Rust files, ~240k lines; findings summarized in this document's "Audit baseline" section).

Status: not started. No implementation, verification, release, or efficacy claimed. This is a planning document only.

## Scope and intent

Bring the `daemon/` workspace (`lattice-core`, `lattice-daemon`) into conformance with the Rust coding standard and close the security-relevant documentation and enforcement gaps the audit found. The audit confirmed the substance is strong — durability, the workspace security boundary, IPC credential handling, and async/blocking discipline are well built. The work here is about enforcement, `unsafe` documentation, and a small number of correctness/clarity fixes, not a rewrite.

Nothing in this plan authorizes live-data mutation, shared-service restart, or destructive git operations. All verification uses isolated fixtures. Each task is accepted on verified evidence (clean build/clippy/fmt, passing tests), not on a completion claim.

## Audit baseline (verified by static reading + measurement)

Strengths to preserve (do not regress):
- Durability: WAL + `synchronous=FULL`, atomic `BEGIN IMMEDIATE` publication, monotonic generations, crash-recovery tests.
- `security/workspace.rs`: per-component `openat` + `O_NOFOLLOW` + `O_DIRECTORY`, `Component::Normal`-only, symlinked-ignore rejection, byte caps.
- `transport_credentials.rs`: 256-bit `/dev/urandom` tokens, 0700/0600 + uid/mode/symlink checks, atomic temp→rename→fsync, constant-time compare, fail-closed.
- Concurrency: blocking SQLite/graph/git work consistently inside `spawn_blocking`; connection mutex guards do not span `.await`; bounded via semaphores.
- SQL injection: `format!`-into-SQL only interpolates compile-time constants / fixed internal allow-lists; values always via `params![]`.
- Broken windows: ~0 real `TODO`/`FIXME`, no real `todo!()`, `println!`/`eprintln!` only in legitimate CLI paths.

Findings to remediate (file:line references are audit anchors; confirm at edit time):
- **F1 (P1) — `unsafe` undocumented.** 135 real `unsafe` blocks/impls/fns in production, 9 with a `// SAFETY:` comment. Five undocumented `unsafe impl Send`: `trusted_check_windows.rs:67`, `managed_sqlite.rs:112`, `:157`, `:1021`, `:1044`. FFI concentrated in `managed_sqlite.rs`, `managed_fs.rs`, `trusted_check_windows.rs`.
- **F2 (P1) — CI/tooling enforcement absent.** No `rustfmt.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, `[lints]` table, or crate-root lint attributes. CI runs only `cargo test` + `cargo build`; no clippy, fmt check, `-D warnings`, or advisory scan. `rust-version = "1.89"` vs CI `@stable` = MSRV drift.
- **F3 (P2) — Unbounded watcher channel.** `watcher/mod.rs:116` `mpsc::unbounded_channel()` fed by filesystem events; large `git checkout` can grow memory without bound. Lower-risk unbounded channels: `socket_server.rs:2212`, `rpc/server.rs:53-55`, `proxy.rs:26`.
- **F4 (P2) — Oversized files.** 74 files > 800 lines; 24 > 1500. `rpc/mcp.rs` ~19,965; `intelligence/agent.rs` 8,682; `memory/store.rs` 6,415; `socket_server.rs` 5,473; `cli.rs` 3,661.
- **F5 (P3) — Misleading `chrono` self-alias.** `lattice-core/src/lib.rs:3` `extern crate self as chrono;`; the real `chrono` is not a dependency; the crate defines its own `DateTime`/`Utc`. Misleads readers and collides if `chrono` is ever added.
- **F6 (P3) — Reachable `unwrap()`.** `install_project.rs:107` `path.file_name().unwrap().to_str().unwrap()` panics on a non-UTF-8 filename, while line 148 of the same file handles non-UTF-8 correctly.
- **F7 (unverified) — Dependency advisories.** Could not run `cargo audit`/`cargo deny` during the audit (cargo not on PATH). Status unknown until F2 wires it into CI.

## Task ledger

Ordered so enforcement lands first (making later fixes stick), then the security-critical documentation, then correctness, then the long-tail structural work.

| Task | Finding | Contract / scope | Dependencies | Status | Verification (planned) |
|---|---|---|---|---|---|
| T1 | F2 | Add `rustfmt.toml`, `clippy.toml` (or `[workspace.lints]`), `rust-toolchain.toml` pinned to the MSRV channel. Choose lint policy: deny warnings, warn-level `clippy::unwrap_used`/`expect_used`/`panic` outside tests, `undocumented_unsafe_blocks`. | none | not started | `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets` run locally and succeed or produce a reviewed baseline |
| T2 | F2, F7 | Extend `.github/workflows/ci.yml`: add `fmt --check`, `clippy -D warnings`, and `cargo deny check` (add `deny.toml`) as required jobs on the existing matrix. Align the toolchain action with the pinned MSRV. | T1 | not started | CI run shows the new jobs executing and gating; `cargo deny` advisory/license result recorded |
| T3 | F2 | Resolve the first clippy/fmt pass to green (or an explicitly-justified, config-level baseline). No per-line `#[allow]` without a reason comment. | T1 | not started | `clippy --workspace --all-targets -- -D warnings` clean; `fmt --check` clean |
| T4 | F1 | Add `// SAFETY:` to every production `unsafe` block, starting with the five `unsafe impl Send`, then the FFI modules (`managed_sqlite.rs`, `managed_fs.rs`, `trusted_check_windows.rs`, remaining). Each states the invariant that makes it sound. | T1 (clippy `undocumented_unsafe_blocks` to detect) | not started | `undocumented_unsafe_blocks` lint clean; count of documented SAFETY blocks == count of unsafe blocks |
| T5 | F3 | Replace the watcher's `unbounded_channel` with a bounded channel plus event coalescing/backpressure; prove no event loss and bounded memory under a flood. Evaluate the other three unbounded channels and bound or document each. | none | not started | New test: simulated large-checkout flood keeps queue bounded and loses no indexable event; existing watcher tests pass |
| T6 | F6 | Fix `install_project.rs:107` to handle non-UTF-8 filenames with a typed error, matching the pattern already at line 148. | none | not started | Regression test: install with a non-UTF-8 project filename returns an error, does not panic |
| T7 | F5 | Rename the `chrono` self-alias to an honest internal module (e.g. `crate::time`) and migrate all `use chrono::...` call sites in one change. | none | not started | Workspace builds; no `extern crate self as chrono`; no remaining `use chrono` in-crate; tests pass |
| T8 | F4 | Split the oversized files along coherent boundaries, worst-first (`rpc/mcp.rs`, then `intelligence/agent.rs`, `memory/store.rs`, `socket_server.rs`, `cli.rs`, …). Behavior-preserving; no public API change without note. Incremental — one file per change. | T1–T3 (so each split lands under enforcement) | not started | After each split: workspace builds, clippy/fmt clean, full `cargo test --workspace` green; file line counts recorded |

## Sequencing notes

- T1→T2→T3 are the critical path: once CI enforces fmt/clippy/`-D warnings`/deny, every later task lands under enforcement and cannot silently regress.
- T4 is the highest-value security task and is best done after T1 so the `undocumented_unsafe_blocks` lint can enumerate the exact set rather than relying on manual grep.
- T5–T7 are independent of each other and can proceed in parallel once T1 lands.
- T8 is long-tail and incremental; it should trail the enforcement work so each split is validated by the same gate.

## Out of scope

- No change to the durability model, the workspace boundary, or the credential scheme — these are verified strengths.
- No multi-user/team or Windows-credential expansion.
- No live-data migration or operator mutation.

## Done means done (per task)

- [ ] `cargo build --workspace` clean with `-D warnings`.
- [ ] `cargo clippy --workspace --all-targets` clean; `cargo fmt --all -- --check` clean.
- [ ] `cargo test --workspace` green on the supported targets (or CI matrix confirms it).
- [ ] For T4: no undocumented `unsafe`; for T5: bounded-channel behavior proven by test; for T6/T7: regression test added.
- [ ] This ledger updated with the verifying evidence; no acceptance on a completion claim alone.

When every task is complete and verified, move this plan to `docs/plans/archive/`.
