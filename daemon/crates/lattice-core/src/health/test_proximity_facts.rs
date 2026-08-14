//! Deterministic test-proximity facts per production file.
//!
//! [`find_relevant_tests`](crate::intelligence::agent::find_relevant_tests)
//! scores candidate tests at request time with a blend of graph-edge signals
//! and fuzzy token/path/name heuristics. This module extracts and persists
//! only the deterministic, graph-edge-based subset of that scoring as facts
//! at publish time; the fuzzy scoring stays exactly as it was and continues
//! to run only at request time inside `find_relevant_tests`.
//!
//! See `docs/plans/2026-08-13-health-engine.md`, section "H2.4 Test-proximity
//! facts".
//!
//! # What counts as a link
//!
//! A production file is "linked" to a test file only when the built
//! [`CodeGraph`] carries at least one edge from a symbol defined in the test
//! file to a symbol defined in the production file (any [`EdgeKind`] counts,
//! matching the direct-dependency signal `find_relevant_tests` treats as its
//! strongest evidence). A shared file stem, a shared directory, or a shared
//! path/name token is exactly the fuzzy heuristic this module deliberately
//! does *not* persist: it can inflate `find_relevant_tests`'s request-time
//! ranking, but it must never inflate a fact `impact` will cite as coverage
//! evidence.
//!
//! "Production file" and "test file" are one classifier, shared with
//! `find_relevant_tests` via [`crate::intelligence::agent::is_test_file`] and
//! [`crate::intelligence::agent::is_test_support_file`], so the two
//! definitions can never drift apart.
//!
//! # Unknown is never zero
//!
//! `linked_test_count: 0` is a real, observed fact about a complete index —
//! not a stand-in for "not analyzed". Whole-snapshot incompleteness (a
//! partial index, a truncated file/edge scan) is reported through
//! [`TestProximityReport::availability`], exactly like the sibling `graph_facts`
//! family; a consumer must check availability before treating any
//! `untested_change` flag as trustworthy.
//!
//! # Determinism
//!
//! Every intermediate collection is ordered (`BTreeMap`/`BTreeSet`) and every
//! output vector is sorted by a total key, so an identical graph yields a
//! byte-identical snapshot regardless of node/edge insertion order.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::git_intelligence::canonical_repository_path;
use crate::graph::CodeGraph;
use crate::intelligence::agent::{is_queryable_graph_file, is_test_file, is_test_support_file};

/// Aggregation version persisted with every snapshot.
///
/// Bump this whenever the meaning of a produced value changes so that stored
/// generations from an older producer are recognisably different rather than
/// silently misread.
pub const TEST_PROXIMITY_FACTS_VERSION: u32 = 1;

/// Upper bound on persisted per-file fact rows.
pub const MAX_FILE_FACTS: usize = 200_000;

/// Upper bound on the evidence list of linked test file paths carried on a
/// single file row.
///
/// `linked_test_count` is always exact; this only bounds how many of the
/// linked paths are cited as evidence, so a file with many linked tests
/// cannot grow its row without bound.
pub const MAX_LINKED_TEST_EVIDENCE: usize = 8;

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

/// Provenance of a test link, kept explicit and extensible even though this
/// fact family only ever persists the edge-derived kind.
///
/// `find_relevant_tests` also scores name/path-heuristic matches, but those
/// are fuzzy and request-time-only by design (see the module doc). This enum
/// exists so the persisted fact can never be confused for a heuristic match,
/// and so a future fact family that *does* want to persist heuristic
/// provenance has a variant ready rather than overloading this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestLinkKind {
    /// At least one real graph edge connects a test symbol to this file.
    GraphEdge,
    /// Reserved for a future fact family: a name/path heuristic match. Never
    /// produced by this module; `find_relevant_tests` keeps that scoring at
    /// request time only.
    NameOrPathHeuristic,
}

impl TestLinkKind {
    /// Stable code used in storage and payloads.
    pub fn as_str(&self) -> &'static str {
        match self {
            TestLinkKind::GraphEdge => "graph_edge",
            TestLinkKind::NameOrPathHeuristic => "name_or_path_heuristic",
        }
    }

    /// Decode a stored code into a link kind.
    pub fn from_code(value: &str) -> Option<Self> {
        match value {
            "graph_edge" => Some(TestLinkKind::GraphEdge),
            "name_or_path_heuristic" => Some(TestLinkKind::NameOrPathHeuristic),
            _ => None,
        }
    }
}

