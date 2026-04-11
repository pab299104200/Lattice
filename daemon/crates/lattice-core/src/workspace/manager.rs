use crate::graph::CodeGraph;
use crate::indexer::Indexer;
use crate::query::capsule::ContextCapsule;
use crate::query::QueryEngine;
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize)]
pub struct CrossRepoEdge {
    pub from_repo: String,
    pub to_repo: String,
    pub dependency_name: String,
    pub edge_type: String, // "npm", "cargo", "pip"
}

pub struct WorkspaceManager {
    repos: HashMap<String, RepoState>,
    cross_repo_edges: Vec<CrossRepoEdge>,
}

struct RepoState {
    #[allow(dead_code)]
    root: PathBuf,
    indexer: Indexer,
}

impl WorkspaceManager {
    pub fn new() -> Self {
        Self {
            repos: HashMap::new(),
            cross_repo_edges: Vec::new(),
        }
    }

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
            let namespaced = repo_rel_path(repo_name, rel_path);
            repo.indexer.index_file_content(&namespaced, content)?;
        }
        Ok(())
    }

    pub fn remove_file(&mut self, repo_name: &str, rel_path: &str) {
        if let Some(repo) = self.repos.get_mut(repo_name) {
            let namespaced = repo_rel_path(repo_name, rel_path);
            repo.indexer.remove_file(&namespaced);
        }
    }

    /// Build a unified graph from all repos.
    pub fn unified_graph(&self) -> CodeGraph {
        self.build_merged_graph()
    }

    pub fn query(&self, query_text: &str) -> ContextCapsule {
        let graph = self.build_merged_graph();
        let mut engine = QueryEngine::new(graph, None, None);
        engine.query(query_text, None, false)
    }

    fn build_merged_graph(&self) -> CodeGraph {
        // For true multi-repo, we merge graphs from all repos
        // by cloning nodes and edges from each into a single graph
        let mut merged = CodeGraph::new();
        for repo in self.repos.values() {
            let source = repo.indexer.graph();
            for node in source.all_nodes() {
                merged.add_node(
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
            for (from, to, kind) in source.all_edges() {
                merged.add_edge(&from.id, &to.id, kind);
            }
        }
        merged
    }

    pub fn repo_count(&self) -> usize {
        self.repos.len()
    }

    pub fn repo_names(&self) -> Vec<&str> {
        self.repos.keys().map(|s| s.as_str()).collect()
    }

    pub fn repo_stats(&self) -> Vec<RepoStats> {
        self.repos
            .iter()
            .map(|(name, state)| {
                let graph = state.indexer.graph();
                RepoStats {
                    name: name.clone(),
                    file_count: state.indexer.file_count(),
                    node_count: graph.node_count(),
                    edge_count: graph.edge_count(),
                }
            })
            .collect()
    }

    /// Detect cross-repo edges by comparing imports in one repo against
    /// exported symbols in other repos. When a symbol in repo A has an import
    /// that resolves to a name exported by repo B, a cross-repo edge is recorded.
    pub fn detect_cross_repo_edges(&mut self) {
        self.cross_repo_edges.clear();

        // Build a map: exported symbol name -> repo name
        let mut exported_symbols: HashMap<String, Vec<String>> = HashMap::new();
        for (repo_name, state) in &self.repos {
            for node in state.indexer.graph().all_nodes() {
                if node.is_exported {
                    exported_symbols
                        .entry(node.name.clone())
                        .or_default()
                        .push(repo_name.clone());
                }
            }
        }

        // For each repo, check if any referenced names match exported symbols
        // from a *different* repo
        for (repo_name, state) in &self.repos {
            for node in state.indexer.graph().all_nodes() {
                // Check references within symbol bodies
                let graph = state.indexer.graph();
                let deps = graph.get_dependencies(&node.id);
                let local_names: std::collections::HashSet<String> =
                    graph.all_nodes().iter().map(|n| n.name.clone()).collect();

                // Also scan import names from the parsed files' import info
                // by looking at the node's body for identifiers that match
                // exported symbols from other repos
                for (sym_name, owner_repos) in &exported_symbols {
                    // Skip if this symbol exists locally in the same repo
                    if local_names.contains(sym_name) && owner_repos.contains(repo_name) {
                        continue;
                    }

                    // Check if the symbol name appears in this node's body
                    // (crude but effective cross-repo reference detection)
                    if node.body.contains(sym_name.as_str()) {
                        for owner_repo in owner_repos {
                            if owner_repo != repo_name {
                                // Determine edge type from file extension
                                let edge_type =
                                    if node.file.ends_with(".ts") || node.file.ends_with(".js") {
                                        "npm"
                                    } else if node.file.ends_with(".rs") {
                                        "cargo"
                                    } else if node.file.ends_with(".py") {
                                        "pip"
                                    } else {
                                        "unknown"
                                    };

                                self.cross_repo_edges.push(CrossRepoEdge {
                                    from_repo: repo_name.clone(),
                                    to_repo: owner_repo.clone(),
                                    dependency_name: sym_name.clone(),
                                    edge_type: edge_type.to_string(),
                                });
                            }
                        }
                    }
                }

                // Suppress unused variable warning
                let _ = deps;
            }
        }

        // Deduplicate edges
        self.cross_repo_edges.sort_by(|a, b| {
            (&a.from_repo, &a.to_repo, &a.dependency_name).cmp(&(
                &b.from_repo,
                &b.to_repo,
                &b.dependency_name,
            ))
        });
        self.cross_repo_edges.dedup_by(|a, b| {
            a.from_repo == b.from_repo
                && a.to_repo == b.to_repo
                && a.dependency_name == b.dependency_name
        });
    }

    /// Get the detected cross-repo edges.
    pub fn cross_repo_edges(&self) -> &[CrossRepoEdge] {
        &self.cross_repo_edges
    }
}

pub fn repo_rel_path(repo_name: &str, rel_path: &str) -> String {
    let normalized = rel_path.replace('\\', "/");
    let trimmed = normalized.trim_start_matches("./").trim_start_matches('/');
    if trimmed.is_empty() {
        repo_name.to_string()
    } else {
        format!("{}/{}", repo_name, trimmed)
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RepoStats {
    pub name: String,
    pub file_count: usize,
    pub node_count: usize,
    pub edge_count: usize,
}
