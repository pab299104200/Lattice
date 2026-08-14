//! Deterministic complexity facts per function and per file.
//!
//! This is a pure producer: `(path, source) -> facts`. It parses with the same
//! grammar the symbol extractor uses and walks it with the node-kind vocabulary
//! each language parser contributes
//! ([`crate::parser::complexity_profile`]), so no grammar-specific knowledge
//! lives here and adding a language is a contained change in that language's
//! parser module.
//!
//! # Conventions
//!
//! * **Cyclomatic complexity** is the McCabe approximation
//!   `complexity = 1 + decision_points`. A function with no control flow scores
//!   1; a function with three independent `if`s scores 4. Decision points are:
//!   conditionals, loops, each non-default `switch`/`match` arm, exception
//!   handlers, guards, ternaries, short-circuiting boolean operators, and (in
//!   Rust) the `?` operator. A bare `else` and an explicit `default`/`_` arm are
//!   the structural fall-through of a construct already counted, so they add
//!   nothing — the standard convention.
//! * **Nesting depth** counts control-flow constructs enclosing the deepest
//!   point of the function body. A body with no control flow is depth 0; one
//!   `if` is depth 1. An `else if` chain stays at one level (it is a sibling
//!   branch, not deeper code).
//! * **Function length** is `end_line - line + 1`, inclusive of the signature
//!   and closing delimiter lines.
//! * **Parameter count** is read from the AST parameter list, excluding
//!   receivers (`self`, `cls`, Go method receivers), which are not declared
//!   parameters of the call.
//! * **Anonymous functions** (closures, inline callbacks) are not units of their
//!   own: their decision points and nesting are attributed to the named
//!   function that encloses them, so no code is counted twice or lost.
//! * **Nested named functions** are units of their own, and their contents are
//!   excluded from the enclosing unit for the same reason.
//!
//! # Unknown is never zero
//!
//! An unsupported language, an absent grammar, a parse failure, or a language
//! with no executable control flow yields
//! [`FactAvailability::Unavailable`] with a stated reason and *no* rollup — never
//! a fabricated complexity of zero. A partially broken parse yields
//! [`FactAvailability::Degraded`] on the file and on each affected function, and
//! a function whose parameter list cannot be read reports
//! `param_count: None` rather than 0.
//!
//! # Joining to symbols
//!
//! Each unit is keyed by `(file, byte_offset)` where `byte_offset` is the same
//! offset the language parser assigns to the corresponding
//! [`crate::symbols::SymbolId`], and `symbol` matches that symbol's name
//! (including `Owner.method` qualification). Consumers join on that pair.

use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser};

use crate::health::config::{active_config, ComplexityThresholds, HealthConfig};
use crate::parser::complexity_profile::{count_parameter_kinds, LanguageComplexityProfile};
use crate::parser::{complexity_profile_for, tree_sitter_language};
use crate::symbols::Language;

/// Whether a fact family could be produced for a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactAvailability {
    /// Facts were produced from a complete, clean parse.
    Available,
    /// Facts were produced but at least one input was incomplete.
    Degraded,
    /// No facts could be produced; the reason is reported, never a zero.
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

/// Why complexity facts are unavailable or degraded for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComplexityUnavailableReason {
    /// The file extension maps to no parser in this crate.
    UnsupportedLanguage,
    /// The language is parsed but has no executable control flow (e.g. Markdown).
    NoExecutableControlFlow,
    /// The grammar refused the source outright.
    ParseFailure,
    /// The parse succeeded but contains error or missing nodes.
    PartialParse,
}

impl ComplexityUnavailableReason {
    /// Stable code used in storage and payloads.
    pub fn as_str(&self) -> &'static str {
        match self {
            ComplexityUnavailableReason::UnsupportedLanguage => "unsupported_language",
            ComplexityUnavailableReason::NoExecutableControlFlow => "no_executable_control_flow",
            ComplexityUnavailableReason::ParseFailure => "parse_failure",
            ComplexityUnavailableReason::PartialParse => "partial_parse",
        }
    }

    /// Decode a stored code into a reason.
    pub fn from_code(value: &str) -> Option<Self> {
        match value {
            "unsupported_language" => Some(ComplexityUnavailableReason::UnsupportedLanguage),
            "no_executable_control_flow" => {
                Some(ComplexityUnavailableReason::NoExecutableControlFlow)
            }
            "parse_failure" => Some(ComplexityUnavailableReason::ParseFailure),
            "partial_parse" => Some(ComplexityUnavailableReason::PartialParse),
            _ => None,
        }
    }
}

