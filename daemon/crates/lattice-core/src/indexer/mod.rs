pub mod lazy;

#[cfg(test)]
mod tests;

use crate::error::LatticeError;
use crate::graph::builder::GraphBuilder;
use crate::graph::CodeGraph;
use crate::parser;
use crate::symbols::ParsedFile;
use std::collections::HashMap;
use std::path::PathBuf;

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

    /// Access the code graph mutably (e.g., for adding LSP edges).
    pub fn graph_mut(&mut self) -> &mut CodeGraph {
        &mut self.graph
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

    /// Parse and index a batch of files, rebuilding the graph once at the end.
    ///
    /// This is much cheaper than calling `index_file_content` repeatedly for
    /// cold-start indexing or explicit reindex requests.
    pub async fn index_file_batch_contents(
        &mut self,
        files: Vec<(String, String)>,
    ) -> anyhow::Result<usize> {
        if files.is_empty() {
            return Ok(0);
        }

        let mut handles = Vec::new();
        for (rel_path, content) in files {
            handles.push(tokio::task::spawn_blocking(move || {
                crate::parser::parse_file(&rel_path, &content)
            }));
        }

        let mut count = 0usize;
        for handle in handles {
            match handle.await {
                Ok(Ok(parsed)) => {
                    self.parsed_files.insert(parsed.file.clone(), parsed);
                    count += 1;
                }
                Ok(Err(e)) => tracing::warn!("Parse error: {}", e),
                Err(e) => tracing::warn!("Task error: {}", e),
            }
        }

        self.rebuild_graph();
        Ok(count)
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

    /// Index a directory using parallel file parsing.
    /// Files are parsed concurrently, then the graph is rebuilt once.
    pub async fn index_directory_parallel(
        &mut self,
        dir: &std::path::Path,
    ) -> anyhow::Result<usize> {
        let files = self.collect_indexable_files(dir)?;
        self.index_file_batch_contents(files).await
    }

    fn collect_indexable_files(
        &self,
        dir: &std::path::Path,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let mut files = Vec::new();
        self.scan_files(dir, dir, &mut files)?;
        Ok(files)
    }

    fn scan_files(
        &self,
        base: &std::path::Path,
        dir: &std::path::Path,
        files: &mut Vec<(String, String)>,
    ) -> anyhow::Result<()> {
        let entries = std::fs::read_dir(dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !crate::watcher::EXCLUDED_DIRS.contains(&dir_name) {
                    self.scan_files(base, &path, files)?;
                }
            } else if path.is_file() {
                let rel_path = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if crate::watcher::should_index_file(&rel_path) {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        files.push((rel_path, content));
                    }
                }
            }
        }
        Ok(())
    }

    /// Re-index a file and return the list of changed symbols.
    /// If a MemoryStore is provided, mark related memories as stale.
    pub fn index_file_with_stale_detection(
        &mut self,
        rel_path: &str,
        content: &str,
        memory_store: Option<&crate::memory::MemoryStore>,
    ) -> Result<Vec<crate::diff::SymbolChange>, LatticeError> {
        let new_parsed = crate::parser::parse_file(rel_path, content)?;
        let had_existing_file = self.parsed_files.contains_key(rel_path);

        // Get old symbols for this file
        let old_symbols = self
            .parsed_files
            .get(rel_path)
            .map(|f| f.symbols.clone())
            .unwrap_or_default();

        // Diff
        let changes = crate::diff::diff_symbols(&old_symbols, &new_parsed.symbols);

        // Mark stale memories for modified/removed symbols
        if let Some(store) = memory_store {
            if had_existing_file {
                let reason = format!("{} changed", rel_path);
                let _ = store.mark_stale_by_file(rel_path, &reason);
            }
            for change in &changes {
                if change.kind == crate::diff::ChangeKind::Modified
                    || change.kind == crate::diff::ChangeKind::Removed
                {
                    let reason =
                        format!("{}() was {:?} in {}", change.name, change.kind, change.file);
                    let _ = store.mark_stale_by_symbol(&change.name, &reason);
                }
            }
        }

        // Update index
        self.parsed_files.insert(rel_path.to_string(), new_parsed);
        self.rebuild_graph();

        Ok(changes)
    }
}
