//! Pure graph-derived health facts.
//!
//! The producer is a total function of the built [`CodeGraph`]. It performs no
//! IO, no clock reads, and no floating-point arithmetic, so an identical graph
//! yields a byte-identical snapshot on every platform. Persistence lives in
//! [`crate::storage::health_graph_facts_store`].
//!
//! Two invariants shape the design:
//!
//! * **Unknown is never zero.** A file with no observed dependency edge has no
//!   defined instability; the fact is `None`, never `0`. Whole-snapshot
//!   incompleteness is reported through [`GraphFactsReport::availability`].
//! * **Determinism before convenience.** Every intermediate collection is
//!   ordered (`BTreeMap`/`BTreeSet`), and every output vector is sorted by a
//!   total key, so no `HashMap` iteration order can leak into a persisted fact.
//!
//! See `docs/plans/2026-08-13-health-engine.md`, section "H2.1 Graph facts".

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::git_intelligence::canonical_repository_path;
use crate::graph::{CodeGraph, EdgeKind};

/// Aggregation version persisted with every snapshot.
///
/// Bump this whenever the meaning of a produced value changes so that stored
/// generations from an older producer are recognisably different rather than
/// silently misread.
pub const GRAPH_FACTS_VERSION: u32 = 1;

/// Upper bound on persisted per-file fact rows.
pub const MAX_FILE_FACTS: usize = 200_000;

/// Upper bound on persisted per-symbol fact rows.
pub const MAX_SYMBOL_FACTS: usize = 1_000_000;

/// Separator between the file part and the name part of a symbol fact key.
const SYMBOL_KEY_SEPARATOR: &str = "::";

/// Edge kinds that constitute a static dependency for health purposes.
///
/// `Contains` is structural nesting, not dependency. `LinksTo`, `Mentions`, and
/// `CoChanges` are documentation- and history-derived; admitting them would mix
/// non-static evidence into a fact family declared to be graph-only.
fn is_dependency_edge(kind: EdgeKind) -> bool {
    match kind {
        EdgeKind::Calls
        | EdgeKind::Imports
        | EdgeKind::Implements
        | EdgeKind::Extends
        | EdgeKind::TypeRef => true,
        EdgeKind::Contains | EdgeKind::LinksTo | EdgeKind::Mentions | EdgeKind::CoChanges => false,
    }
}

/// Effective aggregation bounds persisted alongside a snapshot generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphFactLimits {
    pub max_files: usize,
    pub max_symbols: usize,
}

impl Default for GraphFactLimits {
    fn default() -> Self {
        Self {
            max_files: MAX_FILE_FACTS,
            max_symbols: MAX_SYMBOL_FACTS,
        }
    }
}

impl GraphFactLimits {
    fn bounded(self) -> Self {
        Self {
            max_files: self.max_files.min(MAX_FILE_FACTS),
            max_symbols: self.max_symbols.min(MAX_SYMBOL_FACTS),
        }
    }
}

/// How much of a fact family a consumer is entitled to trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactAvailability {
    /// Every input was observed; the facts are complete.
    Available,
    /// Facts were produced from a partial graph and understate reality.
    Degraded,
    /// Nothing was observed; consumers must not treat absence as zero.
    Unavailable,
}

/// Completeness evidence for one pure aggregation run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphFactsReport {
    pub facts_version: u32,
    pub limits: GraphFactLimits,
    /// Graph nodes examined, including nodes rejected for a non-canonical path.
    pub nodes_seen: u64,
    /// Graph edges examined, including non-dependency and rejected edges.
    pub edges_seen: u64,
    /// Distinct canonical files that contributed at least one node.
    pub files_observed: u32,
    /// Distinct exported symbol keys that contributed at least one definition.
    pub symbols_observed: u32,
    /// Nodes whose file path was not canonical and repository-relative.
    pub invalid_path_nodes: u64,
    /// Dependency edges dropped because an endpoint had a non-canonical path.
    pub invalid_path_edges: u64,
    /// True when file rows were truncated by `limits.max_files`.
    pub file_overflow: bool,
    /// True when symbol rows were truncated by `limits.max_symbols`.
    pub symbol_overflow: bool,
    /// Caller-supplied truth about whether the index behind the graph was whole.
    pub index_complete: bool,
}

