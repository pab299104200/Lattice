//! Immutable Git commit manifests stored atomically with shared parse objects.
use crate::error::LatticeError;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Component, Path};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS commit_manifest_generations(
 generation_id TEXT PRIMARY KEY CHECK(length(generation_id)=64),
 repository_id TEXT NOT NULL, object_format TEXT NOT NULL CHECK(object_format IN('sha1','sha256')),
 commit_oid TEXT NOT NULL, parser_version INTEGER NOT NULL, schema_version INTEGER NOT NULL,
 config_identity TEXT NOT NULL, entry_count INTEGER NOT NULL CHECK(entry_count>=0),
 entries_digest TEXT NOT NULL CHECK(length(entries_digest)=64), complete INTEGER NOT NULL CHECK(complete IN(0,1)),
 created_at INTEGER NOT NULL DEFAULT(unixepoch())
);
CREATE UNIQUE INDEX IF NOT EXISTS complete_commit_manifest_identity
ON commit_manifest_generations(repository_id,object_format,commit_oid,parser_version,schema_version,config_identity)
WHERE complete=1;
CREATE TABLE IF NOT EXISTS commit_manifest_entries(
 generation_id TEXT NOT NULL REFERENCES commit_manifest_generations(generation_id) ON DELETE CASCADE,
 path TEXT NOT NULL, mode INTEGER NOT NULL, blob_oid TEXT NOT NULL,
 content_hash TEXT NOT NULL CHECK(length(content_hash)=64), parse_key TEXT NOT NULL CHECK(length(parse_key)=64),
 entry_digest TEXT NOT NULL CHECK(length(entry_digest)=64),
 PRIMARY KEY(generation_id,path)
);
CREATE INDEX IF NOT EXISTS commit_manifest_path ON commit_manifest_entries(generation_id,path);
CREATE TABLE IF NOT EXISTS commit_parse_pins(
 generation_id TEXT NOT NULL REFERENCES commit_manifest_generations(generation_id) ON DELETE CASCADE,
 cache_key TEXT NOT NULL REFERENCES parsed_file_cache(cache_key) ON DELETE RESTRICT,
 PRIMARY KEY(generation_id,cache_key)
);
CREATE INDEX IF NOT EXISTS commit_parse_pins_key ON commit_parse_pins(cache_key);
CREATE TABLE IF NOT EXISTS commit_manifest_consumers(
 checkout_id TEXT PRIMARY KEY,
 generation_id TEXT NOT NULL REFERENCES commit_manifest_generations(generation_id) ON DELETE RESTRICT,
 bound_at INTEGER NOT NULL DEFAULT(unixepoch())
);
CREATE INDEX IF NOT EXISTS commit_manifest_consumers_generation ON commit_manifest_consumers(generation_id);
"#;

