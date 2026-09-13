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
use sha2::Digest;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::ops::Deref;
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
    conn: GraphConnection,
    path: Option<PathBuf>,
    recovery: GraphStoreRecovery,
    body_objects: Option<super::ContentObjectStore>,
    object_owner_id: Option<String>,
    publication: Option<super::cache_publication::CachePublicationAuthority>,
}

enum GraphConnection {
    Managed(super::managed_sqlite::ManagedSqlite),
    Direct(Connection),
}
impl Deref for GraphConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        match self {
            Self::Managed(v) => v,
            Self::Direct(v) => v,
        }
    }
}

impl GraphStore {
    /// Open a file-based SQLite database with WAL mode enabled.
    pub fn open(path: &Path) -> Result<Self, LatticeError> {
        reject_symlink(path)?;
        let file_name = path.file_name().ok_or_else(|| {
            LatticeError::Storage(format!(
                "Graph database path has no file name: {}",
                path.display()
            ))
        })?;
        let parent = path.parent().ok_or_else(|| {
            LatticeError::Storage(format!(
                "Graph database path has no parent: {}",
                path.display()
            ))
        })?;
        let resolved_path = parent
            .canonicalize()
            .map_err(|e| {
                LatticeError::Storage(format!("Failed to resolve graph database parent: {e}"))
            })?
            .join(file_name);
        let (conn, publication, _opening_accounting) =
            match super::cache_publication::CachePublicationAuthority::for_path(&resolved_path)? {
                Some((authority, leaf)) => {
                    let guard = authority.begin()?;
                    let connection = authority
                        .open_sqlite(
                            &leaf,
                            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
                        )
                        .map_err(|e| {
                            map_sqlite_error(&resolved_path, "open managed database", e)
                        })?;
                    (
                        GraphConnection::Managed(connection),
                        Some(authority),
                        Some(guard),
                    )
                }
                None => (
                    GraphConnection::Direct(
                        Connection::open_with_flags(
                            &resolved_path,
                            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
                        )
                        .map_err(|e| map_sqlite_error(&resolved_path, "open database", e))?,
                    ),
                    None,
                    None,
                ),
            };
        validate_integrity(&conn, &resolved_path)?;

        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| map_sqlite_error(&resolved_path, "set WAL mode", e))?;

        let store = Self {
            conn,
            path: Some(resolved_path),
            recovery: GraphStoreRecovery::None,
            body_objects: None,
            object_owner_id: None,
            publication,
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

    pub fn open_recovering_with_objects(
        path: &Path,
        objects: &Path,
        checkout_id: &str,
    ) -> Result<Self, LatticeError> {
        let object_store = super::ContentObjectStore::open(objects)?;
        let mut store = Self::open_recovering(path)?;
        store.body_objects = Some(object_store);
        store.object_owner_id = Some(checkout_id.to_owned());
        store.migrate_inline_bodies()?;
        store
            .body_objects
            .as_ref()
            .expect("set above")
            .recover_owner(checkout_id, &store.conn)?;
        Ok(store)
    }

    fn migrate_inline_bodies(&self) -> Result<(), LatticeError> {
        let Some(objects) = &self.body_objects else {
            return Ok(());
        };
        let rows = {
            let mut statement = self.conn.prepare("SELECT file,name,byte_offset,body FROM nodes WHERE body_hash IS NULL AND body != ''").map_err(|e| LatticeError::Storage(format!("Failed to inspect inline symbol bodies: {e}")))?;
            let collected = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| {
                    LatticeError::Storage(format!("Failed to read inline symbol bodies: {e}"))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| LatticeError::Storage(e.to_string()))?;
            collected
        };
        if rows.is_empty() {
            return Ok(());
        }
        self.path.as_ref().ok_or_else(|| {
            LatticeError::Storage(
                "repository object storage requires a file-backed graph database".into(),
            )
        })?;
        let proposed = rows
            .iter()
            .map(|(_, _, _, body)| format!("{:x}", sha2::Sha256::digest(body.as_bytes())))
            .collect::<BTreeSet<_>>();
        let owner = self.object_owner_id.as_deref().ok_or_else(|| {
            LatticeError::Storage(
                "repository object storage requires an explicit checkout owner".into(),
            )
        })?;
        let publication = objects.begin_publication(owner, proposed)?;
        let staged = rows
            .into_iter()
            .map(|(file, name, offset, body)| {
                Ok((file, name, offset, objects.put(body.as_bytes())?))
            })
            .collect::<Result<Vec<_>, LatticeError>>()?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| LatticeError::Storage(e.to_string()))?;
        for (file, name, offset, key) in staged {
            tx.execute("UPDATE nodes SET body='',body_hash=?4 WHERE file=?1 AND name=?2 AND byte_offset=?3 AND body_hash IS NULL", params![file,name,offset,key]).map_err(|e| LatticeError::Storage(format!("Failed to migrate inline symbol body: {e}")))?;
        }
        tx.commit().map_err(|e| {
            LatticeError::Storage(format!(
                "Failed to commit inline symbol body migration: {e}"
            ))
        })?;
        publication.promote(&self.conn)
    }

