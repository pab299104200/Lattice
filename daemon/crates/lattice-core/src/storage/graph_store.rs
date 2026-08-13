use super::schema::CREATE_TABLES;
use crate::error::LatticeError;
use crate::graph::model::CodeGraph;
use crate::graph::model::EdgeKind;
use crate::symbols::{Language, ParsedFile, SymbolId, SymbolKind};
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const FILE_INDEX_PARSER_VERSION: i64 = 1;
pub const FILE_INDEX_SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphStoreRecovery {
    None,
    RebuiltCorrupt,
}

impl GraphStoreRecovery {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "healthy",
            Self::RebuiltCorrupt => "rebuilt_corrupt",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIndexEntry {
    pub file: String,
    pub content_hash: String,
    pub mtime_ns: i64,
    pub size_bytes: i64,
    pub parser_version: i64,
    pub schema_version: i64,
    pub last_indexed_at: i64,
}

/// Persistent storage for the code dependency graph backed by SQLite.
pub struct GraphStore {
    conn: Connection,
    path: Option<PathBuf>,
    recovery: GraphStoreRecovery,
}

impl GraphStore {
    /// Open a file-based SQLite database with WAL mode enabled.
    pub fn open(path: &Path) -> Result<Self, LatticeError> {
        reject_symlink(path)?;
        let conn =
            Connection::open(path).map_err(|e| map_sqlite_error(path, "open database", e))?;

        validate_integrity(&conn, path)?;

        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| map_sqlite_error(path, "set WAL mode", e))?;

        let store = Self {
            conn,
            path: Some(path.to_path_buf()),
            recovery: GraphStoreRecovery::None,
        };
        store.initialize()?;
        Ok(store)
    }

    /// Open the derived graph store, replacing it only when SQLite confirms corruption.
    ///
    /// Graph state is reconstructed from workspace source. Memory, vector, and event stores
    /// are separate databases and are never touched by this recovery path.
    pub fn open_recovering(path: &Path) -> Result<Self, LatticeError> {
        match Self::open(path) {
            Ok(store) => Ok(store),
            Err(LatticeError::CorruptStorage { .. }) => {
                remove_derived_graph_files(path)?;
                let mut store = Self::open(path)?;
                store.recovery = GraphStoreRecovery::RebuiltCorrupt;
                Ok(store)
            }
            Err(error) => Err(error),
        }
    }

    /// Open an in-memory SQLite database (for tests).
    pub fn open_in_memory() -> Result<Self, LatticeError> {
        let conn = Connection::open_in_memory().map_err(|e| {
            LatticeError::Storage(format!("Failed to open in-memory database: {}", e))
        })?;

        let store = Self {
            conn,
            path: None,
            recovery: GraphStoreRecovery::None,
        };
        store.initialize()?;
        Ok(store)
    }

    /// Create tables and indexes if they don't already exist.
    fn initialize(&self) -> Result<(), LatticeError> {
        self.conn
            .execute_batch(CREATE_TABLES)
            .map_err(|e| match &self.path {
                Some(path) => map_sqlite_error(path, "initialize schema", e),
                None => LatticeError::Storage(format!("Failed to initialize schema: {}", e)),
            })?;
        Ok(())
    }

    pub fn recovery(&self) -> GraphStoreRecovery {
        self.recovery
    }