#[derive(Clone, Copy, Debug)]
pub struct ManifestLimits {
    pub max_entries: usize,
    pub max_lookup_paths: usize,
}
impl Default for ManifestLimits {
    fn default() -> Self {
        Self {
            max_entries: 200_000,
            max_lookup_paths: 512,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum GitObjectFormat {
    Sha1,
    Sha256,
}
impl GitObjectFormat {
    fn name(&self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }
    fn oid_len(&self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitManifestIdentity {
    pub repository_id: String,
    pub object_format: GitObjectFormat,
    pub commit_oid: String,
    pub parser_version: i64,
    pub schema_version: i64,
    pub config_identity: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitManifestEntry {
    pub path: String,
    pub mode: u32,
    pub blob_oid: String,
    pub content_hash: String,
    pub parse_key: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitManifestQuery {
    pub path: String,
    pub mode: u32,
    pub blob_oid: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedManifest {
    pub generation_id: String,
    pub entry_count: usize,
    pub entries_digest: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManifestRetirement {
    pub removed_generations: usize,
    pub released_pins: usize,
    pub remaining_candidates: bool,
}

/// Schema and transactional operations over the ParsedFileCache connection.
/// Keeping both tables in one database makes parse pins and completeness one
/// SQLite commit; no cross-store atomicity is implied.
pub struct CommitManifestStore;
impl CommitManifestStore {
    pub fn initialize(connection: &Connection) -> Result<(), LatticeError> {
        connection.execute_batch(SCHEMA).map_err(sql)?;
        let has_digest: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('commit_manifest_entries') WHERE name='entry_digest')",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if !has_digest {
            connection
                .execute(
                    "ALTER TABLE commit_manifest_entries ADD COLUMN entry_digest TEXT",
                    [],
                )
                .map_err(sql)?;
        }
        Ok(())
    }

    pub fn publish(
        connection: &mut Connection,
        identity: &CommitManifestIdentity,
        entries: &[CommitManifestEntry],
        limits: ManifestLimits,
    ) -> Result<PublishedManifest, LatticeError> {
        Self::publish_inner(connection, identity, entries, limits, None)
    }

    fn publish_inner(
        connection: &mut Connection,
        identity: &CommitManifestIdentity,
        entries: &[CommitManifestEntry],
        limits: ManifestLimits,
        checkout_id: Option<&str>,
    ) -> Result<PublishedManifest, LatticeError> {
        validate_limits(limits)?;
        validate_identity(identity)?;
        if entries.len() > limits.max_entries {
            return Err(storage("commit manifest exceeds configured entry limit"));
        }
        let mut ordered = entries.to_vec();
        ordered.sort_by(|a, b| a.path.cmp(&b.path));
        for (index, entry) in ordered.iter().enumerate() {
            validate_entry(identity, entry)?;
            if index > 0 && ordered[index - 1].path == entry.path {
                return Err(storage("commit manifest contains duplicate path"));
            }
        }
        let digest = entries_digest(&ordered);
        let generation = generation_id(identity, &digest, ordered.len());
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::initialize(&tx)?;

        if let Some((complete, count, stored_digest)) = tx.query_row(
            "SELECT complete,entry_count,entries_digest FROM commit_manifest_generations WHERE generation_id=?1",
            [&generation], |row| Ok((row.get::<_, bool>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?)),
        ).optional().map_err(sql)? {
            if complete && count == ordered.len() as i64 && stored_digest == digest {
                if let Some(checkout_id) = checkout_id {
                    bind_in_transaction(&tx, checkout_id, &generation)?;
                }
                tx.commit().map_err(sql)?;
                return Ok(PublishedManifest { generation_id: generation, entry_count: ordered.len(), entries_digest: digest });
            }
            return Err(storage("commit manifest generation is incomplete or inconsistent"));
        }

        tx.execute(
            "INSERT INTO commit_manifest_generations(generation_id,repository_id,object_format,commit_oid,parser_version,schema_version,config_identity,entry_count,entries_digest,complete) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,0)",
            params![generation, identity.repository_id, identity.object_format.name(), identity.commit_oid, identity.parser_version, identity.schema_version, identity.config_identity, ordered.len() as i64, digest],
        ).map_err(sql)?;
        let mut insert_entry = tx
            .prepare("INSERT INTO commit_manifest_entries VALUES(?1,?2,?3,?4,?5,?6,?7)")
            .map_err(sql)?;
        for entry in &ordered {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM parsed_file_cache WHERE cache_key=?1 AND content_hash=?2 AND parser_version=?3 AND schema_version=?4 AND config_version=?5)",
                params![entry.parse_key, entry.content_hash, identity.parser_version, identity.schema_version, identity.config_identity], |row| row.get(0),
            ).map_err(sql)?;
            if !exists {
                return Err(storage(
                    "commit manifest references a missing or incompatible parse object",
                ));
            }
            insert_entry
                .execute(params![
                    generation,
                    entry.path,
                    entry.mode,
                    entry.blob_oid,
                    entry.content_hash,
                    entry.parse_key,
                    entry_digest(entry)
                ])
                .map_err(sql)?;
            tx.execute(
                "INSERT OR IGNORE INTO commit_parse_pins VALUES(?1,?2)",
                params![generation, entry.parse_key],
            )
            .map_err(sql)?;
        }
        drop(insert_entry);
        let (entry_count, pin_count): (i64, i64) = tx.query_row(
            "SELECT (SELECT COUNT(*) FROM commit_manifest_entries WHERE generation_id=?1),(SELECT COUNT(DISTINCT cache_key) FROM commit_parse_pins WHERE generation_id=?1)",
            [&generation], |row| Ok((row.get(0)?, row.get(1)?)),
        ).map_err(sql)?;
        if entry_count != ordered.len() as i64
            || (ordered.is_empty() && pin_count != 0)
            || (!ordered.is_empty() && pin_count == 0)
        {
            return Err(storage("commit manifest completeness proof failed"));
        }
        let conflict: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM commit_manifest_generations WHERE repository_id=?1 AND object_format=?2 AND commit_oid=?3 AND parser_version=?4 AND schema_version=?5 AND config_identity=?6 AND complete=1 AND generation_id<>?7)",
            params![identity.repository_id, identity.object_format.name(), identity.commit_oid, identity.parser_version, identity.schema_version, identity.config_identity, generation], |row| row.get(0),
        ).map_err(sql)?;
        if conflict {
            return Err(storage(
                "a different complete manifest already owns this commit identity",
            ));
        }
        tx.execute(
            "UPDATE commit_manifest_generations SET complete=1 WHERE generation_id=?1",
            [&generation],
        )
        .map_err(sql)?;
        if let Some(checkout_id) = checkout_id {
            bind_in_transaction(&tx, checkout_id, &generation)?;
        }
        tx.commit().map_err(sql)?;
        Ok(PublishedManifest {
            generation_id: generation,
            entry_count: ordered.len(),
            entries_digest: digest,
        })
    }

    /// Publishes a complete generation and binds its checkout consumer in the
    /// same SQLite transaction. This is the runtime entry point: a collector
    /// can never observe the new pins without the consumer that needs them.
    pub fn publish_for_checkout(
        connection: &mut Connection,
        identity: &CommitManifestIdentity,
        entries: &[CommitManifestEntry],
        limits: ManifestLimits,
        checkout_id: &str,
    ) -> Result<PublishedManifest, LatticeError> {
        validate_token(checkout_id, "checkout identity")?;
        Self::publish_inner(connection, identity, entries, limits, Some(checkout_id))
    }

    pub fn lookup_exact(
        connection: &Connection,
        identity: &CommitManifestIdentity,
        queries: &[CommitManifestQuery],
        limits: ManifestLimits,
    ) -> Result<Vec<Option<CommitManifestEntry>>, LatticeError> {
        validate_limits(limits)?;
        validate_identity(identity)?;
        if queries.len() > limits.max_lookup_paths {
            return Err(storage(
                "commit manifest lookup exceeds configured path limit",
            ));
        }
        let generation: Option<String> = connection.query_row(
            "SELECT generation_id FROM commit_manifest_generations WHERE repository_id=?1 AND object_format=?2 AND commit_oid=?3 AND parser_version=?4 AND schema_version=?5 AND config_identity=?6 AND complete=1",
            params![identity.repository_id, identity.object_format.name(), identity.commit_oid, identity.parser_version, identity.schema_version, identity.config_identity], |row| row.get(0),
        ).optional().map_err(sql)?;
        let Some(generation) = generation else {
            return Ok(vec![None; queries.len()]);
        };
        let mut query = connection.prepare("SELECT content_hash,parse_key,entry_digest FROM commit_manifest_entries WHERE generation_id=?1 AND path=?2 AND mode=?3 AND blob_oid=?4").map_err(sql)?;
        queries
            .iter()
            .map(|requested| {
                validate_path(&requested.path)?;
                if !matches!(requested.mode, 0o100644 | 0o100755) {
                    return Err(storage(
                        "commit manifest query contains unsupported Git mode",
                    ));
                }
                validate_oid(&requested.blob_oid, identity.object_format.oid_len())?;
                query
                    .query_row(
                        params![
                            generation,
                            requested.path,
                            requested.mode,
                            requested.blob_oid
                        ],
                        |row| {
                            let entry = CommitManifestEntry {
                                path: requested.path.clone(),
                                mode: requested.mode,
                                blob_oid: requested.blob_oid.clone(),
                                content_hash: row.get(0)?,
                                parse_key: row.get(1)?,
                            };
                            let digest: String = row.get(2)?;
                            if digest != entry_digest(&entry) {
                                return Err(rusqlite::Error::InvalidQuery);
                            }
                            Ok(entry)
                        },
                    )
                    .optional()
                    .map_err(sql)
            })
            .collect()
    }

    pub fn lookup_paths(
        connection: &Connection,
        identity: &CommitManifestIdentity,
        paths: &[String],
        limits: ManifestLimits,
    ) -> Result<Vec<Option<CommitManifestEntry>>, LatticeError> {
        validate_limits(limits)?;
        validate_identity(identity)?;
        if paths.len() > limits.max_lookup_paths {
            return Err(storage(
                "commit manifest lookup exceeds configured path limit",
            ));
        }
        let Some(generation) =
            Self::find_complete(connection, identity)?.map(|manifest| manifest.generation_id)
        else {
            return Ok(vec![None; paths.len()]);
        };
        let mut query = connection.prepare("SELECT mode,blob_oid,content_hash,parse_key,entry_digest FROM commit_manifest_entries WHERE generation_id=?1 AND path=?2").map_err(sql)?;
        paths
            .iter()
            .map(|path| {
                validate_path(path)?;
                query
                    .query_row(params![generation, path], |row| {
                        let entry = CommitManifestEntry {
                            path: path.clone(),
                            mode: row.get(0)?,
                            blob_oid: row.get(1)?,
                            content_hash: row.get(2)?,
                            parse_key: row.get(3)?,
                        };
                        let digest: String = row.get(4)?;
                        if digest != entry_digest(&entry) {
                            return Err(rusqlite::Error::InvalidQuery);
                        }
                        Ok(entry)
                    })
                    .optional()
                    .map_err(sql)
            })
            .collect()
    }

    pub fn find_complete(
        connection: &Connection,
        identity: &CommitManifestIdentity,
    ) -> Result<Option<PublishedManifest>, LatticeError> {
        validate_identity(identity)?;
        connection.query_row(
            "SELECT generation_id,entry_count,entries_digest FROM commit_manifest_generations WHERE repository_id=?1 AND object_format=?2 AND commit_oid=?3 AND parser_version=?4 AND schema_version=?5 AND config_identity=?6 AND complete=1",
            params![identity.repository_id, identity.object_format.name(), identity.commit_oid, identity.parser_version, identity.schema_version, identity.config_identity],
            |row| Ok(PublishedManifest { generation_id: row.get(0)?, entry_count: row.get::<_, i64>(1)? as usize, entries_digest: row.get(2)? }),
        ).optional().map_err(sql)
    }

    /// Atomically replace one checkout's base-generation claim. A claim can be
    /// installed only for a complete manifest.
    pub fn bind_checkout(
        connection: &mut Connection,
        checkout_id: &str,
        generation_id: &str,
    ) -> Result<(), LatticeError> {
        validate_token(checkout_id, "checkout identity")?;
        validate_oid(generation_id, 64)?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let complete: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM commit_manifest_generations WHERE generation_id=?1 AND complete=1)",
                [generation_id],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if !complete {
            return Err(storage(
                "checkout cannot bind an incomplete commit manifest",
            ));
        }
        tx.execute(
            "INSERT INTO commit_manifest_consumers(checkout_id,generation_id) VALUES(?1,?2)
             ON CONFLICT(checkout_id) DO UPDATE SET generation_id=excluded.generation_id,bound_at=unixepoch()",
            params![checkout_id, generation_id],
        )
        .map_err(sql)?;
        tx.commit().map_err(sql)
    }

    pub fn release_checkout(
        connection: &Connection,
        checkout_id: &str,
    ) -> Result<(), LatticeError> {
        validate_token(checkout_id, "checkout identity")?;
        connection
            .execute(
                "DELETE FROM commit_manifest_consumers WHERE checkout_id=?1",
                [checkout_id],
            )
            .map_err(sql)?;
        Ok(())
    }

    /// Bounded retirement releases parse pins only for complete generations no
    /// checkout currently names as its base.
    pub fn retire_unreferenced(
        connection: &mut Connection,
        max_generations: usize,
        max_entries: usize,
    ) -> Result<ManifestRetirement, LatticeError> {
        if max_generations == 0 || max_entries == 0 {
            return Err(storage(
                "commit manifest retirement limits must be positive",
            ));
        }
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let mut select = tx.prepare(
            "SELECT generation_id,complete FROM commit_manifest_generations g
             WHERE NOT EXISTS(SELECT 1 FROM commit_manifest_consumers c WHERE c.generation_id=g.generation_id)
             ORDER BY complete,created_at,generation_id LIMIT ?1",
        ).map_err(sql)?;
        let candidates = select
            .query_map([max_generations as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
            })
            .map_err(sql)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql)?;
        drop(select);
        let mut removed_generations = 0usize;
        let mut released_pins = 0usize;
        let mut remaining_budget = max_entries;
        for (generation, complete) in candidates {
            if remaining_budget == 0 {
                break;
            }
            if complete {
                let marked = tx.execute(
                    "UPDATE commit_manifest_generations SET complete=0 WHERE generation_id=?1 AND complete=1 AND NOT EXISTS(SELECT 1 FROM commit_manifest_consumers WHERE generation_id=?1)",
                    [&generation],
                ).map_err(sql)?;
                if marked == 0 {
                    continue;
                }
            }
            let entry_paths = {
                let mut statement = tx.prepare(
                    "SELECT path FROM commit_manifest_entries WHERE generation_id=?1 ORDER BY path LIMIT ?2",
                ).map_err(sql)?;
                let rows = statement
                    .query_map(params![generation, remaining_budget as i64], |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(sql)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(sql)?;
                rows
            };
            for path in &entry_paths {
                tx.execute(
                    "DELETE FROM commit_manifest_entries WHERE generation_id=?1 AND path=?2",
                    params![generation, path],
                )
                .map_err(sql)?;
            }
            remaining_budget -= entry_paths.len();
            let has_entries: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM commit_manifest_entries WHERE generation_id=?1)",
                    [&generation],
                    |row| row.get(0),
                )
                .map_err(sql)?;
            if has_entries || remaining_budget == 0 {
                continue;
            }
            let pin_keys = {
                let mut statement = tx.prepare(
                    "SELECT cache_key FROM commit_parse_pins WHERE generation_id=?1 ORDER BY cache_key LIMIT ?2",
                ).map_err(sql)?;
                let rows = statement
                    .query_map(params![generation, remaining_budget as i64], |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(sql)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(sql)?;
                rows
            };
            for key in &pin_keys {
                tx.execute(
                    "DELETE FROM commit_parse_pins WHERE generation_id=?1 AND cache_key=?2",
                    params![generation, key],
                )
                .map_err(sql)?;
            }
            released_pins += pin_keys.len();
            remaining_budget -= pin_keys.len();
            let has_pins: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM commit_parse_pins WHERE generation_id=?1)",
                    [&generation],
                    |row| row.get(0),
                )
                .map_err(sql)?;
            if !has_pins {
                removed_generations += tx.execute(
                    "DELETE FROM commit_manifest_generations WHERE generation_id=?1 AND complete=0 AND NOT EXISTS(SELECT 1 FROM commit_manifest_consumers WHERE generation_id=?1)",
                    [&generation],
                ).map_err(sql)?;
            }
        }
        let remaining_candidates = tx.query_row("SELECT EXISTS(SELECT 1 FROM commit_manifest_generations g WHERE NOT EXISTS(SELECT 1 FROM commit_manifest_consumers c WHERE c.generation_id=g.generation_id))", [], |row| row.get(0)).map_err(sql)?;
        tx.commit().map_err(sql)?;
        Ok(ManifestRetirement {
            removed_generations,
            released_pins,
            remaining_candidates,
        })
    }
}

