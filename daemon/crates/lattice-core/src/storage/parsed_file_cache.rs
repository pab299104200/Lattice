use crate::error::LatticeError;
use crate::symbols::{Language, ParsedFile};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

pub const PARSED_CACHE_PARSER_VERSION: i64 = 1;
pub const PARSED_CACHE_SCHEMA_VERSION: i64 = 1;
pub const PARSED_CACHE_CONFIG_VERSION: &str = "default-v1";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS parsed_file_cache (
    cache_key TEXT PRIMARY KEY CHECK (length(cache_key) = 64),
    content_hash TEXT NOT NULL CHECK (length(content_hash) = 64),
    language TEXT NOT NULL,
    parser_version INTEGER NOT NULL,
    schema_version INTEGER NOT NULL,
    config_version TEXT NOT NULL,
    payload_sha256 TEXT NOT NULL CHECK (length(payload_sha256) = 64),
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_parsed_file_cache_identity
ON parsed_file_cache(content_hash, language, parser_version, schema_version, config_version);
CREATE TABLE IF NOT EXISTS parsed_file_cache_memberships (
    checkout_id TEXT NOT NULL,
    cache_key TEXT NOT NULL REFERENCES parsed_file_cache(cache_key) ON DELETE CASCADE,
    referenced_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (checkout_id, cache_key)
);
CREATE INDEX IF NOT EXISTS idx_parsed_file_cache_memberships_key
ON parsed_file_cache_memberships(cache_key);
"#;

enum ParsedConnection {
    Persistent(super::managed_sqlite::ManagedSqlite),
    Ephemeral(Connection),
}
impl std::ops::Deref for ParsedConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        match self {
            Self::Persistent(connection) => connection,
            Self::Ephemeral(connection) => connection,
        }
    }
}
impl std::ops::DerefMut for ParsedConnection {
    fn deref_mut(&mut self) -> &mut Connection {
        match self {
            Self::Persistent(connection) => connection,
            Self::Ephemeral(connection) => connection,
        }
    }
}

