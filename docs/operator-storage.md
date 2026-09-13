# Repository storage operations

Lattice exposes repository-scoped storage inspection and maintenance through
`lattice storage`. Every command resolves the canonical Git common directory
and verifies the repository ID and canonical home recorded by the storage
registry. A directory name, remote URL, or caller-supplied ID is not ownership
evidence.

## Inventory and cache maintenance

`lattice storage status --workspace <checkout>` reports logical and allocated
derived-cache bytes, SQLite WAL bytes and free pages, active and retained
checkouts, reclaimable bytes, and bounded lists of unknown and historical
artifacts. It reads the last published checkout accounting snapshot and current
indexed shared-object totals. It never advances maintenance or scans repository
source. Missing, stale, or partial accounting is explicitly incomplete; these
values must not be treated as authoritative zero usage.

Cache deletion is a typed two-step operation:

```text
lattice storage cache plan --workspace <checkout> --output plan.json
lattice storage cache apply --workspace <checkout> --plan plan.json
```

Planning advances one bounded accounting page and may report that another
attempt is needed to finish the inventory. It records accounting metadata but
preserves cache and knowledge payloads. The plan records repository ownership,
canonical home, policy, inventory, exact candidates, and a SHA-256 fingerprint.
Apply rejects edited, foreign, oversized, or stale plans. It then takes the
repository maintenance lock and rechecks each OS checkout lease, idle age, and
positive derived-file allowlist. Unknown files and historical layouts remain in
place. GC uses its durable rename journal and same-filesystem trash recovery.

## Knowledge backup and restore

Backups and restores are offline operations. Stop every Lattice process using
any worktree of the repository, then run:

```text
lattice storage backup --workspace <checkout> --destination <new-directory>
lattice storage restore --workspace <checkout> --backup <directory> \
  --replace-existing --confirm-offline
```

The command refuses active checkout leases, a busy memory-owner lock, symlinks,
and destination overwrite. Backup stages a coherent SQLite `VACUUM INTO` inside
the pinned repository home, copies from that opened descriptor into a newly
created pinned destination directory, checks database integrity, and writes a
manifest containing repository identity, creation time, memory count, restore
floor, and database SHA-256. Cleanup acts only through the pinned descriptors;
a destination renamed during publication is rejected without following its
replacement, and failure cleanup removes the destination name only when it
still identifies the directory created by that backup attempt.

Restore requires the current authority because it is the source of deletion
proof. It verifies the manifest, checksum, integrity, repository identity, and
memory count. It rejects a backup older than the current durable restore floor
and rejects any memory ID covered by a current deletion receipt. Current
receipts and the maximum restore floor are copied into the staged database from
the same backup descriptor whose bytes were hashed. Before replacement, restore
checkpoints the current authority, obtains an exclusive SQLite fence, and
removes the closed generation's WAL and shared-memory sidecars. A durable
`staging` / `prepared` / `old_moved` / `committed` journal makes staging and the two same-directory
renames idempotently recoverable. Restart either finishes a verified staged
authority or restores the rollback; it never serves a half-replaced database.
The journal binds an interrupted operation to the SHA-256 of its source
manifest. Recovery rejects a different backup request before changing the
filesystem; retry with the original backup first. An in-flight journal created
before source-manifest binding also fails closed because its source identity
cannot be reconstructed safely.
Overwrite and the all-processes-stopped assertion are both explicit because
older readers cannot be discovered through current lock contracts.

## Repository relocation

Moving a checkout can change the path-derived repository ID. Lattice does not
guess that two paths or matching remotes identify the same repository. The
offline relocation operation must be given the old identity recorded inside
the moved storage home and the new identity resolved from local Git metadata.
It requires an explicit assertion that every Lattice process using the
repository is stopped because binaries predating this contract cannot be
fenced by a new lock.

Relocation holds the repository maintenance lock, every registered checkout
lease, and the memory-owner lock. It journals `prepared`,
`authority_migrated`, and `completed` states, transactionally migrates proven
memory authorities while preserving memory IDs, scopes, evidence and links,
then changes the storage ownership row and records the old-to-new mapping.
Restarting the exact request resumes idempotently. The recorded mapping is the
only relocation resolver input; remote aliases and directory similarity are
never accepted. Resume validates every journal authority field and its phase;
a row for a different transfer stops recovery. Completion also requires the
single recorded ownership row to transition from the exact old ID and home.

Run the command from the repository at its new local path. The old ID must be
copied from the ownership record or a prior `storage status` result; it is not
inferred from a remote or directory name:

```text
lattice storage relocate --workspace <moved-checkout> \
  --from-repository-id <recorded-old-repo-id> --confirm-offline
```

The command resolves the new ID from that checkout's local Git common
directory, then runs relocation before opening the normal ownership-checked
operator. Repeating the exact command resumes the journal and reports the
recorded relocation rather than creating an alias.

## Historical layout retirement

Historical flat checkout cache files are retired only through an offline
plan/apply operation. Planning first verifies a repository-matching knowledge
backup and records its manifest digest. Each candidate records the exact
checkout-relative path, device and inode, length, and SHA-256. Apply requires
the explicit all-processes-stopped assertion, rechecks the backup and every
candidate, and moves it through descriptor-relative filesystem handles into a
repository-owned retirement trash directory. The `planned` / `moved` /
`deleted` journal makes both the rename and deletion crash-resumable without
treating an unexplained missing source as proof of deletion.

Only the fixed derived-cache allowlist is eligible. Unknown files, directories,
symlinks, snapshots, memory, evidence, and telemetry are preserved. A changed
or missing candidate without a completed journal entry rejects the operation
instead of widening deletion authority.

```text
lattice storage historical plan --workspace <checkout> \
  --backup <verified-backup-directory> --output retirement-plan.json
lattice storage historical apply --workspace <checkout> \
  --plan retirement-plan.json --confirm-offline
```

Both plan outputs use create-new writes, so neither cache nor retirement plans
silently overwrite an existing review artifact.

Per-user and per-cache-class budgets are not part of this repository operator
contract. The current cache policy is repository-wide.

Relocation checks the current Git common directory and derived repository ID
against the new storage home. The former identity must hash to the registered
former Git location, and that old Git directory must no longer exist. Migration
aliases derive only from these recorded paths; callers cannot supply an
arbitrary alias set. Normal, bare, and separate-Git layouts use the same
checks. The operator's old-ID selection and offline confirmation attest that
the repository was moved. The stored proof hash detects an inconsistent or
partially replayed request; it is not cryptographic proof of Git-history
continuity.

Operator database connections, maintenance locks, and memory-owner locks resolve
beneath pinned directory handles. Backup staging and restored-database checks
use the same pinned authority. Replacing a directory pathname during an open
operation cannot redirect its SQLite database, sidecars, or filesystem cleanup
to the replacement directory. This does not authorize a repository relocation;
subsequent fresh opens still require the registered canonical home.
