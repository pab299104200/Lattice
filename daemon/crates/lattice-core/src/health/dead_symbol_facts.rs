//! Pure "no indexed dependents" health facts for exported symbols.
//!
//! The producer is a total function of the built [`CodeGraph`] plus a small,
//! explicitly caller-supplied set of exclusion inputs (see "Known input
//! gaps" below). It performs no IO, no clock reads, and no floating-point
//! arithmetic, so an identical graph and identical exclusion inputs yield a
//! byte-identical snapshot on every platform. Persistence lives in
//! [`crate::storage::health_dead_symbol_facts_store`].
//!
//! See `docs/plans/2026-08-13-health-engine.md`, section "H2.3 Dead-symbol
//! facts".
//!
//! # The fact
//!
//! A candidate is any exported symbol (`is_exported == true`) with zero
//! *indexed* dependents: no other symbol has an edge from
//! [`crate::health::graph_facts::is_dependency_edge`] pointing at it, and no
//! definition of it depends on itself (a purely recursive/self-referential
//! symbol is not "used by something else"). Every candidate is persisted —
//! both the ones ultimately flagged and the ones excluded — because a
//! consumer explaining "why isn't `Foo::bar` flagged" needs the excluded row,
//! not just its absence.
//!
//! # Presentation requirement (non-negotiable)
//!
//! The graph only records edges the indexer actually resolved. A parse
//! failure or a partially indexed workspace can make a genuinely-called
//! symbol *look* like it has zero dependents. This fact family must never be
//! rendered as "dead" or "unused" outright — every render must carry the
//! literal phrase `"no indexed dependents"` (see [`NO_INDEXED_DEPENDENTS_PHRASE`]
//! and [`DeadSymbolCandidate::describe`]), and a degraded or unavailable
//! report must additionally say the absence is not confirmed (design
//! decision 4, "unknown is never zero").
//!
//! # Deterministic exclusions
//!
//! Four categories of legitimate root are excluded from being flagged, even
//! though each individually is "exported with zero indexed dependents":
//!
//! * **Binary entry points** — a symbol named `main`
//!   ([`DEFAULT_BINARY_ENTRY_POINT_NAME`]), or a caller-supplied fact key in
//!   [`DeadSymbolExclusionInputs::extra_entry_point_keys`] (e.g. resolved from
//!   `Cargo.toml` `[[bin]]` targets — filesystem reads are the caller's job,
//!   never this pure producer's).
//! * **Test symbols** — any symbol in a file
//!   [`crate::intelligence::agent::is_test_file`] identifies as a test file,
//!   the same heuristic `find_relevant_tests` uses.
//! * **Crate-root re-exports (`pub use`)** — a symbol whose name is in
//!   [`DeadSymbolExclusionInputs::reexported_names`].
//! * **Trait impl methods** — a method whose unqualified name (the suffix
//!   after the parser's `Owner.method` qualification) is a well-known trait
//!   method invoked implicitly by the language rather than by direct
//!   reference ([`DEFAULT_KNOWN_TRAIT_METHOD_NAMES`]), or a caller-supplied
//!   addition in [`DeadSymbolExclusionInputs::extra_trait_method_names`].
//!
//! # Known input gaps
//!
//! The graph this producer reads does not yet carry two signals a fully
//! precise version of this fact would use directly:
//!
//! * **`pub use` re-exports.** Neither [`crate::graph::GraphNode`] nor
//!   [`crate::symbols::ImportInfo`] currently distinguishes a `pub use` from a
//!   private `use` (the Rust parser's `extract_use` drops `pub use` lines
//!   entirely today — a `pub` prefix fails its `strip_prefix("use ")` and the
//!   whole import is silently skipped). Rather than couple this health module
//!   to a parser fix, [`DeadSymbolExclusionInputs::reexported_names`] takes the
//!   already-resolved leaf names as caller-supplied truth, mirroring how
//!   `index_complete` is caller-supplied truth in
//!   [`crate::health::graph_facts::GraphFactProducer::produce`]. Wiring a real
//!   `pub use` scan into this input is future indexer-integration work (H4),
//!   not a change this fact family's purity should absorb.
//! * **Trait-impl linkage.** [`crate::graph::EdgeKind::Implements`] exists in
//!   the edge vocabulary but the indexer never emits it (`GraphBuilder`
//!   creates `Calls`, `Contains`, `LinksTo`, and `Mentions` edges only), and
//!   the Rust parser's `extract_impl` never reads the `impl Trait for Type`
//!   trait field, so a trait-impl method is byte-for-byte indistinguishable
//!   from an inherent-impl method of the same name. The well-known-name
//!   heuristic above is a deliberate, documented approximation of the real
//!   signal, in the same spirit as `looks_like_bug_fix`'s subject-vocabulary
//!   heuristic being "an explainable heuristic" the spec explicitly permits
//!   (H1.2) rather than a false claim of precision.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::git_intelligence::canonical_repository_path;
use crate::graph::CodeGraph;
use crate::health::graph_facts::{is_dependency_edge, symbol_fact_key};
use crate::intelligence::agent::is_test_file;
use crate::symbols::SymbolKind;

