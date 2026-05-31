pub mod lazy;

#[cfg(test)]
mod tests;

use crate::error::LatticeError;
use crate::graph::builder::GraphBuilder;
use crate::graph::CodeGraph;
use crate::identity::FileId;
use crate::parser;
use crate::symbols::{ParsedFile, Symbol};
use crate::verification::IncrementalVerifier;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Incremental indexer that maintains a code graph from parsed files.
///
/// Supports adding, updating, and removing files. On each change the entire
/// graph is rebuilt from the current set of parsed files so that cross-file
/// edges stay consistent.
pub struct Indexer {
    #[allow(dead_code)]
    root: PathBuf,
    graph: Arc<CodeGraph>,
    graph_snapshot_id: u64,
    parsed_files: HashMap<String, ParsedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexFailureKind {
    ParseError,
    WorkerPanic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexFailure {
    pub file: String,
    pub kind: IndexFailureKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchIndexReport {
    pub requested_count: usize,
    pub indexed_count: usize,
    pub is_partial: bool,
    pub failures: Vec<IndexFailure>,
}

impl Indexer {
    /// Create a new indexer rooted at the given directory.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            graph: Arc::new(CodeGraph::new()),
            graph_snapshot_id: 0,
            parsed_files: HashMap::new(),
        }
    }

    /// Access the current code graph.
    pub fn graph(&self) -> &CodeGraph {
        self.graph.as_ref()
    }

    pub fn graph_arc(&self) -> Arc<CodeGraph> {
        Arc::clone(&self.graph)
    }

    /// Access the code graph mutably (e.g., for adding LSP edges).
    pub fn graph_mut(&mut self) -> &mut CodeGraph {
        Arc::make_mut(&mut self.graph)
    }

    pub fn parsed_files(&self) -> &HashMap<String, ParsedFile> {
        &self.parsed_files
    }

    pub fn graph_snapshot_id(&self) -> u64 {
        self.graph_snapshot_id
    }

    pub fn replace_parsed_files(&mut self, parsed_files: HashMap<String, ParsedFile>) {
        self.parsed_files = parsed_files;
        self.rebuild_graph();
    }

    pub fn replace_index(&mut self, graph: CodeGraph, parsed_files: HashMap<String, ParsedFile>) {
        self.graph = Arc::new(graph);
        self.parsed_files = parsed_files;
        self.graph_snapshot_id = self.graph_snapshot_id.saturating_add(1);
    }

    pub fn replace_shared_index(
        &mut self,
        graph: Arc<CodeGraph>,
        parsed_files: HashMap<String, ParsedFile>,
    ) {
        self.graph = graph;
        self.parsed_files = parsed_files;
        self.graph_snapshot_id = self.graph_snapshot_id.saturating_add(1);
    }

    pub fn into_parts(self) -> (CodeGraph, HashMap<String, ParsedFile>) {
        let graph = Arc::try_unwrap(self.graph).unwrap_or_else(|graph| (*graph).clone());
        (graph, self.parsed_files)
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
        Ok(self
            .index_file_batch_contents_with_report(files)
            .await?
            .indexed_count)
    }

    pub async fn index_file_batch_contents_with_report(
        &mut self,
        files: Vec<(String, String)>,
    ) -> anyhow::Result<BatchIndexReport> {
        let parser =
            Arc::new(|rel_path: &str, content: &str| crate::parser::parse_file(rel_path, content));
        Ok(self
            .index_file_batch_contents_with_parser(files, parser)
            .await)
    }

    #[cfg(test)]
    pub(crate) async fn index_file_batch_contents_with_test_parser(
        &mut self,
        files: Vec<(String, String)>,
        parser: Arc<dyn Fn(&str, &str) -> Result<ParsedFile, LatticeError> + Send + Sync>,
    ) -> BatchIndexReport {
        self.index_file_batch_contents_with_parser(files, parser)
            .await
    }

    async fn index_file_batch_contents_with_parser(
        &mut self,
        files: Vec<(String, String)>,
        parser: Arc<dyn Fn(&str, &str) -> Result<ParsedFile, LatticeError> + Send + Sync>,
    ) -> BatchIndexReport {
        if files.is_empty() {
            return BatchIndexReport {
                requested_count: 0,
                indexed_count: 0,
                is_partial: false,
                failures: Vec::new(),
            };
        }

        let mut handles = Vec::new();
        let requested_count = files.len();
        for (rel_path, content) in files {
            let parser = parser.clone();
            handles.push(tokio::task::spawn_blocking(move || {
                let parse_result = parser(&rel_path, &content);
                (rel_path, parse_result)
            }));
        }

        let mut count = 0usize;
        let mut failures = Vec::new();
        for handle in handles {
            match handle.await {
                Ok((_file, Ok(parsed))) => {
                    self.parsed_files.insert(parsed.file.clone(), parsed);
                    count += 1;
                }
                Ok((file, Err(error))) => {
                    tracing::warn!(file = file.as_str(), "Parse error: {}", error);
                    failures.push(IndexFailure {
                        file,
                        kind: IndexFailureKind::ParseError,
                        message: error.to_string(),
                    });
                }
                Err(error) => {
                    let file = "<worker>".to_string();
                    tracing::warn!(file = file.as_str(), "Task error: {}", error);
                    failures.push(IndexFailure {
                        file,
                        kind: IndexFailureKind::WorkerPanic,
                        message: error.to_string(),
                    });
                }
            }
        }

        self.rebuild_graph();
        BatchIndexReport {
            requested_count,
            indexed_count: count,
            is_partial: count != requested_count || !failures.is_empty(),
            failures,
        }
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
        let previous_graph = Arc::clone(&self.graph);
        let mut builder = GraphBuilder::new();
        for parsed in self.parsed_files.values() {
            builder.add_file(parsed.clone());
        }
        let mut graph = builder.build();
        graph.hydrate_missing_bodies_from(previous_graph.as_ref());
        self.graph = Arc::new(graph);
        strip_symbol_bodies(&mut self.parsed_files);
        self.graph_snapshot_id = self.graph_snapshot_id.saturating_add(1);
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
        let old_symbols = self.diffable_symbols_for_file(rel_path);

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

    pub fn index_file_with_stale_proposals(
        &mut self,
        rel_path: &str,
        content: &str,
        stale_marker: Option<&mut crate::consolidation::StaleMarker<'_>>,
    ) -> Result<Vec<crate::diff::SymbolChange>, LatticeError> {
        let changes = self.index_file_content_with_diff(rel_path, content)?;
        if let Some(marker) = stale_marker {
            let _ = marker.on_graph_change(vec![rel_path.to_string()]);
        }
        Ok(changes)
    }

    pub fn index_file_with_verification_proposals(
        &mut self,
        file_id: FileId,
        content: &str,
        stale_marker: Option<&mut crate::consolidation::StaleMarker<'_>>,
        incremental_verifier: Option<&mut IncrementalVerifier<'_>>,
    ) -> Result<Vec<crate::diff::SymbolChange>, LatticeError> {
        let prior_snapshot_id = self.graph_snapshot_id;
        let changes = self.index_file_content_with_diff(&file_id.repo_relative_path, content)?;
        let new_snapshot_id = self.graph_snapshot_id;
        if let Some(marker) = stale_marker {
            let _ = marker.on_graph_change(vec![file_id.repo_relative_path.clone()]);
        }
        if let Some(verifier) = incremental_verifier {
            let _ = verifier.on_graph_delta(prior_snapshot_id, new_snapshot_id, vec![file_id]);
        }
        Ok(changes)
    }

    fn index_file_content_with_diff(
        &mut self,
        rel_path: &str,
        content: &str,
    ) -> Result<Vec<crate::diff::SymbolChange>, LatticeError> {
        let new_parsed = crate::parser::parse_file(rel_path, content)?;
        let old_symbols = self.diffable_symbols_for_file(rel_path);
        let changes = crate::diff::diff_symbols(&old_symbols, &new_parsed.symbols);
        self.parsed_files.insert(rel_path.to_string(), new_parsed);
        self.rebuild_graph();
        Ok(changes)
    }

    fn diffable_symbols_for_file(&self, rel_path: &str) -> Vec<Symbol> {
        let Some(parsed_file) = self.parsed_files.get(rel_path) else {
            return Vec::new();
        };

        parsed_file
            .symbols
            .iter()
            .map(|symbol| {
                let mut hydrated = symbol.clone();
                if hydrated.body.is_empty() {
                    if let Some(node) = self.graph.get_node(&hydrated.id) {
                        hydrated.body = node.body.to_string();
                    }
                }
                hydrated
            })
            .collect()
    }
}

fn strip_symbol_bodies(parsed_files: &mut HashMap<String, ParsedFile>) {
    for parsed_file in parsed_files.values_mut() {
        for symbol in &mut parsed_file.symbols {
            symbol.body.clear();
        }
    }
}
