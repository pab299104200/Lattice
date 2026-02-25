use std::collections::{HashMap, HashSet, VecDeque};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;
use serde::{Deserialize, Serialize};
use crate::symbols::{Language, SymbolId, SymbolKind};

/// The kind of relationship between two symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeKind {
    Calls,
    Imports,
    Implements,
    Extends,
    TypeRef,
    Contains,
    CoChanges,
}

/// A node in the code dependency graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: SymbolId,
    pub kind: SymbolKind,
    pub name: String,
    pub signature: String,
    pub body: String,
    pub file: String,
    pub line: usize,
    pub end_line: usize,
    pub is_exported: bool,
    pub language: Language,
    pub edit_count: u32,
    pub last_modified: u64,
}

/// Summary statistics for the code graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphStats {
    pub node_count: usize,
    pub edge_count: usize,
    pub file_count: usize,
}

/// In-memory dependency graph backed by petgraph.
pub struct CodeGraph {
    graph: DiGraph<GraphNode, EdgeKind>,
    index: HashMap<SymbolId, NodeIndex>,
}

impl CodeGraph {
    /// Create an empty graph.
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            index: HashMap::new(),
        }
    }

    /// Add or update a node in the graph. Returns the NodeIndex.
    pub fn add_node(
        &mut self,
        id: SymbolId,
        kind: SymbolKind,
        name: String,
        signature: String,
        body: String,
        file: String,
        line: usize,
        end_line: usize,
        is_exported: bool,
        language: Language,
    ) -> NodeIndex {
        if let Some(&idx) = self.index.get(&id) {
            // Update existing node
            let node = &mut self.graph[idx];
            node.kind = kind;
            node.name = name;
            node.signature = signature;
            node.body = body;
            node.file = file;
            node.line = line;
            node.end_line = end_line;
            node.is_exported = is_exported;
            node.language = language;
            idx
        } else {
            // Add new node
            let node = GraphNode {
                id: id.clone(),
                kind,
                name,
                signature,
                body,
                file,
                line,
                end_line,
                is_exported,
                language,
                edit_count: 0,
                last_modified: 0,
            };
            let idx = self.graph.add_node(node);
            self.index.insert(id, idx);
            idx
        }
    }

    /// Add an edge between two nodes. Skips duplicates.
    pub fn add_edge(&mut self, from: &SymbolId, to: &SymbolId, kind: EdgeKind) {
        let from_idx = match self.index.get(from) {
            Some(&idx) => idx,
            None => return,
        };
        let to_idx = match self.index.get(to) {
            Some(&idx) => idx,
            None => return,
        };

        // Check for duplicate edge
        for edge in self.graph.edges_connecting(from_idx, to_idx) {
            if *edge.weight() == kind {
                return;
            }
        }

        self.graph.add_edge(from_idx, to_idx, kind);
    }

    /// Get a node by its SymbolId.
    pub fn get_node(&self, id: &SymbolId) -> Option<&GraphNode> {
        self.index.get(id).map(|&idx| &self.graph[idx])
    }

    /// Get outgoing dependencies of a node (symbols this node depends on).
    pub fn get_dependencies(&self, id: &SymbolId) -> Vec<(&GraphNode, EdgeKind)> {
        let idx = match self.index.get(id) {
            Some(&idx) => idx,
            None => return Vec::new(),
        };

        self.graph
            .neighbors_directed(idx, Direction::Outgoing)
            .filter_map(|neighbor_idx| {
                let edge = self.graph.edges_connecting(idx, neighbor_idx).next()?;
                Some((&self.graph[neighbor_idx], *edge.weight()))
            })
            .collect()
    }

    /// Get incoming dependents of a node (symbols that depend on this node).
    pub fn get_dependents(&self, id: &SymbolId) -> Vec<(&GraphNode, EdgeKind)> {
        let idx = match self.index.get(id) {
            Some(&idx) => idx,
            None => return Vec::new(),
        };

        self.graph
            .neighbors_directed(idx, Direction::Incoming)
            .filter_map(|neighbor_idx| {
                let edge = self.graph.edges_connecting(neighbor_idx, idx).next()?;
                Some((&self.graph[neighbor_idx], *edge.weight()))
            })
            .collect()
    }

    /// BFS traversal in both directions up to `hops` hops. Excludes the start node.
    pub fn n_hop_neighbors(&self, id: &SymbolId, hops: usize) -> Vec<&GraphNode> {
        let idx = match self.index.get(id) {
            Some(&idx) => idx,
            None => return Vec::new(),
        };

        let mut visited = HashSet::new();
        visited.insert(idx);
        let mut queue = VecDeque::new();
        queue.push_back((idx, 0usize));
        let mut result = Vec::new();

        while let Some((current, depth)) = queue.pop_front() {
            if depth >= hops {
                continue;
            }

            // Both directions
            for direction in &[Direction::Outgoing, Direction::Incoming] {
                for neighbor in self.graph.neighbors_directed(current, *direction) {
                    if visited.insert(neighbor) {
                        result.push(&self.graph[neighbor]);
                        queue.push_back((neighbor, depth + 1));
                    }
                }
            }
        }

        result
    }

    /// Remove all nodes belonging to a given file. Rebuilds the index afterward.
    pub fn remove_file_nodes(&mut self, file: &str) {
        // Collect node indices to remove
        let to_remove: Vec<NodeIndex> = self
            .graph
            .node_indices()
            .filter(|&idx| self.graph[idx].file == file)
            .collect();

        // Remove nodes in reverse order to avoid index invalidation issues
        // petgraph swaps the removed node with the last node, so remove from highest index first
        let mut to_remove_sorted = to_remove;
        to_remove_sorted.sort_by(|a, b| b.index().cmp(&a.index()));

        for idx in to_remove_sorted {
            self.graph.remove_node(idx);
        }

        // Rebuild index
        self.index.clear();
        for idx in self.graph.node_indices() {
            self.index.insert(self.graph[idx].id.clone(), idx);
        }
    }

    /// Number of nodes in the graph.
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Number of edges in the graph.
    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    /// Get graph statistics including unique file count.
    pub fn stats(&self) -> GraphStats {
        let files: HashSet<&str> = self
            .graph
            .node_indices()
            .map(|idx| self.graph[idx].file.as_str())
            .collect();

        GraphStats {
            node_count: self.graph.node_count(),
            edge_count: self.graph.edge_count(),
            file_count: files.len(),
        }
    }

    /// Get all node SymbolIds.
    pub fn all_node_ids(&self) -> Vec<&SymbolId> {
        self.graph
            .node_indices()
            .map(|idx| &self.graph[idx].id)
            .collect()
    }

    /// Get all nodes.
    pub fn all_nodes(&self) -> Vec<&GraphNode> {
        self.graph
            .node_indices()
            .map(|idx| &self.graph[idx])
            .collect()
    }

    /// Degree centrality: (in_degree + out_degree) / (total_nodes - 1).
    pub fn centrality(&self, id: &SymbolId) -> f64 {
        let idx = match self.index.get(id) {
            Some(&idx) => idx,
            None => return 0.0,
        };

        let total = self.graph.node_count();
        if total <= 1 {
            return 0.0;
        }

        let in_degree = self
            .graph
            .neighbors_directed(idx, Direction::Incoming)
            .count();
        let out_degree = self
            .graph
            .neighbors_directed(idx, Direction::Outgoing)
            .count();

        (in_degree + out_degree) as f64 / (total - 1) as f64
    }
}

impl Default for CodeGraph {
    fn default() -> Self {
        Self::new()
    }
}
