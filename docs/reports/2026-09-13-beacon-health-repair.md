# Beacon health availability repair

## Verified defects

Beacon's initial live index was ready: 2,290 indexed files, 26,483 nodes,
71,684 edges, no parse failures, and `is_partial=false`. Health reported graph
available, Git unavailable (zero covered files), complexity unavailable (zero),
and test proximity available for 1,454 files (634 per mille of all indexed files).

The persisted Git generation contained 500 included commits, with no invalid
commit/path observations or path overflow. Four commits exceeded the co-change
width bound. The aggregate degraded flag incorrectly suppressed all per-file
history. Complexity had no initial producer request after a clean or warm
startup; it depended on a later watcher edit.

## Implemented behavior

- Graph startup publication requests a full health pass, including warm startup.
- File, symbol and co-change history have independent completeness gates, all
  subject to freshness. Broad commits no longer suppress intact file history.
- MCP health status and rendered summaries disclose the applicable family.
- Documentation without executable control flow does not disable complexity.
  Other unavailable code measurements remain degraded, without invented zeros.
- Health production retries failures with bounded backoff. Sources use the
  workspace read boundary. Complexity batches publish through an atomic active
  generation swap; failures preserve prior persisted facts. Per-repository
  cache files prevent combined-root active-pointer collisions. Generation
  retention is bounded; failed unpublished generations are discarded.
- Status explains coverage denominators and test-proximity scope. These facts
  describe graph proximity, not executed-test coverage or a risk guarantee.

## Verification

`cargo test --workspace -- --test-threads=2` passed: **2,270 passed, zero failed,
39 ignored**. Build/test debug information and incremental compilation were
restricted to limit disk use. Existing ignored tests were not represented as
executed acceptance.

`python3 daemon/tests/health_startup_smoke.py daemon/target/debug/lattice`
passed against a private Git fixture and real isolated daemon. Both cold and
warm starts returned overall availability `available`, with nonzero graph,
Git and complexity coverage, without a file edit. The smoke runner terminated
only its own processes.

The focused Git tests also passed. Independent Sol review identified a misleading
aggregate Markdown suppression message; the corrected family rendering has a
regression. Runtime/store regressions cover retry intent, durable preservation
across source failures, repository isolation, deletion, generation retention,
unpublished cleanup and active-generation protection.

The product deployment configurations are separate from this repair and live in
`/Users/pete/Cadres/shared/scripts/lab-products`. Their controller schemas were
validated. Product adapter/host setup remains required before registration;
creating configuration files did not deploy applications.

## Release activation gate

`cargo build --release` passed. The same cold/warm smoke test passed using
`daemon/target/release/lattice`. The existing PATH entry
`/Users/pete/.local/bin/lattice` points to that binary.

The initial restart attempt was rejected by automatic approval review because
concrete shared-service authorization was required; no process was stopped in
that attempt. The operator explicitly approved restarting Lattice on 2026-09-14.
The verified old daemon PID 40469 was stopped and the release launched as PID
57458. No application service was restarted.

## Live Beacon verification — 2026-09-14

- Index ready: 2,290 files, 26,483 nodes, 71,684 edges; indexing work idle.
- Semantic retrieval: available.
- Git file and symbol history: available across 500 commits. Co-change remains
  degraded because four broad commits exceeded its bound.
- Complexity: 1,907 covered files, consisting of 1,386 clean measurements and
  521 marked `partial_parse`. Another 383 files have no executable control flow.
  These counts were verified against the active repository-specific SQLite
  generation using a read-only connection.
- Test proximity: available for 1,454 eligible files.
- Overall health remains **degraded**, not fully available, because of partial
  complexity parses. `incomplete_analysis` is empty; the former complete absence
  of Git and complexity inputs is resolved.

The live health report includes 8,373 scored paths because Git contributes
historical paths beyond the 2,290 current graph files. Its coverage percentages
therefore use the scored-path union; the new `coverage_basis.denominator` label
saying `all_indexed_files` is inaccurate for this case and requires correction.
Use the absolute family counts above rather than interpreting those percentages
as coverage of current source. This remaining reporting defect and the partial
parse flags were not resolved by restarting the daemon.
