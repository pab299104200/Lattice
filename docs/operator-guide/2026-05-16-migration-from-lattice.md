# Migration From Lattice

This guide describes how to move an existing Lattice workspace into the cognitive workspace successor. It is driven by [## Documentation Requirements](../plans/2026-05-16-cognitive-workspace-fork-plan.md#documentation-requirements), [## Fork Strategy](../plans/2026-05-16-cognitive-workspace-fork-plan.md#fork-strategy), and [Storage Migration Policy](../architecture/2026-05-16-storage-migration-policy.md#policy).

## Overview

Migration preserves useful Lattice assets while replacing memory retrieval, event capture, ranking, and review semantics with the successor contracts. The storage migration order, rollback expectations, compaction snapshot policy, and compatibility constraints are authoritative in [## Migration order](../architecture/2026-05-16-storage-migration-policy.md#migration-order) and [## Rollback](../architecture/2026-05-16-storage-migration-policy.md#rollback).

## Migration steps

1. Read [Storage Migration Policy](../architecture/2026-05-16-storage-migration-policy.md#policy) and confirm the target version.
2. Stop active Lattice daemon processes.
3. Back up the existing workspace `.lattice/` storage directory and record the active daemon binary path.
4. Build the successor daemon with `cd daemon && cargo build --release`.
5. Run pre-flight checks from [## Pre-flight checks](#pre-flight-checks).
6. Run the storage migration command or startup migration path defined by the current daemon release.
7. Reindex the workspace and confirm `index_status` reports a healthy graph.
8. Verify MCP compatibility with representative `context`, `prepare_change`, and `recall` calls.
9. Inspect migrated memory, stale state, proposals, event trace, indexing health, and graph health through MCP tools.
10. Capture post-migration verification evidence in the operator change record.

## Pre-flight checks

- Workspace path is the intended repository.
- Current branch is recorded.
- Existing `.lattice/` storage is backed up.
- SQLite files are not open by another daemon process.
- Disk space can hold the original store, migrated store, payload spillover, and a compaction snapshot.
- MCP clients point at the daemon binary built for the migration.
- Existing MCP clients can tolerate documented shims from [MCP Compatibility Policy](../architecture/2026-05-16-mcp-compatibility-policy.md#legacy-aliases-and-deadlines).

## Data preservation guarantees

Migration must preserve source workspace files, stable identities where possible, legacy memory content, existing compatibility aliases, and audit-relevant event or workflow history that can be mapped safely. New event and memory graph fields may be added, but existing parseable client responses must remain compatible through the documented phase window.

When exact legacy semantics cannot be proven, the migrated memory should be marked `unverified` or `in_review` with provenance rather than upgraded to trusted guidance.

## Rollback procedure

Rollback follows [## Rollback](../architecture/2026-05-16-storage-migration-policy.md#rollback):

1. Stop the successor daemon.
2. Move the migrated store aside without deleting it.
3. Restore the backed-up `.lattice/` storage directory.
4. Restore the prior daemon binary path in MCP client configuration if required.
5. Start the prior daemon and confirm legacy MCP calls respond.
6. Preserve logs and migration diagnostics for analysis.

Do not merge migrated and rolled-back stores by hand. Treat the restored backup as the active source and the migrated store as evidence.

## Post-migration verification

Run these checks after migration:

- `index_status` reports expected workspace, branch, file count, and language mix.
- `get_repo_playbook` returns a bounded summary.
- `context` returns stable handles and suggested expansion.
- `recall` returns migrated memory with scope and verification labels.
- `recall` with `mode=verify` explains a migrated memory state.
- `get_event_trace` returns scoped events for a recent task or session.
- Memory, event, metrics, and health MCP responses do not show unsupported data as authoritative.