/// Repository-level, content-addressed parsed-file cache.
///
/// Payloads are stripped of checkout paths before publication and rebound to
/// the requesting path after a validated hit. One SQLite transaction publishes
/// the complete payload and its checksum atomically.
pub struct ParsedFileCache {
    connection: Mutex<ParsedConnection>,
    path: Option<PathBuf>,
    recovered_corrupt: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedCacheLookup {
    Hit,
    Miss,
    Invalid,
}

impl ParsedFileCache {
    pub fn open(path: &Path) -> Result<Self, LatticeError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to create parsed cache directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        if std::fs::symlink_metadata(path)
            .ok()
            .is_some_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(LatticeError::Storage(format!(
                "Refusing to open parsed-file cache through symlink: {}",
                path.display()
            )));
        }
        let directory = super::SecureDir::open(path.parent().unwrap_or_else(|| Path::new(".")))
            .map_err(|error| {
                LatticeError::Storage(format!("Cannot pin parsed cache parent: {error}"))
            })?;
        let leaf = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| LatticeError::Storage("Invalid parsed cache database name".into()))?;
        Self::open_in(&directory, leaf)
    }

    pub fn open_in(directory: &super::SecureDir, leaf: &str) -> Result<Self, LatticeError> {
        let path = directory.path().join(leaf);
        let connection = open_file_in(directory, leaf, &path)?;
        Ok(Self {
            connection: Mutex::new(ParsedConnection::Persistent(connection)),
            path: Some(path),
            recovered_corrupt: false,
        })
    }

    pub fn open_in_memory() -> Result<Self, LatticeError> {
        let connection = Connection::open_in_memory().map_err(|error| {
            LatticeError::Storage(format!("Failed to open in-memory parsed cache: {error}"))
        })?;
        configure(&connection)?;
        connection.execute_batch(SCHEMA).map_err(|error| {
            LatticeError::Storage(format!("Failed to initialize parsed-file cache: {error}"))
        })?;
        super::commit_manifest::CommitManifestStore::initialize(&connection)?;
        Ok(Self {
            connection: Mutex::new(ParsedConnection::Ephemeral(connection)),
            path: None,
            recovered_corrupt: false,
        })
    }

    pub fn find_commit_manifest(
        &self,
        identity: &super::commit_manifest::CommitManifestIdentity,
    ) -> Result<Option<super::commit_manifest::PublishedManifest>, LatticeError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| LatticeError::Storage("Parsed-file cache lock was poisoned".into()))?;
        super::commit_manifest::CommitManifestStore::find_complete(&connection, identity)
    }

    pub fn publish_and_bind_commit_manifest(
        &self,
        identity: &super::commit_manifest::CommitManifestIdentity,
        entries: &[super::commit_manifest::CommitManifestEntry],
        limits: super::commit_manifest::ManifestLimits,
        checkout_id: &str,
    ) -> Result<super::commit_manifest::PublishedManifest, LatticeError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| LatticeError::Storage("Parsed-file cache lock was poisoned".into()))?;
        super::commit_manifest::CommitManifestStore::publish_for_checkout(
            &mut connection,
            identity,
            entries,
            limits,
            checkout_id,
        )
    }

    pub fn publish_commit_manifest(
        &self,
        identity: &super::commit_manifest::CommitManifestIdentity,
        entries: &[super::commit_manifest::CommitManifestEntry],
        limits: super::commit_manifest::ManifestLimits,
    ) -> Result<super::commit_manifest::PublishedManifest, LatticeError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| LatticeError::Storage("Parsed-file cache lock was poisoned".into()))?;
        super::commit_manifest::CommitManifestStore::publish(
            &mut connection,
            identity,
            entries,
            limits,
        )
    }

    pub fn lookup_commit_paths(
        &self,
        identity: &super::commit_manifest::CommitManifestIdentity,
        paths: &[String],
        limits: super::commit_manifest::ManifestLimits,
    ) -> Result<Vec<Option<super::commit_manifest::CommitManifestEntry>>, LatticeError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| LatticeError::Storage("Parsed-file cache lock was poisoned".into()))?;
        super::commit_manifest::CommitManifestStore::lookup_paths(
            &connection,
            identity,
            paths,
            limits,
        )
    }

    pub fn bind_commit_manifest(
        &self,
        checkout_id: &str,
        generation_id: &str,
    ) -> Result<(), LatticeError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| LatticeError::Storage("Parsed-file cache lock was poisoned".into()))?;
        super::commit_manifest::CommitManifestStore::bind_checkout(
            &mut connection,
            checkout_id,
            generation_id,
        )
    }

    pub fn release_commit_manifest(&self, checkout_id: &str) -> Result<(), LatticeError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| LatticeError::Storage("Parsed-file cache lock was poisoned".into()))?;
        super::commit_manifest::CommitManifestStore::release_checkout(&connection, checkout_id)
    }

    pub fn retire_commit_manifests(
        &self,
        max_generations: usize,
        max_entries: usize,
    ) -> Result<super::commit_manifest::ManifestRetirement, LatticeError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| LatticeError::Storage("Parsed-file cache lock was poisoned".into()))?;
        super::commit_manifest::CommitManifestStore::retire_unreferenced(
            &mut connection,
            max_generations,
            max_entries,
        )
    }

    pub fn parse_key(content_hash: &str, language: Language) -> String {
        cache_key(content_hash, language_name(language))
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn recovered_corrupt(&self) -> bool {
        self.recovered_corrupt
    }

    pub fn get(
        &self,
        content_hash: &str,
        language: Language,
        requested_path: &str,
    ) -> Result<(ParsedCacheLookup, Option<ParsedFile>), LatticeError> {
        let language = language_name(language);
        let cache_key = cache_key(content_hash, language);
        let connection = self.connection.lock().map_err(|_| {
            LatticeError::Storage("Parsed-file cache lock was poisoned".to_string())
        })?;
        let row = connection
            .query_row(
                "SELECT payload_sha256, payload FROM parsed_file_cache
                 WHERE cache_key = ?1 AND content_hash = ?2 AND language = ?3
                   AND parser_version = ?4 AND schema_version = ?5 AND config_version = ?6",
                params![
                    cache_key,
                    content_hash,
                    language,
                    PARSED_CACHE_PARSER_VERSION,
                    PARSED_CACHE_SCHEMA_VERSION,
                    PARSED_CACHE_CONFIG_VERSION,
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|error| {
                LatticeError::Storage(format!("Failed to query parsed-file cache: {error}"))
            })?;
        let Some((expected_sha, payload)) = row else {
            return Ok((ParsedCacheLookup::Miss, None));
        };
        if sha256_hex(payload.as_bytes()) != expected_sha {
            return Ok((ParsedCacheLookup::Invalid, None));
        }
        let mut parsed: ParsedFile = match serde_json::from_str(&payload) {
            Ok(parsed) => parsed,
            Err(_) => return Ok((ParsedCacheLookup::Invalid, None)),
        };
        if !is_path_free(&parsed) || language_name(parsed.language) != language {
            return Ok((ParsedCacheLookup::Invalid, None));
        }
        bind_path(&mut parsed, requested_path);
        Ok((ParsedCacheLookup::Hit, Some(parsed)))
    }

    pub fn put(&self, content_hash: &str, parsed: &ParsedFile) -> Result<(), LatticeError> {
        let language = language_name(parsed.language);
        let cache_key = cache_key(content_hash, language);
        let mut path_free = parsed.clone();
        strip_path(&mut path_free);
        let payload = serde_json::to_string(&path_free).map_err(|error| {
            LatticeError::Storage(format!("Failed to serialize parsed cache payload: {error}"))
        })?;
        let payload_sha = sha256_hex(payload.as_bytes());
        let mut connection = self.connection.lock().map_err(|_| {
            LatticeError::Storage("Parsed-file cache lock was poisoned".to_string())
        })?;
        let transaction = connection.transaction().map_err(|error| {
            LatticeError::Storage(format!("Failed to begin parsed cache transaction: {error}"))
        })?;
        transaction
            .execute(
                "INSERT INTO parsed_file_cache
                 (cache_key, content_hash, language, parser_version, schema_version,
                  config_version, payload_sha256, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(cache_key) DO UPDATE SET
                   payload_sha256 = excluded.payload_sha256,
                   payload = excluded.payload,
                   created_at = unixepoch()",
                params![
                    cache_key,
                    content_hash,
                    language,
                    PARSED_CACHE_PARSER_VERSION,
                    PARSED_CACHE_SCHEMA_VERSION,
                    PARSED_CACHE_CONFIG_VERSION,
                    payload_sha,
                    payload,
                ],
            )
            .map_err(|error| {
                LatticeError::Storage(format!("Failed to publish parsed cache row: {error}"))
            })?;
        transaction.commit().map_err(|error| {
            LatticeError::Storage(format!("Failed to commit parsed cache row: {error}"))
        })?;
        Ok(())
    }

    /// Records that a successfully published checkout graph may refer to this
    /// immutable parse object.  Graph publication calls this only after its
    /// manifest transaction commits; a failed index can therefore never keep
    /// an object alive indefinitely.
    pub fn record_membership(
        &self,
        checkout_id: &str,
        content_hash: &str,
        language: Language,
    ) -> Result<(), LatticeError> {
        if checkout_id.trim().is_empty() {
            return Err(LatticeError::Storage(
                "Parsed-cache checkout id is empty".to_string(),
            ));
        }
        let key = cache_key(content_hash, language_name(language));
        let connection = self.connection.lock().map_err(|_| {
            LatticeError::Storage("Parsed-file cache lock was poisoned".to_string())
        })?;
        connection
            .execute(
                "INSERT INTO parsed_file_cache_memberships (checkout_id, cache_key)\
             SELECT ?1, cache_key FROM parsed_file_cache WHERE cache_key = ?2\
             ON CONFLICT(checkout_id, cache_key) DO UPDATE SET referenced_at = unixepoch()",
                params![checkout_id, key],
            )
            .map_err(|error| {
                LatticeError::Storage(format!("Failed to record parsed-cache membership: {error}"))
            })?;
        Ok(())
    }

    /// Atomically replace one checkout's exact committed manifest membership.
    pub fn replace_membership(
        &self,
        checkout_id: &str,
        files: &[super::FileIndexEntry],
    ) -> Result<(), LatticeError> {
        let error =
            |e: rusqlite::Error| LatticeError::Storage(format!("parsed manifest publication: {e}"));
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| LatticeError::Storage("parsed cache mutex poisoned".into()))?;
        let tx = connection.transaction().map_err(error)?;
        tx.execute(
            "DELETE FROM parsed_file_cache_memberships WHERE checkout_id=?1",
            [checkout_id],
        )
        .map_err(error)?;
        for file in files {
            let language = Language::from_extension(
                Path::new(&file.file)
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or(""),
            );
            let key = cache_key(&file.content_hash, language_name(language));
            tx.execute("INSERT OR IGNORE INTO parsed_file_cache_memberships(checkout_id,cache_key) SELECT ?1,cache_key FROM parsed_file_cache WHERE cache_key=?2",params![checkout_id,key]).map_err(error)?;
        }
        tx.commit().map_err(error)
    }

    /// Reclaim a bounded batch only when no durable checkout membership or
    /// commit pin references the object. Membership retirement is authorized by
    /// the repository GC journal, never inferred from a truncated live-ID list.
    pub fn gc_unreferenced(&self, limit: usize) -> Result<usize, LatticeError> {
        if limit == 0 {
            return Ok(0);
        }
        let mut connection = self.connection.lock().map_err(|_| {
            LatticeError::Storage("Parsed-file cache lock was poisoned".to_string())
        })?;
        let tx = connection.transaction().map_err(|error| {
            LatticeError::Storage(format!("Failed to begin parsed-cache GC: {error}"))
        })?;
        let mut statement = tx
            .prepare(
                "SELECT c.cache_key
                 FROM parsed_file_cache c
                 WHERE NOT EXISTS (
                   SELECT 1 FROM parsed_file_cache_memberships m
                   WHERE m.cache_key = c.cache_key
                 )
                 AND NOT EXISTS (SELECT 1 FROM commit_parse_pins p WHERE p.cache_key=c.cache_key)
                 ORDER BY c.created_at ASC
                 LIMIT ?1",
            )
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to select parsed-cache GC candidates: {error}"
                ))
            })?;
        let keys = statement
            .query_map(params![limit.min(i64::MAX as usize) as i64], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|error| {
                LatticeError::Storage(format!(
                    "Failed to query parsed-cache GC candidates: {error}"
                ))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                LatticeError::Storage(format!("Failed to read parsed-cache GC candidate: {error}"))
            })?;
        drop(statement);
        for key in &keys {
            tx.execute("DELETE FROM parsed_file_cache WHERE cache_key = ?1", [key])
                .map_err(|error| {
                    LatticeError::Storage(format!(
                        "Failed to delete parsed-cache GC candidate: {error}"
                    ))
                })?;
        }
        tx.commit().map_err(|error| {
            LatticeError::Storage(format!("Failed to commit parsed-cache GC: {error}"))
        })?;
        Ok(keys.len())
    }

    #[cfg(test)]
    fn corrupt_payload(&self, content_hash: &str, language: Language) {
        let language = language_name(language);
        let key = cache_key(content_hash, language);
        self.connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE parsed_file_cache SET payload = '{broken' WHERE cache_key = ?1",
                [key],
            )
            .unwrap();
    }
}

