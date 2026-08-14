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

use petgraph::algo::tarjan_scc;
use petgraph::graph::DiGraph;
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

/// Upper bound on persisted unstable-dependency rows.
pub const MAX_UNSTABLE_DEPENDENCIES: usize = 50_000;

/// Default minimum instability gap, in per mille, before a dependency from a
/// more stable file to a less stable one is flagged.
///
/// A small gap is normal churn in any real codebase; the flag is meant for the
/// clear direction violations that make a stable module hostage to a volatile
/// one, so the default deliberately sits well above noise.
pub const DEFAULT_UNSTABLE_DEPENDENCY_THRESHOLD_PER_MILLE: u16 = 250;

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
    pub max_unstable_dependencies: usize,
}

impl Default for GraphFactLimits {
    fn default() -> Self {
        Self {
            max_files: MAX_FILE_FACTS,
            max_symbols: MAX_SYMBOL_FACTS,
            max_unstable_dependencies: MAX_UNSTABLE_DEPENDENCIES,
        }
    }
}

impl GraphFactLimits {
    fn bounded(self) -> Self {
        Self {
            max_files: self.max_files.min(MAX_FILE_FACTS),
            max_symbols: self.max_symbols.min(MAX_SYMBOL_FACTS),
            max_unstable_dependencies: self
                .max_unstable_dependencies
                .min(MAX_UNSTABLE_DEPENDENCIES),
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
    /// Instability gap, in per mille, above which a dependency is flagged.
    pub unstable_dependency_threshold_per_mille: u16,
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
    /// True when unstable-dependency rows were truncated by their limit.
    pub unstable_dependency_overflow: bool,
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
            || self.unstable_dependency_overflow
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
    /// Identifier of this file's strongly connected component: the lexically
    /// smallest member path.
    ///
    /// A dense integer index would be the obvious encoding, but it would
    /// renumber on every unrelated file insertion and so make the incremental
    /// delta claim every file had changed. The representative path only moves
    /// when the component's own membership moves, and it is directly
    /// explainable to a reader ("in the cycle anchored at src/a.rs"). Every
    /// file has one; a file outside any cycle represents itself.
    pub scc_id: String,
    /// Number of files in this file's strongly connected component (>= 1).
    pub scc_size: u32,
    /// True when this file participates in a dependency cycle (`scc_size > 1`).
    pub cycle_member: bool,
    /// Martin instability `Ce / (Ca + Ce)` in per mille, truncated toward zero.
    ///
    /// `None` for a file with no observed dependency in either direction: an
    /// isolated file has no defined instability, and reporting `0` would claim
    /// maximum stability for a file about which nothing is known.
    pub instability_per_mille: Option<u16>,
}

/// A dependency pointing from a more stable file to a less stable one.
///
/// This is the classic stable-dependencies direction violation: the depending
/// file is harder to change than the file it relies on, so the dependency's
/// volatility propagates into code that is supposed to be settled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnstableDependencySignal {
    /// The more stable file, which holds the dependency.
    pub from_path: String,
    /// The less stable file being depended upon.
    pub to_path: String,
    pub from_instability_per_mille: u16,
    pub to_instability_per_mille: u16,
    /// `to_instability_per_mille - from_instability_per_mille`, always > 0.
    pub instability_gap_per_mille: u16,
    /// Kind of the representative import/call edge behind this file pair.
    pub edge_kind: EdgeKind,
    /// Name of the depending symbol that carries the representative edge.
    pub from_symbol: String,
    /// Name of the depended-upon symbol.
    pub to_symbol: String,
    /// First line of the depending symbol's source range (1-based, as parsed).
    pub source_line: u32,
    /// Last line of the depending symbol's source range.
    pub source_end_line: u32,
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
    /// Ordered by descending gap, then by `(from_path, to_path)`; unique pairs.
    pub unstable_dependencies: Vec<UnstableDependencySignal>,
    pub report: GraphFactsReport,
}

impl GraphFactsSnapshot {
    /// Empty snapshots are honest: no graph was available, not zero coupling.
    pub fn empty() -> Self {
        Self {
            files: Vec::new(),
            symbols: Vec::new(),
            unstable_dependencies: Vec::new(),
            report: GraphFactsReport {
                facts_version: GRAPH_FACTS_VERSION,
                limits: GraphFactLimits::default(),
                unstable_dependency_threshold_per_mille:
                    DEFAULT_UNSTABLE_DEPENDENCY_THRESHOLD_PER_MILLE,
                nodes_seen: 0,
                edges_seen: 0,
                files_observed: 0,
                symbols_observed: 0,
                invalid_path_nodes: 0,
                invalid_path_edges: 0,
                file_overflow: false,
                symbol_overflow: false,
                unstable_dependency_overflow: false,
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

    /// Which files' facts moved between `previous` and this snapshot.
    ///
    /// Both vectors are ordered by path, so this is a single linear merge.
    pub fn file_delta(&self, previous: &GraphFactsSnapshot) -> FileFactDelta {
        let mut delta = FileFactDelta::default();
        let mut current = self.files.iter().peekable();
        let mut earlier = previous.files.iter().peekable();
        loop {
            match (current.peek(), earlier.peek()) {
                (Some(new), Some(old)) => match new.path.cmp(&old.path) {
                    std::cmp::Ordering::Less => {
                        delta.added.push(new.path.clone());
                        current.next();
                    }
                    std::cmp::Ordering::Greater => {
                        delta.removed.push(old.path.clone());
                        earlier.next();
                    }
                    std::cmp::Ordering::Equal => {
                        if new != old {
                            delta.updated.push(new.path.clone());
                        }
                        current.next();
                        earlier.next();
                    }
                },
                (Some(new), None) => {
                    delta.added.push(new.path.clone());
                    current.next();
                }
                (None, Some(old)) => {
                    delta.removed.push(old.path.clone());
                    earlier.next();
                }
                (None, None) => break,
            }
        }
        delta
    }
}

/// The per-file difference between two fact generations.
///
/// Facts are republished wholesale, but a consumer that refreshed one file
/// needs to know which files' facts actually moved so it can invalidate only
/// those. Because both snapshots are ordered by path, the comparison is a
/// linear merge and no rescan of the graph is required.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFactDelta {
    /// Paths present now and absent before, ordered.
    pub added: Vec<String>,
    /// Paths present in both whose facts differ, ordered.
    pub updated: Vec<String>,
    /// Paths absent now and present before, ordered.
    pub removed: Vec<String>,
}

impl FileFactDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }

    /// Every touched path, ordered and deduplicated.
    pub fn touched_paths(&self) -> Vec<&str> {
        let mut paths: BTreeSet<&str> = BTreeSet::new();
        for path in self.added.iter().chain(&self.updated).chain(&self.removed) {
            paths.insert(path.as_str());
        }
        paths.into_iter().collect()
    }
}

/// Builds a [`SymbolGraphFacts::key`] from a canonical path and a symbol name.
pub fn symbol_fact_key(path: &str, name: &str) -> String {
    format!("{path}{SYMBOL_KEY_SEPARATOR}{name}")
}

/// Instability `Ce / (Ca + Ce)` in per mille, truncated toward zero.
///
/// Integer arithmetic only: the intermediate product is widened to `u64` so
/// that no realistic degree can overflow, and the result is exact and
/// platform-independent, unlike a rounded float.
pub fn instability_per_mille(fan_in: u32, fan_out: u32) -> Option<u16> {
    let total = u64::from(fan_in) + u64::from(fan_out);
    if total == 0 {
        return None;
    }
    Some((u64::from(fan_out) * 1_000 / total) as u16)
}

/// Pure producer of graph facts.
#[derive(Debug, Clone, Copy)]
pub struct GraphFactProducer {
    limits: GraphFactLimits,
    unstable_dependency_threshold_per_mille: u16,
}

impl Default for GraphFactProducer {
    fn default() -> Self {
        Self {
            limits: GraphFactLimits::default(),
            unstable_dependency_threshold_per_mille:
                DEFAULT_UNSTABLE_DEPENDENCY_THRESHOLD_PER_MILLE,
        }
    }
}

impl GraphFactProducer {
    pub fn new(limits: GraphFactLimits) -> Self {
        Self {
            limits: limits.bounded(),
            ..Self::default()
        }
    }