/// Aggregation version persisted with every snapshot.
///
/// Bump whenever the meaning of a produced value changes so that stored
/// generations from an older producer are recognisably different rather than
/// silently misread.
pub const DEAD_SYMBOL_FACTS_VERSION: u32 = 1;

/// Upper bound on persisted candidate rows.
pub const MAX_CANDIDATES: usize = 1_000_000;

/// The symbol name treated as a binary entry point regardless of caller
/// input. Rust, Go, and several other supported languages all use `main` as
/// the conventional process entry point.
pub const DEFAULT_BINARY_ENTRY_POINT_NAME: &str = "main";

/// The exact phrase every rendering of this fact family must contain. See the
/// module-level "Presentation requirement" section.
pub const NO_INDEXED_DEPENDENTS_PHRASE: &str = "no indexed dependents";

/// Well-known trait method names invoked implicitly by the language or
/// compiler (operator overloads, formatting, iteration, conversion, and
/// derive-macro machinery) rather than by a direct call-graph reference.
///
/// Kept alphabetically sorted for readability; order has no runtime meaning
/// since callers only ever see this collapsed into a [`BTreeSet`].
pub static DEFAULT_KNOWN_TRAIT_METHOD_NAMES: &[&str] = &[
    "add",
    "and_then",
    "as_mut",
    "as_ref",
    "bitand",
    "bitor",
    "bitxor",
    "borrow",
    "borrow_mut",
    "clone",
    "clone_from",
    "cmp",
    "default",
    "deref",
    "deref_mut",
    "deserialize",
    "div",
    "drop",
    "eq",
    "fmt",
    "from",
    "from_iter",
    "hash",
    "index",
    "index_mut",
    "into",
    "into_iter",
    "mul",
    "ne",
    "neg",
    "next",
    "not",
    "partial_cmp",
    "rem",
    "serialize",
    "sub",
    "sum",
    "try_from",
    "try_into",
];

/// Effective aggregation bounds persisted alongside a snapshot generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadSymbolFactLimits {
    pub max_candidates: usize,
}

impl Default for DeadSymbolFactLimits {
    fn default() -> Self {
        Self {
            max_candidates: MAX_CANDIDATES,
        }
    }
}

impl DeadSymbolFactLimits {
    fn bounded(self) -> Self {
        Self {
            max_candidates: self.max_candidates.min(MAX_CANDIDATES),
        }
    }
}

/// How much of this fact family a consumer is entitled to trust.
///
/// Module-local by design: sibling fact families (`graph_facts`,
/// `complexity_facts`, `churn_facts`) each define their own
/// `FactAvailability` today, and unifying them is explicitly deferred to
/// H3.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactAvailability {
    /// Every input was observed; the facts are complete.
    Available,
    /// Facts were produced from a partial index and understate reality.
    Degraded,
    /// Nothing was observed; consumers must not treat absence as zero.
    Unavailable,
}

impl FactAvailability {
    /// Stable code used in storage and payloads.
    pub fn as_str(&self) -> &'static str {
        match self {
            FactAvailability::Available => "available",
            FactAvailability::Degraded => "degraded",
            FactAvailability::Unavailable => "unavailable",
        }
    }

    /// Decode a stored code into an availability.
    pub fn from_code(value: &str) -> Option<Self> {
        match value {
            "available" => Some(FactAvailability::Available),
            "degraded" => Some(FactAvailability::Degraded),
            "unavailable" => Some(FactAvailability::Unavailable),
            _ => None,
        }
    }
}

/// Why a candidate is not flagged despite having zero indexed dependents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeadSymbolExclusionReason {
    /// The symbol is a binary entry point (`main`, or a caller-supplied
    /// entry-point key).
    BinaryEntryPoint,
    /// The symbol is defined in a file [`is_test_file`] identifies as a test.
    TestSymbol,
    /// The symbol's name is publicly re-exported at a crate boundary.
    ReexportedAtCrateRoot,
    /// The symbol is a trait implementation method invoked implicitly.
    TraitImplMethod,
}