    /// Open an in-memory SQLite database (for tests).
    pub fn open_in_memory() -> Result<Self, LatticeError> {
        let conn = Connection::open_in_memory().map_err(|e| {
            LatticeError::Storage(format!("Failed to open in-memory database: {}", e))
        })?;

        let store = Self {
            conn: GraphConnection::Direct(conn),
            path: None,
            recovery: GraphStoreRecovery::None,
            body_objects: None,
            object_owner_id: None,
            publication: None,
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
        if !self.column_exists("nodes", "body_hash")? {
            self.conn
                .execute("ALTER TABLE nodes ADD COLUMN body_hash TEXT", [])
                .map_err(|e| {
                    LatticeError::Storage(format!("Failed to add node body object reference: {e}"))
                })?;
        }
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

    fn column_exists(&self, table: &str, expected: &str) -> Result<bool, LatticeError> {
        let mut statement = self
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(|e| LatticeError::Storage(format!("Failed to inspect {table} schema: {e}")))?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| LatticeError::Storage(format!("Failed to query {table} schema: {e}")))?;
        for column in columns {
            if column.map_err(|e| LatticeError::Storage(e.to_string()))? == expected {
                return Ok(true);
            }
        }
        Ok(false)
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
        self.commit_graph_generation(graph, None).map(|_| ())
    }

    /// Atomically persist a graph generation and return the exact immutable snapshot committed.
    pub fn save_index_snapshot(&self, graph: &CodeGraph) -> Result<IndexSnapshot, LatticeError> {
        let module_digests = self.commit_graph_generation(graph, None)?;
        Ok(IndexSnapshot {
            epoch: module_digests.epoch(),
            graph: Arc::new(graph.clone()),
            module_digests,
        })
    }

    pub fn save_index_snapshot_with_manifest(
        &self,
        graph: &CodeGraph,
        files: &[FileIndexEntry],
    ) -> Result<IndexSnapshot, LatticeError> {
        let module_digests = self.commit_graph_generation(graph, Some(files))?;
        Ok(IndexSnapshot {
            epoch: module_digests.epoch(),
            graph: Arc::new(graph.clone()),
            module_digests,
        })
    }

    fn commit_graph_generation(
        &self,
        graph: &CodeGraph,
        files: Option<&[FileIndexEntry]>,
    ) -> Result<ModuleDigestCache, LatticeError> {
        // Generation happens before SQLite mutation so any invalid graph leaves the prior
        // generation wholly intact.
        let digests = generate_module_digests(graph).map_err(|error| {
            LatticeError::Storage(format!("Failed to generate module digests: {}", error))
        })?;
        let nodes = graph.all_nodes();
        let (body_hashes, publication) = if let Some(objects) = &self.body_objects {
            let expected = nodes
                .iter()
                .map(|node| format!("{:x}", sha2::Sha256::digest(node.body.as_bytes())))
                .collect::<Vec<_>>();
            self.path.as_ref().ok_or_else(|| {
                LatticeError::Storage(
                    "repository object storage requires a file-backed graph database".into(),
                )
            })?;
            // Pin the entire proposed generation before creating any object. The shared
            // publication lock remains held until the committed SQLite epoch is promoted.
            let owner = self.object_owner_id.as_deref().ok_or_else(|| {
                LatticeError::Storage(
                    "repository object storage requires an explicit checkout owner".into(),
                )
            })?;
            let publication = objects
                .begin_publication(owner, expected.iter().cloned().collect::<BTreeSet<_>>())?;
            let stored = nodes
                .iter()
                .map(|node| objects.put(node.body.as_bytes()))
                .collect::<Result<Vec<_>, _>>()?;
            debug_assert_eq!(expected, stored);
            (stored, Some(publication))
        } else {
            (vec![String::new(); nodes.len()], None)
        };
        let _accounting = self
            .publication
            .as_ref()
            .map(|authority| authority.begin())
            .transpose()?;
        // Objects are immutable and durable before SQLite publishes references.
        // An interrupted save can leave harmless unreferenced objects, never dangling refs.
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

        // Temporary membership contains keys only; persistent rows are changed
        // only when their values differ. SQLite rolls back the entire generation.
        tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS desired_nodes(file TEXT,name TEXT,byte_offset INTEGER,PRIMARY KEY(file,name,byte_offset)); DELETE FROM desired_nodes;
            CREATE TEMP TABLE IF NOT EXISTS desired_edges(from_file TEXT,from_name TEXT,from_offset INTEGER,to_file TEXT,to_name TEXT,to_offset INTEGER,kind TEXT);
            CREATE INDEX IF NOT EXISTS temp.idx_desired_edges ON desired_edges(from_file,from_name,from_offset,to_file,to_name,to_offset,kind); DELETE FROM desired_edges;
            CREATE TEMP TABLE IF NOT EXISTS desired_digests(module_path TEXT PRIMARY KEY); DELETE FROM desired_digests;")
            .map_err(|e| LatticeError::Storage(format!("Failed to stage graph delta membership: {e}")))?;

        // Insert all nodes (scoped so prepared statement is dropped before commit)
        {
            let mut insert_node = tx
                .prepare(
                    "INSERT INTO nodes (file, name, byte_offset, kind, signature, body, body_hash, line, end_line, is_exported, language, last_modified) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) ON CONFLICT(file,name,byte_offset) DO UPDATE SET kind=excluded.kind,signature=excluded.signature,body=excluded.body,body_hash=excluded.body_hash,line=excluded.line,end_line=excluded.end_line,is_exported=excluded.is_exported,language=excluded.language,last_modified=excluded.last_modified WHERE nodes.kind IS NOT excluded.kind OR nodes.signature IS NOT excluded.signature OR nodes.body IS NOT excluded.body OR nodes.body_hash IS NOT excluded.body_hash OR nodes.line IS NOT excluded.line OR nodes.end_line IS NOT excluded.end_line OR nodes.is_exported IS NOT excluded.is_exported OR nodes.language IS NOT excluded.language OR nodes.last_modified IS NOT excluded.last_modified",
                )
                .map_err(|e| LatticeError::Storage(format!("Failed to prepare node insert: {}", e)))?;

            let mut membership = tx
                .prepare("INSERT INTO desired_nodes VALUES(?1,?2,?3)")
                .map_err(|e| LatticeError::Storage(e.to_string()))?;
            for (node, body_hash) in nodes.iter().zip(body_hashes.iter()) {
                membership
                    .execute(params![node.file, node.name, node.id.byte_offset as i64])
                    .map_err(|e| LatticeError::Storage(e.to_string()))?;
                insert_node
                    .execute(params![
                        &node.file,
                        &node.name,
                        node.id.byte_offset as i64,
                        format!("{:?}", node.kind),
                        &*node.signature,
                        if self.body_objects.is_some() {
                            ""
                        } else {
                            &*node.body
                        },
                        if body_hash.is_empty() {
                            None
                        } else {
                            Some(body_hash.as_str())
                        },
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
                    "INSERT INTO desired_edges (from_file, from_name, from_offset, to_file, to_name, to_offset, kind) \
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

        tx.execute_batch("DELETE FROM edges WHERE rowid IN (
            SELECT rowid FROM (SELECT rowid,*,row_number() OVER(PARTITION BY from_file,from_name,from_offset,to_file,to_name,to_offset,kind ORDER BY rowid) AS occurrence FROM edges) old
            WHERE occurrence > (SELECT COUNT(*) FROM desired_edges new WHERE new.from_file=old.from_file AND new.from_name=old.from_name AND new.from_offset=old.from_offset AND new.to_file=old.to_file AND new.to_name=old.to_name AND new.to_offset=old.to_offset AND new.kind=old.kind));
            INSERT INTO edges SELECT from_file,from_name,from_offset,to_file,to_name,to_offset,kind FROM
            (SELECT *,row_number() OVER(PARTITION BY from_file,from_name,from_offset,to_file,to_name,to_offset,kind ORDER BY rowid) AS occurrence FROM desired_edges) new
            WHERE occurrence > (SELECT COUNT(*) FROM edges old WHERE new.from_file=old.from_file AND new.from_name=old.from_name AND new.from_offset=old.from_offset AND new.to_file=old.to_file AND new.to_name=old.to_name AND new.to_offset=old.to_offset AND new.kind=old.kind);
            DELETE FROM nodes WHERE NOT EXISTS(SELECT 1 FROM desired_nodes d WHERE d.file=nodes.file AND d.name=nodes.name AND d.byte_offset=nodes.byte_offset);")
            .map_err(|e| LatticeError::Storage(format!("Failed to apply graph membership delta: {e}")))?;

        {
            let mut insert_digest = tx
                .prepare(
                    "INSERT INTO module_digests (module_path, index_epoch, generator_version, input_fingerprint, payload_sha256, payload_json) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(module_path) DO UPDATE SET index_epoch=excluded.index_epoch,generator_version=excluded.generator_version,input_fingerprint=excluded.input_fingerprint,payload_sha256=excluded.payload_sha256,payload_json=excluded.payload_json WHERE module_digests.generator_version IS NOT excluded.generator_version OR module_digests.input_fingerprint IS NOT excluded.input_fingerprint OR module_digests.payload_sha256 IS NOT excluded.payload_sha256 OR module_digests.payload_json IS NOT excluded.payload_json",
                )
                .map_err(|e| {
                    LatticeError::Storage(format!(
                        "Failed to prepare module digest insert: {}",
                        e
                    ))
                })?;
            let mut membership = tx
                .prepare("INSERT INTO desired_digests VALUES(?1)")
                .map_err(|e| LatticeError::Storage(e.to_string()))?;
            for digest in &digests {
                membership
                    .execute([&digest.module_path])
                    .map_err(|e| LatticeError::Storage(e.to_string()))?;
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

        tx.execute("DELETE FROM module_digests WHERE NOT EXISTS(SELECT 1 FROM desired_digests d WHERE d.module_path=module_digests.module_path)", []).map_err(|e| LatticeError::Storage(e.to_string()))?;
        if let Some(files) = files {
            Self::write_file_index_delta(&tx, files)?;
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

        if let Some(publication) = publication {
            publication.promote(&self.conn)?;
        }

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
        let _accounting = self
            .publication
            .as_ref()
            .map(|authority| authority.begin())
            .transpose()?;
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            LatticeError::Storage(format!("Failed to begin file index transaction: {}", e))
        })?;
        Self::write_file_index_delta(&tx, entries)?;
        tx.commit()
            .map_err(|e| LatticeError::Storage(format!("Failed to commit file index: {}", e)))?;
        Ok(())
    }

    fn write_file_index_delta(
        tx: &rusqlite::Transaction<'_>,
        entries: &[FileIndexEntry],
    ) -> Result<(), LatticeError> {
        tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS desired_files(file TEXT PRIMARY KEY); DELETE FROM desired_files;")
            .map_err(|e| LatticeError::Storage(format!("Failed to clear file index: {}", e)))?;
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO file_index (file, content_hash, mtime_ns, size_bytes, parser_version, schema_version, last_indexed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT(file) DO UPDATE SET content_hash=excluded.content_hash,mtime_ns=excluded.mtime_ns,size_bytes=excluded.size_bytes,parser_version=excluded.parser_version,schema_version=excluded.schema_version,last_indexed_at=excluded.last_indexed_at WHERE file_index.content_hash IS NOT excluded.content_hash OR file_index.mtime_ns IS NOT excluded.mtime_ns OR file_index.size_bytes IS NOT excluded.size_bytes OR file_index.parser_version IS NOT excluded.parser_version OR file_index.schema_version IS NOT excluded.schema_version",
                )
                .map_err(|e| LatticeError::Storage(format!("Failed to prepare file index insert: {}", e)))?;
            let mut membership = tx
                .prepare("INSERT INTO desired_files VALUES(?1)")
                .map_err(|e| LatticeError::Storage(e.to_string()))?;
            for entry in entries {
                membership
                    .execute([&entry.file])
                    .map_err(|e| LatticeError::Storage(e.to_string()))?;
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
        tx.execute("DELETE FROM file_index WHERE NOT EXISTS(SELECT 1 FROM desired_files d WHERE d.file=file_index.file)", []).map_err(|e| LatticeError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Load a CodeGraph from the database.
    pub fn load_graph(&self) -> Result<CodeGraph, LatticeError> {
        self.load_graph_from(&self.conn)
    }

    /// Load graph and digest state under one SQLite read transaction.
    pub fn load_index_snapshot(&self) -> Result<IndexSnapshotLoad, LatticeError> {
        let tx = self.conn.unchecked_transaction().map_err(|e| {
            LatticeError::Storage(format!("Failed to begin graph snapshot read: {}", e))
        })?;
        let graph = Arc::new(self.load_graph_from(&tx)?);
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
            if row_epoch <= 0 || row_epoch > epoch {
                return Err(invalid(&format!(
                    "row last-changed epoch {} is outside the committed generation",
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

    fn load_graph_from(&self, conn: &Connection) -> Result<CodeGraph, LatticeError> {
        let mut graph = CodeGraph::new();

        // Load all nodes
        let mut stmt = conn
            .prepare(
                "SELECT file, name, byte_offset, kind, signature, body, body_hash, line, end_line, is_exported, language, last_modified FROM nodes",
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
                let body_hash: Option<String> = row.get(6)?;
                let line: i64 = row.get(7)?;
                let end_line: i64 = row.get(8)?;
                let is_exported: i32 = row.get(9)?;
                let language_str: String = row.get(10)?;
                let last_modified: i64 = row.get(11)?;

                Ok((
                    file,
                    name,
                    byte_offset,
                    kind_str,
                    signature,
                    body,
                    body_hash,
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
                body_hash,
                line,
                end_line,
                is_exported,
                language_str,
                last_modified,
            ) =
                row.map_err(|e| LatticeError::Storage(format!("Failed to read node row: {}", e)))?;

            let body = match (body_hash, &self.body_objects) {
                (Some(key), Some(objects)) => {
                    String::from_utf8(objects.get(&key)?).map_err(|e| {
                        LatticeError::Storage(format!("symbol body object {key} is not UTF-8: {e}"))
                    })?
                }
                (Some(key), None) => {
                    return Err(LatticeError::Storage(format!(
                        "symbol body object {key} requires repository object storage"
                    )))
                }
                (None, _) => body,
            };
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
    use crate::storage::ContentObjectStore;

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

    #[test]
    fn unchanged_generation_writes_no_node_edge_digest_or_manifest_rows() {
        let store = GraphStore::open_in_memory().unwrap();
        let mut graph = graph(&["src/a.rs", "src/b.rs"]);
        graph.add_edge(
            &id("src/a.rs", "symbol_0"),
            &id("src/b.rs", "symbol_1"),
            EdgeKind::Calls,
        );
        let files = vec![FileIndexEntry {
            file: "src/a.rs".into(),
            content_hash: "hash".into(),
            mtime_ns: 1,
            size_bytes: 2,
            parser_version: 1,
            schema_version: 1,
            last_indexed_at: 1,
        }];
        store
            .save_index_snapshot_with_manifest(&graph, &files)
            .unwrap();
        store
            .conn
            .execute_batch("CREATE TABLE mutation_audit(kind TEXT);")
            .unwrap();
        for table in ["nodes", "edges", "module_digests", "file_index"] {
            for operation in ["INSERT", "UPDATE", "DELETE"] {
                store.conn.execute_batch(&format!("CREATE TRIGGER audit_{table}_{operation} AFTER {operation} ON {table} BEGIN INSERT INTO mutation_audit VALUES('{table}'); END;")).unwrap();
            }
        }
        store
            .save_index_snapshot_with_manifest(&graph, &files)
            .unwrap();
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM mutation_audit", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            0
        );
        assert_eq!(ready(store.load_index_snapshot().unwrap()).epoch, 2);
    }

    #[test]
    fn ten_checkout_graphs_share_one_immutable_body_object() {
        let root = tempfile::tempdir().unwrap();
        let objects = root.path().join("objects");
        for checkout in 0..10 {
            let path = root.path().join(format!("graph-{checkout}.db"));
            let store = GraphStore::open_recovering_with_objects(
                &path,
                &objects,
                &format!("checkout-{checkout}"),
            )
            .unwrap();
            store.save_graph(&graph(&["src/a.rs"])).unwrap();
            assert_eq!(store.load_graph().unwrap().node_count(), 1);
            let inline: String = store
                .conn
                .query_row("SELECT body FROM nodes", [], |row| row.get(0))
                .unwrap();
            assert!(inline.is_empty());
        }
        let files = fs::read_dir(&objects)
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                (path.file_name().unwrap().len() == 2).then_some(path)
            })
            .flat_map(|path| fs::read_dir(path).unwrap())
            .count();
        assert_eq!(
            files, 1,
            "ten identical worktrees must store one body object"
        );
        let key = format!("{:x}", sha2::Sha256::digest(b"{}"));
        assert_eq!(
            fs::metadata(objects.join(&key[..2]).join(key))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn corrupt_body_object_fails_truthfully_without_replacing_graph() {
        let root = tempfile::tempdir().unwrap();
        let objects = root.path().join("objects");
        let path = root.path().join("graph.db");
        let store =
            GraphStore::open_recovering_with_objects(&path, &objects, "checkout-corrupt").unwrap();
        store.save_graph(&graph(&["src/a.rs"])).unwrap();
        let key: String = store
            .conn
            .query_row("SELECT body_hash FROM nodes", [], |row| row.get(0))
            .unwrap();
        fs::write(objects.join(&key[..2]).join(&key), "corrupt").unwrap();
        let error = match store.load_graph() {
            Ok(_) => panic!("corrupt object must not hydrate a graph"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("corrupt symbol body object"), "{error}");
        assert!(path.exists());
    }

    #[test]
    fn object_gc_preserves_references_from_every_checkout_and_reclaims_orphans() {
        let root = tempfile::tempdir().unwrap();
        let objects = root.path().join("objects");
        let first = GraphStore::open_recovering_with_objects(
            &root.path().join("first.db"),
            &objects,
            "checkout-first",
        )
        .unwrap();
        let second = GraphStore::open_recovering_with_objects(
            &root.path().join("second.db"),
            &objects,
            "checkout-second",
        )
        .unwrap();
        first.save_graph(&graph(&["src/a.rs"])).unwrap();
        let mut distinct = CodeGraph::new();
        distinct.add_node(
            id("src/b.rs", "symbol_0"),
            SymbolKind::Function,
            "symbol_0".to_string(),
            "fn symbol_0()",
            "a sizeable checkout-specific body",
            "src/b.rs".to_string(),
            1,
            1,
            true,
            Language::Rust,
        );
        second.save_graph(&distinct).unwrap();
        let object_store = ContentObjectStore::open(&objects).unwrap();
        let orphan = object_store.put(b"rolled back generation").unwrap();

        let report = object_store.collect_garbage(16).unwrap();
        assert_eq!(report.removed, 1);
        assert!(object_store.get(&orphan).is_err());
        assert_eq!(first.load_graph().unwrap().node_count(), 1);
        assert_eq!(second.load_graph().unwrap().node_count(), 1);
    }

    #[test]
    fn restart_reconciles_pending_pin_from_actual_committed_epoch() {
        let root = tempfile::tempdir().unwrap();
        let objects = root.path().join("objects");
        let path = root.path().join("graph.db");
        let store =
            GraphStore::open_recovering_with_objects(&path, &objects, "checkout-restart").unwrap();
        store.save_graph(&graph(&["src/a.rs"])).unwrap();
        let object_store = ContentObjectStore::open(&objects).unwrap();
        let live: String = store
            .conn
            .query_row("SELECT body_hash FROM nodes", [], |r| r.get(0))
            .unwrap();
        let stale = object_store.put(b"pre-commit crash").unwrap();
        let pin = object_store
            .begin_publication("checkout-restart", [stale.clone()])
            .unwrap();
        drop(pin); // simulates process death before SQLite commit
        drop(store);

        assert_eq!(object_store.collect_garbage(16).unwrap().removed, 0);
        let reopened =
            GraphStore::open_recovering_with_objects(&path, &objects, "checkout-restart").unwrap();
        drop(reopened);
        assert_eq!(object_store.collect_garbage(16).unwrap().removed, 1);
        assert!(object_store.get(&stale).is_err());
        assert_eq!(object_store.get(&live).unwrap(), b"{}");
    }

    #[test]
    fn restart_after_graph_commit_reconciles_new_refs_before_gc() {
        let root = tempfile::tempdir().unwrap();
        let objects = root.path().join("objects");
        let path = root.path().join("graph.db");
        let store =
            GraphStore::open_recovering_with_objects(&path, &objects, "checkout-post-commit")
                .unwrap();
        store.save_graph(&graph(&["src/a.rs"])).unwrap();
        let object_store = ContentObjectStore::open(&objects).unwrap();
        let old: String = store
            .conn
            .query_row("SELECT body_hash FROM nodes", [], |r| r.get(0))
            .unwrap();
        let new = object_store.put(b"committed replacement body").unwrap();
        let pin = object_store
            .begin_publication("checkout-post-commit", [new.clone()])
            .unwrap();
        store.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        store
            .conn
            .execute("UPDATE nodes SET body='', body_hash=?1", [&new])
            .unwrap();
        store
            .conn
            .execute("UPDATE graph_metadata SET index_epoch=index_epoch+1", [])
            .unwrap();
        store.conn.execute_batch("COMMIT").unwrap();
        drop(pin); // simulates process death after graph commit and before ref promotion
        drop(store);

        assert_eq!(object_store.collect_garbage(16).unwrap().removed, 0);
        let reopened =
            GraphStore::open_recovering_with_objects(&path, &objects, "checkout-post-commit")
                .unwrap();
        assert_eq!(reopened.load_graph().unwrap().node_count(), 1);
        drop(reopened);
        assert_eq!(object_store.collect_garbage(16).unwrap().removed, 1);
        assert!(object_store.get(&old).is_err());
        assert_eq!(
            object_store.get(&new).unwrap(),
            b"committed replacement body"
        );
    }

    #[test]
    fn active_publication_excludes_object_gc() {
        let root = tempfile::tempdir().unwrap();
        let objects = ContentObjectStore::open(&root.path().join("objects")).unwrap();
        let path = root.path().join("graph.db");
        let store = GraphStore::open_recovering_with_objects(
            &path,
            &root.path().join("objects"),
            "checkout-active",
        )
        .unwrap();
        let pin = objects
            .begin_publication("checkout-active", Vec::<String>::new())
            .unwrap();
        assert!(objects
            .collect_garbage(1)
            .unwrap_err()
            .to_string()
            .contains("publication is active"));
        drop(pin);
        drop(store);
    }

    #[test]
    fn graph_manifest_failure_rolls_back_every_generation_component() {
        let store = GraphStore::open_in_memory().unwrap();
        store
            .save_index_snapshot_with_manifest(&graph(&["src/a.rs"]), &[])
            .unwrap();
        store.conn.execute_batch("CREATE TRIGGER fail_manifest BEFORE INSERT ON file_index BEGIN SELECT RAISE(ABORT,'injected manifest failure'); END;").unwrap();
        let files = vec![FileIndexEntry {
            file: "src/b.rs".into(),
            content_hash: "hash".into(),
            mtime_ns: 1,
            size_bytes: 2,
            parser_version: 1,
            schema_version: 1,
            last_indexed_at: 1,
        }];
        assert!(store
            .save_index_snapshot_with_manifest(&graph(&["src/b.rs"]), &files)
            .is_err());
        let loaded = ready(store.load_index_snapshot().unwrap());
        assert_eq!(loaded.epoch, 1);
        assert!(loaded.module_digests.get("src/a.rs").is_some());
        assert!(store.load_file_index().unwrap().is_empty());
    }

    #[test]
    fn delta_preserves_parallel_edge_multiplicity_and_removes_deleted_edges() {
        let store = GraphStore::open_in_memory().unwrap();
        let mut graph = graph(&["src/a.rs", "src/b.rs"]);
        for _ in 0..3 {
            graph.add_edge(
                &id("src/a.rs", "symbol_0"),
                &id("src/b.rs", "symbol_1"),
                EdgeKind::Calls,
            );
        }
        store.save_graph(&graph).unwrap();
        assert_eq!(store.load_graph().unwrap().edge_count(), graph.edge_count());
        store.save_graph(&graph).unwrap();
        assert_eq!(store.load_graph().unwrap().edge_count(), graph.edge_count());
        let empty = CodeGraph::new();
        store.save_graph(&empty).unwrap();
        assert_eq!(store.load_graph().unwrap().edge_count(), 0);
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
                "body_hash",
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
