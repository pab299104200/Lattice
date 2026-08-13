#[cfg(test)]
mod tests;

use crate::error::LatticeError;
use crate::graph::builder::GraphBuilder;
use crate::graph::CodeGraph;
use crate::identity::FileId;
use crate::parser;
use crate::symbols::{ParsedFile, Symbol};
use crate::verification::IncrementalVerifier;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

/// Incremental indexer that maintains a code graph from parsed files.
///
/// Supports adding, updating, and removing files. Changes rebuild only the
/// dependency-connected portion of the graph while preserving the same nodes
/// and edges as a full [`GraphBuilder`] pass.
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
    /// Successfully parsed upserts. Used by shard health to resolve a prior
    /// parse failure for the same path.
    pub indexed_files: Vec<String>,
    /// Requested deletions, whether or not the file had a previous graph
    /// entry. A deleted file can no longer leave the shard partially indexed.
    pub removed_files: Vec<String>,
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
        let parsed = parser::parse_file(rel_path, content)?;
        let previous_names = self.symbol_names_for_files([rel_path]);

        self.parsed_files.insert(rel_path.to_string(), parsed);
        self.rebuild_changed_files([rel_path], previous_names);

        Ok(())
    }

    /// Parse and index a batch of files, updating the graph once at the end.
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
                indexed_files: Vec::new(),
                removed_files: Vec::new(),
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
        let mut indexed_files = Vec::new();
        let mut failures = Vec::new();
        let mut previous_names = HashSet::new();
        for handle in handles {
            match handle.await {
                Ok((_file, Ok(parsed))) => {
                    previous_names.extend(self.symbol_names_for_files([parsed.file.as_str()]));
                    indexed_files.push(parsed.file.clone());
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

        if count > 0 {
            self.rebuild_changed_files(indexed_files.iter().map(String::as_str), previous_names);
        }
        BatchIndexReport {
            requested_count,
            indexed_count: count,
            is_partial: count != requested_count || !failures.is_empty(),
            indexed_files,
            removed_files: Vec::new(),
            failures,
        }
    }

    /// Remove a file from the index and update the affected graph component.
    pub fn remove_file(&mut self, rel_path: &str) {
        let previous_names = self.symbol_names_for_files([rel_path]);
        if self.parsed_files.remove(rel_path).is_some() {
            self.rebuild_changed_files([rel_path], previous_names);
        }
    }

    /// Apply a watcher change-set with one graph rebuild for the entire batch.
    ///
    /// Upserts are parsed synchronously so callers can place the complete
    /// operation on a bounded blocking worker. Parse failures preserve the last
    /// valid representation of that file, matching `index_file_content`.
    pub fn apply_file_batch_contents(
        &mut self,
        upserts: Vec<(String, String)>,
        removals: Vec<String>,
    ) -> BatchIndexReport {
        let requested_count = upserts.len();
        let mut indexed_count = 0;
        let mut indexed_files = Vec::new();
        let mut failures = Vec::new();
        let requested_paths = upserts
            .iter()
            .map(|(rel_path, _)| rel_path.as_str())
            .chain(removals.iter().map(String::as_str));
        let previous_names = self.symbol_names_for_files(requested_paths);
        for (rel_path, content) in upserts {
            match parser::parse_file(&rel_path, &content) {
                Ok(parsed) => {
                    indexed_files.push(rel_path.clone());
                    self.parsed_files.insert(rel_path, parsed);
                    indexed_count += 1;
                }
                Err(error) => failures.push(IndexFailure {
                    file: rel_path,
                    kind: IndexFailureKind::ParseError,
                    message: error.to_string(),
                }),
            }
        }

        let removed_files = removals;
        let removed_count = removed_files
            .iter()
            .filter(|rel_path| self.parsed_files.remove(*rel_path).is_some())
            .count();
        if indexed_count > 0 || removed_count > 0 {
            let changed_files = indexed_files
                .iter()
                .map(String::as_str)
                .chain(removed_files.iter().map(String::as_str));
            self.rebuild_changed_files(changed_files, previous_names);
        }

        BatchIndexReport {
            requested_count,
            indexed_count,
            is_partial: indexed_count != requested_count || !failures.is_empty(),
            indexed_files,
            removed_files,
            failures,
        }
    }

    /// Number of files currently indexed.
    pub fn file_count(&self) -> usize {
        self.parsed_files.len()
    }

    /// Rebuild the code graph from all currently parsed files.
    fn rebuild_graph(&mut self) {
        let previous_graph = Arc::clone(&self.graph);
        let mut graph = GraphBuilder::build_from_files(self.parsed_files.values());
        graph.hydrate_missing_bodies_from(previous_graph.as_ref());
        self.graph = Arc::new(graph);
        strip_symbol_bodies(&mut self.parsed_files);
        self.graph_snapshot_id = self.graph_snapshot_id.saturating_add(1);
    }

    /// Rebuild the smallest safely replaceable graph component for a change.
    ///
    /// Removing a graph node also removes every incoming edge. Therefore the
    /// replaceable component includes the reverse dependency closure of the
    /// changed files. It additionally includes sources whose currently
    /// unresolved references, imports, or document links may start resolving
    /// to a newly added symbol. Unrelated components retain their nodes and
    /// edges without passing through `GraphBuilder`.
    fn rebuild_changed_files<'a>(
        &mut self,
        changed_files: impl IntoIterator<Item = &'a str>,
        mut touched_names: HashSet<String>,
    ) {
        let changed_files = changed_files
            .into_iter()
            .map(str::to_string)
            .collect::<HashSet<_>>();
        if changed_files.is_empty() {
            return;
        }

        for rel_path in &changed_files {
            if let Some(parsed) = self.parsed_files.get(rel_path) {
                extend_symbol_names(&mut touched_names, parsed);
            }
        }

        let changed_markdown = changed_files.iter().any(|rel_path| {
            rel_path.to_ascii_lowercase().ends_with(".md")
                || self
                    .parsed_files
                    .get(rel_path)
                    .is_some_and(|file| file.language == crate::symbols::Language::Markdown)
                || self.graph.all_nodes().iter().any(|node| {
                    node.file == *rel_path && node.language == crate::symbols::Language::Markdown
                })
        });

        let mut affected_files = changed_files.clone();
        for parsed in self.parsed_files.values() {
            if parsed.symbols.iter().any(|symbol| {
                symbol.references.iter().any(|reference| {
                    touched_names.contains(reference)
                        || touched_names.contains(simple_symbol_name(reference))
                })
            }) || parsed.imports.iter().any(|import| {
                changed_files.contains(&resolve_import_target(&parsed.file, &import.source))
            }) || (changed_markdown && !parsed.links.is_empty())
            {
                affected_files.insert(parsed.file.clone());
            }
        }

        // Removing any affected file's nodes also removes incoming edges. Grow
        // the reverse closure until no retained source would lose an edge.
        loop {
            let incoming_sources = self
                .graph
                .all_edges()
                .into_iter()
                .filter(|(_, target, _)| affected_files.contains(&target.file))
                .map(|(source, _, _)| source.file.clone())
                .collect::<Vec<_>>();
            let old_len = affected_files.len();
            affected_files.extend(incoming_sources);
            if affected_files.len() == old_len {
                break;
            }
        }

        let context_files = self.graph_builder_context(&affected_files);
        let mut replacement = GraphBuilder::build_from_files(
            self.parsed_files
                .values()
                .filter(|file| context_files.contains(&file.file)),
        );
        replacement.hydrate_missing_bodies_from(self.graph.as_ref());

        // Mutate the active graph in place when no reader retains a snapshot.
        // Arc's copy-on-write behavior still protects concurrent readers, but
        // the normal watcher path avoids an O(all graph nodes) clone.
        let graph = Arc::make_mut(&mut self.graph);
        for rel_path in &affected_files {
            graph.remove_file_nodes(rel_path);
        }

        for node in replacement
            .all_nodes()
            .into_iter()
            .filter(|node| affected_files.contains(&node.file))
        {
            graph.add_node(
                node.id.clone(),
                node.kind,
                node.name.clone(),
                node.signature.clone(),
                node.body.clone(),
                node.file.clone(),
                node.line,
                node.end_line,
                node.is_exported,
                node.language,
            );
        }
        for (source, target, kind) in replacement
            .all_edges()
            .into_iter()
            .filter(|(source, _, _)| affected_files.contains(&source.file))
        {
            graph.add_edge(&source.id, &target.id, kind);
        }

        strip_symbol_bodies(&mut self.parsed_files);
        self.graph_snapshot_id = self.graph_snapshot_id.saturating_add(1);
    }

    /// Collect the files required for `GraphBuilder` to resolve every outgoing
    /// edge from `affected_files`. This scans compact parsed metadata but does
    /// not rebuild unrelated graph nodes.
    fn graph_builder_context(&self, affected_files: &HashSet<String>) -> HashSet<String> {
        let mut context = affected_files.clone();
        let mut referenced_names = HashSet::new();
        let mut import_targets = HashSet::new();
        let mut needs_document_targets = false;

        for parsed in self
            .parsed_files
            .values()
            .filter(|file| affected_files.contains(&file.file))
        {
            for symbol in &parsed.symbols {
                for reference in &symbol.references {
                    referenced_names.insert(simple_symbol_name(reference).to_string());
                }
            }
            import_targets.extend(
                parsed
                    .imports
                    .iter()
                    .map(|import| resolve_import_target(&parsed.file, &import.source)),
            );
            needs_document_targets |= !parsed.links.is_empty();
        }

        for parsed in self.parsed_files.values() {
            if import_targets.contains(&parsed.file)
                || (needs_document_targets && parsed.language == crate::symbols::Language::Markdown)
                || parsed.symbols.iter().any(|symbol| {
                    referenced_names.contains(simple_symbol_name(&symbol.name))
                        || referenced_names.contains(&symbol.name)
                })
            {
                context.insert(parsed.file.clone());
            }
        }
        context
    }

    fn symbol_names_for_files<'a>(
        &self,
        files: impl IntoIterator<Item = &'a str>,
    ) -> HashSet<String> {
        let mut names = HashSet::new();
        for rel_path in files {
            if let Some(parsed) = self.parsed_files.get(rel_path) {
                extend_symbol_names(&mut names, parsed);
            }
        }
        names
    }

    /// Index a directory using parallel file parsing.
    /// Files are parsed concurrently, then the graph is updated once.
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
        let previous_names = old_symbols
            .iter()
            .flat_map(|symbol| {
                [
                    symbol.name.clone(),
                    simple_symbol_name(&symbol.name).to_string(),
                ]
            })
            .collect();
        self.rebuild_changed_files([rel_path], previous_names);

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
        let previous_names = old_symbols
            .iter()
            .flat_map(|symbol| {
                [
                    symbol.name.clone(),
                    simple_symbol_name(&symbol.name).to_string(),
                ]
            })
            .collect();
        self.parsed_files.insert(rel_path.to_string(), new_parsed);
        self.rebuild_changed_files([rel_path], previous_names);
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

fn extend_symbol_names(names: &mut HashSet<String>, parsed: &ParsedFile) {
    for symbol in &parsed.symbols {
        names.insert(symbol.name.clone());
        names.insert(simple_symbol_name(&symbol.name).to_string());
    }
}

fn simple_symbol_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

fn resolve_import_target(from_file: &str, import_source: &str) -> String {
    if !import_source.starts_with('.') {
        return import_source.to_string();
    }

    let directory = from_file
        .rsplit_once('/')
        .map_or(".", |(directory, _)| directory);
    let mut parts = directory.split('/').collect::<Vec<_>>();
    for segment in import_source.split('/') {
        match segment {
            "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }

    let resolved = parts.join("/");
    let has_extension = resolved
        .rsplit('/')
        .next()
        .is_some_and(|file| file.contains('.'));
    if has_extension {
        resolved
    } else {
        let extension = from_file.rsplit('.').next().unwrap_or("ts");
        format!("{resolved}.{extension}")
    }
}

fn strip_symbol_bodies(parsed_files: &mut HashMap<String, ParsedFile>) {
    for parsed_file in parsed_files.values_mut() {
        for symbol in &mut parsed_file.symbols {
            symbol.body.clear();
        }
    }
}