impl DeadSymbolExclusionReason {
    /// Stable code used in storage and payloads.
    pub fn as_str(&self) -> &'static str {
        match self {
            DeadSymbolExclusionReason::BinaryEntryPoint => "binary_entry_point",
            DeadSymbolExclusionReason::TestSymbol => "test_symbol",
            DeadSymbolExclusionReason::ReexportedAtCrateRoot => "reexported_at_crate_root",
            DeadSymbolExclusionReason::TraitImplMethod => "trait_impl_method",
        }
    }

    /// Decode a stored code into a reason.
    pub fn from_code(value: &str) -> Option<Self> {
        match value {
            "binary_entry_point" => Some(DeadSymbolExclusionReason::BinaryEntryPoint),
            "test_symbol" => Some(DeadSymbolExclusionReason::TestSymbol),
            "reexported_at_crate_root" => Some(DeadSymbolExclusionReason::ReexportedAtCrateRoot),
            "trait_impl_method" => Some(DeadSymbolExclusionReason::TraitImplMethod),
            _ => None,
        }
    }
}

/// Completeness evidence for one pure aggregation run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadSymbolFactsReport {
    pub facts_version: u32,
    pub limits: DeadSymbolFactLimits,
    /// Graph nodes examined, including nodes rejected for a non-canonical path.
    pub nodes_seen: u64,
    /// Graph edges examined, including non-dependency and rejected edges.
    pub edges_seen: u64,
    /// Distinct canonical files that contributed at least one node.
    pub files_observed: u32,
    /// Distinct exported symbol keys observed (candidates and non-candidates).
    pub exported_symbols_observed: u32,
    /// Exported symbol keys with zero indexed dependents, before truncation.
    pub candidates_observed: u32,
    /// Nodes whose file path was not canonical and repository-relative.
    pub invalid_path_nodes: u64,
    /// Dependency edges dropped because an endpoint had a non-canonical path.
    pub invalid_path_edges: u64,
    /// True when candidate rows were truncated by `limits.max_candidates`.
    pub candidate_overflow: bool,
    /// Caller-supplied truth about whether the index behind the graph was whole.
    pub index_complete: bool,
}

impl DeadSymbolFactsReport {
    /// Complete snapshots are the only snapshots eligible to affect scores.
    pub fn is_complete(&self) -> bool {
        !self.is_degraded()
    }

    pub fn is_degraded(&self) -> bool {
        !self.index_complete
            || self.invalid_path_nodes > 0
            || self.invalid_path_edges > 0
            || self.candidate_overflow
    }

    /// No exported symbol observed at all is unavailable, not "nothing is dead".
    pub fn availability(&self) -> FactAvailability {
        if self.exported_symbols_observed == 0 {
            FactAvailability::Unavailable
        } else if self.is_degraded() {
            FactAvailability::Degraded
        } else {
            FactAvailability::Available
        }
    }

    /// The exact caveat every render of a candidate must carry. Always
    /// contains [`NO_INDEXED_DEPENDENTS_PHRASE`] verbatim; a degraded or
    /// unavailable report additionally states the absence is not confirmed
    /// (design decision 4, "unknown is never zero").
    pub fn dependents_caveat(&self) -> String {
        match self.availability() {
            FactAvailability::Available => {
                format!("{NO_INDEXED_DEPENDENTS_PHRASE} (index complete)")
            }
            FactAvailability::Degraded => format!(
                "{NO_INDEXED_DEPENDENTS_PHRASE} (index degraded: absence not confirmed)"
            ),
            FactAvailability::Unavailable => format!(
                "{NO_INDEXED_DEPENDENTS_PHRASE} (index unavailable: absence not confirmed)"
            ),
        }
    }
}

/// One exported symbol with zero indexed dependents, plus why it is or is not
/// flagged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadSymbolCandidate {
    /// `<path>::<name>` fact key, matching
    /// [`crate::health::graph_facts::SymbolGraphFacts::key`].
    pub key: String,
    pub path: String,
    pub name: String,
    pub kind: SymbolKind,
    /// Exported definitions collapsed into this key.
    pub definition_count: u32,
    /// First line of the earliest-starting definition's source range.
    pub source_line: u32,
    /// Last line of that same definition's source range.
    pub source_end_line: u32,
    /// Empty when the symbol is actually flagged as having no indexed
    /// dependents; non-empty names the deterministic exclusion(s) that keep
    /// it off the flagged list. Ordered by declaration order in
    /// [`DeadSymbolExclusionReason`], deduplicated.
    pub exclusion_reasons: Vec<DeadSymbolExclusionReason>,
}

impl DeadSymbolCandidate {
    /// True when no exclusion applies: this symbol is exported, has zero
    /// indexed dependents, and matches none of the deterministic roots.
    pub fn is_flagged(&self) -> bool {
        self.exclusion_reasons.is_empty()
    }