impl GraphFactsReport {
    /// Complete snapshots are the only snapshots eligible to affect scores.
    pub fn is_complete(&self) -> bool {
        !self.is_degraded()
    }

    pub fn is_degraded(&self) -> bool {
        !self.index_complete
            || self.invalid_path_nodes > 0
            || self.invalid_path_edges > 0
            || self.file_overflow
            || self.symbol_overflow
    }

    /// An empty graph is unavailable, not "everything is zero".
    pub fn availability(&self) -> FactAvailability {
        if self.files_observed == 0 {
            FactAvailability::Unavailable
        } else if self.is_degraded() {
            FactAvailability::Degraded
        } else {
            FactAvailability::Available
        }
    }
}

/// Graph facts for one canonical repository-relative file path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileGraphFacts {
    pub path: String,
    /// Distinct other files that depend on this file (afferent coupling, Ca).
    pub fan_in: u32,
    /// Distinct other files this file depends on (efferent coupling, Ce).
    pub fan_out: u32,
}

/// Graph facts for one exported symbol, keyed by `<path>::<name>`.
///
/// Same-named exported definitions in one file (overloads, repeated `impl`
/// members) share a key and are aggregated, because a key that embedded a byte
/// offset would change on every unrelated edit above the symbol and would not
/// be comparable across generations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolGraphFacts {
    pub key: String,
    pub path: String,
    pub name: String,
    /// Distinct other symbols that depend on this symbol.
    pub fan_in: u32,
    /// Distinct other symbols this symbol depends on.
    pub fan_out: u32,
    /// Exported definitions collapsed into this key.
    pub definition_count: u32,
}

/// Stable graph-derived facts consumed by health scoring and verb payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphFactsSnapshot {
    /// Ordered by `path`, unique.
    pub files: Vec<FileGraphFacts>,
    /// Ordered by `key`, unique.
    pub symbols: Vec<SymbolGraphFacts>,
    pub report: GraphFactsReport,
}

impl GraphFactsSnapshot {
    /// Empty snapshots are honest: no graph was available, not zero coupling.
    pub fn empty() -> Self {
        Self {
            files: Vec::new(),
            symbols: Vec::new(),
            report: GraphFactsReport {
                facts_version: GRAPH_FACTS_VERSION,
                limits: GraphFactLimits::default(),
                nodes_seen: 0,
                edges_seen: 0,
                files_observed: 0,
                symbols_observed: 0,
                invalid_path_nodes: 0,
                invalid_path_edges: 0,
                file_overflow: false,
                symbol_overflow: false,
                index_complete: true,
            },
        }
    }

    pub fn availability(&self) -> FactAvailability {
        self.report.availability()
    }

    /// Looks up one file without requiring consumers to rebuild an index.
    pub fn file(&self, path: &str) -> Option<&FileGraphFacts> {
        let path = canonical_repository_path(path)?;
        self.files
            .binary_search_by(|candidate| candidate.path.cmp(&path))
            .ok()
            .map(|index| &self.files[index])
    }

    /// Looks up one exported symbol by its `<path>::<name>` fact key.
    pub fn symbol(&self, key: &str) -> Option<&SymbolGraphFacts> {
        self.symbols
            .binary_search_by(|candidate| candidate.key.as_str().cmp(key))
            .ok()
            .map(|index| &self.symbols[index])
    }
}

/// Builds a [`SymbolGraphFacts::key`] from a canonical path and a symbol name.
pub fn symbol_fact_key(path: &str, name: &str) -> String {
    format!("{path}{SYMBOL_KEY_SEPARATOR}{name}")
}