fn bind_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    checkout_id: &str,
    generation_id: &str,
) -> Result<(), LatticeError> {
    validate_token(checkout_id, "checkout identity")?;
    transaction
        .execute(
            "INSERT INTO commit_manifest_consumers(checkout_id,generation_id) VALUES(?1,?2)
             ON CONFLICT(checkout_id) DO UPDATE SET generation_id=excluded.generation_id,bound_at=unixepoch()",
            params![checkout_id, generation_id],
        )
        .map_err(sql)?;
    Ok(())
}

fn validate_limits(limits: ManifestLimits) -> Result<(), LatticeError> {
    if limits.max_entries == 0 || limits.max_lookup_paths == 0 {
        Err(storage("commit manifest limits must be positive"))
    } else {
        Ok(())
    }
}
fn validate_identity(identity: &CommitManifestIdentity) -> Result<(), LatticeError> {
    validate_token(&identity.repository_id, "repository identity")?;
    validate_oid(&identity.commit_oid, identity.object_format.oid_len())?;
    if identity.parser_version <= 0 || identity.schema_version <= 0 {
        return Err(storage("parser and schema versions must be positive"));
    }
    validate_token(&identity.config_identity, "parser configuration identity")
}
fn validate_entry(
    identity: &CommitManifestIdentity,
    entry: &CommitManifestEntry,
) -> Result<(), LatticeError> {
    validate_path(&entry.path)?;
    if !matches!(entry.mode, 0o100644 | 0o100755) {
        return Err(storage("commit manifest contains unsupported Git mode"));
    }
    validate_oid(&entry.blob_oid, identity.object_format.oid_len())?;
    validate_oid(&entry.content_hash, 64)?;
    validate_oid(&entry.parse_key, 64)
}
fn validate_path(value: &str) -> Result<(), LatticeError> {
    let path = Path::new(value);
    if value.is_empty()
        || value.contains('\\')
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(storage(
            "commit manifest path is not normalized and relative",
        ));
    }
    Ok(())
}
fn validate_token(value: &str, name: &str) -> Result<(), LatticeError> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
    {
        return Err(storage(&format!("invalid {name}")));
    }
    Ok(())
}
fn validate_oid(value: &str, len: usize) -> Result<(), LatticeError> {
    if value.len() != len
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(storage("invalid lowercase hexadecimal object identity"));
    }
    Ok(())
}
fn entries_digest(entries: &[CommitManifestEntry]) -> String {
    let mut hash = Sha256::new();
    for entry in entries {
        for value in [
            &entry.path[..],
            &entry.mode.to_string(),
            &entry.blob_oid,
            &entry.content_hash,
            &entry.parse_key,
        ] {
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value.as_bytes());
        }
    }
    format!("{:x}", hash.finalize())
}
fn entry_digest(entry: &CommitManifestEntry) -> String {
    entries_digest(std::slice::from_ref(entry))
}
fn generation_id(identity: &CommitManifestIdentity, digest: &str, count: usize) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(identity, digest, count)).expect("manifest identity serializes")
        )
    )
}
fn sql(error: rusqlite::Error) -> LatticeError {
    storage(&format!("commit manifest storage failed: {error}"))
}
fn storage(message: &str) -> LatticeError {
    LatticeError::Storage(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity() -> CommitManifestIdentity {
        CommitManifestIdentity {
            repository_id: "repo:proven".into(),
            object_format: GitObjectFormat::Sha1,
            commit_oid: "a".repeat(40),
            parser_version: 1,
            schema_version: 1,
            config_identity: "default-v1".into(),
        }
    }
    fn entry(path: &str, digit: char) -> CommitManifestEntry {
        CommitManifestEntry {
            path: path.into(),
            mode: 0o100644,
            blob_oid: digit.to_string().repeat(40),
            content_hash: digit.to_string().repeat(64),
            parse_key: ((digit as u8 + 1) as char).to_string().repeat(64),
        }
    }
    fn query(entry: &CommitManifestEntry) -> CommitManifestQuery {
        CommitManifestQuery {
            path: entry.path.clone(),
            mode: entry.mode,
            blob_oid: entry.blob_oid.clone(),
        }
    }
    fn initialize(connection: &Connection, entries: &[CommitManifestEntry]) {
        connection.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE parsed_file_cache(cache_key TEXT PRIMARY KEY,content_hash TEXT NOT NULL,language TEXT NOT NULL,parser_version INTEGER NOT NULL,schema_version INTEGER NOT NULL,config_version TEXT NOT NULL,payload_sha256 TEXT NOT NULL,payload TEXT NOT NULL,created_at INTEGER NOT NULL DEFAULT(unixepoch()));").unwrap();
        CommitManifestStore::initialize(connection).unwrap();
        for item in entries {
            connection.execute("INSERT OR IGNORE INTO parsed_file_cache VALUES(?1,?2,'rust',1,1,'default-v1',?3,'{}',unixepoch())",params![item.parse_key,item.content_hash,"f".repeat(64)]).unwrap();
        }
    }
    #[test]
    fn immutable_publish_is_idempotent_and_lookup_is_identity_scoped() {
        let mut db = Connection::open_in_memory().unwrap();
        let entries = vec![entry("src/a.rs", 'a'), entry("src/b.rs", 'b')];
        initialize(&db, &entries);
        let first =
            CommitManifestStore::publish(&mut db, &identity(), &entries, ManifestLimits::default())
                .unwrap();
        let second =
            CommitManifestStore::publish(&mut db, &identity(), &entries, ManifestLimits::default())
                .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            CommitManifestStore::find_complete(&db, &identity()).unwrap(),
            Some(first.clone())
        );
        let found = CommitManifestStore::lookup_exact(
            &db,
            &identity(),
            &[
                query(&entries[1]),
                CommitManifestQuery {
                    path: "missing.rs".into(),
                    mode: 0o100644,
                    blob_oid: "c".repeat(40),
                },
            ],
            ManifestLimits::default(),
        )
        .unwrap();
        assert_eq!(found, vec![Some(entries[1].clone()), None]);
        let mut wrong_mode = query(&entries[0]);
        wrong_mode.mode = 0o100755;
        assert_eq!(
            CommitManifestStore::lookup_exact(
                &db,
                &identity(),
                &[wrong_mode],
                ManifestLimits::default()
            )
            .unwrap(),
            vec![None]
        );
        let mut wrong = identity();
        wrong.config_identity = "other-v1".into();
        assert_eq!(
            CommitManifestStore::lookup_exact(
                &db,
                &wrong,
                &[query(&entries[0])],
                ManifestLimits::default()
            )
            .unwrap(),
            vec![None]
        );
    }
    #[test]
    fn lookup_rejects_entry_payload_changed_outside_publication_contract() {
        let mut db = Connection::open_in_memory().unwrap();
        let entries = vec![entry("src/a.rs", 'a')];
        initialize(&db, &entries);
        CommitManifestStore::publish(&mut db, &identity(), &entries, ManifestLimits::default())
            .unwrap();
        db.execute(
            "UPDATE commit_manifest_entries SET content_hash=?1 WHERE path='src/a.rs'",
            ["b".repeat(64)],
        )
        .unwrap();
        assert!(CommitManifestStore::lookup_paths(
            &db,
            &identity(),
            &["src/a.rs".into()],
            ManifestLimits::default(),
        )
        .is_err());
    }
    #[test]
    fn missing_parse_object_and_invalid_path_roll_back_without_partial_generation() {
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let mut db = Connection::open(file.path()).unwrap();
            initialize(&db, &[]);
            assert!(CommitManifestStore::publish(
                &mut db,
                &identity(),
                &[entry("src/a.rs", 'a')],
                ManifestLimits::default()
            )
            .is_err());
            assert!(CommitManifestStore::publish(
                &mut db,
                &identity(),
                &[entry("../escape.rs", 'a')],
                ManifestLimits::default()
            )
            .is_err());
        }
        let db = Connection::open(file.path()).unwrap();
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM commit_manifest_generations",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
    #[test]
    fn checkout_claims_prevent_retirement_and_release_is_bounded() {
        let mut db = Connection::open_in_memory().unwrap();
        let entries = vec![entry("src/a.rs", 'a')];
        initialize(&db, &entries);
        let _published = CommitManifestStore::publish_for_checkout(
            &mut db,
            &identity(),
            &entries,
            ManifestLimits::default(),
            "checkout-1",
        )
        .unwrap();
        assert_eq!(
            CommitManifestStore::retire_unreferenced(&mut db, 1, 1)
                .unwrap()
                .removed_generations,
            0
        );
        CommitManifestStore::release_checkout(&db, "checkout-1").unwrap();
        let first_page = CommitManifestStore::retire_unreferenced(&mut db, 1, 1).unwrap();
        assert_eq!(first_page.removed_generations, 0);
        assert!(first_page.remaining_candidates);
        let retired = CommitManifestStore::retire_unreferenced(&mut db, 1, 1).unwrap();
        assert_eq!(retired.removed_generations, 1);
        assert_eq!(retired.released_pins, 1);
        let objects: i64 = db
            .query_row("SELECT COUNT(*) FROM parsed_file_cache", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            objects, 1,
            "manifest retirement releases pins but parse GC owns deletion"
        );
    }
    #[test]
    fn retirement_never_exceeds_entry_budget() {
        let mut db = Connection::open_in_memory().unwrap();
        let entries = vec![entry("src/a.rs", 'a'), entry("src/b.rs", 'b')];
        initialize(&db, &entries);
        CommitManifestStore::publish(&mut db, &identity(), &entries, ManifestLimits::default())
            .unwrap();
        let mut removed = 0;
        let mut released = 0;
        for _ in 0..6 {
            let report = CommitManifestStore::retire_unreferenced(&mut db, 1, 1).unwrap();
            assert!(report.released_pins <= 1);
            removed += report.removed_generations;
            released += report.released_pins;
            if !report.remaining_candidates {
                break;
            }
        }
        assert_eq!(removed, 1);
        assert_eq!(released, 2);
    }

    #[test]
    fn concurrent_identical_publish_converges_on_one_complete_generation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("parsed.db");
        let entries = vec![entry("src/a.rs", 'a')];
        {
            let db = Connection::open(&path).unwrap();
            db.busy_timeout(std::time::Duration::from_secs(2)).unwrap();
            initialize(&db, &entries);
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let path = path.clone();
            let entries = entries.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                let mut db = Connection::open(path).unwrap();
                db.busy_timeout(std::time::Duration::from_secs(2)).unwrap();
                barrier.wait();
                CommitManifestStore::publish(
                    &mut db,
                    &identity(),
                    &entries,
                    ManifestLimits::default(),
                )
                .unwrap()
            }));
        }
        let a = threads.remove(0).join().unwrap();
        let b = threads.remove(0).join().unwrap();
        assert_eq!(a, b);
        let db = Connection::open(path).unwrap();
        let complete: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM commit_manifest_generations WHERE complete=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(complete, 1);
    }
}