    /// Human-readable evidence line. Always carries the report's
    /// [`DeadSymbolFactsReport::dependents_caveat`] verbatim, satisfying the
    /// spec's non-negotiable presentation requirement.
    pub fn describe(&self, report: &DeadSymbolFactsReport) -> String {
        let caveat = report.dependents_caveat();
        if self.is_flagged() {
            format!("{} ({} exported, {caveat})", self.key, self.kind.short_code())
        } else {
            let reasons: Vec<&str> = self
                .exclusion_reasons
                .iter()
                .map(DeadSymbolExclusionReason::as_str)
                .collect();
            format!(
                "{} ({} exported, {caveat}, excluded: {})",
                self.key,
                self.kind.short_code(),
                reasons.join(", ")
            )
        }
    }
}

/// Stable dead-symbol facts consumed by health scoring and verb payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadSymbolFactsSnapshot {
    /// Every exported, zero-indexed-dependent symbol observed: flagged and
    /// excluded alike. Ordered by `key`, unique.
    pub candidates: Vec<DeadSymbolCandidate>,
    pub report: DeadSymbolFactsReport,
}

impl DeadSymbolFactsSnapshot {
    /// Empty snapshots are honest: no graph was available, not "nothing is dead".
    pub fn empty() -> Self {
        Self {
            candidates: Vec::new(),
            report: DeadSymbolFactsReport {
                facts_version: DEAD_SYMBOL_FACTS_VERSION,
                limits: DeadSymbolFactLimits::default(),
                nodes_seen: 0,
                edges_seen: 0,
                files_observed: 0,
                exported_symbols_observed: 0,
                candidates_observed: 0,
                invalid_path_nodes: 0,
                invalid_path_edges: 0,
                candidate_overflow: false,
                index_complete: true,
            },
        }
    }

    pub fn availability(&self) -> FactAvailability {
        self.report.availability()
    }

    /// Looks up one candidate by its `<path>::<name>` fact key.
    pub fn candidate(&self, key: &str) -> Option<&DeadSymbolCandidate> {
        self.candidates
            .binary_search_by(|candidate| candidate.key.as_str().cmp(key))
            .ok()
            .map(|index| &self.candidates[index])
    }

    /// Only the candidates actually flagged (no exclusion applies), ordered
    /// by key.
    pub fn flagged(&self) -> impl Iterator<Item = &DeadSymbolCandidate> {
        self.candidates.iter().filter(|candidate| candidate.is_flagged())
    }
}

/// Caller-supplied exclusion evidence the graph does not yet carry directly.
/// See the module-level "Known input gaps" section for why these are inputs
/// rather than something this pure producer derives itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeadSymbolExclusionInputs {
    /// Leaf names publicly re-exported anywhere via `pub use` at a crate
    /// boundary (e.g. `pub use foo::Bar;` contributes `"Bar"`).
    pub reexported_names: BTreeSet<String>,
    /// Additional binary-entry-point fact keys (`<path>::<name>`) beyond the
    /// built-in [`DEFAULT_BINARY_ENTRY_POINT_NAME`] heuristic, e.g. resolved
    /// by the caller from `Cargo.toml` `[[bin]]` sections.
    pub extra_entry_point_keys: BTreeSet<String>,
    /// Additional unqualified trait-method names treated like
    /// [`DEFAULT_KNOWN_TRAIT_METHOD_NAMES`].
    pub extra_trait_method_names: BTreeSet<String>,
}

/// Pure producer of dead-symbol facts.
#[derive(Debug, Clone)]
pub struct DeadSymbolFactProducer {
    limits: DeadSymbolFactLimits,
    known_trait_method_names: BTreeSet<&'static str>,
}

impl Default for DeadSymbolFactProducer {
    fn default() -> Self {
        Self {
            limits: DeadSymbolFactLimits::default(),
            known_trait_method_names: DEFAULT_KNOWN_TRAIT_METHOD_NAMES.iter().copied().collect(),
        }
    }
}

impl DeadSymbolFactProducer {
    pub fn new(limits: DeadSymbolFactLimits) -> Self {
        Self {
            limits: limits.bounded(),
            ..Self::default()
        }
    }

    /// Produces a snapshot from `graph`.
    ///
    /// `exclusions` supplies the evidence the graph itself does not carry
    /// (see "Known input gaps"). `index_complete` is the caller's truth about
    /// whether the index behind the graph covered the whole workspace; it
    /// never changes a computed value, it only marks the snapshot degraded
    /// so consumers refuse to treat an absent dependent as a confirmed
    /// absence.
    pub fn produce(
        &self,
        graph: &CodeGraph,
        exclusions: &DeadSymbolExclusionInputs,
        index_complete: bool,
    ) -> DeadSymbolFactsSnapshot {
        let limits = self.limits.bounded();
        let mut nodes_seen = 0_u64;
        let mut invalid_path_nodes = 0_u64;
        let mut files: BTreeSet<String> = BTreeSet::new();
        let mut symbols: std::collections::BTreeMap<String, SymbolAccumulator> =
            std::collections::BTreeMap::new();

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
            let line = saturating_u32(node.line);
            let end_line = saturating_u32(node.end_line);
            let entry = symbols.entry(key).or_insert_with(|| SymbolAccumulator {
                path: path.clone(),
                name: name.to_owned(),
                kind: node.kind,
                definition_count: 0,
                source_line: line,
                source_end_line: end_line,
                has_dependent: false,
            });
            entry.definition_count = entry.definition_count.saturating_add(1);
            // Deterministic tie-break: the earliest-starting definition's
            // range is the one cited as evidence, independent of graph
            // traversal order.
            if (line, end_line) < (entry.source_line, entry.source_end_line) {
                entry.source_line = line;
                entry.source_end_line = end_line;
            }
        }