/// Pure producer of graph facts.
#[derive(Debug, Clone, Copy, Default)]
pub struct GraphFactProducer {
    limits: GraphFactLimits,
}

impl GraphFactProducer {
    pub fn new(limits: GraphFactLimits) -> Self {
        Self {
            limits: limits.bounded(),
        }
    }

    /// Produces a snapshot from `graph`.
    ///
    /// `index_complete` is the caller's truth about whether the index behind
    /// the graph covered the whole workspace. A `false` value never changes a
    /// computed number; it only marks the snapshot degraded so consumers can
    /// refuse to treat an absent dependent as a real absence.
    pub fn produce(&self, graph: &CodeGraph, index_complete: bool) -> GraphFactsSnapshot {
        let limits = self.limits.bounded();
        let mut nodes_seen = 0_u64;
        let mut invalid_path_nodes = 0_u64;

        // Canonical file set, plus the per-node canonical path used by the edge
        // pass. Nodes with an unusable path are excluded from both.
        let mut files: BTreeSet<String> = BTreeSet::new();
        let mut symbols: BTreeMap<String, SymbolAccumulator> = BTreeMap::new();

        for node in graph.all_nodes() {
            nodes_seen = nodes_seen.saturating_add(1);
            let Some(path) = canonical_repository_path(&node.file) else {
                invalid_path_nodes = invalid_path_nodes.saturating_add(1);
                continue;
            };
            files.insert(path.clone());
            if !node.is_exported {
                continue;
            }
            let name = node.name.trim();
            if name.is_empty() {
                continue;
            }
            let key = symbol_fact_key(&path, name);
            let entry = symbols.entry(key).or_insert_with(|| SymbolAccumulator {
                path: path.clone(),
                name: name.to_owned(),
                definition_count: 0,
                dependents: BTreeSet::new(),
                dependencies: BTreeSet::new(),
            });
            entry.definition_count = entry.definition_count.saturating_add(1);
        }

        let mut edges_seen = 0_u64;
        let mut invalid_path_edges = 0_u64;
        let mut file_coupling: BTreeMap<String, FileCoupling> = BTreeMap::new();

        for (from, to, kind) in graph.all_edges() {
            edges_seen = edges_seen.saturating_add(1);
            if !is_dependency_edge(kind) {
                continue;
            }
            let (Some(from_path), Some(to_path)) = (
                canonical_repository_path(&from.file),
                canonical_repository_path(&to.file),
            ) else {
                invalid_path_edges = invalid_path_edges.saturating_add(1);
                continue;
            };

            if from_path != to_path {
                file_coupling
                    .entry(from_path.clone())
                    .or_default()
                    .dependencies
                    .insert(to_path.clone());
                file_coupling
                    .entry(to_path.clone())
                    .or_default()
                    .dependents
                    .insert(from_path.clone());
            }

            let from_name = from.name.trim();
            let to_name = to.name.trim();
            if from_name.is_empty() || to_name.is_empty() {
                continue;
            }
            let from_key = symbol_fact_key(&from_path, from_name);
            let to_key = symbol_fact_key(&to_path, to_name);
            if from_key == to_key {
                continue;
            }
            if let Some(entry) = symbols.get_mut(&to_key) {
                entry.dependents.insert(from_key.clone());
            }
            if let Some(entry) = symbols.get_mut(&from_key) {
                entry.dependencies.insert(to_key);
            }
        }

        let files_observed = saturating_u32(files.len());
        let symbols_observed = saturating_u32(symbols.len());
        let file_overflow = files.len() > limits.max_files;
        let symbol_overflow = symbols.len() > limits.max_symbols;

        let file_facts: Vec<FileGraphFacts> = files
            .into_iter()
            .take(limits.max_files)
            .map(|path| {
                let coupling = file_coupling.get(&path);
                FileGraphFacts {
                    fan_in: coupling.map_or(0, |value| saturating_u32(value.dependents.len())),
                    fan_out: coupling.map_or(0, |value| saturating_u32(value.dependencies.len())),
                    path,
                }
            })
            .collect();

        let symbol_facts: Vec<SymbolGraphFacts> = symbols
            .into_iter()
            .take(limits.max_symbols)
            .map(|(key, accumulator)| SymbolGraphFacts {
                key,
                path: accumulator.path,
                name: accumulator.name,
                fan_in: saturating_u32(accumulator.dependents.len()),
                fan_out: saturating_u32(accumulator.dependencies.len()),
                definition_count: accumulator.definition_count,
            })
            .collect();

        GraphFactsSnapshot {
            files: file_facts,
            symbols: symbol_facts,
            report: GraphFactsReport {
                facts_version: GRAPH_FACTS_VERSION,
                limits,
                nodes_seen,
                edges_seen,
                files_observed,
                symbols_observed,
                invalid_path_nodes,
                invalid_path_edges,
                file_overflow,
                symbol_overflow,
                index_complete,
            },
        }
    }
}