/// Complexity facts for a single named function or method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolComplexityFacts {
    /// Workspace-relative file path.
    pub file: String,
    /// Symbol name, qualified as the language parser qualifies it.
    pub symbol: String,
    /// Byte offset of the symbol, matching `SymbolId::byte_offset`.
    pub byte_offset: usize,
    /// 1-based first line of the function.
    pub line: usize,
    /// 1-based last line of the function.
    pub end_line: usize,
    /// Lines spanned, inclusive.
    pub function_length: u32,
    /// Decision points counted in the body.
    pub branch_count: u32,
    /// `1 + branch_count`.
    pub cyclomatic_complexity: u32,
    /// Deepest control-flow nesting reached in the body.
    pub max_nesting_depth: u32,
    /// Declared parameters, or `None` when the parameter list could not be read.
    pub param_count: Option<u32>,
    /// Availability of this function's facts.
    pub availability: FactAvailability,
}

impl SymbolComplexityFacts {
    /// Whether this function exceeds any threshold in the active config.
    pub fn exceeds_any_threshold(&self, thresholds: &ComplexityThresholds) -> bool {
        self.cyclomatic_complexity > thresholds.high_cyclomatic_complexity
            || self.function_length > thresholds.high_function_length
            || self.max_nesting_depth > thresholds.high_nesting_depth
            || self
                .param_count
                .map(|count| count > thresholds.high_param_count)
                .unwrap_or(false)
    }
}

/// Per-file aggregate over the file's function facts.
///
/// Every extremum is `Option` so that a file with no functions reports "no
/// functions" instead of a fabricated zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileComplexityRollup {
    /// Number of function units found.
    pub function_count: u32,
    /// Functions whose parameter list could not be read.
    pub functions_with_unknown_param_count: u32,
    /// Highest cyclomatic complexity in the file.
    pub max_cyclomatic_complexity: Option<u32>,
    /// Nearest-rank percentile of cyclomatic complexity (percentile from config).
    pub p90_cyclomatic_complexity: Option<u32>,
    /// Longest function, in lines.
    pub max_function_length: Option<u32>,
    /// Nearest-rank percentile of function length.
    pub p90_function_length: Option<u32>,
    /// Deepest nesting in the file.
    pub max_nesting_depth: Option<u32>,
    /// Functions over the complexity threshold.
    pub functions_over_complexity_threshold: u32,
    /// Functions over the length threshold.
    pub functions_over_length_threshold: u32,
    /// Functions over the nesting threshold.
    pub functions_over_nesting_threshold: u32,
    /// Functions over the parameter-count threshold (known counts only).
    pub functions_over_param_threshold: u32,
    /// Share of functions over at least one threshold, in per-mille
    /// (truncating integer division).
    pub over_threshold_share_per_mille: u32,
    /// Mean cyclomatic complexity in per-mille (truncating integer division).
    pub mean_cyclomatic_complexity_per_mille: u32,
    /// Percentile used for the `p90_*` fields, in per-mille, echoed from config.
    pub percentile_per_mille: u32,
}

/// Complexity facts for one file: its function units plus the rollup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileComplexityFacts {
    /// Workspace-relative file path.
    pub file: String,
    /// Language the facts were produced under.
    pub language: Language,
    /// Availability of the file's facts.
    pub availability: FactAvailability,
    /// Why facts are missing or partial, when they are.
    pub unavailable_reason: Option<ComplexityUnavailableReason>,
    /// Human-readable justification for an exempt language.
    pub exemption_reason: Option<String>,
    /// Version of the health config the thresholds came from.
    pub config_version: u32,
    /// Per-function facts, ordered by byte offset.
    pub symbols: Vec<SymbolComplexityFacts>,
    /// Aggregate, absent when no facts could be produced.
    pub rollup: Option<FileComplexityRollup>,
}

impl FileComplexityFacts {
    /// A file for which no facts exist, carrying the reason.
    fn unavailable(
        file: &str,
        language: Language,
        reason: ComplexityUnavailableReason,
        exemption_reason: Option<String>,
        config_version: u32,
    ) -> Self {
        Self {
            file: file.to_string(),
            language,
            availability: FactAvailability::Unavailable,
            unavailable_reason: Some(reason),
            exemption_reason,
            config_version,
            symbols: Vec::new(),
            rollup: None,
        }
    }
}

/// Compute complexity facts for a file using the active health config.
pub fn compute_file_complexity_facts(file: &str, source: &str) -> FileComplexityFacts {
    compute_file_complexity_facts_with(file, source, active_config())
}

