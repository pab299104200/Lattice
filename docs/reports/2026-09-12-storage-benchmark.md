# Storage lifecycle benchmark

## Final compiled build — September 13

Binary SHA256 `cfb306cc811ef03c0e87ef4f9deffb440e59113f057a2c8ef6a16ce3784a2b3b` completed 100 real ONNX worktrees and
passed all 18 unchanged fixed storage gates. Raw result:
`/tmp/lattice-storage-benchmark-sep13-canonical-freeze.json`; execution log:
`/tmp/lattice-storage-benchmark-sep13-canonical-freeze.log`; audit:
`/private/var/folders/q2/g_4_z6w95r7chfc5dx6fhnz40000gn/T/lattice-storage-benchmark-ynif9wgp`. No ResourceWarnings were reported.

| Checkouts | Allocated MiB | Cold indexing mean, seconds | Edit readiness mean, seconds |
|---:|---:|---:|---:|
| 1 | 15.156 | 3.751 | 4.068 |
| 5 | 40.055 | 1.624 | 2.938 |
| 20 | 67.953 | 1.241 | 2.619 |

At 20 worktrees, parsed-byte reuse was 92.735% and embedding-byte reuse 92.622%.
At 100, allocated storage was 169.762 MiB.
The 200 ready public queries measured 29.23 ms p50 and 41.87 ms p95.
CPU was 58.23 s, physical writes 2,259,898,368 bytes,
and peak physical footprint 471,794,648 bytes. Sampled WAL
peak was 38,967,440 bytes across 2214 100 ms samples with zero errors.

GC removed 97 unleased bundles and retained 3 active checkouts. Independent
allocated bytes fell from 178,008,064 to 68,325,376,
reclaiming **104.602 MiB (109,682,688 bytes)**.
Plan and apply reported the same delta. Filesystem deltas also include private
daemon writes and checkpoint effects. These fixture results do not establish an
overall speedup or agent efficacy. Full supported Rust verification passed 2,248
checks; private smoke v4 and seven-fixture public delivery preflight passed on
this same binary. Remote CI/native Windows and real long-plan usefulness remain
unverified. The user will deploy and test with the Beacon agent; no shared
service was restarted here.

## Earlier measured release 1d1d7f7


The preceding measured release (September 13) completed all 100 fixture worktrees
and passed all 18 fixed storage checks. This release preceded the final canonical memory-render correction and is
historical measurement evidence rather than current acceptance. It is a synthetic storage measurement; it does not
establish agent efficacy or total production disk savings.

- Binary SHA-256: `1d1d7f712b6d3206b744bd80b6965c1e596e8e1273022816362ea6dfac7fc601`.
- Raw result: `/tmp/lattice-storage-benchmark-sep13-empty-final.json`.
- Execution log: `/tmp/lattice-storage-benchmark-sep13-empty-final.log`; it prints the output path and contains no `ResourceWarning`.
- Audit fixture: `/private/var/folders/q2/g_4_z6w95r7chfc5dx6fhnz40000gn/T/lattice-storage-benchmark-z_h81ojc`.
- Policy: `sep13-macos-40-files-100-worktrees-v1`, unchanged from the fixed
  acceptance policy.

This measurement supplies the storage evidence for R3 (checkout ownership,
bounded accounting, leases, and reference-aware reclamation) and R4 (shared
immutable parse/body/embedding objects and graph deltas) in the remediation
plan. It does not close the plan's remaining agent-trial, CI, or native-Windows
gates.

## Preceding release measurements

The fixture creates forty Rust files and 100 worktrees, indexes each checkout,
edits one file per checkout, and waits for persisted changed-content membership
and an exact public symbol/file query. Real ONNX inference produces 80 embedding
memberships per checkout. The edited file and function embedding object keys
must change; unrelated membership changes cannot pass.

| Checkouts | Allocated MiB | Mean cold indexing, seconds | Mean edit to public readiness, seconds | Eligible parsed file reuse | Eligible parsed byte reuse |
|---:|---:|---:|---:|---:|---:|
| 1 | 15.445 | 3.003 | 2.972 | 0.000% | 0.000% |
| 5 | 36.504 | 2.422 | 2.548 | 78.000% | 78.108% |
| 20 | 60.359 | 1.813 | 1.263 | 92.625% | 92.735% |

The corresponding embedding membership reuse is 0.000%, 78.000%, and 92.625%;
embedding byte reuse is 0.000%, 77.996%, and 92.622%. At 100 worktrees, parsed
eligible file and byte reuse are 96.525% and 96.628%; embedding eligible file and
byte reuse are 96.525% and 96.523%. These percentages compare eligible object
memberships or referenced payload bytes. They do not represent reductions in
total filesystem size. Each checkout still materializes its resolved graph and
search accelerators locally.

The fresh cold and edit wall-time means are slower than the prior accepted
measurement; this report makes no overall speedup claim and does not infer a
cause. The 200 ready public queries measured **39.24 ms p50 / 53.96 ms p95**.
The query definition is `ready context --mode focused --json` after persisted
membership and changed-content checks. At 100 worktrees, allocated storage
before removal was **168.629 MiB** (`176,820,224` bytes).

