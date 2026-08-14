//! Language-contributed vocabulary for complexity fact production.
//!
//! Each language parser owns the tree-sitter node kinds that describe its own
//! grammar (`parser/<language>.rs::complexity_profile`). The generic walker in
//! [`crate::health::complexity_facts`] consumes these profiles and never hard
//! codes a grammar-specific kind, so adding a language is a contained addition
//! to that language's parser module.
//!
//! Node kinds recorded in each profile were verified against the grammars this
//! crate depends on by dumping parse trees for representative samples
//! (`cargo run -p lattice-core --example dump_tree`). Do not change a kind
//! without re-verifying it against the grammar in use.

use tree_sitter::Node;

use crate::symbols::Language;

/// Identity of a complexity unit (a named function or method).
///
/// `byte_offset` is deliberately the same offset the language parser uses for
/// the corresponding [`crate::symbols::SymbolId`], so complexity facts join to
/// graph symbols on `(file, byte_offset)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitIdentity {
    pub name: String,
    pub byte_offset: usize,
}

/// Whether a language participates in complexity fact production at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileApplicability {
    /// The language has functions and control flow; facts are produced.
    Supported,
    /// The language has no executable control flow (documented per language).
    /// Facts are reported as unavailable rather than as a fabricated zero.
    NotApplicable {
        /// Human-readable justification surfaced in the fact payload.
        reason: &'static str,
    },
}

/// A node kind that counts as a decision point only when a named field is present
/// (for example a Rust `match` arm guard: `match_pattern` with a `condition`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardedKind {
    pub kind: &'static str,
    pub field: &'static str,
}

/// Language hook deciding whether a branch-kind node is an unconditional default.
pub type DefaultBranchHook = fn(Node, &[u8]) -> bool;

/// Language hook counting a function's declared parameters.
pub type ParameterCountHook = fn(Node, &[u8]) -> Option<u32>;

/// Language hook naming and keying a function unit.
pub type UnitIdentityHook = fn(Node, &[u8]) -> Option<UnitIdentity>;

/// The per-language node-kind sets used to compute complexity facts.
#[derive(Debug, Clone, Copy)]
pub struct LanguageComplexityProfile {
    pub language: Language,
    pub applicability: ProfileApplicability,
    /// Node kinds that introduce a complexity unit (function/method/closure).
    pub function_kinds: &'static [&'static str],
    /// Node kinds that are decision points on their own.
    pub branch_kinds: &'static [&'static str],
    /// Parent node kinds that may carry a short-circuiting operator.
    pub boolean_operator_parent_kinds: &'static [&'static str],
    /// Operator token kinds that short-circuit and therefore branch.
    pub boolean_operator_kinds: &'static [&'static str],
    /// Kinds counted only when the named field is present.
    pub guarded_kinds: &'static [GuardedKind],
    /// Node kinds that increase structural nesting depth.
    pub nesting_kinds: &'static [&'static str],
    /// Parent kinds through which a nesting node does *not* deepen nesting, so
    /// that an `else if` chain reads as one level rather than N.
    pub nesting_transparent_parent_kinds: &'static [&'static str],
    /// Parent field names through which a nesting node does not deepen nesting
    /// (grammars that attach `else if` directly as the `alternative` field).
    pub nesting_transparent_fields: &'static [&'static str],
    /// Field name holding a function's parameter list.
    pub parameter_list_field: &'static str,
    /// Node kinds inside the parameter list that count as one declared parameter.
    pub parameter_kinds: &'static [&'static str],
    /// Language hook: is this branch-kind node an unconditional default arm?
    /// Defaults (`default:`, `_ =>`, `case _:`) are the structural `else` of a
    /// dispatch and are not decision points under the McCabe convention.
    pub is_default_branch: Option<DefaultBranchHook>,
    /// Language hook: full override of parameter counting, given the function node.
    pub count_parameters: Option<ParameterCountHook>,
    /// Language hook: naming/keying override, given the function node.
    /// Returning `None` means the node is anonymous: its contents are attributed
    /// to the enclosing unit instead of forming a unit of its own.
    pub unit_identity: Option<UnitIdentityHook>,
}

impl LanguageComplexityProfile {
    /// Whether facts can be produced for this language.
    pub fn is_supported(&self) -> bool {
        matches!(self.applicability, ProfileApplicability::Supported)
    }

    /// The documented reason a language is exempt, when it is.
    pub fn not_applicable_reason(&self) -> Option<&'static str> {
        match self.applicability {
            ProfileApplicability::Supported => None,
            ProfileApplicability::NotApplicable { reason } => Some(reason),
        }
    }

    /// Default unit identity: the `name` field of the node, keyed at its start byte.
    pub fn default_identity(node: Node, source: &[u8]) -> Option<UnitIdentity> {
        let name = node.child_by_field_name("name")?;
        Some(UnitIdentity {
            name: node_text(name, source),
            byte_offset: node.start_byte(),
        })
    }
}

/// Text of a node, empty when the range is not valid UTF-8.
pub fn node_text(node: Node, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or("").to_string()
}

/// Name of the nearest ancestor of one of `owner_kinds`, used to qualify methods
/// the same way the language parsers qualify them (`Owner.method`).
pub fn enclosing_owner_name(
    node: Node,
    source: &[u8],
    owner_kinds: &[&str],
    name_field: &str,
) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if owner_kinds.contains(&parent.kind()) {
            if let Some(name) = parent.child_by_field_name(name_field) {
                return Some(node_text(name, source));
            }
            return None;
        }
        current = parent.parent();
    }
    None
}

/// Count named children of the parameter list whose kind is a declared parameter.
pub fn count_parameter_kinds(
    node: Node,
    parameter_list_field: &str,
    parameter_kinds: &[&str],
) -> Option<u32> {
    let list = node.child_by_field_name(parameter_list_field)?;
    let mut cursor = list.walk();
    let count = list
        .named_children(&mut cursor)
        .filter(|child| parameter_kinds.contains(&child.kind()))
        .count();
    Some(count as u32)
}