#[derive(Debug, Default)]
struct FileCoupling {
    dependents: BTreeSet<String>,
    dependencies: BTreeSet<String>,
}

#[derive(Debug)]
struct SymbolAccumulator {
    path: String,
    name: String,
    definition_count: u32,
    dependents: BTreeSet<String>,
    dependencies: BTreeSet<String>,
}

fn saturating_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
pub(crate) mod fixtures {
    use crate::graph::{CodeGraph, EdgeKind};
    use crate::symbols::{Language, SymbolId, SymbolKind};

    pub fn symbol(file: &str, name: &str) -> SymbolId {
        SymbolId {
            file: file.to_owned(),
            name: name.to_owned(),
            byte_offset: 0,
        }
    }

    pub fn symbol_at(file: &str, name: &str, byte_offset: usize) -> SymbolId {
        SymbolId {
            file: file.to_owned(),
            name: name.to_owned(),
            byte_offset,
        }
    }

    /// Adds an exported function node with an explicit source range.
    pub fn add_symbol(graph: &mut CodeGraph, id: &SymbolId, exported: bool, line: usize) {
        graph.add_node(
            id.clone(),
            SymbolKind::Function,
            id.name.clone(),
            format!("fn {}()", id.name),
            "",
            id.file.clone(),
            line,
            line + 4,
            exported,
            Language::Rust,
        );
    }

