use std::path::Path;
use rusqlite::{Connection, params};
use crate::error::LatticeError;
use crate::graph::model::CodeGraph;
use crate::symbols::{Language, SymbolId, SymbolKind};
use crate::graph::model::EdgeKind;
use super::schema::CREATE_TABLES;

/// Persistent storage for the code dependency graph backed by SQLite.
pub struct GraphStore {
    conn: Connection,
}

impl GraphStore {
    /// Open a file-based SQLite database with WAL mode enabled.
    pub fn open(path: &Path) -> Result<Self, LatticeError> {
        let conn = Connection::open(path)
            .map_err(|e| LatticeError::Storage(format!("Failed to open database: {}", e)))?;

        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| LatticeError::Storage(format!("Failed to set WAL mode: {}", e)))?;

        let store = Self { conn };
        store.initialize()?;
        Ok(store)
    }

    /// Open an in-memory SQLite database (for tests).
    pub fn open_in_memory() -> Result<Self, LatticeError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| LatticeError::Storage(format!("Failed to open in-memory database: {}", e)))?;

        let store = Self { conn };
        store.initialize()?;
        Ok(store)
    }

    /// Create tables and indexes if they don't already exist.
    fn initialize(&self) -> Result<(), LatticeError> {
        self.conn
            .execute_batch(CREATE_TABLES)
            .map_err(|e| LatticeError::Storage(format!("Failed to initialize schema: {}", e)))?;
        Ok(())
    }

    /// Save a CodeGraph to the database, replacing any previous data.
    pub fn save_graph(&self, graph: &CodeGraph) -> Result<(), LatticeError> {
        let tx = self.conn.unchecked_transaction()
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
                        node.file,
                        node.name,
                        node.id.byte_offset as i64,
                        format!("{:?}", node.kind),
                        node.signature,
                        node.body,
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
                        from_node.id.file,
                        from_node.id.name,
                        from_node.id.byte_offset as i64,
                        to_node.id.file,
                        to_node.id.name,
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
            let (file, name, byte_offset, kind_str, signature, body, line, end_line, is_exported, language_str, edit_count, last_modified) =
                row.map_err(|e| LatticeError::Storage(format!("Failed to read node row: {}", e)))?;

            let kind = parse_symbol_kind(&kind_str)
                .ok_or_else(|| LatticeError::Storage(format!("Unknown SymbolKind: {}", kind_str)))?;
            let language = parse_language(&language_str)
                .ok_or_else(|| LatticeError::Storage(format!("Unknown Language: {}", language_str)))?;

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

                Ok((from_file, from_name, from_offset, to_file, to_name, to_offset, kind_str))
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
        "CoChanges" => Some(EdgeKind::CoChanges),
        _ => None,
    }
}
