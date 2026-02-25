#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use crate::error::LatticeError;
use crate::graph::CodeGraph;
use crate::graph::builder::GraphBuilder;
use crate::parser;
use crate::symbols::ParsedFile;

/// Incremental indexer that maintains a code graph from parsed files.
///
/// Supports adding, updating, and removing files. On each change the entire
/// graph is rebuilt from the current set of parsed files so that cross-file
/// edges stay consistent.
pub struct Indexer {
    #[allow(dead_code)]
    root: PathBuf,
    graph: CodeGraph,
    parsed_files: HashMap<String, ParsedFile>,
}

impl Indexer {
    /// Create a new indexer rooted at the given directory.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            graph: CodeGraph::new(),
            parsed_files: HashMap::new(),
        }
    }

    /// Access the current code graph.
    pub fn graph(&self) -> &CodeGraph {
        &self.graph
    }

    /// Parse and index a single file by its relative path and content.
    ///
    /// If the file was previously indexed, its old symbols are replaced.
    /// After parsing, the entire graph is rebuilt from all known files.
    pub fn index_file_content(
        &mut self,
        rel_path: &str,
        content: &str,
    ) -> Result<(), LatticeError> {
        // Parse the file
        let parsed = parser::parse_file(rel_path, content)?;

        // Store / replace in the map
        self.parsed_files.insert(rel_path.to_string(), parsed);

        // Rebuild the graph from all parsed files
        self.rebuild_graph();

        Ok(())
    }

    /// Remove a file from the index and rebuild the graph.
    pub fn remove_file(&mut self, rel_path: &str) {
        self.parsed_files.remove(rel_path);
        self.rebuild_graph();
    }

    /// Number of files currently indexed.
    pub fn file_count(&self) -> usize {
        self.parsed_files.len()
    }

    /// Rebuild the code graph from all currently parsed files.
    fn rebuild_graph(&mut self) {
        let mut builder = GraphBuilder::new();
        for parsed in self.parsed_files.values() {
            builder.add_file(parsed.clone());
        }
        self.graph = builder.build();
    }
}