    pub fn add_edge(graph: &mut CodeGraph, from: &SymbolId, to: &SymbolId, kind: EdgeKind) {
        graph.add_edge(from, to, kind);
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    /// a.rs -> b.rs -> c.rs, plus an intra-file edge that must not couple.
    fn chain_graph() -> CodeGraph {
        let mut graph = CodeGraph::new();
        let a = symbol("src/a.rs", "alpha");
        let a_helper = symbol_at("src/a.rs", "alpha_helper", 100);
        let b = symbol("src/b.rs", "beta");
        let c = symbol("src/c.rs", "gamma");
        add_symbol(&mut graph, &a, true, 1);
        add_symbol(&mut graph, &a_helper, false, 20);
        add_symbol(&mut graph, &b, true, 1);
        add_symbol(&mut graph, &c, true, 1);
        add_edge(&mut graph, &a, &a_helper, EdgeKind::Calls);
        add_edge(&mut graph, &a, &b, EdgeKind::Imports);
        add_edge(&mut graph, &b, &c, EdgeKind::Calls);
        graph
    }

    #[test]
    fn file_fan_counts_distinct_other_files_only() {
        let snapshot = GraphFactProducer::default().produce(&chain_graph(), true);
        let paths: Vec<&str> = snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(paths, ["src/a.rs", "src/b.rs", "src/c.rs"]);

        let a = snapshot.file("src/a.rs").unwrap();
        assert_eq!((a.fan_in, a.fan_out), (0, 1));
        let b = snapshot.file("src/b.rs").unwrap();
        assert_eq!((b.fan_in, b.fan_out), (1, 1));
        let c = snapshot.file("src/c.rs").unwrap();
        assert_eq!((c.fan_in, c.fan_out), (1, 0));
    }

    #[test]
    fn repeated_edges_between_two_files_count_once() {
        let mut graph = CodeGraph::new();
        let first = symbol("src/a.rs", "one");
        let second = symbol_at("src/a.rs", "two", 50);
        let target = symbol("src/b.rs", "target");
        add_symbol(&mut graph, &first, true, 1);
        add_symbol(&mut graph, &second, true, 10);
        add_symbol(&mut graph, &target, true, 1);
        add_edge(&mut graph, &first, &target, EdgeKind::Calls);
        add_edge(&mut graph, &second, &target, EdgeKind::TypeRef);

        let snapshot = GraphFactProducer::default().produce(&graph, true);
        assert_eq!(snapshot.file("src/a.rs").unwrap().fan_out, 1);
        assert_eq!(snapshot.file("src/b.rs").unwrap().fan_in, 1);
        assert_eq!(snapshot.symbol("src/b.rs::target").unwrap().fan_in, 2);
    }

    #[test]
    fn non_dependency_edges_never_couple_files() {
        let mut graph = CodeGraph::new();
        let module = symbol("src/a.rs", "module");
        let member = symbol("src/b.rs", "member");
        add_symbol(&mut graph, &module, true, 1);
        add_symbol(&mut graph, &member, true, 1);
        for kind in [
            EdgeKind::Contains,
            EdgeKind::LinksTo,
            EdgeKind::Mentions,
            EdgeKind::CoChanges,
        ] {
            add_edge(&mut graph, &module, &member, kind);
        }

        let snapshot = GraphFactProducer::default().produce(&graph, true);
        assert_eq!(snapshot.file("src/a.rs").unwrap().fan_out, 0);
        assert_eq!(snapshot.file("src/b.rs").unwrap().fan_in, 0);
        assert_eq!(snapshot.report.edges_seen, 4);
    }

    #[test]
    fn only_exported_symbols_receive_symbol_facts() {
        let snapshot = GraphFactProducer::default().produce(&chain_graph(), true);
        let keys: Vec<&str> = snapshot
            .symbols
            .iter()
            .map(|symbol| symbol.key.as_str())
            .collect();
        assert_eq!(
            keys,
            ["src/a.rs::alpha", "src/b.rs::beta", "src/c.rs::gamma"]
        );
        // The private helper is still a valid dependency of an exported symbol.
        assert_eq!(snapshot.symbol("src/a.rs::alpha").unwrap().fan_out, 2);
    }

    #[test]
    fn same_named_definitions_collapse_into_one_key() {
        let mut graph = CodeGraph::new();
        let first = symbol_at("src/a.rs", "New", 0);
        let second = symbol_at("src/a.rs", "New", 400);
        let caller = symbol("src/b.rs", "caller");
        add_symbol(&mut graph, &first, true, 1);
        add_symbol(&mut graph, &second, true, 40);
        add_symbol(&mut graph, &caller, true, 1);
        add_edge(&mut graph, &caller, &first, EdgeKind::Calls);
        add_edge(&mut graph, &caller, &second, EdgeKind::Calls);

        let snapshot = GraphFactProducer::default().produce(&graph, true);
        let facts = snapshot.symbol("src/a.rs::New").unwrap();
        assert_eq!(facts.definition_count, 2);
        assert_eq!(facts.fan_in, 1);
    }

    #[test]
    fn non_canonical_paths_are_excluded_and_reported_as_degraded() {
        let mut graph = CodeGraph::new();
        let outside = symbol("../outside.rs", "escape");
        let inside = symbol("src/inside.rs", "held");
        add_symbol(&mut graph, &outside, true, 1);
        add_symbol(&mut graph, &inside, true, 1);
        add_edge(&mut graph, &outside, &inside, EdgeKind::Calls);

        let snapshot = GraphFactProducer::default().produce(&graph, true);
        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.file("src/inside.rs").unwrap().fan_in, 0);
        assert_eq!(snapshot.report.invalid_path_nodes, 1);
        assert_eq!(snapshot.report.invalid_path_edges, 1);
        assert_eq!(snapshot.availability(), FactAvailability::Degraded);
    }

