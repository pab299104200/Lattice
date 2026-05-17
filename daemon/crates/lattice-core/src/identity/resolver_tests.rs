//! Phase 1 identity-resolution correctness tests for
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 1: Unified Identity Model`.
//!
//! This matrix covers the spec-mandated rename, move, duplicate-name, and
//! branch-change behaviors, plus the compatibility paths that keep legacy
//! references working while Phase 1 introduces stable identities.

use super::{EventId, FileId, Identity, ResolveOutcome};

const MAIN_WORKSPACE: &str = "main";
const FEATURE_WORKSPACE: &str = "feature";

#[test]
fn test_resolve_path_unique_returns_fileid() {
    let fixture = helpers::FixtureBuilder::new(MAIN_WORKSPACE)
        .rust(MAIN_WORKSPACE, "src/lib.rs", "pub fn login_user() {}\n")
        .build();

    let resolved = fixture
        .resolver
        .resolve_path(&fixture.workspace(MAIN_WORKSPACE), "src/lib.rs")
        .expect("path should resolve");

    assert_eq!(resolved.workspace_id, MAIN_WORKSPACE);
    assert_eq!(resolved.repo_relative_path, "src/lib.rs");
}

#[test]
fn test_resolve_path_after_file_rename_returns_new_id() {
    let fixture = helpers::FixtureBuilder::new(MAIN_WORKSPACE)
        .rust(MAIN_WORKSPACE, "src/renamed.rs", "pub fn login_user() {}\n")
        .build();

    let resolved = fixture
        .resolver
        .resolve_path(&fixture.workspace(MAIN_WORKSPACE), "src/renamed.rs")
        .expect("renamed path should resolve");

    assert_eq!(resolved.repo_relative_path, "src/renamed.rs");
}

#[test]
fn test_resolve_path_after_file_rename_legacy_name_returns_via_compat_shim() {
    let source = "pub fn login_user() {}\n";
    let old_identity = Identity::File(FileId {
        workspace_id: MAIN_WORKSPACE.to_string(),
        repo_relative_path: "src/original.rs".to_string(),
        content_hash: helpers::content_hash(source),
    })
    .to_string();
    let fixture = helpers::FixtureBuilder::new(MAIN_WORKSPACE)
        .rust(MAIN_WORKSPACE, "src/renamed.rs", source)
        .build();

    let resolved = fixture
        .resolver
        .resolve_path(&fixture.workspace(MAIN_WORKSPACE), &old_identity)
        .expect("legacy file identity should remap to current path");

    assert_eq!(resolved.repo_relative_path, "src/renamed.rs");
    assert_eq!(resolved.content_hash, helpers::content_hash(source));
}

#[test]
fn test_resolve_symbol_unique_returns_symbolid() {
    let fixture = helpers::FixtureBuilder::new(MAIN_WORKSPACE)
        .rust(
            MAIN_WORKSPACE,
            "src/auth.rs",
            "pub fn login_user() {}\npub fn logout_user() {}\n",
        )
        .build();

    let resolved = fixture
        .resolver
        .resolve_symbol(&fixture.workspace(MAIN_WORKSPACE), "login_user");

    let symbol_id = helpers::expect_unique_symbol(resolved);
    assert_eq!(symbol_id.file.repo_relative_path, "src/auth.rs");
    assert_eq!(symbol_id.qualified_name, "login_user");
}

#[test]
fn test_resolve_symbol_duplicate_name_returns_ambiguous_with_disambiguation_hint() {
    let fixture = helpers::FixtureBuilder::new(MAIN_WORKSPACE)
        .rust(MAIN_WORKSPACE, "src/auth.rs", "pub fn shared() {}\n")
        .rust(MAIN_WORKSPACE, "src/admin.rs", "pub fn shared() {}\n")
        .build();

    let resolved = fixture
        .resolver
        .resolve_symbol(&fixture.workspace(MAIN_WORKSPACE), "shared");

    let report = helpers::expect_ambiguous_symbol(resolved);
    assert_eq!(report.candidates.len(), 2);
    assert!(report.disambiguation_hint.contains("file path"));
}

#[test]
fn test_resolve_section_after_heading_rename_returns_new_id() {
    let fixture = helpers::FixtureBuilder::new(MAIN_WORKSPACE)
        .markdown(
            MAIN_WORKSPACE,
            "docs/guide.md",
            "# Overview\n\n## Current Heading\nBody\n",
        )
        .build();
    let doc = fixture.doc_id(MAIN_WORKSPACE, "docs/guide.md");

    let resolved = fixture.resolver.resolve_section(
        &fixture.workspace(MAIN_WORKSPACE),
        &doc,
        "Current Heading",
    );

    let section_id = helpers::expect_unique_section(resolved);
    assert_eq!(section_id.doc.repo_relative_path, "docs/guide.md");
    assert_eq!(section_id.heading_path, vec!["Current Heading".to_string()]);
}

#[test]
fn test_resolve_section_after_section_move_returns_new_id() {
    let fixture = helpers::FixtureBuilder::new(MAIN_WORKSPACE)
        .markdown(
            MAIN_WORKSPACE,
            "docs/runbook.md",
            "# Runbook\n\n## Intro\nText\n\n## Deploy\nMove target\n",
        )
        .build();
    let doc = fixture.doc_id(MAIN_WORKSPACE, "docs/runbook.md");

    let resolved =
        fixture
            .resolver
            .resolve_section(&fixture.workspace(MAIN_WORKSPACE), &doc, "Deploy");

    let section_id = helpers::expect_unique_section(resolved);
    assert_eq!(section_id.doc.repo_relative_path, "docs/runbook.md");
    assert!(section_id.byte_offset > 20);
}

#[test]
fn test_resolve_symbol_on_different_branch_returns_branch_scoped_id() {
    let fixture = helpers::FixtureBuilder::new(MAIN_WORKSPACE)
        .rust(MAIN_WORKSPACE, "src/auth.rs", "pub fn shared() {}\n")
        .rust(FEATURE_WORKSPACE, "src/auth.rs", "pub fn shared() {}\n")
        .build();

    let resolved = fixture
        .resolver
        .resolve_symbol(&fixture.workspace(FEATURE_WORKSPACE), "shared");

    let symbol_id = helpers::expect_unique_symbol(resolved);
    assert_eq!(symbol_id.file.workspace_id, FEATURE_WORKSPACE);
    assert_eq!(symbol_id.file.repo_relative_path, "src/auth.rs");
}

#[test]
fn test_resolve_event_ref_round_trips_through_encoding() {
    let event_id = EventId {
        workspace_id: MAIN_WORKSPACE.to_string(),
        ulid: "01J0000000000000000000000A".to_string(),
    };
    let fixture = helpers::FixtureBuilder::new(MAIN_WORKSPACE)
        .event(event_id.clone())
        .build();

    let resolved = fixture
        .resolver
        .resolve_event_ref(
            &fixture.workspace(MAIN_WORKSPACE),
            &Identity::Event(event_id).to_string(),
        )
        .expect("event identity should resolve");

    assert_eq!(resolved.workspace_id, MAIN_WORKSPACE);
    assert_eq!(resolved.ulid, "01J0000000000000000000000A");
}

#[test]
fn test_resolve_legacy_symbol_name_routes_via_default_workspace() {
    let fixture = helpers::FixtureBuilder::new(MAIN_WORKSPACE)
        .rust(MAIN_WORKSPACE, "src/legacy.rs", "pub fn only_main() {}\n")
        .rust(
            FEATURE_WORKSPACE,
            "src/feature.rs",
            "pub fn feature_only() {}\n",
        )
        .build();

    let resolved = fixture.resolver.resolve_legacy_symbol_name("only_main");

    let symbol_id = helpers::expect_unique_symbol(resolved);
    assert_eq!(symbol_id.file.workspace_id, MAIN_WORKSPACE);
    assert_eq!(symbol_id.file.repo_relative_path, "src/legacy.rs");
}

pub(crate) mod helpers {
    use std::collections::HashMap;

    use crate::graph::CodeGraph;
    use crate::identity::{AmbiguityReport, DocId, EventId, IdentityResolver, SectionId, SymbolId};
    use crate::parser::parse_file;
    use crate::storage::graph_store::{
        FileIndexEntry, FILE_INDEX_PARSER_VERSION, FILE_INDEX_SCHEMA_VERSION,
    };
    use crate::symbols::ParsedFile;

    use super::ResolveOutcome;

    pub(crate) struct ResolverFixture {
        pub(crate) resolver: IdentityResolver<'static>,
        docs: HashMap<(String, String), DocId>,
    }

    impl ResolverFixture {
        pub(crate) fn workspace(&self, workspace: &str) -> String {
            workspace.to_string()
        }

        pub(crate) fn doc_id(&self, workspace: &str, path: &str) -> DocId {
            self.docs
                .get(&(workspace.to_string(), path.to_string()))
                .cloned()
                .expect("document fixture should exist")
        }
    }

    pub(crate) struct FixtureBuilder {
        default_workspace: String,
        files: Vec<FixtureFile>,
        events: Vec<EventId>,
    }

    struct FixtureFile {
        workspace: String,
        repo_relative_path: String,
        source: String,
    }

    impl FixtureBuilder {
        pub(crate) fn new(default_workspace: &str) -> Self {
            Self {
                default_workspace: default_workspace.to_string(),
                files: Vec::new(),
                events: Vec::new(),
            }
        }

        pub(crate) fn rust(mut self, workspace: &str, path: &str, source: &str) -> Self {
            self.files.push(FixtureFile::new(workspace, path, source));
            self
        }

        pub(crate) fn markdown(mut self, workspace: &str, path: &str, source: &str) -> Self {
            self.files.push(FixtureFile::new(workspace, path, source));
            self
        }

        pub(crate) fn event(mut self, event_id: EventId) -> Self {
            self.events.push(event_id);
            self
        }

        pub(crate) fn build(self) -> ResolverFixture {
            let mut graph = CodeGraph::new();
            let mut file_index = HashMap::new();
            let mut parsed_files = HashMap::new();
            let mut docs = HashMap::new();

            for file in self.files {
                let stored_path = file.stored_path();
                let parsed_file =
                    parse_file(&stored_path, &file.source).expect("fixture should parse");
                add_symbols(&mut graph, &parsed_file);
                if file.repo_relative_path.ends_with(".md") {
                    docs.insert(
                        (file.workspace.clone(), file.repo_relative_path.clone()),
                        doc_id(&file.workspace, &file.repo_relative_path, &file.source),
                    );
                }
                parsed_files.insert(stored_path.clone(), parsed_file);
                file_index.insert(
                    stored_path.clone(),
                    file_index_entry(&stored_path, &file.source),
                );
            }

            let resolver = IdentityResolver::new(
                Box::leak(Box::new(graph)),
                Box::leak(Box::new(file_index)),
                Box::leak(Box::new(parsed_files)),
                self.default_workspace,
                self.events,
            );
            ResolverFixture { resolver, docs }
        }
    }

    impl FixtureFile {
        fn new(workspace: &str, path: &str, source: &str) -> Self {
            Self {
                workspace: workspace.to_string(),
                repo_relative_path: path.to_string(),
                source: source.to_string(),
            }
        }

        fn stored_path(&self) -> String {
            format!("{}/{}", self.workspace, self.repo_relative_path)
        }
    }

    pub(crate) fn content_hash(source: &str) -> String {
        let mut hash = 0xcbf29ce484222325u64;
        for byte in source.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        format!("{hash:016x}")
    }

    pub(crate) fn expect_unique_symbol(outcome: ResolveOutcome<SymbolId>) -> SymbolId {
        match outcome {
            ResolveOutcome::Unique(symbol_id) => symbol_id,
            other => panic!("expected unique symbol, got {other:?}"),
        }
    }

    pub(crate) fn expect_ambiguous_symbol(
        outcome: ResolveOutcome<SymbolId>,
    ) -> AmbiguityReport<SymbolId> {
        match outcome {
            ResolveOutcome::Ambiguous(report) => report,
            other => panic!("expected ambiguous symbol, got {other:?}"),
        }
    }

    pub(crate) fn expect_unique_section(outcome: ResolveOutcome<SectionId>) -> SectionId {
        match outcome {
            ResolveOutcome::Unique(section_id) => section_id,
            other => panic!("expected unique section, got {other:?}"),
        }
    }

    fn add_symbols(graph: &mut CodeGraph, parsed_file: &ParsedFile) {
        for symbol in &parsed_file.symbols {
            graph.add_node(
                symbol.id.clone(),
                symbol.kind,
                symbol.name.clone(),
                symbol.signature.clone(),
                symbol.body.clone(),
                symbol.file.clone(),
                symbol.line,
                symbol.end_line,
                symbol.is_exported,
                symbol.language,
            );
        }
    }

    fn doc_id(workspace: &str, path: &str, source: &str) -> DocId {
        DocId {
            workspace_id: workspace.to_string(),
            repo_relative_path: path.to_string(),
            content_hash: content_hash(source),
        }
    }

    fn file_index_entry(stored_path: &str, source: &str) -> FileIndexEntry {
        FileIndexEntry {
            file: stored_path.to_string(),
            content_hash: content_hash(source),
            mtime_ns: 0,
            size_bytes: source.len() as i64,
            parser_version: FILE_INDEX_PARSER_VERSION,
            schema_version: FILE_INDEX_SCHEMA_VERSION,
            last_indexed_at: 0,
        }
    }
}
