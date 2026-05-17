use crate::symbols::{Language, SymbolId, SymbolKind};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// The kind of relationship between two symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeKind {
    Calls,
    Imports,
    Implements,
    Extends,
    TypeRef,
    Contains,
    LinksTo,
    Mentions,
    CoChanges,
}

impl EdgeKind {
    /// Compact single-char code for token-efficient output.
    pub fn short_code(&self) -> &'static str {
        match self {
            EdgeKind::Calls => "C",
            EdgeKind::Imports => "I",
            EdgeKind::Implements => "M",
            EdgeKind::Extends => "E",
            EdgeKind::TypeRef => "T",
            EdgeKind::Contains => "N",
            EdgeKind::LinksTo => "L",
            EdgeKind::Mentions => "R",
            EdgeKind::CoChanges => "X",
        }
    }
}

/// A node in the code dependency graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: SymbolId,
    pub kind: SymbolKind,
    pub name: String,
    pub signature: Arc<str>,
    pub body: Arc<str>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphPathStep {
    pub node: GraphNode,
    pub edge_kind: EdgeKind,
    pub depth: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphTraversalPath {
    pub target: GraphNode,
    pub steps: Vec<GraphPathStep>,
}

/// In-memory dependency graph backed by petgraph.
#[derive(Clone)]
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
        signature: impl Into<Arc<str>>,
        body: impl Into<Arc<str>>,
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
            node.signature = signature.into();
            node.body = body.into();
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
                signature: signature.into(),
                body: body.into(),
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

    /// Bounded BFS traversal in both directions with the path that produced each target.
    pub fn n_hop_neighbor_paths(&self, id: &SymbolId, hops: usize) -> Vec<GraphTraversalPath> {
        let idx = match self.index.get(id) {
            Some(&idx) => idx,
            None => return Vec::new(),
        };

        let mut visited = HashSet::new();
        visited.insert(idx);
        let mut queue = VecDeque::new();
        queue.push_back((idx, 0usize, Vec::<GraphPathStep>::new()));
        let mut result = Vec::new();

        while let Some((current, depth, path)) = queue.pop_front() {
            if depth >= hops {
                continue;
            }

            for (neighbor, edge_kind) in self.neighbor_edges(current) {
                if !visited.insert(neighbor) {
                    continue;
                }
                let mut next_path = path.clone();
                next_path.push(GraphPathStep {
                    node: self.graph[neighbor].clone(),
                    edge_kind,
                    depth: depth + 1,
                });
                result.push(GraphTraversalPath {
                    target: self.graph[neighbor].clone(),
                    steps: next_path.clone(),
                });
                queue.push_back((neighbor, depth + 1, next_path));
            }
        }

        result
    }

    fn neighbor_edges(&self, idx: NodeIndex) -> Vec<(NodeIndex, EdgeKind)> {
        let mut edges = Vec::new();
        for direction in [Direction::Outgoing, Direction::Incoming] {
            for neighbor in self.graph.neighbors_directed(idx, direction) {
                if let Some(edge_kind) = self.edge_kind_between(idx, neighbor, direction) {
                    edges.push((neighbor, edge_kind));
                }
            }
        }
        edges
    }

    fn edge_kind_between(
        &self,
        idx: NodeIndex,
        neighbor: NodeIndex,
        direction: Direction,
    ) -> Option<EdgeKind> {
        let (from, to) = match direction {
            Direction::Outgoing => (idx, neighbor),
            Direction::Incoming => (neighbor, idx),
        };
        self.graph
            .edges_connecting(from, to)
            .next()
            .map(|edge| *edge.weight())
    }

    /// BFS traversal following only incoming edges (dependents) up to `hops` hops.
    /// Returns all transitive dependents, excluding the start node.
    pub fn get_transitive_dependents(&self, id: &SymbolId, hops: usize) -> Vec<&GraphNode> {
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

            // Only follow incoming edges (reverse direction = dependents)
            for neighbor in self.graph.neighbors_directed(current, Direction::Incoming) {
                if visited.insert(neighbor) {
                    result.push(&self.graph[neighbor]);
                    queue.push_back((neighbor, depth + 1));
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

    /// Get all edges as (from_node, to_node, edge_kind) triples.
    pub fn all_edges(&self) -> Vec<(&GraphNode, &GraphNode, EdgeKind)> {
        self.graph
            .edge_indices()
            .filter_map(|edge_idx| {
                let (from_idx, to_idx) = self.graph.edge_endpoints(edge_idx)?;
                let kind = *self.graph.edge_weight(edge_idx)?;
                Some((&self.graph[from_idx], &self.graph[to_idx], kind))
            })
            .collect()
    }

    /// Get a mutable reference to a node by its petgraph NodeIndex.
    pub fn get_node_mut_by_index(&mut self, idx: NodeIndex) -> Option<&mut GraphNode> {
        self.graph.node_weight_mut(idx)
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

    /// Find all simple paths from one node to another, following only Calls edges.
    /// Returns up to `max_results` paths, each path is a Vec of &GraphNode.
    pub fn find_call_paths(
        &self,
        from: &SymbolId,
        to: &SymbolId,
        max_depth: usize,
        max_results: usize,
    ) -> Vec<Vec<&GraphNode>> {
        let from_idx = match self.index.get(from) {
            Some(&idx) => idx,
            None => return Vec::new(),
        };
        let to_idx = match self.index.get(to) {
            Some(&idx) => idx,
            None => return Vec::new(),
        };

        // Build a subgraph containing only Calls edges
        let calls_only: petgraph::graph::DiGraph<NodeIndex, ()> = {
            let mut sub = petgraph::graph::DiGraph::new();
            let mut idx_map: HashMap<NodeIndex, petgraph::graph::NodeIndex> = HashMap::new();
            for idx in self.graph.node_indices() {
                let new_idx = sub.add_node(idx);
                idx_map.insert(idx, new_idx);
            }
            for edge_idx in self.graph.edge_indices() {
                if let Some((src, tgt)) = self.graph.edge_endpoints(edge_idx) {
                    if let Some(weight) = self.graph.edge_weight(edge_idx) {
                        if *weight == EdgeKind::Calls {
                            if let (Some(&s), Some(&t)) = (idx_map.get(&src), idx_map.get(&tgt)) {
                                sub.add_edge(s, t, ());
                            }
                        }
                    }
                }
            }
            sub
        };

        // Map our from/to indices through the subgraph
        let sub_from = calls_only
            .node_indices()
            .find(|&idx| calls_only[idx] == from_idx);
        let sub_to = calls_only
            .node_indices()
            .find(|&idx| calls_only[idx] == to_idx);

        let (sub_from, sub_to) = match (sub_from, sub_to) {
            (Some(f), Some(t)) => (f, t),
            _ => return Vec::new(),
        };

        // max_depth is number of edges/hops; intermediate_nodes = edges - 1
        let max_intermediates = max_depth.saturating_sub(1);
        let paths: Vec<Vec<petgraph::graph::NodeIndex>> = petgraph::algo::all_simple_paths(
            &calls_only,
            sub_from,
            sub_to,
            0,
            Some(max_intermediates),
        )
        .take(max_results)
        .collect();

        paths
            .iter()
            .map(|path| {
                path.iter()
                    .map(|&sub_idx| {
                        let original_idx = calls_only[sub_idx];
                        &self.graph[original_idx]
                    })
                    .collect()
            })
            .collect()
    }
}

impl Default for CodeGraph {
    fn default() -> Self {
        Self::new()
    }
}
