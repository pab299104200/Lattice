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
"#;

/// Repository-level, content-addressed parsed-file cache.
///
/// Payloads are stripped of checkout paths before publication and rebound to
/// the requesting path after a validated hit. One SQLite transaction publishes
/// the complete payload and its checksum atomically.
pub struct ParsedFileCache {
    connection: Mutex<Connection>,
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
        match open_file(path) {
            Ok(connection) => Ok(Self {
                connection: Mutex::new(connection),
                path: Some(path.to_path_buf()),
                recovered_corrupt: false,
            }),
            Err(LatticeError::CorruptStorage { .. }) => {
                remove_cache_files(path)?;
                Ok(Self {
                    connection: Mutex::new(open_file(path)?),
                    path: Some(path.to_path_buf()),
                    recovered_corrupt: true,
                })
            }
            Err(error) => Err(error),
        }
    }

    pub fn open_in_memory() -> Result<Self, LatticeError> {
        let connection = Connection::open_in_memory().map_err(|error| {
            LatticeError::Storage(format!("Failed to open in-memory parsed cache: {error}"))
        })?;
        configure(&connection)?;
        connection.execute_batch(SCHEMA).map_err(|error| {
            LatticeError::Storage(format!("Failed to initialize parsed-file cache: {error}"))
        })?;
        Ok(Self {
            connection: Mutex::new(connection),
            path: None,
            recovered_corrupt: false,
        })
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

fn open_file(path: &Path) -> Result<Connection, LatticeError> {
    let connection = Connection::open(path).map_err(|error| map_open_error(path, error))?;
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

fn remove_cache_files(path: &Path) -> Result<(), LatticeError> {
    for target in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        match std::fs::remove_file(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(LatticeError::Storage(format!(
                    "Failed to remove corrupt parsed cache {}: {error}",
                    target.display()
                )))
            }
        }
    }
    Ok(())
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
    fn corrupt_database_is_rebuilt_without_touching_other_repository_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("parsed-cache.db");
        std::fs::write(&path, b"not a sqlite database").unwrap();
        let cache = ParsedFileCache::open(&path).unwrap();
        assert!(cache.recovered_corrupt());
        let (lookup, parsed) = cache
            .get(&content_sha256(b"missing"), Language::Rust, "src/lib.rs")
            .unwrap();
        assert_eq!(lookup, ParsedCacheLookup::Miss);
        assert!(parsed.is_none());
    }
}