    #[test]
    fn paths_are_canonicalized_before_keying() {
        let mut graph = CodeGraph::new();
        let messy = symbol("./src//a.rs", "alpha");
        let target = symbol("src\\b.rs", "beta");
        add_symbol(&mut graph, &messy, true, 1);
        add_symbol(&mut graph, &target, true, 1);
        add_edge(&mut graph, &messy, &target, EdgeKind::Imports);

        let snapshot = GraphFactProducer::default().produce(&graph, true);
        assert_eq!(snapshot.file("src/a.rs").unwrap().fan_out, 1);
        assert_eq!(snapshot.symbol("src/b.rs::beta").unwrap().fan_in, 1);
        assert_eq!(snapshot.report.invalid_path_nodes, 0);
    }

    #[test]
    fn empty_graph_is_unavailable_not_zero() {
        let snapshot = GraphFactProducer::default().produce(&CodeGraph::new(), true);
        assert_eq!(snapshot.availability(), FactAvailability::Unavailable);
        assert_eq!(snapshot, GraphFactsSnapshot::empty());
    }

    #[test]
    fn incomplete_index_degrades_without_altering_counts() {
        let graph = chain_graph();
        let complete = GraphFactProducer::default().produce(&graph, true);
        let partial = GraphFactProducer::default().produce(&graph, false);
        assert_eq!(complete.files, partial.files);
        assert_eq!(complete.symbols, partial.symbols);
        assert_eq!(complete.availability(), FactAvailability::Available);
        assert_eq!(partial.availability(), FactAvailability::Degraded);
    }

    #[test]
    fn production_is_deterministic_across_insertion_orders() {
        let forward = GraphFactProducer::default().produce(&chain_graph(), true);
        let mut reversed = CodeGraph::new();
        let c = symbol("src/c.rs", "gamma");
        let b = symbol("src/b.rs", "beta");
        let a_helper = symbol_at("src/a.rs", "alpha_helper", 100);
        let a = symbol("src/a.rs", "alpha");
        add_symbol(&mut reversed, &c, true, 1);
        add_symbol(&mut reversed, &b, true, 1);
        add_symbol(&mut reversed, &a_helper, false, 20);
        add_symbol(&mut reversed, &a, true, 1);
        add_edge(&mut reversed, &b, &c, EdgeKind::Calls);
        add_edge(&mut reversed, &a, &b, EdgeKind::Imports);
        add_edge(&mut reversed, &a, &a_helper, EdgeKind::Calls);

        let backward = GraphFactProducer::default().produce(&reversed, true);
        assert_eq!(forward.files, backward.files);
        assert_eq!(forward.symbols, backward.symbols);
        assert_eq!(
            serde_json::to_string(&forward).unwrap(),
            serde_json::to_string(&backward).unwrap()
        );
    }

    #[test]
    fn limits_truncate_and_flag_overflow() {
        let producer = GraphFactProducer::new(GraphFactLimits {
            max_files: 2,
            max_symbols: 1,
        });
        let snapshot = producer.produce(&chain_graph(), true);
        assert_eq!(snapshot.files.len(), 2);
        assert_eq!(snapshot.files[0].path, "src/a.rs");
        assert_eq!(snapshot.symbols.len(), 1);
        assert!(snapshot.report.file_overflow);
        assert!(snapshot.report.symbol_overflow);
        assert_eq!(snapshot.report.files_observed, 3);
        assert_eq!(snapshot.availability(), FactAvailability::Degraded);
    }
}