        let mut edges_seen = 0_u64;
        let mut invalid_path_edges = 0_u64;
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
            let from_name = from.name.trim();
            let to_name = to.name.trim();
            if from_name.is_empty() || to_name.is_empty() {
                continue;
            }
            let from_key = symbol_fact_key(&from_path, from_name);
            let to_key = symbol_fact_key(&to_path, to_name);
            if from_key == to_key {
                // A symbol calling/referencing itself is not "used by
                // something else"; a purely recursive symbol with no other
                // caller must still be eligible to be flagged.
                continue;
            }
            if let Some(entry) = symbols.get_mut(&to_key) {
                entry.has_dependent = true;
            }
        }

        let files_observed = saturating_u32(files.len());
        let exported_symbols_observed = saturating_u32(symbols.len());

        let mut candidates: Vec<DeadSymbolCandidate> = symbols
            .into_iter()
            .filter(|(_, accumulator)| !accumulator.has_dependent)
            .map(|(key, accumulator)| {
                let exclusion_reasons =
                    self.exclusion_reasons(&key, &accumulator, exclusions);
                DeadSymbolCandidate {
                    key,
                    path: accumulator.path,
                    name: accumulator.name,
                    kind: accumulator.kind,
                    definition_count: accumulator.definition_count,
                    source_line: accumulator.source_line,
                    source_end_line: accumulator.source_end_line,
                    exclusion_reasons,
                }
            })
            .collect();
        // `symbols` was a `BTreeMap<String, _>`, so `candidates` is already
        // ordered by key; `sort` here documents that invariant explicitly and
        // stays cheap (near-sorted input) if the source ever changes.
        candidates.sort_by(|left, right| left.key.cmp(&right.key));

        let candidates_observed = saturating_u32(candidates.len());
        let candidate_overflow = candidates.len() > limits.max_candidates;
        candidates.truncate(limits.max_candidates);

        DeadSymbolFactsSnapshot {
            candidates,
            report: DeadSymbolFactsReport {
                facts_version: DEAD_SYMBOL_FACTS_VERSION,
                limits,
                nodes_seen,
                edges_seen,
                files_observed,
                exported_symbols_observed,
                candidates_observed,
                invalid_path_nodes,
                invalid_path_edges,
                candidate_overflow,
                index_complete,
            },
        }
    }

    fn exclusion_reasons(
        &self,
        key: &str,
        accumulator: &SymbolAccumulator,
        exclusions: &DeadSymbolExclusionInputs,
    ) -> Vec<DeadSymbolExclusionReason> {
        let mut reasons = Vec::new();
        if accumulator.name == DEFAULT_BINARY_ENTRY_POINT_NAME
            || exclusions.extra_entry_point_keys.contains(key)
        {
            reasons.push(DeadSymbolExclusionReason::BinaryEntryPoint);
        }
        if is_test_file(&accumulator.path) {
            reasons.push(DeadSymbolExclusionReason::TestSymbol);
        }
        if exclusions.reexported_names.contains(&accumulator.name) {
            reasons.push(DeadSymbolExclusionReason::ReexportedAtCrateRoot);
        }
        // The Rust parser qualifies impl methods as `Owner.method`; strip the
        // owner so the trait-method-name heuristic matches the method itself
        // regardless of the type implementing it.
        let unqualified_name = accumulator
            .name
            .rsplit('.')
            .next()
            .unwrap_or(accumulator.name.as_str());
        if self.known_trait_method_names.contains(unqualified_name)
            || exclusions
                .extra_trait_method_names
                .contains(unqualified_name)
        {
            reasons.push(DeadSymbolExclusionReason::TraitImplMethod);
        }
        reasons
    }
}

#[derive(Debug, Clone)]
struct SymbolAccumulator {
    path: String,
    name: String,
    kind: SymbolKind,
    definition_count: u32,
    source_line: u32,
    source_end_line: u32,
    has_dependent: bool,
}

fn saturating_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
pub(crate) mod fixtures {
    use crate::graph::CodeGraph;
    use crate::symbols::{Language, SymbolId, SymbolKind};

