//! Explicit, crash-resumable transfer of a repository storage authority.
//!
//! A relocation is authorized by the ownership record already stored in the
//! moved home, never by a remote URL or a similarly named directory.

use super::managed_fs::SecureDir;
use super::managed_sqlite::ManagedSqlite;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct RepositoryRelocationRequest {
    /// The new, physically present repository home (the moved `.lattice`).
    pub current_home: PathBuf,
    pub new_repository_id: String,
    /// Actual Git common directory after the physical repository move.
    pub current_git_common_dir: PathBuf,
    /// Operator-selected former authority, checked against the stored home.
    pub expected_old_repository_id: String,
    /// Required because binaries predating the maintenance lock cannot be
    /// fenced by this implementation.
    pub operator_confirms_all_lattice_processes_stopped: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepositoryRelocationOutcome {
    pub old_repository_id: String,
    pub new_repository_id: String,
    pub old_canonical_home: PathBuf,
    pub new_canonical_home: PathBuf,
    pub proof_hash: String,
    pub migrated_memories: usize,
    pub migrated_dependents: usize,
    pub migrated_serialized_states: usize,
    pub resumed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecordedRelocation {
    pub old_repository_id: String,
    pub new_repository_id: String,
    pub old_canonical_home: PathBuf,
    pub new_canonical_home: PathBuf,
    pub proof_hash: String,
}

pub fn relocate_repository_home(
    request: RepositoryRelocationRequest,
) -> Result<RepositoryRelocationOutcome> {
    if !request.operator_confirms_all_lattice_processes_stopped {
        bail!("repository relocation requires explicit confirmation that every Lattice process using the repository is stopped; pre-contract binaries cannot be fenced");
    }
    validate_repo_id(&request.new_repository_id)?;
    let meta = fs::symlink_metadata(&request.current_home)
        .context("relocated repository home is unavailable")?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        bail!("relocated repository home must be a non-symlink directory");
    }
    let home = request.current_home.canonicalize()?;
    let current_git = request
        .current_git_common_dir
        .canonicalize()
        .context("relocation requires the present Git common directory")?;
    verify_git_home(&current_git, &home, &request.new_repository_id)?;
    let managed = SecureDir::open(&home)?;
    let registry_path = home.join("storage-registry.db");
    regular(&registry_path, "storage registry")?;
    let memory_path = home.join("memories.db");
    regular(&memory_path, "memory authority")?;

    let _maintenance = exclusive(&managed)?;
    let _memory_owner =
        crate::memory::RepositoryMemoryOwner::acquire_in(&managed, Duration::from_secs(1))?;
    let mut registry = ManagedSqlite::open(
        &managed,
        "storage-registry.db",
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    registry.busy_timeout(Duration::from_secs(1))?;
    schema(&registry)?;
    let _checkout_fences = acquire_checkout_fences(&registry, &managed)?;
    let (old_id, old_home): (String, String) = registry
        .query_row(
            "SELECT repository_id,canonical_home FROM repository_home WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .context("storage registry has no repository ownership record")?;

    if old_id == request.new_repository_id && Path::new(&old_home) == home {
        return completed(&registry, &home, &request.new_repository_id)?.context(
            "repository already uses the requested identity, but has no completed relocation proof",
        );
    }
    if request.expected_old_repository_id != old_id {
        bail!("recorded repository identity differs from the operator-selected former authority");
    }
    let old_home_path = Path::new(&old_home);
    let old_git = git_dir_for_home(old_home_path)?;
    if repository_id_for_git(&old_git) != old_id {
        bail!("recorded former repository identity does not match its registered Git directory");
    }
    if old_git != current_git
        && old_git
            .try_exists()
            .context("cannot establish former Git directory availability")?
    {
        bail!("former Git directory still exists; relocating a copy cannot establish a physical repository move");
    }
    // No caller-provided alias set may rewrite foreign memory authority.
    let mut proven_old_identities =
        BTreeSet::from([old_id.clone(), old_git.to_string_lossy().into_owned()]);
    if old_git.file_name().is_some_and(|name| name == ".git") {
        if let Some(root) = old_git.parent() {
            proven_old_identities.insert(root.to_string_lossy().into_owned());
        }
    }
    let proof_hash = proof_hash(
        &old_id,
        &old_home,
        &request.new_repository_id,
        &home,
        &proven_old_identities,
    );
    let new_home = home.to_string_lossy().into_owned();
    let prior: Option<(String, String, String, String, String)> = registry
        .query_row(
            "SELECT old_repository_id,new_repository_id,old_home,new_home,state FROM repository_relocation_journal WHERE proof_hash=?1",
            [&proof_hash],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    if let Some((prior_old_id, prior_new_id, prior_old_home, prior_new_home, state)) = &prior {
        if prior_old_id != &old_id
            || prior_new_id != &request.new_repository_id
            || prior_old_home != &old_home
            || prior_new_home != &new_home
            || !matches!(state.as_str(), "prepared" | "authority_migrated")
        {
            bail!("existing relocation journal does not match the requested authority transfer");
        }
    }
    let resumed = prior.is_some();
    registry.execute(
        "INSERT OR IGNORE INTO repository_relocation_journal(proof_hash,old_repository_id,new_repository_id,old_home,new_home,state) VALUES(?1,?2,?3,?4,?5,'prepared')",
        params![proof_hash, old_id, request.new_repository_id, old_home, new_home],
    )?;
    registry.execute_batch("PRAGMA wal_checkpoint(FULL)")?;

    let memory = ManagedSqlite::open(&managed, "memories.db", OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    memory.busy_timeout(Duration::from_secs(1))?;
    let report = crate::memory::identity_migration::migrate_identities(
        &memory,
        &request.new_repository_id,
        &proven_old_identities,
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    registry.execute(
        "UPDATE repository_relocation_journal SET state='authority_migrated' WHERE proof_hash=?1",
        [&proof_hash],
    )?;

    let tx = registry.transaction()?;
    let owner_updates = tx.execute("UPDATE repository_home SET repository_id=?1,canonical_home=?2 WHERE id=1 AND repository_id=?3 AND canonical_home=?4",
        params![request.new_repository_id, new_home, old_id, old_home])?;
    if owner_updates != 1 {
        bail!("repository ownership changed during relocation; refusing to complete the journal");
    }
    tx.execute("UPDATE repository_relocation_journal SET state='completed',completed_at=unixepoch() WHERE proof_hash=?1", [&proof_hash])?;
    tx.execute("INSERT OR REPLACE INTO repository_relocations(old_repository_id,new_repository_id,old_home,new_home,proof_hash,completed_at) VALUES(?1,?2,?3,?4,?5,unixepoch())",
        params![old_id, request.new_repository_id, old_home, new_home, proof_hash])?;
    tx.commit()?;
    registry.execute_batch("PRAGMA wal_checkpoint(FULL)")?;
    managed.sync()?;
    Ok(RepositoryRelocationOutcome {
        old_repository_id: old_id,
        new_repository_id: request.new_repository_id,
        old_canonical_home: old_home.into(),
        new_canonical_home: home,
        proof_hash,
        migrated_memories: report.migrated_memories,
        migrated_dependents: report.migrated_dependents,
        migrated_serialized_states: report.migrated_serialized_states,
        resumed,
    })
}

fn repository_id_for_git(path: &Path) -> String {
    format!(
        "repo_{:x}",
        Sha256::digest(path.as_os_str().as_encoded_bytes())
    )
}

fn git_dir_for_home(home: &Path) -> Result<PathBuf> {
    let parent = home
        .parent()
        .context("repository storage home has no parent")?;
    match home.file_name().and_then(|name| name.to_str()) {
        Some(".lattice") => Ok(parent.join(".git")),
        Some("lattice") => Ok(parent.to_path_buf()),
        _ => bail!("registered home is not a recognized Git-owned storage layout"),
    }
}

fn verify_git_home(common: &Path, home: &Path, repository_id: &str) -> Result<()> {
    if repository_id_for_git(common) != repository_id || git_dir_for_home(home)? != common {
        bail!("relocation target identity/home does not match its actual Git common directory");
    }
    let output = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(common)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .context("cannot verify relocation Git metadata")?;
    if !output.status.success() {
        bail!("relocation target Git metadata is unavailable or invalid");
    }
    let reported = PathBuf::from(String::from_utf8(output.stdout)?.trim());
    let reported = if reported.is_absolute() {
        reported
    } else {
        common.join(reported)
    };
    if reported.canonicalize()? != common {
        bail!("relocation target is not the Git common authority");
    }
    Ok(())
}

pub fn resolve_recorded_relocation(
    home: &Path,
    new_repository_id: &str,
) -> Result<Option<RecordedRelocation>> {
    let home = home.canonicalize()?;
    regular(&home.join("storage-registry.db"), "storage registry")?;
    let managed = SecureDir::open(&home)?;
    let conn = ManagedSqlite::open(
        &managed,
        "storage-registry.db",
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let has_registry: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='repository_relocations')",
        [],
        |row| row.get(0),
    )?;
    if !has_registry {
        return Ok(None);
    }
    let row = conn.query_row(
        "SELECT old_repository_id,new_repository_id,old_home,new_home,proof_hash FROM repository_relocations WHERE new_repository_id=?1 AND new_home=?2 ORDER BY completed_at DESC LIMIT 1",
        params![new_repository_id, home.to_string_lossy()], |r| Ok(RecordedRelocation { old_repository_id:r.get(0)?, new_repository_id:r.get(1)?, old_canonical_home:PathBuf::from(r.get::<_,String>(2)?), new_canonical_home:PathBuf::from(r.get::<_,String>(3)?), proof_hash:r.get(4)? }),
    ).optional()?;
    if let Some(record) = &row {
        validate_recorded_relocation(&conn, &home, new_repository_id, record)?;
    }
    Ok(row)
}

fn validate_recorded_relocation(
    connection: &Connection,
    home: &Path,
    repository_id: &str,
    record: &RecordedRelocation,
) -> Result<()> {
    validate_repo_id(repository_id)?;
    validate_repo_id(&record.old_repository_id)?;
    let (owner, recorded_home): (String, String) = connection.query_row(
        "SELECT repository_id,canonical_home FROM repository_home WHERE id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if owner != repository_id
        || Path::new(&recorded_home) != home
        || record.new_repository_id != repository_id
        || record.new_canonical_home != home
    {
        bail!("recorded relocation does not match the current storage authority");
    }
    if !record.old_canonical_home.is_absolute()
        || record
            .old_canonical_home
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        bail!("recorded relocation former home is invalid");
    }
    let old_git = git_dir_for_home(&record.old_canonical_home)?;
    if repository_id_for_git(&old_git) != record.old_repository_id {
        bail!("recorded relocation former identity does not match its Git home");
    }
    let mut aliases = BTreeSet::from([
        record.old_repository_id.clone(),
        old_git.to_string_lossy().into_owned(),
    ]);
    if old_git.file_name().is_some_and(|name| name == ".git") {
        if let Some(parent) = old_git.parent() {
            aliases.insert(parent.to_string_lossy().into_owned());
        }
    }
    let expected = proof_hash(
        &record.old_repository_id,
        &record.old_canonical_home.to_string_lossy(),
        repository_id,
        home,
        &aliases,
    );
    if expected != record.proof_hash {
        bail!("recorded relocation proof checksum is invalid");
    }
    let committed: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM repository_relocation_journal WHERE proof_hash=?1 AND old_repository_id=?2 AND new_repository_id=?3 AND old_home=?4 AND new_home=?5 AND state='completed')",
        params![record.proof_hash, record.old_repository_id, repository_id, record.old_canonical_home.to_string_lossy(), home.to_string_lossy()], |row| row.get(0),
    )?;
    if !committed {
        bail!("recorded relocation has no matching completed journal");
    }
    Ok(())
}

fn completed(
    conn: &Connection,
    home: &Path,
    new_id: &str,
) -> Result<Option<RepositoryRelocationOutcome>> {
    let record = conn.query_row("SELECT old_repository_id,new_repository_id,old_home,new_home,proof_hash FROM repository_relocations WHERE new_repository_id=?1 AND new_home=?2 ORDER BY completed_at DESC LIMIT 1", params![new_id, home.to_string_lossy()], |r| Ok(RecordedRelocation { old_repository_id:r.get(0)?,new_repository_id:r.get(1)?,old_canonical_home:PathBuf::from(r.get::<_,String>(2)?),new_canonical_home:PathBuf::from(r.get::<_,String>(3)?),proof_hash:r.get(4)? })).optional()?;
    record
        .map(|record| {
            validate_recorded_relocation(conn, home, new_id, &record)?;
            Ok(RepositoryRelocationOutcome {
                old_repository_id: record.old_repository_id,
                new_repository_id: record.new_repository_id,
                old_canonical_home: record.old_canonical_home,
                new_canonical_home: record.new_canonical_home,
                proof_hash: record.proof_hash,
                migrated_memories: 0,
                migrated_dependents: 0,
                migrated_serialized_states: 0,
                resumed: true,
            })
        })
        .transpose()
}

fn schema(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS repository_relocation_journal(proof_hash TEXT PRIMARY KEY,old_repository_id TEXT NOT NULL,new_repository_id TEXT NOT NULL,old_home TEXT NOT NULL,new_home TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN('prepared','authority_migrated','completed')),completed_at INTEGER); CREATE TABLE IF NOT EXISTS repository_relocations(old_repository_id TEXT NOT NULL,new_repository_id TEXT NOT NULL,old_home TEXT NOT NULL,new_home TEXT NOT NULL,proof_hash TEXT PRIMARY KEY,completed_at INTEGER NOT NULL);")?;
    Ok(())
}
fn validate_repo_id(id: &str) -> Result<()> {
    if !id.starts_with("repo_")
        || id.len() != 69
        || !id[5..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("relocation requires a canonical Git repository id")
    }
    Ok(())
}
fn proof_hash(
    old_id: &str,
    old_home: &str,
    new_id: &str,
    new_home: &Path,
    proof: &BTreeSet<String>,
) -> String {
    let mut d = Sha256::new();
    for v in [
        old_id,
        old_home,
        new_id,
        new_home.to_string_lossy().as_ref(),
    ] {
        d.update(v.as_bytes());
        d.update([0]);
    }
    for v in proof {
        d.update(v.as_bytes());
        d.update([0]);
    }
    format!("sha256:{:x}", d.finalize())
}
fn regular(path: &Path, label: &str) -> Result<()> {
    let m = fs::symlink_metadata(path).with_context(|| format!("{label} is unavailable"))?;
    if m.file_type().is_symlink() || !m.is_file() {
        bail!("{label} must be a regular non-symlink file")
    }
    Ok(())
}
fn exclusive(home: &SecureDir) -> Result<File> {
    let f = home.open_or_create_file("maintenance.lock")?;
    f.try_lock()
        .map_err(|e| anyhow::anyhow!("repository maintenance is already running: {e}"))?;
    Ok(f)
}

fn acquire_checkout_fences(conn: &Connection, home: &SecureDir) -> Result<Vec<File>> {
    let ids = conn
        .prepare("SELECT checkout_id FROM checkout_registry ORDER BY checkout_id")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let leases = home.open_dir("leases")?;
    let mut locks = Vec::with_capacity(ids.len());
    for id in ids {
        if id.contains('/') || id.contains("..") {
            bail!("storage registry contains an invalid checkout identity");
        }
        let file = leases.open_or_create_file(&format!("{id}.lock"))?;
        file.try_lock().map_err(|_| {
            anyhow::anyhow!("repository relocation rejected: active checkout lease `{id}`")
        })?;
        locks.push(file);
    }
    Ok(locks)
}

use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;
    use crate::storage::StorageRegistry;

    fn fixture(layout: u8) -> (tempfile::TempDir, RepositoryRelocationRequest, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let old_root = directory.path().join("old");
        let mut command = std::process::Command::new("git");
        command.args(["init", "-q"]);
        if layout == 1 {
            command.arg("--bare");
        }
        let checkout = if layout == 2 {
            fs::create_dir_all(&old_root).unwrap();
            command
                .arg("--separate-git-dir")
                .arg(old_root.join("metadata"));
            old_root.join("work")
        } else {
            old_root.clone()
        };
        assert!(command.arg(&checkout).status().unwrap().success());
        if layout == 2 {
            fs::write(checkout.join(".git"), "gitdir: ../metadata\n").unwrap();
        }
        let old_git = match layout {
            1 => old_root.clone(),
            2 => old_root.join("metadata"),
            _ => old_root.join(".git"),
        }
        .canonicalize()
        .unwrap();
        let old_home = if layout == 0 {
            old_git.parent().unwrap().join(".lattice")
        } else {
            old_git.join("lattice")
        };
        let old_id = repository_id_for_git(&old_git);
        drop(StorageRegistry::open(&old_home, &old_id).unwrap());
        let store = MemoryStore::open(&old_home.join("memories.db")).unwrap();
        store.with_connection(|conn| {
            conn.execute("INSERT INTO memories(id,content,memory_type,scope,workspace_id,created_at,last_accessed,access_count)VALUES('m','retained','pattern','repo',?1,1,1,0)",[&old_id]).unwrap();
            conn.execute("INSERT INTO memories(id,content,memory_type,scope,workspace_id,created_at,last_accessed,access_count)VALUES('foreign','isolated','pattern','repo','foreign-repository',1,1,0)",[]).unwrap();
            Ok(())
        }).unwrap();
        drop(store);
        let new_root = directory.path().join("new");
        fs::rename(&old_root, &new_root).unwrap();
        let new_git = match layout {
            1 => new_root.clone(),
            2 => new_root.join("metadata"),
            _ => new_root.join(".git"),
        }
        .canonicalize()
        .unwrap();
        let home = if layout == 0 {
            new_git.parent().unwrap().join(".lattice")
        } else {
            new_git.join("lattice")
        };
        let request = RepositoryRelocationRequest {
            current_home: home,
            new_repository_id: repository_id_for_git(&new_git),
            current_git_common_dir: new_git,
            expected_old_repository_id: old_id,
            operator_confirms_all_lattice_processes_stopped: true,
        };
        (directory, request, old_git)
    }

    #[test]
    fn physical_normal_bare_and_separate_git_moves_migrate_once_and_preserve_foreign_rows() {
        for layout in [0, 1, 2] {
            let (_directory, request, _) = fixture(layout);
            let first = relocate_repository_home(request.clone()).unwrap();
            assert_eq!(first.migrated_memories, 1);
            let second = relocate_repository_home(request.clone()).unwrap();
            assert!(second.resumed);
            assert_eq!(
                resolve_recorded_relocation(&request.current_home, &request.new_repository_id)
                    .unwrap()
                    .unwrap()
                    .old_repository_id,
                request.expected_old_repository_id
            );
            let db = Connection::open(request.current_home.join("memories.db")).unwrap();
            assert_eq!(
                db.query_row(
                    "SELECT workspace_id FROM memories WHERE id='foreign'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
                "foreign-repository"
            );
        }
    }

    #[test]
    fn recorded_relocation_requires_consistent_identity_checksum_and_completed_journal() {
        for mutation in [
            "UPDATE repository_relocations SET old_repository_id='repo_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff'",
            "UPDATE repository_relocations SET proof_hash='tampered'",
            "UPDATE repository_home SET repository_id='repo_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff'",
            "UPDATE repository_relocation_journal SET state='prepared'",
        ] {
            let (_directory, request, _) = fixture(0);
            relocate_repository_home(request.clone()).unwrap();
            let connection = Connection::open(request.current_home.join("storage-registry.db")).unwrap();
            connection.execute(mutation, []).unwrap();
            drop(connection);
            assert!(resolve_recorded_relocation(&request.current_home, &request.new_repository_id).is_err(), "{mutation}");
            assert!(relocate_repository_home(request).is_err(), "{mutation}");
        }
    }

    #[test]
    fn resume_rejects_mismatched_prepared_and_migrated_journal_rows() {
        for state in ["prepared", "authority_migrated"] {
            let (_directory, request, _) = fixture(0);
            let connection =
                Connection::open(request.current_home.join("storage-registry.db")).unwrap();
            schema(&connection).unwrap();
            let (old_id, old_home): (String, String) = connection
                .query_row(
                    "SELECT repository_id,canonical_home FROM repository_home WHERE id=1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            let old_git = git_dir_for_home(Path::new(&old_home)).unwrap();
            let mut aliases =
                BTreeSet::from([old_id.clone(), old_git.to_string_lossy().into_owned()]);
            if old_git.file_name().is_some_and(|name| name == ".git") {
                aliases.insert(old_git.parent().unwrap().to_string_lossy().into_owned());
            }
            let proof = proof_hash(
                &old_id,
                &old_home,
                &request.new_repository_id,
                &request.current_home,
                &aliases,
            );
            connection.execute(
                "INSERT INTO repository_relocation_journal(proof_hash,old_repository_id,new_repository_id,old_home,new_home,state) VALUES(?1,?2,?3,?4,'/mismatched/home',?5)",
                params![proof, old_id, request.new_repository_id, old_home, state],
            ).unwrap();
            drop(connection);

            assert!(relocate_repository_home(request)
                .unwrap_err()
                .to_string()
                .contains("does not match"));
        }
    }

    #[test]
    fn completion_requires_exactly_one_owner_transition() {
        let (_directory, request, _) = fixture(0);
        let connection =
            Connection::open(request.current_home.join("storage-registry.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER suppress_repository_owner_update BEFORE UPDATE ON repository_home BEGIN SELECT RAISE(IGNORE); END;",
            )
            .unwrap();
        drop(connection);

        assert!(relocate_repository_home(request)
            .unwrap_err()
            .to_string()
            .contains("ownership changed during relocation"));
    }

    #[test]
    fn rejects_missing_offline_assertion_and_forged_git_identity() {
        let (_directory, request, _) = fixture(0);
        let mut offline = request.clone();
        offline.operator_confirms_all_lattice_processes_stopped = false;
        assert!(relocate_repository_home(offline)
            .unwrap_err()
            .to_string()
            .contains("explicit confirmation"));
        let mut foreign = request.clone();
        foreign.expected_old_repository_id = format!("repo_{}", "f".repeat(64));
        assert!(relocate_repository_home(foreign)
            .unwrap_err()
            .to_string()
            .contains("operator-selected"));
        let mut forged = request;
        forged.new_repository_id = format!("repo_{}", "f".repeat(64));
        assert!(relocate_repository_home(forged)
            .unwrap_err()
            .to_string()
            .contains("actual Git"));
    }

    #[test]
    fn copied_home_cannot_claim_a_move_while_old_git_authority_exists() {
        let (_directory, request, old_git) = fixture(0);
        fs::create_dir_all(old_git).unwrap();
        assert!(relocate_repository_home(request)
            .unwrap_err()
            .to_string()
            .contains("still exists"));
    }

    #[test]
    fn active_checkout_lease_blocks_relocation() {
        let (_directory, request, _) = fixture(0);
        let home = super::super::managed_fs::SecureDir::open(&request.current_home).unwrap();
        let checkout_id = format!("checkout_{}", "1".repeat(64));
        let registry = Connection::open(request.current_home.join("storage-registry.db")).unwrap();
        registry
            .execute(
                "INSERT INTO checkout_registry(checkout_id,root,last_seen) VALUES(?1,?2,1)",
                params![checkout_id, request.current_home.to_string_lossy()],
            )
            .unwrap();
        let lease = home
            .open_dir("leases")
            .unwrap()
            .open_or_create_file(&format!("{checkout_id}.lock"))
            .unwrap();
        lease.lock_shared().unwrap();
        assert!(relocate_repository_home(request)
            .unwrap_err()
            .to_string()
            .contains("active checkout lease"));
    }
}