    /// Overrides the instability gap above which a dependency is flagged.
    ///
    /// Values above 1000 are clamped: no real gap can exceed the per-mille
    /// range, and a larger threshold would silently disable the fact.
    pub fn with_unstable_dependency_threshold(mut self, threshold_per_mille: u16) -> Self {
        self.unstable_dependency_threshold_per_mille = threshold_per_mille.min(1_000);
        self
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
        let mut pair_evidence: BTreeMap<(String, String), EdgeEvidence> = BTreeMap::new();

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

                // Keep one representative edge per ordered file pair, chosen by
                // a total order so that the cited source range does not depend
                // on graph traversal order.
                let candidate = EdgeEvidence {
                    source_line: saturating_u32(from.line),
                    source_end_line: saturating_u32(from.end_line),
                    from_symbol: from.name.clone(),
                    to_symbol: to.name.clone(),
                    edge_kind: kind,
                };
                pair_evidence
                    .entry((from_path.clone(), to_path.clone()))
                    .and_modify(|existing| {
                        if candidate.sort_key() < existing.sort_key() {
                            *existing = candidate.clone();
                        }
                    })
                    .or_insert(candidate);
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

        // Components are computed over every observed file, before any row
        // truncation, so a retained file never reports a cycle smaller than the
        // one it is actually in.
        let ordered_files: Vec<String> = files.into_iter().collect();
        let components = file_components(&ordered_files, &file_coupling);

        let file_facts: Vec<FileGraphFacts> = ordered_files
            .iter()
            .enumerate()
            .take(limits.max_files)
            .map(|(position, path)| {
                let coupling = file_coupling.get(path);
                let component = &components[position];
                let fan_in = coupling.map_or(0, |value| saturating_u32(value.dependents.len()));
                let fan_out = coupling.map_or(0, |value| saturating_u32(value.dependencies.len()));
                FileGraphFacts {
                    path: path.clone(),
                    fan_in,
                    fan_out,
                    scc_id: ordered_files[component.representative].clone(),
                    scc_size: component.size,
                    cycle_member: component.size > 1,
                    instability_per_mille: instability_per_mille(fan_in, fan_out),
                }
            })
            .collect();

        // Instability is derived from the untruncated coupling map, so a
        // dependency is judged against real degrees even when file rows spill.
        let instabilities: BTreeMap<&str, u16> = file_coupling
            .iter()
            .filter_map(|(path, coupling)| {
                instability_per_mille(
                    saturating_u32(coupling.dependents.len()),
                    saturating_u32(coupling.dependencies.len()),
                )
                .map(|value| (path.as_str(), value))
            })
            .collect();

        let mut unstable_dependencies: Vec<UnstableDependencySignal> = pair_evidence
            .iter()
            .filter_map(|((from_path, to_path), evidence)| {
                let from_instability = *instabilities.get(from_path.as_str())?;
                let to_instability = *instabilities.get(to_path.as_str())?;
                let gap = to_instability.checked_sub(from_instability)?;
                (gap > self.unstable_dependency_threshold_per_mille).then(|| {
                    UnstableDependencySignal {
                        from_path: from_path.clone(),
                        to_path: to_path.clone(),
                        from_instability_per_mille: from_instability,
                        to_instability_per_mille: to_instability,
                        instability_gap_per_mille: gap,
                        edge_kind: evidence.edge_kind,
                        from_symbol: evidence.from_symbol.clone(),
                        to_symbol: evidence.to_symbol.clone(),
                        source_line: evidence.source_line,
                        source_end_line: evidence.source_end_line,
                    }
                })
            })
            .collect();
        unstable_dependencies.sort_by(|left, right| {
            right
                .instability_gap_per_mille
                .cmp(&left.instability_gap_per_mille)
                .then_with(|| left.from_path.cmp(&right.from_path))
                .then_with(|| left.to_path.cmp(&right.to_path))
        });
        let unstable_dependency_overflow =
            unstable_dependencies.len() > limits.max_unstable_dependencies;
        unstable_dependencies.truncate(limits.max_unstable_dependencies);

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
            unstable_dependencies,
            report: GraphFactsReport {
                facts_version: GRAPH_FACTS_VERSION,
                limits,
                unstable_dependency_threshold_per_mille: self
                    .unstable_dependency_threshold_per_mille,
                nodes_seen,
                edges_seen,
                files_observed,
                symbols_observed,
                invalid_path_nodes,
                invalid_path_edges,
                file_overflow,
                symbol_overflow,
                unstable_dependency_overflow,
                index_complete,
            },
        }
    }
}

/// The representative import/call edge cited by an unstable-dependency flag.
#[derive(Debug, Clone)]
struct EdgeEvidence {
    source_line: u32,
    source_end_line: u32,
    from_symbol: String,
    to_symbol: String,
    edge_kind: EdgeKind,
}