    /// Save a CodeGraph to the database, replacing any previous data.
    pub fn save_graph(&self, graph: &CodeGraph) -> Result<(), LatticeError> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| LatticeError::Storage(format!("Failed to begin transaction: {}", e)))?;

        // Delete all existing data
        tx.execute("DELETE FROM edges", [])
            .map_err(|e| LatticeError::Storage(format!("Failed to clear edges: {}", e)))?;
        tx.execute("DELETE FROM nodes", [])
            .map_err(|e| LatticeError::Storage(format!("Failed to clear nodes: {}", e)))?;

        // Insert all nodes (scoped so prepared statement is dropped before commit)
        {
            let mut insert_node = tx
                .prepare(
                    "INSERT INTO nodes (file, name, byte_offset, kind, signature, body, line, end_line, is_exported, language, edit_count, last_modified) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                )
                .map_err(|e| LatticeError::Storage(format!("Failed to prepare node insert: {}", e)))?;

            for node in graph.all_nodes() {
                insert_node
                    .execute(params![
                        &node.file,
                        &node.name,
                        node.id.byte_offset as i64,
                        format!("{:?}", node.kind),
                        &*node.signature,
                        &*node.body,
                        node.line as i64,
                        node.end_line as i64,
                        node.is_exported as i32,
                        format!("{:?}", node.language),
                        node.edit_count as i64,
                        node.last_modified as i64,
                    ])
                    .map_err(|e| LatticeError::Storage(format!("Failed to insert node: {}", e)))?;
            }
        }

        // Insert all edges (scoped so prepared statement is dropped before commit)
        {
            let mut insert_edge = tx
                .prepare(
                    "INSERT INTO edges (from_file, from_name, from_offset, to_file, to_name, to_offset, kind) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .map_err(|e| LatticeError::Storage(format!("Failed to prepare edge insert: {}", e)))?;

            for (from_node, to_node, edge_kind) in graph.all_edges() {
                insert_edge
                    .execute(params![
                        &from_node.id.file,
                        &from_node.id.name,
                        from_node.id.byte_offset as i64,
                        &to_node.id.file,
                        &to_node.id.name,
                        to_node.id.byte_offset as i64,
                        format!("{:?}", edge_kind),
                    ])
                    .map_err(|e| LatticeError::Storage(format!("Failed to insert edge: {}", e)))?;
            }
        }

        tx.commit()
            .map_err(|e| LatticeError::Storage(format!("Failed to commit transaction: {}", e)))?;

        Ok(())
    }

    pub fn load_file_index(&self) -> Result<HashMap<String, FileIndexEntry>, LatticeError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT file, content_hash, mtime_ns, size_bytes, parser_version, schema_version, last_indexed_at FROM file_index",
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to prepare file index query: {}", e)))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(FileIndexEntry {
                    file: row.get(0)?,
                    content_hash: row.get(1)?,
                    mtime_ns: row.get(2)?,
                    size_bytes: row.get(3)?,
                    parser_version: row.get(4)?,
                    schema_version: row.get(5)?,
                    last_indexed_at: row.get(6)?,
                })
            })
            .map_err(|e| LatticeError::Storage(format!("Failed to query file index: {}", e)))?;

        let mut entries = HashMap::new();
        for row in rows {
            let entry = row.map_err(|e| {
                LatticeError::Storage(format!("Failed to read file index row: {}", e))
            })?;
            entries.insert(entry.file.clone(), entry);
        }
        Ok(entries)
    }

    pub fn save_file_index(&self, entries: &[FileIndexEntry]) -> Result<(), LatticeError> {
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            LatticeError::Storage(format!("Failed to begin file index transaction: {}", e))
        })?;
        tx.execute("DELETE FROM file_index", [])
            .map_err(|e| LatticeError::Storage(format!("Failed to clear file index: {}", e)))?;
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO file_index (file, content_hash, mtime_ns, size_bytes, parser_version, schema_version, last_indexed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .map_err(|e| LatticeError::Storage(format!("Failed to prepare file index insert: {}", e)))?;
            for entry in entries {
                insert
                    .execute(params![
                        &entry.file,
                        &entry.content_hash,
                        entry.mtime_ns,
                        entry.size_bytes,
                        entry.parser_version,
                        entry.schema_version,
                        entry.last_indexed_at,
                    ])
                    .map_err(|e| {
                        LatticeError::Storage(format!("Failed to insert file index row: {}", e))
                    })?;
            }
        }
        tx.commit()
            .map_err(|e| LatticeError::Storage(format!("Failed to commit file index: {}", e)))?;
        Ok(())
    }

    pub fn load_parsed_files(&self) -> Result<HashMap<String, ParsedFile>, LatticeError> {
        let mut stmt = self
            .conn
            .prepare("SELECT file, payload FROM parsed_files")
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to prepare parsed files query: {}", e))
            })?;
        let rows = stmt
            .query_map([], |row| {
                let file: String = row.get(0)?;
                let payload: String = row.get(1)?;
                Ok((file, payload))
            })
            .map_err(|e| LatticeError::Storage(format!("Failed to query parsed files: {}", e)))?;

        let mut files = HashMap::new();
        for row in rows {
            let (file, payload) = row.map_err(|e| {
                LatticeError::Storage(format!("Failed to read parsed file row: {}", e))
            })?;
            let parsed: ParsedFile = serde_json::from_str(&payload).map_err(|e| {
                LatticeError::Storage(format!("Failed to deserialize parsed file {}: {}", file, e))
            })?;
            files.insert(file, parsed);
        }
        Ok(files)
    }

    pub fn save_parsed_files(
        &self,
        parsed_files: &HashMap<String, ParsedFile>,
    ) -> Result<(), LatticeError> {
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            LatticeError::Storage(format!("Failed to begin parsed files transaction: {}", e))
        })?;
        tx.execute("DELETE FROM parsed_files", [])
            .map_err(|e| LatticeError::Storage(format!("Failed to clear parsed files: {}", e)))?;
        {
            let mut insert = tx
                .prepare("INSERT INTO parsed_files (file, payload) VALUES (?1, ?2)")
                .map_err(|e| {
                    LatticeError::Storage(format!("Failed to prepare parsed file insert: {}", e))
                })?;
            for (file, parsed) in parsed_files {
                let payload = serde_json::to_string(parsed).map_err(|e| {
                    LatticeError::Storage(format!(
                        "Failed to serialize parsed file {}: {}",
                        file, e
                    ))
                })?;
                insert.execute(params![file, payload]).map_err(|e| {
                    LatticeError::Storage(format!("Failed to insert parsed file {}: {}", file, e))
                })?;
            }
        }
        tx.commit()
            .map_err(|e| LatticeError::Storage(format!("Failed to commit parsed files: {}", e)))?;
        Ok(())
    }

    /// Load a CodeGraph from the database.
    pub fn load_graph(&self) -> Result<CodeGraph, LatticeError> {
        let mut graph = CodeGraph::new();

        // Load all nodes
        let mut stmt = self
            .conn
            .prepare(
                "SELECT file, name, byte_offset, kind, signature, body, line, end_line, is_exported, language, edit_count, last_modified FROM nodes",
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to prepare node query: {}", e)))?;

        let node_rows = stmt
            .query_map([], |row| {
                let file: String = row.get(0)?;
                let name: String = row.get(1)?;
                let byte_offset: i64 = row.get(2)?;
                let kind_str: String = row.get(3)?;
                let signature: String = row.get(4)?;
                let body: String = row.get(5)?;
                let line: i64 = row.get(6)?;
                let end_line: i64 = row.get(7)?;
                let is_exported: i32 = row.get(8)?;
                let language_str: String = row.get(9)?;
                let edit_count: i64 = row.get(10)?;
                let last_modified: i64 = row.get(11)?;

                Ok((
                    file,
                    name,
                    byte_offset,
                    kind_str,
                    signature,
                    body,
                    line,
                    end_line,
                    is_exported,
                    language_str,
                    edit_count,
                    last_modified,
                ))
            })
            .map_err(|e| LatticeError::Storage(format!("Failed to query nodes: {}", e)))?;

        for row in node_rows {
            let (
                file,
                name,
                byte_offset,
                kind_str,
                signature,
                body,
                line,
                end_line,
                is_exported,
                language_str,
                edit_count,
                last_modified,
            ) =
                row.map_err(|e| LatticeError::Storage(format!("Failed to read node row: {}", e)))?;

            let kind = parse_symbol_kind(&kind_str).ok_or_else(|| {
                LatticeError::Storage(format!("Unknown SymbolKind: {}", kind_str))
            })?;
            let language = parse_language(&language_str).ok_or_else(|| {
                LatticeError::Storage(format!("Unknown Language: {}", language_str))
            })?;

            let id = SymbolId {
                file: file.clone(),
                name: name.clone(),
                byte_offset: byte_offset as usize,
            };

            let idx = graph.add_node(
                id,
                kind,
                name,
                signature,
                body,
                file,
                line as usize,
                end_line as usize,
                is_exported != 0,
                language,
            );

            // Update edit_count and last_modified on the node
            if let Some(node) = graph.get_node_mut_by_index(idx) {
                node.edit_count = edit_count as u32;
                node.last_modified = last_modified as u64;
            }
        }

        // Load all edges
        let mut stmt = self
            .conn
            .prepare(
                "SELECT from_file, from_name, from_offset, to_file, to_name, to_offset, kind FROM edges",
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to prepare edge query: {}", e)))?;

        let edge_rows = stmt
            .query_map([], |row| {
                let from_file: String = row.get(0)?;
                let from_name: String = row.get(1)?;
                let from_offset: i64 = row.get(2)?;
                let to_file: String = row.get(3)?;
                let to_name: String = row.get(4)?;
                let to_offset: i64 = row.get(5)?;
                let kind_str: String = row.get(6)?;

                Ok((
                    from_file,
                    from_name,
                    from_offset,
                    to_file,
                    to_name,
                    to_offset,
                    kind_str,
                ))
            })
            .map_err(|e| LatticeError::Storage(format!("Failed to query edges: {}", e)))?;

        for row in edge_rows {
            let (from_file, from_name, from_offset, to_file, to_name, to_offset, kind_str) =
                row.map_err(|e| LatticeError::Storage(format!("Failed to read edge row: {}", e)))?;

            let edge_kind = parse_edge_kind(&kind_str)
                .ok_or_else(|| LatticeError::Storage(format!("Unknown EdgeKind: {}", kind_str)))?;

            let from_id = SymbolId {
                file: from_file,
                name: from_name,
                byte_offset: from_offset as usize,
            };
            let to_id = SymbolId {
                file: to_file,
                name: to_name,
                byte_offset: to_offset as usize,
            };

            graph.add_edge(&from_id, &to_id, edge_kind);
        }

        Ok(graph)
    }

    /// Count distinct files in the persisted graph without materializing node bodies.
    pub fn persisted_graph_file_count(&self) -> Result<usize, LatticeError> {
        self.conn
            .query_row("SELECT COUNT(DISTINCT file) FROM nodes", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| count.max(0) as usize)
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to count persisted graph files: {}", e))
            })
    }

    /// Sum the SQLite database, WAL, and SHM file sizes for warm-load safety checks.
    pub fn persisted_graph_disk_bytes(&self) -> Result<Option<u64>, LatticeError> {
        let Some(path) = &self.path else {
            return Ok(None);
        };

        let mut total = file_len_if_exists(path)?;
        total = total.saturating_add(file_len_if_exists(&path.with_extension("db-wal"))?);
        total = total.saturating_add(file_len_if_exists(&path.with_extension("db-shm"))?);
        Ok(Some(total))
    }
}

