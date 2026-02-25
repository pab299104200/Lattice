use std::collections::HashMap;
use std::path::PathBuf;
use anyhow::Result;
use crate::indexer::Indexer;
use crate::graph::CodeGraph;
use crate::query::QueryEngine;
use crate::query::capsule::ContextCapsule;

pub struct WorkspaceManager {
    repos: HashMap<String, RepoState>,
}

struct RepoState {
    #[allow(dead_code)]
    root: PathBuf,
    indexer: Indexer,
}

impl WorkspaceManager {
    pub fn new() -> Self { Self { repos: HashMap::new() } }

    pub fn add_repo(&mut self, name: String, root: PathBuf) -> Result<()> {
        let indexer = Indexer::new(root.clone());
        self.repos.insert(name, RepoState { root, indexer });
        Ok(())
    }

    pub fn remove_repo(&mut self, name: &str) {
        self.repos.remove(name);
    }

    pub fn index_file(&mut self, repo_name: &str, rel_path: &str, content: &str) -> Result<()> {
        if let Some(repo) = self.repos.get_mut(repo_name) {
            repo.indexer.index_file_content(rel_path, content)?;
        }
        Ok(())
    }

    pub fn remove_file(&mut self, repo_name: &str, rel_path: &str) {
        if let Some(repo) = self.repos.get_mut(repo_name) {
            repo.indexer.remove_file(rel_path);
        }
    }

    /// Build a unified graph from all repos.
    pub fn unified_graph(&self) -> CodeGraph {
        self.build_merged_graph()
    }

    pub fn query(&self, query_text: &str) -> ContextCapsule {
        let graph = self.build_merged_graph();
        let engine = QueryEngine::new(graph, None);
        engine.query(query_text, None)
    }

    fn build_merged_graph(&self) -> CodeGraph {
        // For true multi-repo, we merge graphs from all repos
        // by cloning nodes and edges from each into a single graph
        let mut merged = CodeGraph::new();
        for repo in self.repos.values() {
            let source = repo.indexer.graph();
            for node in source.all_nodes() {
                merged.add_node(
                    node.id.clone(), node.kind, node.name.clone(),
                    node.signature.clone(), node.body.clone(),
                    node.file.clone(), node.line, node.end_line,
                    node.is_exported, node.language,
                );
            }
            for (from, to, kind) in source.all_edges() {
                merged.add_edge(&from.id, &to.id, kind);
            }
        }
        merged
    }

    pub fn repo_count(&self) -> usize { self.repos.len() }

    pub fn repo_names(&self) -> Vec<&str> {
        self.repos.keys().map(|s| s.as_str()).collect()
    }

    pub fn repo_stats(&self) -> Vec<RepoStats> {
        self.repos.iter().map(|(name, state)| {
            let graph = state.indexer.graph();
            RepoStats {
                name: name.clone(),
                file_count: state.indexer.file_count(),
                node_count: graph.node_count(),
                edge_count: graph.edge_count(),
            }
        }).collect()
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RepoStats {
    pub name: String,
    pub file_count: usize,
    pub node_count: usize,
    pub edge_count: usize,
}
