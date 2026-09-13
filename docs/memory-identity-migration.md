# Repository memory identity migration

Lattice derives repository authority from the canonical Git common directory. During startup it inventories only local identities that Git proves belong to that common directory: the common directory, the active Git directory, and paths returned by `git worktree list --porcelain`. Remote URLs and directory-name similarity are never ownership evidence.

While holding the repository memory initialization lease, Lattice replaces those historical path authorities with the current opaque repository ID in one immediate SQLite transaction. It updates memories, consolidation and verification jobs, working-memory checkpoints, session-digest deliveries, and capture tombstones. Stable memory and dependent IDs, session and branch scope, provenance, evidence, and links remain unchanged. Exact authority strings inside checkpoint and consolidation state are rewritten, and checkpoint hashes are recomputed. Pending consolidation proposals are rejected and must be regenerated under the current authority; retained applied and rejected proposals remain available for audit and replay.

Each migrated row has before and after authority fingerprints. Each completed run records the proof-set hash, aggregate before and after checksums, and row counts. Repeating or restarting migration is idempotent. A failed statement rolls back the migration, including its journal. Identities outside Git's proof set remain unchanged and are reported as isolated inventory; Lattice does not add fallback aliases that would widen their scope.

Repositories whose common Git directory is not a `.git` child, including separate-Git-directory and bare layouts, keep repository-owned Lattice state beneath `<common-git-dir>/lattice`. This makes the physical memory home stable across worktrees without guessing a checkout owner.

When the Git common directory itself moves, startup deliberately refuses the
old ownership record. Relocation is an operator-attested offline operation: the
operator supplies the recorded old identity and confirms that every Lattice
process is stopped. Lattice verifies the new Git common directory and rejects
the operation while the registered old Git path still exists, then migrates
only identities derived from the recorded old home. The journal checksum is a
consistency check over that exact transfer, not cryptographic evidence that two
Git object databases have the same history. A remote URL or directory-name
similarity is never accepted as authority evidence.
