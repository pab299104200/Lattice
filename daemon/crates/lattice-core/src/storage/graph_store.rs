use super::schema::{CREATE_NODES_TABLE, CREATE_NODE_INDEX, CREATE_TABLES};
use crate::error::LatticeError;
use crate::graph::digest::{
    generate_module_digests, GeneratedModuleDigest, ModuleDigestPayload,
    MODULE_DIGEST_GENERATOR_VERSION, MODULE_DIGEST_SCHEMA_VERSION,
};
use crate::graph::model::CodeGraph;
use crate::graph::model::EdgeKind;
use crate::symbols::{Language, SymbolId, SymbolKind};
use rusqlite::{params, Connection};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const FILE_INDEX_PARSER_VERSION: i64 = 1;
pub const FILE_INDEX_SCHEMA_VERSION: i64 = 1;

const MODULE_DIGEST_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS graph_metadata (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    index_epoch INTEGER NOT NULL CHECK (index_epoch >= 0),
    digest_schema_version INTEGER NOT NULL CHECK (digest_schema_version > 0)
);

INSERT OR IGNORE INTO graph_metadata
    (singleton, index_epoch, digest_schema_version)
VALUES
    (1, 0, 1);

CREATE TABLE IF NOT EXISTS module_digests (
    module_path TEXT PRIMARY KEY,
    index_epoch INTEGER NOT NULL CHECK (index_epoch > 0),
    generator_version INTEGER NOT NULL CHECK (generator_version > 0),
    input_fingerprint TEXT NOT NULL CHECK (length(input_fingerprint) = 64),
    payload_sha256 TEXT NOT NULL CHECK (length(payload_sha256) = 64),
    payload_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_module_digests_epoch
    ON module_digests(index_epoch, module_path);
"#;

/// Immutable, validated module digests for one committed graph generation.
#[derive(Debug, Clone)]
pub struct ModuleDigestCache {
    epoch: u64,
    digests: Arc<BTreeMap<String, GeneratedModuleDigest>>,
    token_postings: Arc<BTreeMap<String, Arc<[String]>>>,
}

impl ModuleDigestCache {
    fn new(epoch: u64, digests: Vec<GeneratedModuleDigest>) -> Self {
        let digests = digests
            .into_iter()
            .map(|digest| (digest.module_path.clone(), digest))
            .collect::<BTreeMap<_, _>>();
        let mut postings = BTreeMap::<String, BTreeSet<String>>::new();
        for (path, digest) in &digests {
            for term in &digest.payload.search_terms {
                postings
                    .entry(term.clone())
                    .or_default()
                    .insert(path.clone());
            }
        }
        let token_postings = postings
            .into_iter()
            .map(|(term, paths)| {
                let paths = paths.into_iter().collect::<Vec<_>>();
                (term, Arc::<[String]>::from(paths))
            })
            .collect();
        Self {
            epoch,
            digests: Arc::new(digests),
            token_postings: Arc::new(token_postings),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn len(&self) -> usize {
        self.digests.len()
    }

    pub fn is_empty(&self) -> bool {
        self.digests.is_empty()
    }

    pub fn get(&self, module_path: &str) -> Option<&GeneratedModuleDigest> {
        self.digests.get(module_path)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &GeneratedModuleDigest)> {
        self.digests
            .iter()
            .map(|(path, digest)| (path.as_str(), digest))
    }

    pub fn postings(&self, token: &str) -> &[String] {
        self.token_postings
            .get(token)
            .map(AsRef::as_ref)
            .unwrap_or_default()
    }
}

/// Graph and digest cache committed under the same monotonically increasing epoch.
#[derive(Clone)]
pub struct IndexSnapshot {
    pub epoch: u64,
    pub graph: Arc<CodeGraph>,
    pub module_digests: ModuleDigestCache,
}

/// Result of warming graph state from storage.
pub enum IndexSnapshotLoad {
    Ready(IndexSnapshot),
    /// A pre-digest database remains usable for graph-backed fallback but needs reindexing.
    DigestCacheMissing {
        graph: Arc<CodeGraph>,
    },
}

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
        self.migrate_legacy_nodes_schema()?;
        self.conn
            .execute_batch(CREATE_NODES_TABLE)
            .map_err(|e| match &self.path {
                Some(path) => map_sqlite_error(path, "initialize nodes schema", e),
                None => LatticeError::Storage(format!("Failed to initialize nodes schema: {}", e)),
            })?;
        self.conn
            .execute_batch(CREATE_TABLES)
            .map_err(|e| match &self.path {
                Some(path) => map_sqlite_error(path, "initialize supporting schema", e),
                None => {
                    LatticeError::Storage(format!("Failed to initialize supporting schema: {}", e))
                }
            })?;
        self.conn
            .execute_batch(CREATE_NODE_INDEX)
            .map_err(|e| match &self.path {
                Some(path) => map_sqlite_error(path, "initialize node index", e),
                None => LatticeError::Storage(format!("Failed to initialize node index: {}", e)),
            })?;
        self.conn
            .execute_batch(MODULE_DIGEST_TABLES)
            .map_err(|e| match &self.path {
                Some(path) => map_sqlite_error(path, "initialize module digest schema", e),
                None => LatticeError::Storage(format!(
                    "Failed to initialize module digest schema: {}",
                    e
                )),
            })?;
        Ok(())
    }

    /// Rebuild pre-removal node tables without disturbing the rest of the graph store.
    ///
    /// SQLite table rebuilds are transactional, so either the legacy table remains intact or
    /// the canonical table (and its index) is fully installed. The column check makes this safe
    /// to run on every open.
    fn migrate_legacy_nodes_schema(&self) -> Result<(), LatticeError> {
        const LEGACY_NODES_TABLE: &str = "nodes__lattice_legacy_edit_count";

        let tx = self.conn.unchecked_transaction().map_err(|e| {
            LatticeError::Storage(format!("Failed to begin nodes schema migration: {}", e))
        })?;
        let has_edit_count = {
            let mut columns = tx.prepare("PRAGMA table_info(nodes)").map_err(|e| {
                LatticeError::Storage(format!(
                    "Failed to inspect nodes schema for migration: {}",
                    e
                ))
            })?;
            let names = columns
                .query_map([], |row| row.get::<_, String>(1))
                .map_err(|e| {
                    LatticeError::Storage(format!(
                        "Failed to query nodes schema for migration: {}",
                        e
                    ))
                })?;
            let mut found = false;
            for name in names {
                if name.map_err(|e| {
                    LatticeError::Storage(format!(
                        "Failed to read nodes schema for migration: {}",
                        e
                    ))
                })? == "edit_count"
                {
                    found = true;
                    break;
                }
            }
            found
        };

        if has_edit_count {
            tx.execute(
                &format!("ALTER TABLE nodes RENAME TO {LEGACY_NODES_TABLE}"),
                [],
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to rename legacy nodes table: {}", e))
            })?;
            tx.execute_batch(CREATE_NODES_TABLE).map_err(|e| {
                LatticeError::Storage(format!("Failed to create migrated nodes table: {}", e))
            })?;
            tx.execute(
                &format!(
                    "INSERT INTO nodes (file, name, byte_offset, kind, signature, body, line, end_line, is_exported, language, last_modified) \
                     SELECT file, name, byte_offset, kind, signature, body, line, end_line, is_exported, language, last_modified \
                     FROM {LEGACY_NODES_TABLE}"
                ),
                [],
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to copy legacy nodes during migration: {}", e))
            })?;
            tx.execute(&format!("DROP TABLE {LEGACY_NODES_TABLE}"), [])
                .map_err(|e| {
                    LatticeError::Storage(format!("Failed to remove legacy nodes table: {}", e))
                })?;
            tx.execute_batch(CREATE_NODE_INDEX).map_err(|e| {
                LatticeError::Storage(format!("Failed to recreate migrated node index: {}", e))
            })?;
        }

        tx.commit().map_err(|e| {
            LatticeError::Storage(format!("Failed to commit nodes schema migration: {}", e))
        })
    }

    pub fn recovery(&self) -> GraphStoreRecovery {
        self.recovery
    }

    /// Save a CodeGraph to the database, replacing any previous data.
    pub fn save_graph(&self, graph: &CodeGraph) -> Result<(), LatticeError> {
        self.commit_graph_generation(graph).map(|_| ())
    }

    /// Atomically persist a graph generation and return the exact immutable snapshot committed.
    pub fn save_index_snapshot(&self, graph: &CodeGraph) -> Result<IndexSnapshot, LatticeError> {
        let module_digests = self.commit_graph_generation(graph)?;
        Ok(IndexSnapshot {
            epoch: module_digests.epoch(),
            graph: Arc::new(graph.clone()),
            module_digests,
        })
    }

    fn commit_graph_generation(
        &self,
        graph: &CodeGraph,
    ) -> Result<ModuleDigestCache, LatticeError> {
        // Generation happens before SQLite mutation so any invalid graph leaves the prior
        // generation wholly intact.
        let digests = generate_module_digests(graph).map_err(|error| {
            LatticeError::Storage(format!("Failed to generate module digests: {}", error))
        })?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| LatticeError::Storage(format!("Failed to begin transaction: {}", e)))?;

        let current_epoch: i64 = tx
            .query_row(
                "SELECT index_epoch FROM graph_metadata WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to load graph index epoch: {}", e))
            })?;
        let next_epoch = current_epoch.checked_add(1).ok_or_else(|| {
            LatticeError::Storage("Graph index epoch exhausted at i64::MAX".to_string())
        })?;
        if next_epoch <= 0 {
            return Err(LatticeError::Storage(format!(
                "Invalid persisted graph index epoch: {}",
                current_epoch
            )));
        }

        // Delete all existing data
        tx.execute("DELETE FROM edges", [])
            .map_err(|e| LatticeError::Storage(format!("Failed to clear edges: {}", e)))?;
        tx.execute("DELETE FROM nodes", [])
            .map_err(|e| LatticeError::Storage(format!("Failed to clear nodes: {}", e)))?;
        tx.execute("DELETE FROM module_digests", [])
            .map_err(|e| LatticeError::Storage(format!("Failed to clear module digests: {}", e)))?;

        // Insert all nodes (scoped so prepared statement is dropped before commit)
        {
            let mut insert_node = tx
                .prepare(
                    "INSERT INTO nodes (file, name, byte_offset, kind, signature, body, line, end_line, is_exported, language, last_modified) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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

        {
            let mut insert_digest = tx
                .prepare(
                    "INSERT INTO module_digests (module_path, index_epoch, generator_version, input_fingerprint, payload_sha256, payload_json) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .map_err(|e| {
                    LatticeError::Storage(format!(
                        "Failed to prepare module digest insert: {}",
                        e
                    ))
                })?;
            for digest in &digests {
                insert_digest
                    .execute(params![
                        &digest.module_path,
                        next_epoch,
                        i64::from(digest.generator_version),
                        &digest.input_fingerprint,
                        &digest.payload_sha256,
                        &digest.payload_json,
                    ])
                    .map_err(|e| {
                        LatticeError::Storage(format!(
                            "Failed to insert module digest {}: {}",
                            digest.module_path, e
                        ))
                    })?;
            }
        }

        tx.execute(
            "UPDATE graph_metadata SET index_epoch = ?1, digest_schema_version = ?2 WHERE singleton = 1",
            params![next_epoch, i64::from(MODULE_DIGEST_SCHEMA_VERSION)],
        )
        .map_err(|e| {
            LatticeError::Storage(format!("Failed to publish graph index epoch: {}", e))
        })?;

        tx.commit()
            .map_err(|e| LatticeError::Storage(format!("Failed to commit transaction: {}", e)))?;

        Ok(ModuleDigestCache::new(next_epoch as u64, digests))
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

    /// Load a CodeGraph from the database.
    pub fn load_graph(&self) -> Result<CodeGraph, LatticeError> {
        Self::load_graph_from(&self.conn)
    }

    /// Load graph and digest state under one SQLite read transaction.
    pub fn load_index_snapshot(&self) -> Result<IndexSnapshotLoad, LatticeError> {
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            LatticeError::Storage(format!("Failed to begin graph snapshot read: {}", e))
        })?;
        let graph = Arc::new(Self::load_graph_from(&tx)?);
        let (metadata_rows, epoch, schema_version): (i64, i64, i64) = tx
            .query_row(
                "SELECT COUNT(*), COALESCE(MAX(index_epoch), 0), COALESCE(MAX(digest_schema_version), 0) FROM graph_metadata",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to load graph digest metadata: {}", e))
            })?;
        if metadata_rows != 1 {
            return Err(LatticeError::Storage(format!(
                "Invalid graph digest metadata: expected one singleton row, found {}",
                metadata_rows
            )));
        }
        if epoch < 0 {
            return Err(LatticeError::Storage(format!(
                "Invalid graph digest metadata epoch: {}",
                epoch
            )));
        }
        if epoch == 0 {
            let digest_rows: i64 = tx
                .query_row("SELECT COUNT(*) FROM module_digests", [], |row| row.get(0))
                .map_err(|e| {
                    LatticeError::Storage(format!(
                        "Failed to inspect epoch-zero module digest cache: {}",
                        e
                    ))
                })?;
            if digest_rows != 0 {
                return Err(LatticeError::Storage(format!(
                    "Invalid graph digest cache at epoch 0: found {} module rows",
                    digest_rows
                )));
            }
            tx.commit().map_err(|e| {
                LatticeError::Storage(format!("Failed to finish graph snapshot read: {}", e))
            })?;
            return Ok(IndexSnapshotLoad::DigestCacheMissing { graph });
        }
        if schema_version != i64::from(MODULE_DIGEST_SCHEMA_VERSION) {
            return Err(LatticeError::Storage(format!(
                "Invalid graph digest metadata at epoch {}: expected schema version {}, found {}",
                epoch, MODULE_DIGEST_SCHEMA_VERSION, schema_version
            )));
        }

        let digests = Self::load_and_validate_module_digests(&tx, &graph, epoch)?;
        tx.commit().map_err(|e| {
            LatticeError::Storage(format!("Failed to finish graph snapshot read: {}", e))
        })?;
        let module_digests = ModuleDigestCache::new(epoch as u64, digests);
        Ok(IndexSnapshotLoad::Ready(IndexSnapshot {
            epoch: epoch as u64,
            graph,
            module_digests,
        }))
    }

    fn load_and_validate_module_digests(
        conn: &Connection,
        graph: &CodeGraph,
        epoch: i64,
    ) -> Result<Vec<GeneratedModuleDigest>, LatticeError> {
        let expected = generate_module_digests(graph).map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to validate module digests at epoch {} against graph: {}",
                epoch, error
            ))
        })?;
        let expected = expected
            .into_iter()
            .map(|digest| (digest.module_path.clone(), digest))
            .collect::<BTreeMap<_, _>>();
        let mut stmt = conn
            .prepare(
                "SELECT module_path, index_epoch, generator_version, input_fingerprint, payload_sha256, payload_json FROM module_digests ORDER BY module_path",
            )
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to prepare module digest query: {}", e))
            })?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .map_err(|e| LatticeError::Storage(format!("Failed to query module digests: {}", e)))?;

        let mut hydrated = Vec::with_capacity(expected.len());
        for row in rows {
            let (
                path,
                row_epoch,
                generator_version,
                input_fingerprint,
                payload_sha256,
                payload_json,
            ) = row.map_err(|e| {
                LatticeError::Storage(format!("Failed to read module digest row: {}", e))
            })?;
            let invalid = |reason: &str| {
                LatticeError::Storage(format!(
                    "Invalid module digest {:?} at epoch {}: {}",
                    path, epoch, reason
                ))
            };
            if row_epoch != epoch {
                return Err(invalid(&format!(
                    "row epoch {} does not match metadata epoch",
                    row_epoch
                )));
            }
            let expected_digest = expected
                .get(&path)
                .ok_or_else(|| invalid("module is absent from the committed graph"))?;
            if generator_version != i64::from(MODULE_DIGEST_GENERATOR_VERSION) {
                return Err(invalid(&format!(
                    "expected generator version {}, found {}",
                    MODULE_DIGEST_GENERATOR_VERSION, generator_version
                )));
            }
            if input_fingerprint != expected_digest.input_fingerprint {
                return Err(invalid(
                    "input fingerprint does not match the committed graph",
                ));
            }
            if payload_sha256 != expected_digest.payload_sha256 {
                return Err(invalid(
                    "payload SHA-256 does not match the generated payload",
                ));
            }
            let payload: ModuleDigestPayload = serde_json::from_str(&payload_json)
                .map_err(|e| invalid(&format!("payload JSON cannot be decoded: {}", e)))?;
            if payload.schema_version != MODULE_DIGEST_SCHEMA_VERSION {
                return Err(invalid(&format!(
                    "expected payload schema version {}, found {}",
                    MODULE_DIGEST_SCHEMA_VERSION, payload.schema_version
                )));
            }
            if payload.module_path != path {
                return Err(invalid("payload module path does not match its row key"));
            }
            let canonical = serde_json::to_string(&payload)
                .map_err(|e| invalid(&format!("payload cannot be canonically encoded: {}", e)))?;
            if canonical != payload_json {
                return Err(invalid("payload JSON is not canonical"));
            }
            if payload_json != expected_digest.payload_json || payload != expected_digest.payload {
                return Err(invalid("payload facts do not match the committed graph"));
            }
            hydrated.push(expected_digest.clone());
        }
        if hydrated.len() != expected.len() {
            let loaded = hydrated
                .iter()
                .map(|digest| digest.module_path.as_str())
                .collect::<BTreeSet<_>>();
            let missing = expected
                .keys()
                .filter(|path| !loaded.contains(path.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            return Err(LatticeError::Storage(format!(
                "Invalid module digest cache at epoch {}: missing rows for {}",
                epoch,
                missing.join(", ")
            )));
        }
        Ok(hydrated)
    }

    fn load_graph_from(conn: &Connection) -> Result<CodeGraph, LatticeError> {
        let mut graph = CodeGraph::new();

        // Load all nodes
        let mut stmt = conn
            .prepare(
                "SELECT file, name, byte_offset, kind, signature, body, line, end_line, is_exported, language, last_modified FROM nodes",
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
                let last_modified: i64 = row.get(10)?;

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

            // Restore persisted recency metadata after constructing the node.
            if let Some(node) = graph.get_node_mut_by_index(idx) {
                node.last_modified = last_modified as u64;
            }
        }

        // Load all edges
        let mut stmt = conn
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

#[cfg(test)]
mod module_digest_tests {
    use super::*;

    const LEGACY_SUPPORTING_TABLES: &str = r#"
CREATE TABLE edges (
    from_file TEXT NOT NULL,
    from_name TEXT NOT NULL,
    from_offset INTEGER NOT NULL,
    to_file TEXT NOT NULL,
    to_name TEXT NOT NULL,
    to_offset INTEGER NOT NULL,
    kind TEXT NOT NULL
);
CREATE TABLE file_index (
    file TEXT PRIMARY KEY,
    content_hash TEXT NOT NULL,
    mtime_ns INTEGER NOT NULL,
    size_bytes INTEGER NOT NULL,
    parser_version INTEGER NOT NULL,
    schema_version INTEGER NOT NULL,
    last_indexed_at INTEGER NOT NULL
);
CREATE TABLE parsed_files (
    file TEXT PRIMARY KEY,
    payload TEXT NOT NULL
);
"#;

    fn id(file: &str, name: &str) -> SymbolId {
        SymbolId {
            file: file.to_string(),
            name: name.to_string(),
            byte_offset: 0,
        }
    }

    fn graph(files: &[&str]) -> CodeGraph {
        let mut graph = CodeGraph::new();
        for (line, file) in files.iter().enumerate() {
            let name = format!("symbol_{line}");
            graph.add_node(
                id(file, &name),
                SymbolKind::Function,
                name.clone(),
                format!("fn {name}()"),
                "{}",
                (*file).to_string(),
                line + 1,
                line + 1,
                true,
                Language::Rust,
            );
        }
        graph
    }

    fn ready(load: IndexSnapshotLoad) -> IndexSnapshot {
        match load {
            IndexSnapshotLoad::Ready(snapshot) => snapshot,
            IndexSnapshotLoad::DigestCacheMissing { .. } => panic!("expected ready snapshot"),
        }
    }

    fn column_names(conn: &Connection, table: &str) -> Vec<String> {
        let mut statement = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        statement
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn opening_legacy_store_removes_edit_count_and_preserves_persisted_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy-graph.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            r#"
CREATE TABLE nodes (
    file TEXT NOT NULL,
    name TEXT NOT NULL,
    byte_offset INTEGER NOT NULL,
    kind TEXT NOT NULL,
    signature TEXT NOT NULL,
    body TEXT NOT NULL,
    line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    is_exported INTEGER NOT NULL,
    language TEXT NOT NULL,
    edit_count INTEGER NOT NULL DEFAULT 0,
    last_modified INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (file, name, byte_offset)
);
CREATE INDEX idx_nodes_file ON nodes(file);
{LEGACY_SUPPORTING_TABLES}
INSERT INTO nodes VALUES
    ('src/a.rs', 'a', 0, 'Function', 'fn a()', '{{}}', 1, 1, 1, 'Rust', 19, 101),
    ('src/b.rs', 'b', 7, 'Function', 'fn b()', '{{}}', 2, 2, 0, 'Rust', 23, 202);
INSERT INTO edges VALUES ('src/a.rs', 'a', 0, 'src/b.rs', 'b', 7, 'Calls');
INSERT INTO file_index VALUES ('src/a.rs', 'hash', 11, 22, 1, 1, 33);
INSERT INTO parsed_files VALUES ('src/a.rs', '{{"legacy":true}}');
"#
        ))
        .unwrap();
        drop(conn);

        let store = GraphStore::open(&path).unwrap();
        let columns = column_names(&store.conn, "nodes");
        assert!(!columns.iter().any(|column| column == "edit_count"));
        assert_eq!(
            columns,
            [
                "file",
                "name",
                "byte_offset",
                "kind",
                "signature",
                "body",
                "line",
                "end_line",
                "is_exported",
                "language",
                "last_modified",
            ]
        );

        let graph = store.load_graph().unwrap();
        assert_eq!(graph.node_count(), 2);
        assert_eq!(graph.edge_count(), 1);
        assert_eq!(
            graph.get_node(&id("src/a.rs", "a")).unwrap().last_modified,
            101
        );
        assert_eq!(
            store.load_file_index().unwrap()["src/a.rs"].content_hash,
            "hash"
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT payload FROM parsed_files WHERE file = 'src/a.rs'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            r#"{"legacy":true}"#
        );
        drop(store);

        // Reopening exercises the no-op path and must not rebuild or discard any data.
        let reopened = GraphStore::open(&path).unwrap();
        assert!(!column_names(&reopened.conn, "nodes")
            .iter()
            .any(|column| column == "edit_count"));
        assert_eq!(reopened.load_graph().unwrap().node_count(), 2);
        assert_eq!(reopened.load_graph().unwrap().edge_count(), 1);
    }

    #[test]
    fn failed_legacy_nodes_rebuild_rolls_back_the_entire_migration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid-legacy-graph.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            r#"
CREATE TABLE nodes (
    file TEXT NOT NULL,
    name TEXT NOT NULL,
    byte_offset INTEGER NOT NULL,
    kind TEXT NOT NULL,
    signature TEXT NOT NULL,
    body TEXT NOT NULL,
    line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    is_exported INTEGER NOT NULL,
    language TEXT NOT NULL,
    edit_count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (file, name, byte_offset)
);
{LEGACY_SUPPORTING_TABLES}
INSERT INTO nodes VALUES
    ('src/a.rs', 'a', 0, 'Function', 'fn a()', '{{}}', 1, 1, 1, 'Rust', 19);
INSERT INTO edges VALUES ('src/a.rs', 'a', 0, 'src/a.rs', 'a', 0, 'Calls');
INSERT INTO file_index VALUES ('src/a.rs', 'hash', 11, 22, 1, 1, 33);
"#
        ))
        .unwrap();
        drop(conn);

        let error = GraphStore::open(&path).err().unwrap().to_string();
        assert!(
            error.contains("Failed to copy legacy nodes during migration"),
            "{error}"
        );

        let conn = Connection::open(&path).unwrap();
        assert!(column_names(&conn, "nodes")
            .iter()
            .any(|column| column == "edit_count"));
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM edges", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM file_index", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'nodes__lattice_legacy_edit_count'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn fresh_store_reports_digest_cache_missing_without_rejecting_graph() {
        let store = GraphStore::open_in_memory().unwrap();
        match store.load_index_snapshot().unwrap() {
            IndexSnapshotLoad::DigestCacheMissing { graph } => {
                assert_eq!(graph.node_count(), 0);
            }
            IndexSnapshotLoad::Ready(_) => panic!("fresh store must have epoch zero"),
        }
    }

    #[test]
    fn snapshot_round_trip_hydrates_digests_and_sorted_postings() {
        let store = GraphStore::open_in_memory().unwrap();
        let saved = store
            .save_index_snapshot(&graph(&["src/z.rs", "src/a.rs"]))
            .unwrap();
        let loaded = ready(store.load_index_snapshot().unwrap());

        assert_eq!(saved.epoch, 1);
        assert_eq!(loaded.epoch, saved.epoch);
        assert_eq!(loaded.graph.node_count(), 2);
        assert_eq!(loaded.module_digests.len(), 2);
        assert_eq!(
            loaded.module_digests.postings("rust"),
            &["src/a.rs".to_string(), "src/z.rs".to_string()]
        );
    }

    #[test]
    fn replacement_advances_epoch_and_removes_vanished_modules() {
        let store = GraphStore::open_in_memory().unwrap();
        store
            .save_index_snapshot(&graph(&["src/a.rs", "src/b.rs"]))
            .unwrap();
        let replaced = store.save_index_snapshot(&graph(&["src/b.rs"])).unwrap();
        let loaded = ready(store.load_index_snapshot().unwrap());

        assert_eq!(replaced.epoch, 2);
        assert_eq!(loaded.module_digests.len(), 1);
        assert!(loaded.module_digests.get("src/a.rs").is_none());
        assert!(loaded.module_digests.get("src/b.rs").is_some());
    }

    #[test]
    fn hydration_rejects_tampered_payload_instead_of_serving_subset() {
        let store = GraphStore::open_in_memory().unwrap();
        store
            .save_index_snapshot(&graph(&["src/a.rs", "src/b.rs"]))
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE module_digests SET payload_json = '{}' WHERE module_path = 'src/a.rs'",
                [],
            )
            .unwrap();

        let error = store.load_index_snapshot().err().unwrap().to_string();
        assert!(error.contains("src/a.rs"), "{error}");
        assert!(error.contains("payload JSON cannot be decoded"), "{error}");
    }

    #[test]
    fn digest_generation_failure_preserves_prior_generation() {
        let store = GraphStore::open_in_memory().unwrap();
        store.save_index_snapshot(&graph(&["src/good.rs"])).unwrap();
        let invalid = graph(&["/absolute.rs"]);

        assert!(store.save_index_snapshot(&invalid).is_err());
        let loaded = ready(store.load_index_snapshot().unwrap());
        assert_eq!(loaded.epoch, 1);
        assert!(loaded.module_digests.get("src/good.rs").is_some());
        assert!(loaded.module_digests.get("/absolute.rs").is_none());
    }

    #[test]
    fn digest_persistence_failure_rolls_back_graph_digests_and_epoch() {
        let store = GraphStore::open_in_memory().unwrap();
        store.save_index_snapshot(&graph(&["src/old.rs"])).unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER reject_new_digest BEFORE INSERT ON module_digests \
                 WHEN NEW.module_path = 'src/new.rs' BEGIN SELECT RAISE(ABORT, 'rejected'); END;",
            )
            .unwrap();

        assert!(store.save_index_snapshot(&graph(&["src/new.rs"])).is_err());
        let loaded = ready(store.load_index_snapshot().unwrap());
        assert_eq!(loaded.epoch, 1);
        assert!(loaded.module_digests.get("src/old.rs").is_some());
        assert!(loaded.module_digests.get("src/new.rs").is_none());
    }

    #[test]
    fn empty_graph_commits_positive_epoch_and_empty_cache() {
        let store = GraphStore::open_in_memory().unwrap();
        let saved = store.save_index_snapshot(&CodeGraph::new()).unwrap();
        let loaded = ready(store.load_index_snapshot().unwrap());

        assert_eq!(saved.epoch, 1);
        assert_eq!(loaded.epoch, 1);
        assert!(loaded.module_digests.is_empty());
        assert_eq!(loaded.graph.node_count(), 0);
    }
}
