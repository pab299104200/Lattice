use std::collections::HashMap;

use crate::graph::CodeGraph;
use crate::identity::{DocId, Identity, IdentityResolver};
use crate::parser::parse_file;
use crate::retrieval_v1::anchors::{
    extract_anchors, resolve_anchors, AnchorDiagnostic, AnchorKind, AnchorResolution,
};
use crate::retrieval_v1::intent::classify_intent;
use crate::storage::graph_store::{
    FileIndexEntry, FILE_INDEX_PARSER_VERSION, FILE_INDEX_SCHEMA_VERSION,
};
use crate::symbols::ParsedFile;

const WORKSPACE: &str = "main";

#[test]
fn detects_every_anchor_variant_from_canonical_inputs() {
    let task = "\
Debug `src/auth.rs::login_user()` because thread 'main' panicked at src/auth.rs:12:5\n\
Run `rg login_user daemon/crates/lattice-core/src/retrieval_v1/anchors.rs`\n\
Call the MCP tool prepare_change before GET /v1/workspaces\n\
Set LATTICE_INDEX_ROOT and retrieval_v1.anchor.limit\n\
Update `docs/guide.md#Retrieval Engine`\n";
    let anchors = extract_anchors(task, &classify_intent(task));
    let kinds = anchors.iter().map(|anchor| anchor.kind).collect::<Vec<_>>();

    assert!(kinds.contains(&AnchorKind::Path));
    assert!(kinds.contains(&AnchorKind::Symbol));
    assert!(kinds.contains(&AnchorKind::Error));
    assert!(kinds.contains(&AnchorKind::Command));
    assert!(kinds.contains(&AnchorKind::Api));
    assert!(kinds.contains(&AnchorKind::ConfigKey));
}

#[test]
fn debug_intent_surfaces_error_anchors_first() {
    let task =
        "Debug login_user because error[E0425]: cannot find value `login_user` in this scope";
    let anchors = extract_anchors(task, &classify_intent(task));

    assert_eq!(
        anchors.first().map(|anchor| anchor.kind),
        Some(AnchorKind::Error)
    );
}

#[test]
fn ambiguous_symbol_names_return_candidate_set_and_reason() {
    let fixture = fixture_builder()
        .rust(WORKSPACE, "src/auth.rs", "pub fn shared() {}\n")
        .rust(WORKSPACE, "src/admin.rs", "pub fn shared() {}\n")
        .build();
    let task = "Refactor shared to reduce branching";
    let resolved = resolve_anchors(
        extract_anchors(task, &classify_intent(task)),
        &fixture.resolver,
    );
    let shared = resolved
        .into_iter()
        .find(|anchor| anchor.anchor_text == "shared")
        .expect("shared symbol anchor");

    match shared.resolution {
        AnchorResolution::Ambiguous { candidates, reason } => {
            assert_eq!(candidates.len(), 2);
            assert!(reason.contains("file path"));
        }
        other => panic!("expected ambiguous symbol resolution, got {other:?}"),
    }
}

#[test]
fn unknown_paths_resolve_to_unresolved_without_panicking() {
    let fixture = fixture_builder()
        .rust(WORKSPACE, "src/auth.rs", "pub fn login_user() {}\n")
        .build();
    let task = "Investigate missing file `src/missing.rs`";
    let resolved = resolve_anchors(
        extract_anchors(task, &classify_intent(task)),
        &fixture.resolver,
    );
    let path = resolved
        .into_iter()
        .find(|anchor| anchor.kind == AnchorKind::Path)
        .expect("path anchor");

    match path.resolution {
        AnchorResolution::Unresolved { reason } => {
            assert!(reason.contains("no file identity found"));
        }
        other => panic!("expected unresolved path anchor, got {other:?}"),
    }
}

#[test]
fn resolver_round_trip_preserves_source_spans() {
    let fixture = fixture_builder()
        .rust(WORKSPACE, "src/auth.rs", "pub fn login_user() {}\n")
        .build();
    let task = "Modify `src/auth.rs::login_user()` now";
    let anchors = extract_anchors(task, &classify_intent(task));
    let original_span = anchors
        .iter()
        .find(|anchor| anchor.anchor_text.contains("src/auth.rs::login_user"))
        .map(|anchor| anchor.source_span)
        .expect("symbol span");
    let resolved = resolve_anchors(anchors, &fixture.resolver);
    let resolved_span = resolved
        .iter()
        .find(|anchor| anchor.anchor_text.contains("src/auth.rs::login_user"))
        .map(|anchor| anchor.source_span)
        .expect("resolved span");

    assert_eq!(original_span, resolved_span);
}

