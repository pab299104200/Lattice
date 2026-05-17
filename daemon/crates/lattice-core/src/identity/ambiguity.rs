//! Ambiguity reporting for Phase 1 identity resolution.
//!
//! This module implements the ambiguity contract required by
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 1: Unified Identity Model`: ambiguous references must return
//! diagnostics instead of an arbitrary match.

use std::fmt;

/// Identity resolution result that preserves ambiguity and not-found states.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveOutcome<T> {
    Unique(T),
    Ambiguous(AmbiguityReport<T>),
    NotFound(super::resolver::ResolveError),
}

/// A structured ambiguity response for callers that need to disambiguate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmbiguityReport<T> {
    pub query: String,
    pub candidates: Vec<T>,
    pub disambiguation_hint: String,
}

impl<T> AmbiguityReport<T> {
    pub fn new(
        query: impl Into<String>,
        candidates: Vec<T>,
        disambiguation_hint: impl Into<String>,
    ) -> Self {
        Self {
            query: query.into(),
            candidates,
            disambiguation_hint: disambiguation_hint.into(),
        }
    }
}

impl<T> fmt::Display for AmbiguityReport<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} matches found for `{}`; {}",
            self.candidates.len(),
            self.query,
            self.disambiguation_hint
        )
    }
}

pub fn symbol_disambiguation_hint(has_multiple_files: bool) -> String {
    if has_multiple_files {
        "Add the parent module or file path, or use the stable symbol identity.".to_string()
    } else {
        "Add the byte offset or use the stable symbol identity.".to_string()
    }
}

pub fn section_disambiguation_hint() -> String {
    "Add the full heading path, byte offset, or stable section identity.".to_string()
}

pub fn test_disambiguation_hint() -> String {
    "Add the test file path or use the stable symbol identity.".to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        section_disambiguation_hint, symbol_disambiguation_hint, test_disambiguation_hint,
        AmbiguityReport,
    };

    #[test]
    fn display_reports_candidate_count_and_hint() {
        let report = AmbiguityReport::new(
            "render",
            vec!["src/a.rs::render", "src/b.rs::render"],
            symbol_disambiguation_hint(true),
        );

        let rendered = report.to_string();

        assert!(rendered.contains("2 matches found"));
        assert!(rendered.contains("render"));
        assert!(rendered.contains("file path"));
    }

    #[test]
    fn hint_helpers_match_expected_resolution_guidance() {
        assert!(symbol_disambiguation_hint(true).contains("file path"));
        assert!(symbol_disambiguation_hint(false).contains("byte offset"));
        assert!(section_disambiguation_hint().contains("heading path"));
        assert!(test_disambiguation_hint().contains("test file path"));
    }
}