Daemon cumulative CPU was **54.04 seconds**, physical writes were
**1,532,227,584 bytes**, and peak physical footprint was **472,515,520 bytes**,
measured using macOS `proc_pid_rusage_v4`. The aggregate WAL high-water mark was
**34.765 MiB** (`36,454,240` bytes), sampled 1,541 times at 100 ms intervals
with zero sampling errors. This is a sampled peak, not an exact continuous
maximum. Child resource accounting includes the isolated daemon, CLI calls, and
fixture Git operations; maximum child RSS is not a simultaneous process-tree
total.

## Reclamation

Public storage plan and apply both exited successfully. Apply deleted 97
unleased checkout bundles; three active leased checkouts were retained. The
independent allocated-byte measurement fell from **170.629 MiB**
(`178,917,376` bytes) to **66.051 MiB** (`69,259,264` bytes):
**104.578 MiB** (`109,658,112` bytes) reclaimed. The plan estimate and apply
reported release were each **104.578 MiB** (`109,658,112` bytes), exactly equal
to the independent net allocated-byte delta. Filesystem measurement includes
concurrent private-daemon writes and SQLite checkpoint effects; it is independent
of apply's reported deletion count. The independent logical-byte delta was
**94.998 MiB** (`99,613,112` bytes), which is a separate accounting measure.

The fixed gates require exactly 100 completed worktrees; proven nonzero real
embeddings; at least 90% parsed and embedding reuse by both membership and
payload bytes at 20 worktrees; ready-query p95 at most 100 ms; at most 128/256
MiB allocated at 20/100 worktrees; daemon CPU at most 120 seconds, peak footprint
at most 1 GiB, physical writes at most 3 GiB; sampled WAL at most 128 MiB with
valid samples; successful plan/apply; and at least 64 MiB independently
reclaimed. All 18 checks passed. These are fixture regression bounds, not
portable product guarantees.

## Verification and limits

The fresh artifact records real ONNX embedding objects and memberships, and its
execution log contains no `ResourceWarning`. The full Rust workspace verification passed **2,236 tests**
with **0 failures** and **39 ignored** tests. The churn benchmark does not inject every
corruption, crash, disk-full, traversal, held-reader, or recovery failure; those
require focused and integration regressions. Native Windows runtime and remote
CI execution remain separate unrun gates.

Reproduce with an installed, checksum-verified embedding model and compatible
ONNX runtime:

```sh
ORT_DYLIB_PATH=/path/to/libonnxruntime.dylib \
python3 -W error::ResourceWarning tools/lattice-storage-benchmark.py \
  --binary daemon/target/release/lattice --with-embeddings \
  --output /tmp/lattice-storage-benchmark-new.json --keep-fixture
```

The harness owns a private Git fixture, HOME/XDG directories, and daemon
endpoint; it clears inherited organization-memory configuration and terminates
only its own child daemon. The checksum-verified model is read from the existing
model cache. No live knowledge or shared daemon was modified by this measurement.

## Earlier measurements

The immediately previous September 13 measurement is preserved as historical
evidence from `/tmp/lattice-storage-benchmark-sep13-expansion-final.json` on
binary SHA `1080d27b4d11b2706922e1cceb6816f705f38ee8d190d81428ea97d5b8ae527b`.
It recorded 100-worktree allocated storage of **168.758 MiB**
(`176,955,392` bytes), **28.10 ms p50 / 32.85 ms p95** ready-query latency,
and 1/5/20 cold means of 2.618, 2.246, and 2.836 seconds. Its edit means were
1.861, 1.983, and 1.811 seconds. The current release uses a different binary
and artifact; these measurements remain historical and do not establish an
overall speedup.

The earlier accepted September 13 measurement is preserved here as historical
evidence from `/tmp/lattice-storage-benchmark-sep13-measured.json` on binary
SHA `e03f8b3ee04d565cb9886b436f6213afccdc156368f48372c5c29a11ca0f5ef0`:

| Checkouts | Mean cold indexing, seconds | Mean edit to public readiness, seconds |
|---:|---:|---:|
| 1 | 2.263 | 0.633 |
| 5 | 0.819 | 0.646 |
| 20 | 0.579 | 0.660 |

That artifact recorded **168.371 MiB** allocated at 100 worktrees
(`176,549,888` bytes), **23.25 ms p50 / 28.64 ms p95** ready-query latency,
and an independently measured GC allocated-byte delta of **104.578 MiB**
(`109,658,112` bytes). The fresh run's cold and edit means above are slower than
these historical values. The two runs have different frozen binaries and
measurement artifacts; these values do not support an overall speedup claim.

The September 12 `/tmp/lattice-storage-benchmark-results.json` run completed
100 worktrees but timed initial requests before proving bootstrap completion,
lacked embedding and byte-weighted reuse measurements, and did not independently
measure GC's physical delta. It is historical evidence only.

The earlier September 13 instrumented artifact
`/tmp/lattice-storage-benchmark-sep13-instrumented.json` was produced on SHA
`6c3b0c4acaf83a30f515e0d1c70fe9cfeda40ded2d5d56dbeb99043c4cb1d9a9` and supplied
the fixed regression baseline. Its GC number was apply-reported rather than
independently measured, and it predates the stricter edited-file/function object
checks. The fresh frozen-release artifact above supersedes those measurements
for current acceptance evidence.
