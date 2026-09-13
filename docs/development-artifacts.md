# Development artifacts

This repository produces several kinds of local and CI artifacts. They have
different owners and lifetimes; a cleanup operation must never treat them as
interchangeable.

## Cargo caches and build output

Local development and test profiles disable incremental compilation and retain
line-table debug information. This keeps useful source-line backtraces while
avoiding the large incremental and full-debug artifact accumulation observed
during parallel remediation work. Full debugger information can be requested
explicitly with `CARGO_PROFILE_DEV_DEBUG=2` or `CARGO_PROFILE_TEST_DEBUG=2`;
that opt-in consumes additional disk space. The release profile is unchanged.

GitHub Actions may cache the Cargo registry, Git checkout, and `daemon/target`
to reduce repeat build time. Cache keys include the runner OS, Rust toolchain,
target triple, Cargo profile, feature set, and `daemon/Cargo.lock` hash. A
cache is an optimization only: a miss must produce the same build from a clean
checkout, and a stale cache may be discarded without migration.

The `target` cache is job-scoped by its key. Do not force concurrent local
agents or jobs to share one target directory until lock contention and the
resulting wall-clock and I/O costs have been measured. Keep incremental output
optional: when retained, it must remain behind the same toolchain/target/
profile/features key and the repository's bounded Actions cache eviction and
quota. It must not become a second durable store.

Do not delete user-owned `target` directories, Cargo caches, or Lattice runtime
data as part of CI cleanup. Build cleanup and Lattice-managed cache GC are
separate operations with separate owners.

## Supported checkout deliverables

The supported clean-checkout CI surface is the Rust daemon and CLI, the agent
hook packages and installer, and the storage and efficacy harness contracts.
The existing untracked `extension/` is user-owned work, not a reproducible
tracked deliverable of this remediation. Stale CI jobs that assumed its source
and dependencies were checked in have been removed, including the masked lint
failure. Its local files remain untouched. Supporting an extension later
requires its full source, dependency lockfile, build, tests, and blocking lint
checks in the same change.

## CI artifacts

Cross-platform release jobs upload only the release binary and its SHA-256
sidecar. These are disposable per-job outputs used to inspect or download a
build; they are not a package registry or a source of runtime state. CI does
not upload test scratch directories, workspace `.lattice` state, Cargo
incremental directories, or arbitrary files from the checkout. Retention is
bounded by the workflow's artifact retention setting and may be shortened by
repository policy.

Shell harnesses use private temporary directories and remove them at exit.
The hook harness does not retain fixture state. The worth-it benchmark emits a
JSON report to stdout by default, or to the caller-selected new path supplied
with `--output`; it does not write generated logs or reports under
`docs/plans`. Callers that need a retained report must choose an explicit
artifact directory and upload that file as a job artifact with a bounded
retention period.

## Ignored benchmark outputs

Ignored Rust benchmark tests write generated JSON only below `daemon/target/`.
They are local run artifacts and must not be committed. The Retrieval V1
snapshot test writes
`daemon/target/benchmark-results/retrieval_v1_metrics.json`; the large-repo
hardening suite writes
`daemon/target/benchmark-results/large_repo_results.json`; and event payload
timing tests write individual reports below `daemon/target/event_budget/`.
For example, run the retrieval and large-repo suites from `daemon/` with:

```bash
cargo test -p lattice-core --lib retrieval_v1::benchmark -- --ignored --test-threads=1
cargo test -p lattice-core --lib hardening::large_repo_tests -- --ignored --test-threads=1
```

Checked-in JSON under
`docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/` remains
baseline input and historical evidence. Benchmark producers must never
overwrite it.

The large-repo suite caps generated fixtures at 20 files and event-log
compaction at 1,000 events by default. Its report records the measured fixture
size, which is distinct from the named target size. Set
`LATTICE_FULL_LARGE_REPO_PERF=1` only for an explicit full-scale run; that mode
generates the declared 5,000, 25,000, 100,000, or 250,000 file fixture and
uses 1,000,000 compaction events. A default-capped result does not establish a
250,000-file performance claim.

Some ignored query benchmarks require private local corpora or services. If
`/home/pete/rmm_server` or the referenced product checkout is unavailable,
record the case as unavailable rather than passed; do not download, create, or
substitute private fixture data during verification.

The 90 reviewed historical planning logs were moved intact from `docs/plans`
to `.local-artifacts/historical/2026-05-16-cognitive-workspace-fork-build/logs/`.
The [checksum inventory](reports/2026-09-13-historical-run-log-inventory.json)
records all 376,971,728 preserved bytes. Original tracked versions remain in Git
history. This explicit historical archive is not automatically deleted by build
cleanup or Lattice GC. New efficacy runs use their producer-owned temporary
namespace with a seven-day horizon and bounded inventory/count; only matching
producer markers authorize cleanup.