/// Compute complexity facts for a file under an explicit config version.
///
/// Deterministic: identical `(file, source, config)` always produce byte-identical
/// facts, with units ordered by byte offset.
pub fn compute_file_complexity_facts_with(
    file: &str,
    source: &str,
    config: &HealthConfig,
) -> FileComplexityFacts {
    let extension = file.rsplit('.').next().unwrap_or("");
    let language = Language::from_extension(extension);

    let profile = match complexity_profile_for(language) {
        Some(profile) => profile,
        None => {
            return FileComplexityFacts::unavailable(
                file,
                language,
                ComplexityUnavailableReason::UnsupportedLanguage,
                None,
                config.version,
            )
        }
    };

    if let Some(reason) = profile.not_applicable_reason() {
        return FileComplexityFacts::unavailable(
            file,
            language,
            ComplexityUnavailableReason::NoExecutableControlFlow,
            Some(reason.to_string()),
            config.version,
        );
    }

    let grammar = match tree_sitter_language(language) {
        Some(grammar) => grammar,
        None => {
            return FileComplexityFacts::unavailable(
                file,
                language,
                ComplexityUnavailableReason::UnsupportedLanguage,
                None,
                config.version,
            )
        }
    };

    let mut parser = Parser::new();
    if parser.set_language(&grammar).is_err() {
        return FileComplexityFacts::unavailable(
            file,
            language,
            ComplexityUnavailableReason::ParseFailure,
            None,
            config.version,
        );
    }

    let tree = match parser.parse(source, None) {
        Some(tree) => tree,
        None => {
            return FileComplexityFacts::unavailable(
                file,
                language,
                ComplexityUnavailableReason::ParseFailure,
                None,
                config.version,
            )
        }
    };

    let mut walker = UnitWalker {
        file,
        source: source.as_bytes(),
        profile,
        units: Vec::new(),
    };
    walker.visit(tree.root_node(), None, 0);

    let mut symbols = walker.units;
    symbols.sort_by_key(|unit| unit.byte_offset);

    let file_degraded = tree.root_node().has_error();
    let availability = if file_degraded {
        FactAvailability::Degraded
    } else {
        FactAvailability::Available
    };
    let unavailable_reason = if file_degraded {
        Some(ComplexityUnavailableReason::PartialParse)
    } else {
        None
    };

    let rollup = Some(roll_up(&symbols, language, config));

    FileComplexityFacts {
        file: file.to_string(),
        language,
        availability,
        unavailable_reason,
        exemption_reason: None,
        config_version: config.version,
        symbols,
        rollup,
    }
}

/// Aggregate per-function facts into the per-file rollup.
///
/// Pure and integer-only: percentiles use the nearest-rank method over the
/// ascending sorted values, shares are truncating per-mille divisions.
pub fn roll_up(
    symbols: &[SymbolComplexityFacts],
    language: Language,
    config: &HealthConfig,
) -> FileComplexityRollup {
    let thresholds = config.complexity_for(language);
    let percentile = config.rollup_percentile_per_mille;
    let count = symbols.len() as u32;

    let mut complexities: Vec<u32> = symbols
        .iter()
        .map(|unit| unit.cyclomatic_complexity)
        .collect();
    let mut lengths: Vec<u32> = symbols.iter().map(|unit| unit.function_length).collect();
    complexities.sort_unstable();
    lengths.sort_unstable();

    let complexity_total: u64 = complexities.iter().map(|value| *value as u64).sum();

    let over_complexity = symbols
        .iter()
        .filter(|unit| unit.cyclomatic_complexity > thresholds.high_cyclomatic_complexity)
        .count() as u32;
    let over_length = symbols
        .iter()
        .filter(|unit| unit.function_length > thresholds.high_function_length)
        .count() as u32;
    let over_nesting = symbols
        .iter()
        .filter(|unit| unit.max_nesting_depth > thresholds.high_nesting_depth)
        .count() as u32;
    let over_params = symbols
        .iter()
        .filter(|unit| {
            unit.param_count
                .map(|params| params > thresholds.high_param_count)
                .unwrap_or(false)
        })
        .count() as u32;
    let over_any = symbols
        .iter()
        .filter(|unit| unit.exceeds_any_threshold(thresholds))
        .count() as u32;
    let unknown_params = symbols
        .iter()
        .filter(|unit| unit.param_count.is_none())
        .count() as u32;

    let (over_threshold_share_per_mille, mean_per_mille) = if count == 0 {
        (0, 0)
    } else {
        (
            (over_any as u64 * 1000 / count as u64) as u32,
            (complexity_total * 1000 / count as u64) as u32,
        )
    };

    FileComplexityRollup {
        function_count: count,
        functions_with_unknown_param_count: unknown_params,
        max_cyclomatic_complexity: complexities.last().copied(),
        p90_cyclomatic_complexity: nearest_rank(&complexities, percentile),
        max_function_length: lengths.last().copied(),
        p90_function_length: nearest_rank(&lengths, percentile),
        max_nesting_depth: symbols.iter().map(|unit| unit.max_nesting_depth).max(),
        functions_over_complexity_threshold: over_complexity,
        functions_over_length_threshold: over_length,
        functions_over_nesting_threshold: over_nesting,
        functions_over_param_threshold: over_params,
        over_threshold_share_per_mille,
        mean_cyclomatic_complexity_per_mille: mean_per_mille,
        percentile_per_mille: percentile,
    }
}