/// Effective aggregation bounds persisted alongside a snapshot generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestProximityLimits {
    pub max_files: usize,
    pub max_linked_test_evidence: usize,
}

impl Default for TestProximityLimits {
    fn default() -> Self {
        Self {
            max_files: MAX_FILE_FACTS,
            max_linked_test_evidence: MAX_LINKED_TEST_EVIDENCE,
        }
    }
}

impl TestProximityLimits {
    fn bounded(self) -> Self {
        Self {
            max_files: self.max_files.min(MAX_FILE_FACTS),
            max_linked_test_evidence: self.max_linked_test_evidence.min(MAX_LINKED_TEST_EVIDENCE),
        }
    }
}

/// Completeness evidence for one pure aggregation run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestProximityReport {
    pub facts_version: u32,
    pub limits: TestProximityLimits,
    /// Graph nodes examined, including nodes rejected for a non-canonical path.
    pub nodes_seen: u64,
    /// Graph edges examined, including edges rejected or not test-to-production.
    pub edges_seen: u64,
    /// Distinct canonical production files that contributed at least one node.
    pub production_files_observed: u32,
    /// Distinct canonical test files that contributed at least one node.
    pub test_files_observed: u32,
    /// Nodes whose file path was not canonical and repository-relative.
    pub invalid_path_nodes: u64,
    /// Edges dropped because an endpoint had a non-canonical path.
    pub invalid_path_edges: u64,
    /// True when file rows were truncated by `limits.max_files`.
    pub file_overflow: bool,
    /// Caller-supplied truth about whether the index behind the graph was whole.
    pub index_complete: bool,
}

impl TestProximityReport {
    /// Complete snapshots are the only snapshots eligible to affect scores.
    pub fn is_complete(&self) -> bool {
        !self.is_degraded()
    }

    pub fn is_degraded(&self) -> bool {
        !self.index_complete
            || self.invalid_path_nodes > 0
            || self.invalid_path_edges > 0
            || self.file_overflow
    }

    /// An empty graph is unavailable, not "everything is untested".
    pub fn availability(&self) -> FactAvailability {
        if self.production_files_observed == 0 {
            FactAvailability::Unavailable
        } else if self.is_degraded() {
            FactAvailability::Degraded
        } else {
            FactAvailability::Available
        }
    }
}

/// Test-proximity facts for one canonical repository-relative production file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTestProximityFacts {
    pub path: String,
    /// Distinct test files linked to this production file via a real graph
    /// edge. Never inflated by name/path/token similarity.
    pub linked_test_count: u32,
    /// `Some(GraphEdge)` when `linked_test_count > 0`; `None` when this file
    /// has no edge-linked test. Never `NameOrPathHeuristic` — see the type
    /// doc.
    pub strongest_link_kind: Option<TestLinkKind>,
    /// `true` exactly when `linked_test_count == 0`: this production file has
    /// no edge-linked test coverage. `impact` (H4) intersects this per-file
    /// fact with a diff's changed-file list to report untested changes; the
    /// fact itself is diff-independent so it stays cheap to query per file.
    pub untested_change: bool,
    /// Up to [`MAX_LINKED_TEST_EVIDENCE`] linked test file paths, ordered.
    /// `linked_test_count` is authoritative even when this list is
    /// truncated; it exists only so a consumer can cite concrete evidence
    /// ("linked to tests/foo_test.rs") without a second query.
    pub linked_test_files: Vec<String>,
}

/// Stable test-proximity facts consumed by health scoring and `impact` (H4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestProximitySnapshot {
    /// Ordered by `path`, unique.
    pub files: Vec<FileTestProximityFacts>,
    pub report: TestProximityReport,
}

impl TestProximitySnapshot {
    /// Empty snapshots are honest: no graph was available, not zero tests.
    pub fn empty() -> Self {
        Self {
            files: Vec::new(),
            report: TestProximityReport {
                facts_version: TEST_PROXIMITY_FACTS_VERSION,
                limits: TestProximityLimits::default(),
                nodes_seen: 0,
                edges_seen: 0,
                production_files_observed: 0,
                test_files_observed: 0,
                invalid_path_nodes: 0,
                invalid_path_edges: 0,
                file_overflow: false,
                index_complete: true,
            },
        }
    }

    pub fn availability(&self) -> FactAvailability {
        self.report.availability()
    }