pub fn content_sha256(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

fn configure(connection: &Connection) -> Result<(), LatticeError> {
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to enable parsed cache reference integrity: {error}"
            ))
        })?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| {
            LatticeError::Storage(format!("Failed to set parsed cache timeout: {error}"))
        })?;
    retry_while_busy(|| connection.pragma_update(None, "journal_mode", "WAL")).map_err(
        |error| LatticeError::Storage(format!("Failed to enable parsed cache WAL: {error}")),
    )?;
    retry_while_busy(|| connection.pragma_update(None, "wal_autocheckpoint", 1000)).map_err(
        |error| {
            LatticeError::Storage(format!(
                "Failed to configure parsed cache checkpoint: {error}"
            ))
        },
    )?;
    Ok(())
}

fn open_file_in(
    directory: &super::SecureDir,
    leaf: &str,
    path: &Path,
) -> Result<super::managed_sqlite::ManagedSqlite, LatticeError> {
    let connection = super::managed_sqlite::ManagedSqlite::open(
        directory,
        leaf,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
    )
    .map_err(|error| map_open_error(path, error))?;
    let integrity: String =
        retry_while_busy(|| connection.query_row("PRAGMA quick_check(1)", [], |row| row.get(0)))
            .map_err(|error| map_open_error(path, error))?;
    if !integrity.eq_ignore_ascii_case("ok") {
        return Err(LatticeError::CorruptStorage {
            path: path.display().to_string(),
            message: integrity,
        });
    }
    configure(&connection)?;
    retry_while_busy(|| connection.execute_batch(SCHEMA)).map_err(|error| {
        LatticeError::Storage(format!("Failed to initialize parsed-file cache: {error}"))
    })?;
    super::commit_manifest::CommitManifestStore::initialize(&connection)?;
    Ok(connection)
}