#[test]
fn diagnostics_round_trip_through_serde() {
    let fixture = fixture_builder()
        .rust(WORKSPACE, "src/auth.rs", "pub fn login_user() {}\n")
        .markdown(
            WORKSPACE,
            "docs/guide.md",
            "# Guide\n\n## Retrieval Engine\nText\n",
        )
        .build();
    let task = "Update `docs/guide.md#Retrieval Engine` and `src/auth.rs::login_user()`";
    let resolved = resolve_anchors(
        extract_anchors(task, &classify_intent(task)),
        &fixture.resolver,
    );
    let diagnostics = resolved
        .iter()
        .map(|anchor| anchor.diagnostics())
        .collect::<Vec<AnchorDiagnostic>>();

    let serialized = serde_json::to_string(&diagnostics).expect("serialize diagnostics");
    let restored: Vec<AnchorDiagnostic> =
        serde_json::from_str(&serialized).expect("deserialize diagnostics");

    assert_eq!(diagnostics, restored);
}

#[test]
fn empty_input_returns_empty_vector() {
    let intent = classify_intent("");
    let extracted = extract_anchors("", &intent);
    assert!(extracted.is_empty());

    let fixture = fixture_builder().build();
    let resolved = resolve_anchors(extracted, &fixture.resolver);
    assert!(resolved.is_empty());
}

#[test]
fn command_api_config_and_error_anchors_can_resolve_via_existing_identities() {
    let fixture = fixture_builder()
        .rust(
            WORKSPACE,
            "src/tools.rs",
            "pub fn prepare_change() {}\npub const LATTICE_INDEX_ROOT: &str = \"/tmp\";\n",
        )
        .rust(WORKSPACE, "src/auth.rs", "pub fn login_user() {}\n")
        .rust(
            WORKSPACE,
            "daemon/crates/lattice-core/src/retrieval_v1/anchors.rs",
            "pub fn anchor_fixture_target() {}\n",
        )
        .build();
    let task = "\
thread 'main' panicked at src/auth.rs:12:5\n\
rg login_user daemon/crates/lattice-core/src/retrieval_v1/anchors.rs\n\
Call the MCP tool prepare_change\n\
Set LATTICE_INDEX_ROOT\n";
    let resolved = resolve_anchors(
        extract_anchors(task, &classify_intent(task)),
        &fixture.resolver,
    );

    assert_resolved_kind(&resolved, AnchorKind::Error);
    assert_resolved_kind(&resolved, AnchorKind::Command);
    assert_resolved_kind(&resolved, AnchorKind::Api);
    assert_resolved_kind(&resolved, AnchorKind::ConfigKey);
}

fn assert_resolved_kind(
    resolved: &[crate::retrieval_v1::anchors::ResolvedAnchor],
    kind: AnchorKind,
) {
    let anchor = resolved
        .iter()
        .find(|anchor| anchor.kind == kind)
        .unwrap_or_else(|| panic!("missing {kind:?} anchor"));
    match &anchor.resolution {
        AnchorResolution::Resolved(Identity::File(_))
        | AnchorResolution::Resolved(Identity::Symbol(_))
        | AnchorResolution::Resolved(Identity::Section(_)) => {}
        other => panic!("expected resolved {kind:?} anchor, got {other:?}"),
    }
}

struct ResolverFixture {
    resolver: IdentityResolver<'static>,
    #[allow(dead_code)]
    docs: HashMap<(String, String), DocId>,
}

struct FixtureBuilder {
    files: Vec<FixtureFile>,
}

struct FixtureFile {
    workspace: String,
    repo_relative_path: String,
    source: String,
}

fn fixture_builder() -> FixtureBuilder {
    FixtureBuilder { files: Vec::new() }
}

impl FixtureBuilder {
    fn rust(mut self, workspace: &str, path: &str, source: &str) -> Self {
        self.files.push(FixtureFile::new(workspace, path, source));
        self
    }

    fn markdown(mut self, workspace: &str, path: &str, source: &str) -> Self {
        self.files.push(FixtureFile::new(workspace, path, source));
        self
    }

    fn build(self) -> ResolverFixture {
        let mut graph = CodeGraph::new();
        let mut file_index = HashMap::new();
        let mut parsed_files = HashMap::new();
        let mut docs = HashMap::new();

        for file in self.files {
            let stored_path = file.stored_path();
            let parsed_file = parse_file(&stored_path, &file.source).expect("fixture parses");
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
            WORKSPACE.to_string(),
            Vec::new(),
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

fn content_hash(source: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in source.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
