# Adoption follow-through metrics

`lattice metrics` measures follow-through only when Lattice observes a real
filesystem edit after a `context` or `impact` response. It does not treat
memory text, workflow summaries, or caller-provided prose as edit evidence.

## Correlation contract

Each runtime has one generated session identity. Tool calls record the files
that the successful response actually returned. The workspace watcher records
subsequent indexable-file changes with that same runtime identity.

An edit credits at most one uncredited prior `context` or `impact` call when:

- it is in the same runtime session;
- the changed path was returned by that call; and
- it occurs within one hour of the response.

This intentionally produces under-counting when Lattice cannot establish that
relationship. It must not credit an edit from another session, an unrelated
file, or text claiming that a file changed.

## Storage and retention

Records append to `.lattice/adoption_metrics.jsonl`. Metrics reads replay the
log deterministically; the log is compacted at most once per process per day,
retaining the most recent 90 days. This avoids a blocking full-ledger rewrite
on every MCP call while bounding long-lived workspaces.