fn retry_while_busy<T>(mut operation: impl FnMut() -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    const RETRIES: usize = 50;
    for attempt in 0..RETRIES {
        match operation() {
            Err(error) if is_busy(&error) && attempt + 1 < RETRIES => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result,
        }
    }
    unreachable!("bounded SQLite retry loop always returns on its final attempt")
}

fn is_busy(error: &rusqlite::Error) -> bool {
    use rusqlite::ErrorCode;
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn map_open_error(path: &Path, error: rusqlite::Error) -> LatticeError {
    use rusqlite::ErrorCode;
    if matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    ) {
        LatticeError::CorruptStorage {
            path: path.display().to_string(),
            message: error.to_string(),
        }
    } else {
        LatticeError::Storage(format!(
            "Failed to open parsed-file cache {}: {error}",
            path.display()
        ))
    }
}

fn cache_key(content_hash: &str, language: &str) -> String {
    sha256_hex(
        format!(
            "{content_hash}\0{language}\0{PARSED_CACHE_PARSER_VERSION}\0{PARSED_CACHE_SCHEMA_VERSION}\0{PARSED_CACHE_CONFIG_VERSION}"
        )
        .as_bytes(),
    )
}

fn language_name(language: Language) -> &'static str {
    match language {
        Language::TypeScript => "typescript",
        Language::JavaScript => "javascript",
        Language::Python => "python",
        Language::Rust => "rust",
        Language::Go => "go",
        Language::Java => "java",
        Language::Markdown => "markdown",
        Language::Unknown => "unknown",
    }
}