    pub fn symbol(file: &str, name: &str) -> SymbolId {
        SymbolId {
            file: file.to_owned(),
            name: name.to_owned(),
            byte_offset: 0,
        }
    }

    /// Adds an exported node with an explicit source range and kind.
    pub fn add_symbol_kind(
        graph: &mut CodeGraph,
        id: &SymbolId,
        kind: SymbolKind,
        exported: bool,
        line: usize,
    ) {
        graph.add_node(
            id.clone(),
            kind,
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

    pub fn add_symbol(graph: &mut CodeGraph, id: &SymbolId, exported: bool, line: usize) {
        add_symbol_kind(graph, id, SymbolKind::Function, exported, line);
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{add_symbol, add_symbol_kind, symbol};
    use super::*;
    use crate::graph::{CodeGraph, EdgeKind};
    use crate::symbols::{SymbolId, SymbolKind};

    #[test]
    fn symbol_with_a_real_dependent_is_not_a_candidate() {
        let mut graph = CodeGraph::new();
        let used = symbol("src/lib.rs", "used_fn");
        let caller = symbol("src/caller.rs", "caller_fn");
        add_symbol(&mut graph, &used, true, 1);
        add_symbol(&mut graph, &caller, true, 1);
        graph.add_edge(&caller, &used, EdgeKind::Calls);

        let snapshot = DeadSymbolFactProducer::default().produce(
            &graph,
            &DeadSymbolExclusionInputs::default(),
            true,
        );

        assert!(snapshot.candidate("src/lib.rs::used_fn").is_none());
        assert_eq!(snapshot.report.exported_symbols_observed, 2);
        assert_eq!(snapshot.report.candidates_observed, 1);
    }

    #[test]
    fn truly_dead_exported_symbol_is_flagged() {
        let mut graph = CodeGraph::new();
        let dead = symbol("src/lib.rs", "unused_fn");
        add_symbol(&mut graph, &dead, true, 1);

        let snapshot = DeadSymbolFactProducer::default().produce(
            &graph,
            &DeadSymbolExclusionInputs::default(),
            true,
        );

        let candidate = snapshot
            .candidate("src/lib.rs::unused_fn")
            .expect("candidate present");
        assert!(candidate.is_flagged());
        assert!(candidate.exclusion_reasons.is_empty());
        assert_eq!(snapshot.flagged().count(), 1);
        assert!(candidate.describe(&snapshot.report).contains(NO_INDEXED_DEPENDENTS_PHRASE));
    }

    #[test]
    fn a_purely_recursive_symbol_with_no_external_caller_is_flagged() {
        let mut graph = CodeGraph::new();
        let recursive = symbol("src/lib.rs", "recurse");
        add_symbol(&mut graph, &recursive, true, 1);
        graph.add_edge(&recursive, &recursive, EdgeKind::Calls);

        let snapshot = DeadSymbolFactProducer::default().produce(
            &graph,
            &DeadSymbolExclusionInputs::default(),
            true,
        );

        let candidate = snapshot.candidate("src/lib.rs::recurse").unwrap();
        assert!(candidate.is_flagged());
    }

    #[test]
    fn not_exported_symbol_is_never_a_candidate() {
        let mut graph = CodeGraph::new();
        let private = symbol("src/lib.rs", "private_fn");
        add_symbol(&mut graph, &private, false, 1);

        let snapshot = DeadSymbolFactProducer::default().produce(
            &graph,
            &DeadSymbolExclusionInputs::default(),
            true,
        );

        assert!(snapshot.candidates.is_empty());
        assert_eq!(snapshot.report.exported_symbols_observed, 0);
        assert_eq!(snapshot.availability(), FactAvailability::Unavailable);
    }

    #[test]
    fn binary_entry_point_main_is_excluded_not_flagged() {
        let mut graph = CodeGraph::new();
        // A real `fn main` is typically not `pub`, but a caller (e.g. a
        // library re-exporting a runnable `main`) can still export one; the
        // exclusion must hold regardless of export visibility conventions.
        let main_fn = symbol("src/main.rs", "main");
        add_symbol(&mut graph, &main_fn, true, 1);

        let snapshot = DeadSymbolFactProducer::default().produce(
            &graph,
            &DeadSymbolExclusionInputs::default(),
            true,
        );

        let candidate = snapshot.candidate("src/main.rs::main").unwrap();
        assert!(!candidate.is_flagged());
        assert_eq!(
            candidate.exclusion_reasons,
            vec![DeadSymbolExclusionReason::BinaryEntryPoint]
        );
        assert_eq!(snapshot.flagged().count(), 0);
    }

    #[test]
    fn caller_supplied_extra_entry_point_is_excluded() {
        let mut graph = CodeGraph::new();
        let cli_entry = symbol("src/bin/tool.rs", "run");
        add_symbol(&mut graph, &cli_entry, true, 1);
        let mut exclusions = DeadSymbolExclusionInputs::default();
        exclusions
            .extra_entry_point_keys
            .insert(symbol_fact_key("src/bin/tool.rs", "run"));

        let snapshot =
            DeadSymbolFactProducer::default().produce(&graph, &exclusions, true);

        let candidate = snapshot.candidate("src/bin/tool.rs::run").unwrap();
        assert_eq!(
            candidate.exclusion_reasons,
            vec![DeadSymbolExclusionReason::BinaryEntryPoint]
        );
    }

    #[test]
    fn test_symbol_is_excluded_not_flagged() {
        let mut graph = CodeGraph::new();
        let test_helper = symbol("tests/support.rs", "make_fixture");
        add_symbol(&mut graph, &test_helper, true, 1);

        let snapshot = DeadSymbolFactProducer::default().produce(
            &graph,
            &DeadSymbolExclusionInputs::default(),
            true,
        );

        let candidate = snapshot.candidate("tests/support.rs::make_fixture").unwrap();
        assert!(!candidate.is_flagged());
        assert_eq!(
            candidate.exclusion_reasons,
            vec![DeadSymbolExclusionReason::TestSymbol]
        );
    }

    #[test]
    fn crate_root_reexport_is_excluded_not_flagged() {
        let mut graph = CodeGraph::new();
        let reexported = symbol("src/internal/widget.rs", "Widget");
        add_symbol(&mut graph, &reexported, true, 1);
        let mut exclusions = DeadSymbolExclusionInputs::default();
        exclusions.reexported_names.insert("Widget".to_owned());

        let snapshot =
            DeadSymbolFactProducer::default().produce(&graph, &exclusions, true);

        let candidate = snapshot
            .candidate("src/internal/widget.rs::Widget")
            .unwrap();
        assert!(!candidate.is_flagged());
        assert_eq!(
            candidate.exclusion_reasons,
            vec![DeadSymbolExclusionReason::ReexportedAtCrateRoot]
        );
    }

    #[test]
    fn trait_impl_method_is_excluded_not_flagged() {
        let mut graph = CodeGraph::new();
        // Mirrors the parser's `Owner.method` qualification for
        // `impl Display for Widget { fn fmt(...) {} }`.
        let fmt_method = symbol("src/internal/widget.rs", "Widget.fmt");
        add_symbol_kind(&mut graph, &fmt_method, SymbolKind::Method, true, 10);

        let snapshot = DeadSymbolFactProducer::default().produce(
            &graph,
            &DeadSymbolExclusionInputs::default(),
            true,
        );

        let candidate = snapshot
            .candidate("src/internal/widget.rs::Widget.fmt")
            .unwrap();
        assert!(!candidate.is_flagged());
        assert_eq!(
            candidate.exclusion_reasons,
            vec![DeadSymbolExclusionReason::TraitImplMethod]
        );
    }

    #[test]
    fn caller_supplied_extra_trait_method_name_is_excluded() {
        let mut graph = CodeGraph::new();
        let handler = symbol("src/internal/widget.rs", "Widget.on_custom_event");
        add_symbol_kind(&mut graph, &handler, SymbolKind::Method, true, 10);
        let mut exclusions = DeadSymbolExclusionInputs::default();
        exclusions
            .extra_trait_method_names
            .insert("on_custom_event".to_owned());

        let snapshot =
            DeadSymbolFactProducer::default().produce(&graph, &exclusions, true);

        let candidate = snapshot
            .candidate("src/internal/widget.rs::Widget.on_custom_event")
            .unwrap();
        assert_eq!(
            candidate.exclusion_reasons,
            vec![DeadSymbolExclusionReason::TraitImplMethod]
        );
    }

    #[test]
    fn a_candidate_can_carry_multiple_exclusion_reasons_in_declared_order() {
        let mut graph = CodeGraph::new();
        // A test-file trait impl method named `main` is contrived but must
        // still compose correctly and deterministically.
        let odd = symbol("tests/support.rs", "Fixture.fmt");
        add_symbol_kind(&mut graph, &odd, SymbolKind::Method, true, 1);

        let snapshot = DeadSymbolFactProducer::default().produce(
            &graph,
            &DeadSymbolExclusionInputs::default(),
            true,
        );

        let candidate = snapshot.candidate("tests/support.rs::Fixture.fmt").unwrap();
        assert_eq!(
            candidate.exclusion_reasons,
            vec![
                DeadSymbolExclusionReason::TestSymbol,
                DeadSymbolExclusionReason::TraitImplMethod,
            ]
        );
    }

    #[test]
    fn producing_twice_from_the_same_graph_is_byte_identical() {
        let mut graph = CodeGraph::new();
        let a = symbol("src/a.rs", "a_fn");
        let b = symbol("src/b.rs", "b_fn");
        add_symbol(&mut graph, &a, true, 1);
        add_symbol(&mut graph, &b, true, 1);
        graph.add_edge(&b, &a, EdgeKind::Calls);

        let producer = DeadSymbolFactProducer::default();
        let exclusions = DeadSymbolExclusionInputs::default();
        let first = producer.produce(&graph, &exclusions, true);
        let second = producer.produce(&graph, &exclusions, true);

        assert_eq!(first, second);
        let first_json = serde_json::to_string(&first).unwrap();
        let second_json = serde_json::to_string(&second).unwrap();
        assert_eq!(first_json, second_json);
    }

    #[test]
    fn candidates_are_ordered_by_key_regardless_of_insertion_order() {
        let mut graph = CodeGraph::new();
        let z = symbol("src/z.rs", "z_fn");
        let a = symbol("src/a.rs", "a_fn");
        add_symbol(&mut graph, &z, true, 1);
        add_symbol(&mut graph, &a, true, 1);

        let snapshot = DeadSymbolFactProducer::default().produce(
            &graph,
            &DeadSymbolExclusionInputs::default(),
            true,
        );

        let keys: Vec<&str> = snapshot.candidates.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["src/a.rs::a_fn", "src/z.rs::z_fn"]);
    }

    #[test]
    fn incomplete_index_degrades_availability_and_widens_the_caveat() {
        let mut graph = CodeGraph::new();
        let dead = symbol("src/lib.rs", "unused_fn");
        add_symbol(&mut graph, &dead, true, 1);

        let snapshot = DeadSymbolFactProducer::default().produce(
            &graph,
            &DeadSymbolExclusionInputs::default(),
            false,
        );

        assert_eq!(snapshot.availability(), FactAvailability::Degraded);
        assert!(!snapshot.report.is_complete());
        let candidate = snapshot.candidate("src/lib.rs::unused_fn").unwrap();
        let text = candidate.describe(&snapshot.report);
        assert!(text.contains(NO_INDEXED_DEPENDENTS_PHRASE));
        assert!(text.contains("not confirmed"));
    }

    #[test]
    fn invalid_path_nodes_also_degrade_availability() {
        let mut graph = CodeGraph::new();
        let good = symbol("src/lib.rs", "unused_fn");
        add_symbol(&mut graph, &good, true, 1);
        // An absolute path is not canonical/repository-relative.
        let bad = symbol("/etc/passwd", "sneaky");
        add_symbol(&mut graph, &bad, true, 1);

        let snapshot = DeadSymbolFactProducer::default().produce(
            &graph,
            &DeadSymbolExclusionInputs::default(),
            true,
        );

        assert!(snapshot.report.invalid_path_nodes > 0);
        assert!(snapshot.report.is_degraded());
        assert_eq!(snapshot.availability(), FactAvailability::Degraded);
    }

    #[test]
    fn candidate_overflow_is_reported_and_truncates() {
        let mut graph = CodeGraph::new();
        for index in 0..5 {
            let sym = symbol(&format!("src/f{index}.rs"), "unused_fn");
            add_symbol(&mut graph, &sym, true, 1);
        }
        let producer = DeadSymbolFactProducer::new(DeadSymbolFactLimits { max_candidates: 2 });

        let snapshot = producer.produce(&graph, &DeadSymbolExclusionInputs::default(), true);

        assert_eq!(snapshot.candidates.len(), 2);
        assert_eq!(snapshot.report.candidates_observed, 5);
        assert!(snapshot.report.candidate_overflow);
        assert!(snapshot.report.is_degraded());
    }

    #[test]
    fn empty_snapshot_is_honest_about_unavailability() {
        let snapshot = DeadSymbolFactsSnapshot::empty();
        assert_eq!(snapshot.availability(), FactAvailability::Unavailable);
        assert!(snapshot.candidates.is_empty());
    }

    #[test]
    fn multiple_definitions_of_one_key_use_the_earliest_source_range() {
        let mut graph = CodeGraph::new();
        let later = SymbolId {
            file: "src/lib.rs".to_owned(),
            name: "overloaded".to_owned(),
            byte_offset: 200,
        };
        let earlier = SymbolId {
            file: "src/lib.rs".to_owned(),
            name: "overloaded".to_owned(),
            byte_offset: 5,
        };
        add_symbol(&mut graph, &later, true, 50);
        add_symbol(&mut graph, &earlier, true, 3);

        let snapshot = DeadSymbolFactProducer::default().produce(
            &graph,
            &DeadSymbolExclusionInputs::default(),
            true,
        );

        let candidate = snapshot.candidate("src/lib.rs::overloaded").unwrap();
        assert_eq!(candidate.definition_count, 2);
        assert_eq!(candidate.source_line, 3);
    }
}