/// Nearest-rank percentile over an ascending sorted slice, integer arithmetic
/// only: rank = ceil(percentile * n / 1000), clamped into range.
fn nearest_rank(sorted: &[u32], percentile_per_mille: u32) -> Option<u32> {
    if sorted.is_empty() {
        return None;
    }
    let n = sorted.len() as u64;
    let rank = (percentile_per_mille as u64 * n).div_ceil(1000).max(1);
    let index = (rank as usize - 1).min(sorted.len() - 1);
    Some(sorted[index])
}

/// Walks a parse tree, attributing decision points and nesting to the innermost
/// enclosing *named* function unit.
struct UnitWalker<'a> {
    file: &'a str,
    source: &'a [u8],
    profile: &'a LanguageComplexityProfile,
    units: Vec<SymbolComplexityFacts>,
}

impl<'a> UnitWalker<'a> {
    fn visit(&mut self, node: Node, current: Option<usize>, depth: u32) {
        if self.profile.function_kinds.contains(&node.kind()) {
            if let Some(index) = self.open_unit(node) {
                self.visit_children(node, Some(index), 0);
                return;
            }
            // Anonymous function: its body belongs to the enclosing unit.
        }

        let mut depth = depth;
        if let Some(index) = current {
            if node.is_error() || node.is_missing() {
                self.units[index].availability = FactAvailability::Degraded;
            }

            let decisions = self.decision_points(node);
            if decisions > 0 {
                let unit = &mut self.units[index];
                unit.branch_count += decisions;
                unit.cyclomatic_complexity = unit.branch_count + 1;
            }

            if self.profile.nesting_kinds.contains(&node.kind())
                && !self.is_nesting_transparent(node)
            {
                depth += 1;
                let unit = &mut self.units[index];
                if depth > unit.max_nesting_depth {
                    unit.max_nesting_depth = depth;
                }
            }
        }

        self.visit_children(node, current, depth);
    }

    fn visit_children(&mut self, node: Node, current: Option<usize>, depth: u32) {
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        for child in children {
            self.visit(child, current, depth);
        }
    }

    /// Start a unit for a named function node, returning its accumulator index.
    fn open_unit(&mut self, node: Node) -> Option<usize> {
        let identity = match self.profile.unit_identity {
            Some(hook) => hook(node, self.source)?,
            None => LanguageComplexityProfile::default_identity(node, self.source)?,
        };

        let param_count = match self.profile.count_parameters {
            Some(hook) => hook(node, self.source),
            None => count_parameter_kinds(
                node,
                self.profile.parameter_list_field,
                self.profile.parameter_kinds,
            ),
        };

        let line = node.start_position().row + 1;
        let end_line = node.end_position().row + 1;
        let availability = if param_count.is_none() {
            FactAvailability::Degraded
        } else {
            FactAvailability::Available
        };

        self.units.push(SymbolComplexityFacts {
            file: self.file.to_string(),
            symbol: identity.name,
            byte_offset: identity.byte_offset,
            line,
            end_line,
            function_length: (end_line.saturating_sub(line) + 1) as u32,
            branch_count: 0,
            cyclomatic_complexity: 1,
            max_nesting_depth: 0,
            param_count,
            availability,
        });
        Some(self.units.len() - 1)
    }

    /// Decision points contributed by this node alone.
    fn decision_points(&self, node: Node) -> u32 {
        let mut points = 0;

        if self.profile.branch_kinds.contains(&node.kind())
            && !self
                .profile
                .is_default_branch
                .map(|hook| hook(node, self.source))
                .unwrap_or(false)
        {
            points += 1;
        }

        if self
            .profile
            .boolean_operator_parent_kinds
            .contains(&node.kind())
        {
            let mut cursor = node.walk();
            points += node
                .children(&mut cursor)
                .filter(|child| self.profile.boolean_operator_kinds.contains(&child.kind()))
                .count() as u32;
        }

        for guarded in self.profile.guarded_kinds {
            if node.kind() == guarded.kind && node.child_by_field_name(guarded.field).is_some() {
                points += 1;
            }
        }

        points
    }

    /// Whether a nesting node continues its parent's level (an `else if`) rather
    /// than opening a deeper one.
    fn is_nesting_transparent(&self, node: Node) -> bool {
        let Some(parent) = node.parent() else {
            return false;
        };
        if self
            .profile
            .nesting_transparent_parent_kinds
            .contains(&parent.kind())
        {
            return true;
        }
        self.profile.nesting_transparent_fields.iter().any(|field| {
            parent
                .child_by_field_name(*field)
                .map(|child| child.id() == node.id())
                .unwrap_or(false)
        })
    }
}

#[cfg(test)]
#[path = "complexity_facts_tests.rs"]
mod tests;