impl EdgeEvidence {
    /// A total order over candidate edges for one file pair. `EdgeKind` has no
    /// `Ord`, so its stable short code stands in for it.
    fn sort_key(&self) -> (u32, u32, &str, &str, &'static str) {
        (
            self.source_line,
            self.source_end_line,
            self.from_symbol.as_str(),
            self.to_symbol.as_str(),
            self.edge_kind.short_code(),
        )
    }
}

#[derive(Debug, Clone)]
struct ComponentMembership {
    /// Position of the component's lexically smallest member.
    representative: usize,
    size: u32,
}

/// Assigns each file its strongly connected component over the file-level
/// dependency condensation, using Tarjan's algorithm.
///
/// `ordered_files` must be lexically sorted and unique; the returned vector is
/// parallel to it. Component ids are derived from the smallest member position
/// rather than from Tarjan's discovery order, because discovery order depends
/// on traversal start points and would not be a stable persisted value.
fn file_components(
    ordered_files: &[String],
    coupling: &BTreeMap<String, FileCoupling>,
) -> Vec<ComponentMembership> {
    let positions: BTreeMap<&str, usize> = ordered_files
        .iter()
        .enumerate()
        .map(|(position, path)| (path.as_str(), position))
        .collect();

    let mut condensation: DiGraph<(), ()> = DiGraph::with_capacity(ordered_files.len(), 0);
    let nodes: Vec<_> = ordered_files
        .iter()
        .map(|_| condensation.add_node(()))
        .collect();
    for (path, edges) in coupling {
        let Some(&from) = positions.get(path.as_str()) else {
            continue;
        };
        for dependency in &edges.dependencies {
            let Some(&to) = positions.get(dependency.as_str()) else {
                continue;
            };
            condensation.add_edge(nodes[from], nodes[to], ());
        }
    }

    let mut memberships: Vec<ComponentMembership> = (0..ordered_files.len())
        .map(|position| ComponentMembership {
            representative: position,
            size: 1,
        })
        .collect();
    for component in tarjan_scc(&condensation) {
        let members: Vec<usize> = component.into_iter().map(|node| node.index()).collect();
        // `ordered_files` is lexically sorted, so the smallest position is the
        // lexically smallest member path.
        let Some(&representative) = members.iter().min() else {
            continue;
        };
        let size = saturating_u32(members.len());
        for member in members {
            memberships[member] = ComponentMembership {
                representative,
                size,
            };
        }
    }
    memberships
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

    /// Three-file cycle `cyc/a -> cyc/b -> cyc/c -> cyc/a`, an acyclic entry
    /// point `app/main.rs -> cyc/a`, and a leaf `util/leaf.rs` the cycle uses.
    fn cyclic_graph() -> CodeGraph {
        let mut graph = CodeGraph::new();
        let a = symbol("cyc/a.rs", "a_fn");
        let b = symbol("cyc/b.rs", "b_fn");
        let c = symbol("cyc/c.rs", "c_fn");
        let main = symbol("app/main.rs", "main");
        let leaf = symbol("util/leaf.rs", "leaf");
        for id in [&a, &b, &c, &main, &leaf] {
            add_symbol(&mut graph, id, true, 1);
        }
        add_edge(&mut graph, &a, &b, EdgeKind::Calls);
        add_edge(&mut graph, &b, &c, EdgeKind::Calls);
        add_edge(&mut graph, &c, &a, EdgeKind::Calls);
        add_edge(&mut graph, &main, &a, EdgeKind::Imports);
        add_edge(&mut graph, &b, &leaf, EdgeKind::TypeRef);
        graph
    }

    #[test]
    fn tarjan_identifies_the_cycle_and_leaves_acyclic_files_alone() {
        let snapshot = GraphFactProducer::default().produce(&cyclic_graph(), true);

        let cycle: Vec<&FileGraphFacts> = snapshot
            .files
            .iter()
            .filter(|file| file.cycle_member)
            .collect();
        let cycle_paths: Vec<&str> = cycle.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(cycle_paths, ["cyc/a.rs", "cyc/b.rs", "cyc/c.rs"]);
        assert!(cycle.iter().all(|file| file.scc_size == 3));
        let ids: BTreeSet<&str> = cycle.iter().map(|file| file.scc_id.as_str()).collect();
        assert_eq!(
            ids.into_iter().collect::<Vec<_>>(),
            ["cyc/a.rs"],
            "cycle members share the component's smallest member as their id"
        );

        for path in ["app/main.rs", "util/leaf.rs"] {
            let facts = snapshot.file(path).unwrap();
            assert_eq!(facts.scc_size, 1);
            assert!(!facts.cycle_member);
        }
        // Three components: {app/main.rs}, {cyc/a,b,c}, {util/leaf.rs}.
        let components: BTreeSet<&str> = snapshot
            .files
            .iter()
            .map(|file| file.scc_id.as_str())
            .collect();
        assert_eq!(
            components.into_iter().collect::<Vec<_>>(),
            ["app/main.rs", "cyc/a.rs", "util/leaf.rs"]
        );
    }

    #[test]
    fn component_id_is_the_smallest_member_and_acyclic_files_represent_themselves() {
        let snapshot = GraphFactProducer::default().produce(&cyclic_graph(), true);
        assert_eq!(snapshot.file("app/main.rs").unwrap().scc_id, "app/main.rs");
        assert_eq!(
            snapshot.file("util/leaf.rs").unwrap().scc_id,
            "util/leaf.rs"
        );
        for path in ["cyc/a.rs", "cyc/b.rs", "cyc/c.rs"] {
            assert_eq!(snapshot.file(path).unwrap().scc_id, "cyc/a.rs");
        }
    }

    #[test]
    fn inserting_an_unrelated_file_does_not_renumber_existing_components() {
        let producer = GraphFactProducer::default();
        let before = producer.produce(&cyclic_graph(), true);

        // A dense integer component index would have shifted every id that
        // sorts after this new path, making the delta claim the whole
        // repository had changed.
        let mut graph = cyclic_graph();
        let stranger = symbol("aaa/first.rs", "stranger");
        add_symbol(&mut graph, &stranger, true, 1);
        let after = producer.produce(&graph, true);

        let delta = after.file_delta(&before);
        assert_eq!(delta.added, ["aaa/first.rs"]);
        assert!(delta.updated.is_empty());
        assert!(delta.removed.is_empty());
    }

    #[test]
    fn mutual_file_dependency_is_a_two_file_cycle() {
        let mut graph = CodeGraph::new();
        let left = symbol("src/left.rs", "left");
        let right = symbol("src/right.rs", "right");
        add_symbol(&mut graph, &left, true, 1);
        add_symbol(&mut graph, &right, true, 1);
        add_edge(&mut graph, &left, &right, EdgeKind::Calls);
        add_edge(&mut graph, &right, &left, EdgeKind::Imports);

        let snapshot = GraphFactProducer::default().produce(&graph, true);
        for path in ["src/left.rs", "src/right.rs"] {
            let facts = snapshot.file(path).unwrap();
            assert_eq!(facts.scc_size, 2);
            assert!(facts.cycle_member);
            assert_eq!(facts.scc_id, "src/left.rs");
        }
    }

    #[test]
    fn intra_file_symbol_cycle_is_not_a_file_cycle() {
        let mut graph = CodeGraph::new();
        let first = symbol_at("src/only.rs", "ping", 0);
        let second = symbol_at("src/only.rs", "pong", 200);
        add_symbol(&mut graph, &first, true, 1);
        add_symbol(&mut graph, &second, true, 20);
        add_edge(&mut graph, &first, &second, EdgeKind::Calls);
        add_edge(&mut graph, &second, &first, EdgeKind::Calls);

        let snapshot = GraphFactProducer::default().produce(&graph, true);
        let facts = snapshot.file("src/only.rs").unwrap();
        assert!(!facts.cycle_member);
        assert_eq!(facts.scc_size, 1);
        assert_eq!((facts.fan_in, facts.fan_out), (0, 0));
    }

    #[test]
    fn truncated_snapshots_still_report_the_full_component_size() {
        let producer = GraphFactProducer::new(GraphFactLimits {
            max_files: 2,
            ..GraphFactLimits::default()
        });
        let snapshot = producer.produce(&cyclic_graph(), true);
        assert_eq!(snapshot.files.len(), 2);
        // app/main.rs and cyc/a.rs survive; cyc/a.rs still knows its cycle is 3.
        assert_eq!(snapshot.file("cyc/a.rs").unwrap().scc_size, 3);
        assert!(snapshot.report.file_overflow);
    }

    #[test]
    fn instability_is_exact_integer_per_mille() {
        // Truncation toward zero, never rounding, and never a float.
        assert_eq!(instability_per_mille(0, 1), Some(1_000));
        assert_eq!(instability_per_mille(1, 0), Some(0));
        assert_eq!(instability_per_mille(1, 1), Some(500));
        assert_eq!(instability_per_mille(2, 1), Some(333));
        assert_eq!(instability_per_mille(1, 2), Some(666));
        assert_eq!(instability_per_mille(3, 4), Some(571));
        // 1/3 of a mille is dropped, not rounded up to 334.
        assert_eq!(instability_per_mille(2_000_000, 1_000_000), Some(333));
        assert_eq!(instability_per_mille(u32::MAX, u32::MAX), Some(500));
    }

    #[test]
    fn isolated_files_have_unknown_instability_not_zero() {
        let mut graph = CodeGraph::new();
        let lonely = symbol("src/lonely.rs", "lonely");
        add_symbol(&mut graph, &lonely, true, 1);

        let snapshot = GraphFactProducer::default().produce(&graph, true);
        let facts = snapshot.file("src/lonely.rs").unwrap();
        assert_eq!((facts.fan_in, facts.fan_out), (0, 0));
        assert_eq!(facts.instability_per_mille, None);
    }

    #[test]
    fn chain_instability_matches_martin_definition() {
        let snapshot = GraphFactProducer::default().produce(&chain_graph(), true);
        // a.rs depends on one file and nothing depends on it: maximally unstable.
        assert_eq!(
            snapshot.file("src/a.rs").unwrap().instability_per_mille,
            Some(1_000)
        );
        // b.rs is depended on once and depends once.
        assert_eq!(
            snapshot.file("src/b.rs").unwrap().instability_per_mille,
            Some(500)
        );
        // c.rs depends on nothing: maximally stable.
        assert_eq!(
            snapshot.file("src/c.rs").unwrap().instability_per_mille,
            Some(0)
        );
    }

    /// `src/core.rs` (Ca 2, Ce 1 -> instability 333) reaching into
    /// `src/volatile.rs` (Ca 1, Ce 3 -> instability 750) is a direction
    /// violation with a gap of 417 per mille.
    fn violation_graph() -> CodeGraph {
        let mut graph = CodeGraph::new();
        let core = symbol("src/core.rs", "core_entry");
        let consumer_one = symbol("src/one.rs", "one");
        let consumer_two = symbol("src/two.rs", "two");
        let volatile = symbol("src/volatile.rs", "volatile_helper");
        let sinks = [
            symbol("src/sink_a.rs", "sink_a"),
            symbol("src/sink_b.rs", "sink_b"),
            symbol("src/sink_c.rs", "sink_c"),
        ];
        for id in [&core, &consumer_one, &consumer_two, &volatile] {
            add_symbol(&mut graph, id, true, 1);
        }
        for sink in &sinks {
            add_symbol(&mut graph, sink, true, 1);
        }
        add_edge(&mut graph, &consumer_one, &core, EdgeKind::Imports);
        add_edge(&mut graph, &consumer_two, &core, EdgeKind::Imports);
        add_edge(&mut graph, &core, &volatile, EdgeKind::Calls);
        for sink in &sinks {
            add_edge(&mut graph, &volatile, sink, EdgeKind::Calls);
        }
        graph
    }

    #[test]
    fn unstable_dependency_carries_both_keys_and_the_source_range() {
        let snapshot = GraphFactProducer::default().produce(&violation_graph(), true);
        assert_eq!(snapshot.unstable_dependencies.len(), 1);
        let signal = &snapshot.unstable_dependencies[0];
        assert_eq!(signal.from_path, "src/core.rs");
        assert_eq!(signal.to_path, "src/volatile.rs");
        assert_eq!(signal.from_instability_per_mille, 333);
        assert_eq!(signal.to_instability_per_mille, 750);
        assert_eq!(signal.instability_gap_per_mille, 417);
        assert_eq!(signal.edge_kind, EdgeKind::Calls);
        assert_eq!(signal.from_symbol, "core_entry");
        assert_eq!(signal.to_symbol, "volatile_helper");
        assert_eq!((signal.source_line, signal.source_end_line), (1, 5));
    }

    #[test]
    fn threshold_is_strict_and_bounds_the_flag() {
        let graph = violation_graph();
        // The observed gap is exactly 417.
        let just_below = GraphFactProducer::default()
            .with_unstable_dependency_threshold(416)
            .produce(&graph, true);
        assert_eq!(just_below.unstable_dependencies.len(), 1);

        let exactly_at = GraphFactProducer::default()
            .with_unstable_dependency_threshold(417)
            .produce(&graph, true);
        assert!(
            exactly_at.unstable_dependencies.is_empty(),
            "the comparison is strictly greater than the threshold"
        );

        let disabled = GraphFactProducer::default()
            .with_unstable_dependency_threshold(u16::MAX)
            .produce(&graph, true);
        assert_eq!(
            disabled.report.unstable_dependency_threshold_per_mille, 1_000,
            "an out-of-range threshold is clamped into the per-mille domain"
        );
        assert!(disabled.unstable_dependencies.is_empty());
    }

    #[test]
    fn dependencies_toward_more_stable_files_are_never_flagged() {
        let snapshot = GraphFactProducer::default()
            .with_unstable_dependency_threshold(0)
            .produce(&chain_graph(), true);
        // Every edge in the chain runs from less stable to more stable.
        assert!(snapshot.unstable_dependencies.is_empty());
    }

    #[test]
    fn unstable_dependencies_are_ordered_by_gap_then_path() {
        // hub.rs: Ca 6, Ce 3 -> 333. mid.rs: Ca 1, Ce 1 -> 500 (gap 167).
        // alt.rs and worst.rs: Ca 1, Ce 3 -> 750 (gap 417), and are symmetric,
        // so their equal gaps must break on the dependency path.
        let mut graph = CodeGraph::new();
        let hub = symbol("src/hub.rs", "hub_fn");
        let mid = symbol("src/mid.rs", "mid_fn");
        let alt = symbol("src/alt.rs", "alt_fn");
        let worst = symbol("src/worst.rs", "worst_fn");
        let mid_sink = symbol("src/mid_sink.rs", "mid_sink_fn");
        let shared: Vec<_> = (0..3)
            .map(|index| symbol(&format!("src/shared_{index}.rs"), "shared_fn"))
            .collect();
        let dependents: Vec<_> = (0..6)
            .map(|index| symbol(&format!("src/user_{index}.rs"), "user_fn"))
            .collect();

        for id in [&hub, &mid, &alt, &worst, &mid_sink] {
            add_symbol(&mut graph, id, true, 1);
        }
        for id in shared.iter().chain(dependents.iter()) {
            add_symbol(&mut graph, id, true, 1);
        }
        for user in &dependents {
            add_edge(&mut graph, user, &hub, EdgeKind::Imports);
        }
        for target in [&mid, &alt, &worst] {
            add_edge(&mut graph, &hub, target, EdgeKind::Calls);
        }
        add_edge(&mut graph, &mid, &mid_sink, EdgeKind::Calls);
        for target in &shared {
            add_edge(&mut graph, &alt, target, EdgeKind::Calls);
            add_edge(&mut graph, &worst, target, EdgeKind::Calls);
        }

        let snapshot = GraphFactProducer::default()
            .with_unstable_dependency_threshold(0)
            .produce(&graph, true);
        let pairs: Vec<(&str, &str, u16)> = snapshot
            .unstable_dependencies
            .iter()
            .map(|signal| {
                (
                    signal.from_path.as_str(),
                    signal.to_path.as_str(),
                    signal.instability_gap_per_mille,
                )
            })
            .collect();
        assert_eq!(
            pairs,
            [
                ("src/hub.rs", "src/alt.rs", 417),
                ("src/hub.rs", "src/worst.rs", 417),
                ("src/hub.rs", "src/mid.rs", 167),
            ]
        );
    }

    #[test]
    fn unstable_dependency_rows_truncate_and_flag_overflow() {
        let producer = GraphFactProducer::new(GraphFactLimits {
            max_unstable_dependencies: 0,
            ..GraphFactLimits::default()
        });
        let snapshot = producer.produce(&violation_graph(), true);
        assert!(snapshot.unstable_dependencies.is_empty());
        assert!(snapshot.report.unstable_dependency_overflow);
        assert_eq!(snapshot.availability(), FactAvailability::Degraded);
    }

    #[test]
    fn representative_edge_is_the_lowest_source_range_not_traversal_order() {
        let mut graph = CodeGraph::new();
        let late = symbol_at("src/core.rs", "late_caller", 900);
        let early = symbol_at("src/core.rs", "early_caller", 10);
        let consumer_one = symbol("src/one.rs", "one");
        let consumer_two = symbol("src/two.rs", "two");
        let volatile = symbol("src/volatile.rs", "volatile_helper");
        let sinks = [
            symbol("src/sink_a.rs", "sink_a"),
            symbol("src/sink_b.rs", "sink_b"),
            symbol("src/sink_c.rs", "sink_c"),
        ];
        add_symbol(&mut graph, &late, true, 90);
        add_symbol(&mut graph, &early, true, 12);
        for id in [&consumer_one, &consumer_two, &volatile] {
            add_symbol(&mut graph, id, true, 1);
        }
        for sink in &sinks {
            add_symbol(&mut graph, sink, true, 1);
        }
        add_edge(&mut graph, &consumer_one, &late, EdgeKind::Imports);
        add_edge(&mut graph, &consumer_two, &late, EdgeKind::Imports);
        add_edge(&mut graph, &late, &volatile, EdgeKind::Calls);
        add_edge(&mut graph, &early, &volatile, EdgeKind::TypeRef);
        for sink in &sinks {
            add_edge(&mut graph, &volatile, sink, EdgeKind::Calls);
        }

        let snapshot = GraphFactProducer::default().produce(&graph, true);
        let signal = &snapshot.unstable_dependencies[0];
        assert_eq!(signal.from_symbol, "early_caller");
        assert_eq!(signal.edge_kind, EdgeKind::TypeRef);
        assert_eq!((signal.source_line, signal.source_end_line), (12, 16));
    }

    #[test]
    fn reproducing_an_unchanged_graph_yields_an_empty_delta() {
        let producer = GraphFactProducer::default();
        let before = producer.produce(&chain_graph(), true);
        let after = producer.produce(&chain_graph(), true);
        assert!(after.file_delta(&before).is_empty());
    }

    #[test]
    fn reindexing_one_file_moves_only_that_file_and_its_new_neighbour() {
        let producer = GraphFactProducer::default();
        let before = producer.produce(&chain_graph(), true);

        // Simulate an incremental reindex of src/b.rs that adds one import.
        let mut graph = chain_graph();
        let b = symbol("src/b.rs", "beta");
        let d = symbol("src/d.rs", "delta");
        add_symbol(&mut graph, &d, true, 1);
        add_edge(&mut graph, &b, &d, EdgeKind::Imports);
        let after = producer.produce(&graph, true);

        let delta = after.file_delta(&before);
        assert_eq!(delta.added, ["src/d.rs"]);
        assert_eq!(delta.updated, ["src/b.rs"]);
        assert!(delta.removed.is_empty());
        assert_eq!(delta.touched_paths(), ["src/b.rs", "src/d.rs"]);

        // The untouched files are byte-identical, so a consumer may keep them.
        for path in ["src/a.rs", "src/c.rs"] {
            assert_eq!(before.file(path), after.file(path));
        }
    }

    #[test]
    fn deleting_a_file_is_reported_as_removed_with_its_dependents_updated() {
        let producer = GraphFactProducer::default();
        let before = producer.produce(&chain_graph(), true);

        let mut graph = chain_graph();
        graph.remove_file_nodes("src/c.rs");
        let after = producer.produce(&graph, true);

        let delta = after.file_delta(&before);
        assert_eq!(delta.removed, ["src/c.rs"]);
        assert_eq!(delta.updated, ["src/b.rs"]);
        assert!(delta.added.is_empty());
        assert_eq!(before.file("src/a.rs"), after.file("src/a.rs"));
    }

    #[test]
    fn a_change_that_closes_a_cycle_honestly_updates_every_member() {
        let producer = GraphFactProducer::default();
        let before = producer.produce(&chain_graph(), true);

        // One new edge turns the acyclic chain into a three-file cycle, so the
        // delta must report all three files rather than only the edited one.
        let mut graph = chain_graph();
        let a = symbol("src/a.rs", "alpha");
        let c = symbol("src/c.rs", "gamma");
        add_edge(&mut graph, &c, &a, EdgeKind::Calls);
        let after = producer.produce(&graph, true);

        let delta = after.file_delta(&before);
        assert_eq!(delta.updated, ["src/a.rs", "src/b.rs", "src/c.rs"]);
        assert!(after
            .files
            .iter()
            .all(|file| file.cycle_member && file.scc_size == 3));
    }

    #[test]
    fn limits_truncate_and_flag_overflow() {
        let producer = GraphFactProducer::new(GraphFactLimits {
            max_files: 2,
            max_symbols: 1,
            ..GraphFactLimits::default()
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
