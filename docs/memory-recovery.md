# Memory storage recovery

Lattice treats repository and organization memory databases as durable authority. Startup never renames, deletes, replaces, or quarantines a database or its WAL/SHM files after an open failure. The graph service may continue with memory marked unavailable, but every memory mutation fails so the caller cannot mistake process-local state for an acknowledged durable write.

Repository memory initialization and schema migration run while holding `memory-owner.lock` in the database directory. The lock file is opened as a regular, non-symlink leaf relative to a pinned directory descriptor. `RepositoryMemoryOwner` retains a duplicate of that exact directory capability; `directory()` and `open_store(leaf)` let initialization open `memories.db` through the same authority. `MemoryStore::open_in` uses the managed SQLite VFS for the database, rollback journal, WAL, SHM, directory synchronization, and SQLite temporary files. A concurrent rename or replacement of the directory pathname therefore cannot redirect either the ownership lock or database I/O. The database-path compatibility entry points pin the parent once and delegate to these descriptor-relative APIs. This advisory kernel lock is shared across daemon processes, has a bounded acquisition deadline, and is released if its owner exits. Ordinary reads and writes use SQLite locking after initialization; the owner lock is not held around long-running indexing or checks.

SQLite's native pathname syscall table is process-global, so the statically linked, bundled SQLite image installs Lattice's managed-path dispatcher in a platform pre-main constructor, before application threads can open SQLite. Daemon startup calls `ManagedSqlite::initialize_process` before creating its runtime to validate that installation and fail if the constructor did not run or another component displaced the hooks. Managed database opens only validate this state; they never attempt a lazy, concurrent hook installation. `/lattice-managed` and `C:\lattice-managed` are reserved internal namespaces; unknown and expired capability tokens fail closed and are never resolved by the host filesystem.

Open failures distinguish contention, access denial/read-only media, full storage, unsupported newer schemas, corruption/not-a-database, and other storage failures. Operators should first stop all Lattice processes that can access the repository authority, retain the database together with matching `-wal` and `-shm` files, and make a coherent SQLite backup before repair. A future schema requires a compatible newer Lattice binary. Access and capacity failures should be corrected in place. Busy failures should be retried after the competing owner exits.

Corruption recovery is deliberately offline and explicit. `lattice storage backup`
creates a coherent SQLite copy plus an ownership and checksum manifest.
`lattice storage restore --replace-existing` validates integrity and memory
counts, enforces the current restore floor and deletion receipts, and replaces
the closed authority through a same-directory rollback rename. Both operations
reject active checkout leases and a busy memory-owner lock. See
`docs/operator-storage.md` for the complete procedure.

Status reports `memory_store.status: unavailable` with the storage `kind`, database `path`, original `reason`, and an actionable `recovery` instruction. Busy stores should be retried after the owner exits; access failures require restoring database and parent permissions; full stores require freeing volume space; newer schemas require upgrading while preserving the database; corrupt stores must be preserved before documented offline restore. Graph-only `lattice status` responses carry the same `memory_store` object.