    /// Looks up one production file without requiring consumers to rebuild an
    /// index.
    pub fn file(&self, path: &str) -> Option<&FileTestProximityFacts> {
        let path = canonical_repository_path(path)?;
        self.files
            .binary_search_by(|candidate| candidate.path.cmp(&path))
            .ok()
            .map(|index| &self.files[index])
    }

    /// Which files' facts moved between `previous` and this snapshot.
    ///
    /// Both vectors are ordered by path, so this is a single linear merge.
    pub fn file_delta(&self, previous: &TestProximitySnapshot) -> FileFactDelta {
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

/// Pure producer of test-proximity facts.
#[derive(Debug, Clone, Copy)]
pub struct TestProximityFactProducer {
    limits: TestProximityLimits,
}

impl Default for TestProximityFactProducer {
    fn default() -> Self {
        Self {
            limits: TestProximityLimits::default(),
        }
    }
}

impl TestProximityFactProducer {
    pub fn new(limits: TestProximityLimits) -> Self {
        Self {
            limits: limits.bounded(),
        }
    }

    /// Produces a snapshot from `graph`.
    ///
    /// `index_complete` is the caller's truth about whether the index behind
    /// the graph covered the whole workspace. A `false` value never changes a
    /// computed number; it only marks the snapshot degraded so consumers
    /// refuse to treat an absent link as a real absence.
    pub fn produce(&self, graph: &CodeGraph, index_complete: bool) -> TestProximitySnapshot {
        let limits = self.limits.bounded();
        let mut nodes_seen = 0_u64;
        let mut invalid_path_nodes = 0_u64;

        let mut production_files: BTreeSet<String> = BTreeSet::new();
        let mut test_files: BTreeSet<String> = BTreeSet::new();

        for node in graph.all_nodes() {
            nodes_seen = nodes_seen.saturating_add(1);
            let Some(path) = canonical_repository_path(&node.file) else {
                invalid_path_nodes = invalid_path_nodes.saturating_add(1);
                continue;
            };
            if !is_queryable_graph_file(&path) {
                continue;
            }
            if is_test_file(&path) {
                // A support file (e.g. conftest.py) is neither a production
                // file nor a countable test: it carries no independent test
                // symbols of its own for `find_relevant_tests` to score, and
                // treating it as a linked test would fabricate coverage.
                if !is_test_support_file(&path) {
                    test_files.insert(path);
                }
            } else {
                production_files.insert(path);
            }
        }

        let mut edges_seen = 0_u64;
        let mut invalid_path_edges = 0_u64;
        // production path -> distinct linking test paths.
        let mut links: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        for (from, to, _kind) in graph.all_edges() {
            edges_seen = edges_seen.saturating_add(1);
            let (Some(from_path), Some(to_path)) = (
                canonical_repository_path(&from.file),
                canonical_repository_path(&to.file),
            ) else {
                invalid_path_edges = invalid_path_edges.saturating_add(1);
                continue;
            };
            if from_path == to_path {
                continue;
            }
            // Only edges running from a test symbol to a production symbol
            // count as a link: this mirrors `find_relevant_tests`'s strongest
            // signal (`graph.get_dependencies` from the test node), read in
            // the equivalent direction over `all_edges`. Any edge kind
            // counts, matching that same signal.
            if !test_files.contains(&from_path) || !production_files.contains(&to_path) {
                continue;
            }
            links.entry(to_path).or_default().insert(from_path);
        }

        let production_files_observed = saturating_u32(production_files.len());
        let test_files_observed = saturating_u32(test_files.len());
        let file_overflow = production_files.len() > limits.max_files;

        let files: Vec<FileTestProximityFacts> = production_files
            .into_iter()
            .take(limits.max_files)
            .map(|path| {
                let linked = links.get(&path);
                let linked_test_count = linked.map_or(0, |value| saturating_u32(value.len()));
                let strongest_link_kind =
                    (linked_test_count > 0).then_some(TestLinkKind::GraphEdge);
                let linked_test_files: Vec<String> = linked
                    .into_iter()
                    .flatten()
                    .take(limits.max_linked_test_evidence)
                    .cloned()
                    .collect();
                FileTestProximityFacts {
                    path,
                    linked_test_count,
                    strongest_link_kind,
                    untested_change: linked_test_count == 0,
                    linked_test_files,
                }
            })
            .collect();

        TestProximitySnapshot {
            files,
            report: TestProximityReport {
                facts_version: TEST_PROXIMITY_FACTS_VERSION,
                limits,
                nodes_seen,
                edges_seen,
                production_files_observed,
                test_files_observed,
                invalid_path_nodes,
                invalid_path_edges,
                file_overflow,
                index_complete,
            },
        }
    }
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
    use crate::graph::EdgeKind;

    #[test]
    fn a_production_file_with_a_real_edge_from_a_test_is_linked() {
        let mut graph = CodeGraph::new();
        let prod = symbol("src/widget.rs", "make_widget");
        let test = symbol("tests/widget_test.rs", "test_make_widget");
        add_symbol(&mut graph, &prod, true, 1);
        add_symbol(&mut graph, &test, true, 1);
        add_edge(&mut graph, &test, &prod, EdgeKind::Calls);

        let snapshot = TestProximityFactProducer::default().produce(&graph, true);
        let facts = snapshot.file("src/widget.rs").unwrap();
        assert_eq!(facts.linked_test_count, 1);
        assert_eq!(facts.strongest_link_kind, Some(TestLinkKind::GraphEdge));
        assert!(!facts.untested_change);
        assert_eq!(facts.linked_test_files, ["tests/widget_test.rs"]);
    }

    #[test]
    fn multiple_tests_linking_the_same_file_count_distinctly() {
        let mut graph = CodeGraph::new();
        let prod = symbol("src/widget.rs", "make_widget");
        let unit_test = symbol("tests/widget_test.rs", "unit_case");
        let integration_test = symbol("tests/integration/widget_test.rs", "integration_case");
        add_symbol(&mut graph, &prod, true, 1);
        add_symbol(&mut graph, &unit_test, true, 1);
        add_symbol(&mut graph, &integration_test, true, 1);
        add_edge(&mut graph, &unit_test, &prod, EdgeKind::Calls);
        add_edge(&mut graph, &integration_test, &prod, EdgeKind::Calls);

        let snapshot = TestProximityFactProducer::default().produce(&graph, true);
        let facts = snapshot.file("src/widget.rs").unwrap();
        assert_eq!(facts.linked_test_count, 2);
        assert_eq!(
            facts.linked_test_files,
            ["tests/integration/widget_test.rs", "tests/widget_test.rs"]
        );
    }

    #[test]
    fn repeated_edges_from_the_same_test_file_count_once() {
        let mut graph = CodeGraph::new();
        let prod = symbol("src/widget.rs", "make_widget");
        let prod_helper = symbol_at("src/widget.rs", "helper", 50);
        let test = symbol("tests/widget_test.rs", "test_make_widget");
        add_symbol(&mut graph, &prod, true, 1);
        add_symbol(&mut graph, &prod_helper, true, 10);
        add_symbol(&mut graph, &test, true, 1);
        add_edge(&mut graph, &test, &prod, EdgeKind::Calls);
        add_edge(&mut graph, &test, &prod_helper, EdgeKind::Calls);

        let snapshot = TestProximityFactProducer::default().produce(&graph, true);
        assert_eq!(snapshot.file("src/widget.rs").unwrap().linked_test_count, 1);
    }

    #[test]
    fn a_production_file_with_no_linked_test_is_untested() {
        let mut graph = CodeGraph::new();
        let prod = symbol("src/orphan.rs", "do_thing");
        add_symbol(&mut graph, &prod, true, 1);

        let snapshot = TestProximityFactProducer::default().produce(&graph, true);
        let facts = snapshot.file("src/orphan.rs").unwrap();
        assert_eq!(facts.linked_test_count, 0);
        assert_eq!(facts.strongest_link_kind, None);
        assert!(facts.untested_change);
        assert!(facts.linked_test_files.is_empty());
    }

    /// A stem/path match (`src/orphan.rs` <-> `tests/orphan_test.rs`) with no
    /// graph edge between them must never count: the fuzzy heuristic that
    /// `find_relevant_tests` applies at request time is deliberately excluded
    /// from this persisted fact.
    #[test]
    fn a_name_and_path_match_with_no_graph_edge_does_not_count_as_linked() {
        let mut graph = CodeGraph::new();
        let prod = symbol("src/orphan.rs", "do_thing");
        let unrelated_test = symbol("tests/orphan_test.rs", "test_do_thing");
        add_symbol(&mut graph, &prod, true, 1);
        add_symbol(&mut graph, &unrelated_test, true, 1);
        // No edge between them: same stem, same-ish path, zero graph link.

        let snapshot = TestProximityFactProducer::default().produce(&graph, true);
        let facts = snapshot.file("src/orphan.rs").unwrap();
        assert_eq!(facts.linked_test_count, 0);
        assert_eq!(facts.strongest_link_kind, None);
        assert!(facts.untested_change);
        // The unrelated test file must never appear in production facts.
        assert!(snapshot.file("tests/orphan_test.rs").is_none());
    }

    #[test]
    fn an_edge_from_production_into_a_test_file_does_not_count_as_a_link() {
        // Direction matters: a production helper that happens to be called
        // from within a test fixture is not the test depending on the
        // production file, so it must not fabricate coverage.
        let mut graph = CodeGraph::new();
        let prod = symbol("src/widget.rs", "make_widget");
        let test = symbol("tests/widget_test.rs", "fixture_helper");
        add_symbol(&mut graph, &prod, true, 1);
        add_symbol(&mut graph, &test, true, 1);
        add_edge(&mut graph, &prod, &test, EdgeKind::Calls);

        let snapshot = TestProximityFactProducer::default().produce(&graph, true);
        assert_eq!(snapshot.file("src/widget.rs").unwrap().linked_test_count, 0);
    }

    #[test]
    fn a_test_support_file_is_never_counted_as_a_linking_test() {
        let mut graph = CodeGraph::new();
        let prod = symbol("src/widget.rs", "make_widget");
        let conftest = symbol("tests/conftest.py", "fixture");
        add_symbol(&mut graph, &prod, true, 1);
        add_symbol(&mut graph, &conftest, true, 1);
        add_edge(&mut graph, &conftest, &prod, EdgeKind::Calls);

        let snapshot = TestProximityFactProducer::default().produce(&graph, true);
        assert_eq!(snapshot.file("src/widget.rs").unwrap().linked_test_count, 0);
    }

    #[test]
    fn test_files_never_receive_production_fact_rows() {
        let mut graph = CodeGraph::new();
        let prod = symbol("src/widget.rs", "make_widget");
        let test = symbol("tests/widget_test.rs", "test_make_widget");
        add_symbol(&mut graph, &prod, true, 1);
        add_symbol(&mut graph, &test, true, 1);
        add_edge(&mut graph, &test, &prod, EdgeKind::Calls);

        let snapshot = TestProximityFactProducer::default().produce(&graph, true);
        assert!(snapshot.file("tests/widget_test.rs").is_none());
        assert_eq!(snapshot.files.len(), 1);
    }

    #[test]
    fn empty_graph_is_unavailable_not_zero() {
        let snapshot = TestProximityFactProducer::default().produce(&CodeGraph::new(), true);
        assert_eq!(snapshot.availability(), FactAvailability::Unavailable);
        assert_eq!(snapshot, TestProximitySnapshot::empty());
    }

    #[test]
    fn incomplete_index_degrades_without_altering_counts() {
        let mut graph = CodeGraph::new();
        let prod = symbol("src/widget.rs", "make_widget");
        let test = symbol("tests/widget_test.rs", "test_make_widget");
        add_symbol(&mut graph, &prod, true, 1);
        add_symbol(&mut graph, &test, true, 1);
        add_edge(&mut graph, &test, &prod, EdgeKind::Calls);

        let complete = TestProximityFactProducer::default().produce(&graph, true);
        let partial = TestProximityFactProducer::default().produce(&graph, false);
        assert_eq!(complete.files, partial.files);
        assert_eq!(complete.availability(), FactAvailability::Available);
        assert_eq!(partial.availability(), FactAvailability::Degraded);
    }

    #[test]
    fn non_canonical_paths_are_excluded_and_reported_as_degraded() {
        let mut graph = CodeGraph::new();
        let outside_test = symbol("../outside_test.rs", "test_escape");
        let prod = symbol("src/widget.rs", "make_widget");
        add_symbol(&mut graph, &outside_test, true, 1);
        add_symbol(&mut graph, &prod, true, 1);
        add_edge(&mut graph, &outside_test, &prod, EdgeKind::Calls);

        let snapshot = TestProximityFactProducer::default().produce(&graph, true);
        assert_eq!(snapshot.file("src/widget.rs").unwrap().linked_test_count, 0);
        assert_eq!(snapshot.report.invalid_path_nodes, 1);
        assert_eq!(snapshot.report.invalid_path_edges, 1);
        assert_eq!(snapshot.availability(), FactAvailability::Degraded);
    }

    #[test]
    fn production_is_deterministic_across_insertion_orders() {
        let mut forward_graph = CodeGraph::new();
        let a_prod = symbol("src/a.rs", "alpha");
        let a_test = symbol("tests/a_test.rs", "test_alpha");
        let b_prod = symbol("src/b.rs", "beta");
        add_symbol(&mut forward_graph, &a_prod, true, 1);
        add_symbol(&mut forward_graph, &a_test, true, 1);
        add_symbol(&mut forward_graph, &b_prod, true, 1);
        add_edge(&mut forward_graph, &a_test, &a_prod, EdgeKind::Calls);
        let forward = TestProximityFactProducer::default().produce(&forward_graph, true);

        let mut reversed_graph = CodeGraph::new();
        add_symbol(&mut reversed_graph, &b_prod, true, 1);
        add_symbol(&mut reversed_graph, &a_test, true, 1);
        add_symbol(&mut reversed_graph, &a_prod, true, 1);
        add_edge(&mut reversed_graph, &a_test, &a_prod, EdgeKind::Calls);
        let backward = TestProximityFactProducer::default().produce(&reversed_graph, true);

        assert_eq!(forward.files, backward.files);
        assert_eq!(
            serde_json::to_string(&forward).unwrap(),
            serde_json::to_string(&backward).unwrap()
        );
    }

    #[test]
    fn limits_truncate_files_and_evidence_but_keep_the_exact_count() {
        let producer = TestProximityFactProducer::new(TestProximityLimits {
            max_files: 1,
            max_linked_test_evidence: 1,
        });
        let mut graph = CodeGraph::new();
        let prod_a = symbol("src/a.rs", "alpha");
        let prod_b = symbol("src/b.rs", "beta");
        let test_one = symbol("tests/one_test.rs", "case_one");
        let test_two = symbol("tests/two_test.rs", "case_two");
        add_symbol(&mut graph, &prod_a, true, 1);
        add_symbol(&mut graph, &prod_b, true, 1);
        add_symbol(&mut graph, &test_one, true, 1);
        add_symbol(&mut graph, &test_two, true, 1);
        add_edge(&mut graph, &test_one, &prod_a, EdgeKind::Calls);
        add_edge(&mut graph, &test_two, &prod_a, EdgeKind::Calls);

        let snapshot = producer.produce(&graph, true);
        assert_eq!(snapshot.files.len(), 1);
        assert!(snapshot.report.file_overflow);
        let facts = snapshot.file("src/a.rs").unwrap();
        // The count stays exact even though the evidence list is capped.
        assert_eq!(facts.linked_test_count, 2);
        assert_eq!(facts.linked_test_files.len(), 1);
        assert_eq!(snapshot.availability(), FactAvailability::Degraded);
    }

    #[test]
    fn reproducing_an_unchanged_graph_yields_an_empty_delta() {
        let mut graph = CodeGraph::new();
        let prod = symbol("src/widget.rs", "make_widget");
        let test = symbol("tests/widget_test.rs", "test_make_widget");
        add_symbol(&mut graph, &prod, true, 1);
        add_symbol(&mut graph, &test, true, 1);
        add_edge(&mut graph, &test, &prod, EdgeKind::Calls);

        let producer = TestProximityFactProducer::default();
        let before = producer.produce(&graph, true);
        let after = producer.produce(&graph, true);
        assert!(after.file_delta(&before).is_empty());
    }

    #[test]
    fn a_new_test_moves_only_the_files_it_touches() {
        let mut graph = CodeGraph::new();
        let prod_a = symbol("src/a.rs", "alpha");
        let prod_b = symbol("src/b.rs", "beta");
        add_symbol(&mut graph, &prod_a, true, 1);
        add_symbol(&mut graph, &prod_b, true, 1);
        let producer = TestProximityFactProducer::default();
        let before = producer.produce(&graph, true);

        let new_test = symbol("tests/a_test.rs", "test_alpha");
        add_symbol(&mut graph, &new_test, true, 1);
        add_edge(&mut graph, &new_test, &prod_a, EdgeKind::Calls);
        let after = producer.produce(&graph, true);

        let delta = after.file_delta(&before);
        assert_eq!(delta.updated, ["src/a.rs"]);
        assert!(delta.added.is_empty());
        assert!(delta.removed.is_empty());
        assert_eq!(before.file("src/b.rs"), after.file("src/b.rs"));
    }
}