fn strip_path(parsed: &mut ParsedFile) {
    parsed.file.clear();
    for symbol in &mut parsed.symbols {
        symbol.file.clear();
        symbol.id.file.clear();
    }
    for link in &mut parsed.links {
        link.from.file.clear();
    }
}

fn bind_path(parsed: &mut ParsedFile, path: &str) {
    parsed.file = path.to_string();
    for symbol in &mut parsed.symbols {
        symbol.file = path.to_string();
        symbol.id.file = path.to_string();
    }
    for link in &mut parsed.links {
        link.from.file = path.to_string();
    }
}

fn is_path_free(parsed: &ParsedFile) -> bool {
    parsed.file.is_empty()
        && parsed
            .symbols
            .iter()
            .all(|symbol| symbol.file.is_empty() && symbol.id.file.is_empty())
        && parsed.links.iter().all(|link| link.from.file.is_empty())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;
    use std::sync::Arc;

    #[test]
    fn round_trip_rebinds_content_to_each_checkout_path() {
        let cache = ParsedFileCache::open_in_memory().unwrap();
        let parsed = parser::parse_file("primary/src/lib.rs", "pub fn shared() {}\n").unwrap();
        let hash = content_sha256(b"pub fn shared() {}\n");
        cache.put(&hash, &parsed).unwrap();
        let raw_payload: String = cache
            .connection
            .lock()
            .unwrap()
            .query_row("SELECT payload FROM parsed_file_cache", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(!raw_payload.contains("primary/src/lib.rs"));
        let (lookup, rebound) = cache
            .get(&hash, Language::Rust, "linked/src/lib.rs")
            .unwrap();
        assert_eq!(lookup, ParsedCacheLookup::Hit);
        let rebound = rebound.unwrap();
        assert_eq!(rebound.file, "linked/src/lib.rs");
        assert!(rebound.symbols.iter().all(|symbol| {
            symbol.file == "linked/src/lib.rs" && symbol.id.file == "linked/src/lib.rs"
        }));
    }

    #[test]
    fn malformed_payload_is_an_invalid_miss() {
        let cache = ParsedFileCache::open_in_memory().unwrap();
        let parsed = parser::parse_file("src/lib.rs", "fn value() {}\n").unwrap();
        let hash = content_sha256(b"fn value() {}\n");
        cache.put(&hash, &parsed).unwrap();
        cache.corrupt_payload(&hash, Language::Rust);
        let (lookup, parsed) = cache.get(&hash, Language::Rust, "src/lib.rs").unwrap();
        assert_eq!(lookup, ParsedCacheLookup::Invalid);
        assert!(parsed.is_none());
    }

    #[test]
    fn concurrent_publishers_leave_a_complete_readable_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("parsed-cache.db");
        let hash = content_sha256(b"pub fn shared() {}\n");
        let mut threads = Vec::new();
        for index in 0..8 {
            let path = path.clone();
            let hash = hash.clone();
            threads.push(std::thread::spawn(move || {
                let cache = ParsedFileCache::open(&path).unwrap();
                let parsed = parser::parse_file(
                    &format!("checkout-{index}/src/lib.rs"),
                    "pub fn shared() {}\n",
                )
                .unwrap();
                cache.put(&hash, &parsed).unwrap();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        let cache = Arc::new(ParsedFileCache::open(&path).unwrap());
        let (lookup, parsed) = cache.get(&hash, Language::Rust, "final.rs").unwrap();
        assert_eq!(lookup, ParsedCacheLookup::Hit);
        assert_eq!(parsed.unwrap().file, "final.rs");
    }

    #[test]
    fn corrupt_database_is_reported_without_deleting_shared_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("parsed-cache.db");
        std::fs::write(&path, b"not a sqlite database").unwrap();
        assert!(matches!(
            ParsedFileCache::open(&path),
            Err(LatticeError::CorruptStorage { .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"not a sqlite database");
    }

    #[test]
    fn membership_gc_keeps_live_checkout_objects_only() {
        let cache = ParsedFileCache::open_in_memory().unwrap();
        let first = parser::parse_file("src/one.rs", "fn one() {}\n").unwrap();
        let second = parser::parse_file("src/two.rs", "fn two() {}\n").unwrap();
        let first_hash = content_sha256(b"fn one() {}\n");
        let second_hash = content_sha256(b"fn two() {}\n");
        cache.put(&first_hash, &first).unwrap();
        cache.put(&second_hash, &second).unwrap();
        cache
            .record_membership("checkout-a", &first_hash, Language::Rust)
            .unwrap();
        assert_eq!(cache.gc_unreferenced(8).unwrap(), 1);
        assert_eq!(
            cache
                .get(&first_hash, Language::Rust, "src/one.rs")
                .unwrap()
                .0,
            ParsedCacheLookup::Hit
        );
        assert_eq!(
            cache
                .get(&second_hash, Language::Rust, "src/two.rs")
                .unwrap()
                .0,
            ParsedCacheLookup::Miss
        );
    }
}