fn validate_integrity(conn: &Connection, path: &Path) -> Result<(), LatticeError> {
    let result: String = conn
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .map_err(|error| map_sqlite_error(path, "validate database integrity", error))?;
    if result.eq_ignore_ascii_case("ok") {
        Ok(())
    } else {
        Err(LatticeError::CorruptStorage {
            path: path.display().to_string(),
            message: result,
        })
    }
}

fn map_sqlite_error(path: &Path, operation: &str, error: rusqlite::Error) -> LatticeError {
    use rusqlite::ErrorCode;

    if matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    ) {
        LatticeError::CorruptStorage {
            path: path.display().to_string(),
            message: format!("Failed to {}: {}", operation, error),
        }
    } else {
        LatticeError::Storage(format!("Failed to {}: {}", operation, error))
    }
}

fn reject_symlink(path: &Path) -> Result<(), LatticeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(LatticeError::Storage(format!(
            "Refusing to open graph database through symlink: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(LatticeError::Storage(format!(
            "Failed to inspect graph database path {}: {}",
            path.display(),
            error
        ))),
    }
}

fn remove_derived_graph_files(path: &Path) -> Result<(), LatticeError> {
    for target in [
        path.to_path_buf(),
        sqlite_sidecar(path, "-wal"),
        sqlite_sidecar(path, "-shm"),
    ] {
        match fs::remove_file(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(LatticeError::Storage(format!(
                    "Failed to remove corrupt derived graph file {}: {}",
                    target.display(),
                    error
                )))
            }
        }
    }
    Ok(())
}

fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn file_len_if_exists(path: &Path) -> Result<u64, LatticeError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(LatticeError::Storage(format!(
            "Failed to read persisted graph file size for {}: {}",
            path.display(),
            error
        ))),
    }
}

/// Parse a SymbolKind from its Debug format string.
fn parse_symbol_kind(s: &str) -> Option<SymbolKind> {
    match s {
        "Function" => Some(SymbolKind::Function),
        "Class" => Some(SymbolKind::Class),
        "Interface" => Some(SymbolKind::Interface),
        "TypeAlias" => Some(SymbolKind::TypeAlias),
        "Enum" => Some(SymbolKind::Enum),
        "Module" => Some(SymbolKind::Module),
        "Variable" => Some(SymbolKind::Variable),
        "Constant" => Some(SymbolKind::Constant),
        "Method" => Some(SymbolKind::Method),
        "Trait" => Some(SymbolKind::Trait),
        "Struct" => Some(SymbolKind::Struct),
        "Document" => Some(SymbolKind::Document),
        "Section" => Some(SymbolKind::Section),
        _ => None,
    }
}

/// Parse a Language from its Debug format string.
fn parse_language(s: &str) -> Option<Language> {
    match s {
        "TypeScript" => Some(Language::TypeScript),
        "JavaScript" => Some(Language::JavaScript),
        "Python" => Some(Language::Python),
        "Rust" => Some(Language::Rust),
        "Go" => Some(Language::Go),
        "Java" => Some(Language::Java),
        "Markdown" => Some(Language::Markdown),
        "Unknown" => Some(Language::Unknown),
        _ => None,
    }
}

/// Parse an EdgeKind from its Debug format string.
fn parse_edge_kind(s: &str) -> Option<EdgeKind> {
    match s {
        "Calls" => Some(EdgeKind::Calls),
        "Imports" => Some(EdgeKind::Imports),
        "Implements" => Some(EdgeKind::Implements),
        "Extends" => Some(EdgeKind::Extends),
        "TypeRef" => Some(EdgeKind::TypeRef),
        "Contains" => Some(EdgeKind::Contains),
        "LinksTo" => Some(EdgeKind::LinksTo),
        "Mentions" => Some(EdgeKind::Mentions),
        "CoChanges" => Some(EdgeKind::CoChanges),
        _ => None,
    }
}
