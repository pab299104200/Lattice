//! Phase 7 verification integration tests.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Non-Negotiable Product Properties`:
//! "Every stale or contradicted memory is surfaced as such, not hidden behind recency."
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Risks — Stale Memory Leakage`:
//! "Control: freshness indexes, graph-change-triggered verification, stale labels in all memory surfaces, and tests that stale memory cannot rank as trusted."
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Risks — Scope Leakage`:
//! "Control: scope-aware queries, enforced filters in store APIs, and negative tests."
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 7: Verification And Freshness` DoD:
//! "stale or contradicted memory cannot appear as normal trusted guidance".

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use tracing_subscriber::fmt::MakeWriter;

use super::{
    BundleSection, ExpiryScanner, IncrementalVerifier, ScopeFilter, SurfacingPipeline,
    VerificationObserver, WorkspaceFileReader,
};
use crate::consolidation::llm::{
    BudgetCatalog, ConsolidationMode, ContradictionCandidatePair, ContradictionDetectionJob,
    LlmDriver, LlmDriverError, LlmJobContext, LlmJobServices, LlmRequest, LlmResponse,
};
use crate::consolidation::{ConsolidationConfig, ConsolidationJobRuntime, ProposalDecision};
use crate::events::{BranchRef, EventStore, EventWriter, FlushPolicy};
use crate::graph::CodeGraph;
use crate::identity::{FileId, Identity, MemoryId, OperatorId};
use crate::memory::{
    EvidenceSpan, Memory, MemoryEvidence, MemoryScope, MemoryStore, MemoryStructuredFields,
    MemoryType, MemoryVerificationStatus,
};
use crate::retrieval_v1::{
    Candidate, CandidateSource, MemoryScoringMetadata, RankedCandidate, ScoringContext,
};
use crate::storage::graph_store::{
    FileIndexEntry, FILE_INDEX_PARSER_VERSION, FILE_INDEX_SCHEMA_VERSION,
};
use crate::symbols::{Language, ParsedFile};
use crate::{DateTime, Utc};

#[test]
fn test_stale_memory_never_appears_in_trusted_bundle() {
    let harness = VerificationHarness::new();
    let memory_id = harness.seed_span_memory("mem-stale", "src/auth.ts", "const FLAG = true;\n");
    let changed = harness.file_id("src/auth.ts", "hash-auth-v2");
    let report = harness.reverify_changed_file(
        &changed,
        "const FLAG = false;\n",
        ObservationRecorder::default(),
    );
    let bundle = harness.bundle_for_ids(&[memory_id.as_str()]);

    assert_eq!(report.stale, 1);
    assert_sections(&bundle, &memory_id, BundleSection::Stale);
}

#[test]
fn test_contradicted_memory_never_appears_in_trusted_bundle() {
    let mut harness = VerificationHarness::new();
    let contradicted_id = harness.seed_plain_memory("mem-b", "The build never uses cargo check.");
    let _contradictor_id =
        harness.seed_plain_memory("mem-a", "The build uses cargo check for verification.");

    harness.apply_contradiction("mem-b", "mem-a");
    let bundle = harness.bundle_for_ids(&[contradicted_id.as_str()]);

    assert_sections(&bundle, &contradicted_id, BundleSection::Contradicted);
}

#[test]
fn test_expired_memory_never_appears_in_trusted_bundle() {
    let harness = VerificationHarness::new();
    let memory_id = harness.seed_file_memory("mem-expired", "src/expiry.rs");
    harness
        .memory_store
        .set_expires_at(&memory_id, DateTime::from_unix_seconds(99))
        .expect("expiry stores");
    harness.scan_expiry(DateTime::from_unix_seconds(100));
    let bundle = harness.bundle_for_ids(&[memory_id.as_str()]);

    assert_sections(&bundle, &memory_id, BundleSection::Expired);
}

#[test]
fn test_invalidated_memory_never_appears_in_trusted_bundle() {
    let harness = VerificationHarness::new();
    let memory_id = harness.seed_file_memory("mem-invalidated", "src/delete-me.ts");
    harness.write_file("src/delete-me.ts", "export const alive = true;\n");
    harness.delete_file("src/delete-me.ts");
    let deleted = harness.file_id("src/delete-me.ts", "hash-delete-v2");
    let report = harness.reverify_deleted_file(&deleted, ObservationRecorder::default());
    let bundle = harness.bundle_for_ids(&[memory_id.as_str()]);

    assert_eq!(report.invalidated, 1);
    assert_sections(&bundle, &memory_id, BundleSection::Invalidated);
}

#[test]
fn test_scope_leak_blocked_at_store_boundary_for_all_scopes() {
    let harness = VerificationHarness::new();
    let logs = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufferWriter(logs.clone()))
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let ids = harness.seed_scope_leak_fixture();
    let results = harness.run_scope_leak_queries();
    let events = harness
        .memory_store
        .scope_filter_events()
        .expect("scope events load");
    let log_output = String::from_utf8(logs.lock().expect("log lock").clone()).expect("utf8");

    assert!(results.into_iter().all(|memory| memory.is_none()));
    assert_eq!(events.len(), 4);
    assert_eq!(
        events
            .iter()
            .map(|event| event.memory_id.as_str())
            .collect::<Vec<_>>(),
        ids.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(log_output.contains("scope_leak_blocked"));
}

#[test]
fn test_ranker_excludes_non_verified_from_top_n() {
    let harness = VerificationHarness::new();
    let ids = harness.seed_ranker_fixture();
    let ranked = ids
        .iter()
        .enumerate()
        .map(|(index, id)| ranked_memory(id, 100.0 - index as f32))
        .collect::<Vec<_>>();
    let ctx = harness.scoring_context_for(&ids);

    let trusted = SurfacingPipeline::trusted_ranked_top_n(ranked, &ctx, 5);

    assert_eq!(trusted.len(), 1);
    assert_eq!(
        trusted[0].candidate.identity,
        memory_identity("mem-verified")
    );
}

#[test]
fn test_incremental_verification_narrows_to_changed_files_only() {
    let harness = VerificationHarness::new();
    let files = harness.seed_many_file_memories(100);
    let changed = files[7].clone();
    let observer = ObservationRecorder::default();
    let report = harness.reverify_unchanged_content(&changed, &files, &observer);

    assert_eq!(report.impacted, 1);
    assert_eq!(observer.memory_ids(), vec!["mem-007"]);
}

#[test]
fn test_verifier_emits_explainable_label_reason_for_every_non_verified_state() {
    let harness = VerificationHarness::new();
    let ids = harness.seed_explainable_reason_fixture();
    let bundle = harness.bundle_for_ids(&ids.iter().map(String::as_str).collect::<Vec<_>>());

    assert_eq!(bundle.advisory.len(), 7);
    assert_reason_contains(
        &bundle,
        "mem-unverified",
        "scope mismatch workspace-main/main",
    );
    assert_reason_contains(&bundle, "mem-in-review", "file `src/review.rs`");
    assert_reason_contains(&bundle, "mem-stale-reason", "span byte range 0..18");
    assert_reason_contains(&bundle, "mem-contradicted-reason", "contradicted by mem-a");
    assert_reason_contains(&bundle, "mem-superseded-reason", "superseded by mem-newer");
    assert_reason_contains(&bundle, "mem-expired-reason", "expiry 2026-05-17T00:00:00Z");
    assert_reason_contains(&bundle, "mem-invalidated-reason", "file id src/deleted.rs");
}

struct VerificationHarness {
    _dir: tempfile::TempDir,
    workspace_root: PathBuf,
    runtime_db: PathBuf,
    memory_store: MemoryStore,
    _event_store: Arc<EventStore>,
    event_writer: EventWriter,
    driver: FakeDriver,
}

impl VerificationHarness {
    fn new() -> Self {
        let dir = tempdir().expect("tempdir");
        let event_store = Arc::new(EventStore::open_in_memory().expect("event store"));
        let event_writer =
            EventWriter::new(event_store.clone(), "workspace-main".to_string(), 4096)
                .with_flush_policy(FlushPolicy::Sync);
        Self {
            workspace_root: dir.path().to_path_buf(),
            runtime_db: dir.path().join("verification.sqlite"),
            memory_store: MemoryStore::open(&dir.path().join("memory.sqlite"))
                .expect("memory store"),
            _event_store: event_store,
            event_writer,
            driver: FakeDriver::new(),
            _dir: dir,
        }
    }

    fn runtime(&self) -> ConsolidationJobRuntime {
        ConsolidationJobRuntime::new(
            Connection::open(&self.runtime_db).expect("runtime db opens"),
            ConsolidationConfig::default(),
        )
        .expect("runtime opens")
    }

    fn file_id(&self, path: &str, content_hash: &str) -> FileId {
        FileId {
            workspace_id: "workspace-main".to_string(),
            repo_relative_path: path.to_string(),
            content_hash: content_hash.to_string(),
        }
    }

    fn write_file(&self, path: &str, contents: &str) {
        let full_path = self.workspace_root.join(path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).expect("parent dir creates");
        }
        std::fs::write(full_path, contents).expect("file writes");
    }

    fn delete_file(&self, path: &str) {
        let full_path = self.workspace_root.join(path);
        if full_path.exists() {
            std::fs::remove_file(full_path).expect("file deletes");
        }
    }

    fn seed_plain_memory(&self, id: &str, content: &str) -> String {
        self.store_memory(base_memory(id, content, MemoryScope::Branch, vec![]))
    }

    fn seed_file_memory(&self, id: &str, path: &str) -> String {
        let memory_id = self.store_memory(base_memory(
            id,
            &format!("memory {id}"),
            MemoryScope::Branch,
            vec![path.to_string()],
        ));
        self.set_fields(&memory_id, file_fields(path));
        memory_id
    }

    fn seed_span_memory(&self, id: &str, path: &str, contents: &str) -> String {
        self.write_file(path, contents);
        let file_id = self.file_id(path, "hash-auth-v1");
        let span = EvidenceSpan {
            file_id: file_id.clone(),
            byte_start: 0,
            byte_end: contents.len() as u32,
            line_start: 1,
            line_end: 1,
        };
        let evidence = MemoryEvidence {
            kind: "file".to_string(),
            reference: Some(path.to_string()),
            detail: None,
            captured_at: Some(100),
            span: Some(span),
            evidence_content_hash: Some(hash_bytes(contents.as_bytes())),
        };
        let memory_id = self.store_memory(base_memory(
            id,
            &format!("memory {id}"),
            MemoryScope::Branch,
            vec![path.to_string()],
        ));
        self.set_fields(&memory_id, fields_with_evidence(vec![evidence]));
        memory_id
    }

    fn seed_scope_leak_fixture(&self) -> Vec<String> {
        vec![
            self.store_memory(base_memory(
                "mem-session-leak",
                "session leak",
                MemoryScope::Session,
                vec![],
            )),
            self.store_memory(branch_memory("mem-branch-leak", "feat/one", None)),
            self.store_memory(repo_memory("mem-repo-leak", "workspace-other", None)),
            self.store_memory(org_memory("mem-org-leak", "org-a")),
        ]
    }

    fn run_scope_leak_queries(&self) -> Vec<Option<Memory>> {
        vec![
            self.memory_store
                .get_by_id_scoped(
                    "mem-session-leak",
                    &ScopeFilter::new("workspace-main", Some(branch("main")), None)
                        .for_session("different-session"),
                )
                .expect("session scope lookup"),
            self.memory_store
                .get_by_id_scoped(
                    "mem-branch-leak",
                    &ScopeFilter::new("workspace-main", Some(branch("feat/two")), None),
                )
                .expect("branch scope lookup"),
            self.memory_store
                .get_by_id_scoped(
                    "mem-repo-leak",
                    &ScopeFilter::new("workspace-main", Some(branch("main")), None),
                )
                .expect("repo scope lookup"),
            self.memory_store
                .get_by_id_scoped(
                    "mem-org-leak",
                    &ScopeFilter::new(
                        "workspace-main",
                        Some(branch("main")),
                        Some("org-b".to_string()),
                    ),
                )
                .expect("org scope lookup"),
        ]
    }

    fn seed_ranker_fixture(&self) -> Vec<String> {
        let statuses = [
            ("mem-verified", MemoryVerificationStatus::Verified),
            ("mem-stale", MemoryVerificationStatus::Stale),
            ("mem-contradicted", MemoryVerificationStatus::Contradicted),
            ("mem-expired", MemoryVerificationStatus::Expired),
            ("mem-invalidated", MemoryVerificationStatus::Invalidated),
        ];
        statuses
            .into_iter()
            .map(|(id, status)| self.store_with_status(id, status, None, vec!["src/ranker.rs"]))
            .collect()
    }

    fn seed_many_file_memories(&self, count: usize) -> Vec<FileId> {
        let mut files = Vec::with_capacity(count);
        for index in 0..count {
            let path = format!("src/file_{index:03}.rs");
            let file_id = self.file_id(&path, &format!("hash-{index:03}"));
            let memory_id = format!("mem-{index:03}");
            self.store_with_status(
                &memory_id,
                MemoryVerificationStatus::Unverified,
                None,
                vec![path.as_str()],
            );
            self.set_fields(&memory_id, file_fields(&path));
            files.push(file_id);
        }
        files
    }

    fn seed_explainable_reason_fixture(&self) -> Vec<String> {
        vec![
            self.store_with_status(
                "mem-unverified",
                MemoryVerificationStatus::Unverified,
                Some("scope mismatch workspace-main/main"),
                vec!["src/unverified.rs"],
            ),
            self.store_with_status(
                "mem-in-review",
                MemoryVerificationStatus::InReview,
                Some("awaiting verifier review"),
                vec!["src/review.rs"],
            ),
            self.store_with_status(
                "mem-stale-reason",
                MemoryVerificationStatus::Stale,
                Some("span byte range 0..18 changed in src/stale.rs"),
                vec!["src/stale.rs"],
            ),
            self.store_with_status(
                "mem-contradicted-reason",
                MemoryVerificationStatus::Contradicted,
                Some("contradicted by mem-a"),
                vec!["src/contradicted.rs"],
            ),
            self.store_with_status(
                "mem-superseded-reason",
                MemoryVerificationStatus::Superseded,
                Some("superseded by mem-newer"),
                vec!["src/superseded.rs"],
            ),
            self.store_with_status(
                "mem-expired-reason",
                MemoryVerificationStatus::Expired,
                Some("expiry 2026-05-17T00:00:00Z"),
                vec!["src/expired.rs"],
            ),
            self.store_with_status(
                "mem-invalidated-reason",
                MemoryVerificationStatus::Invalidated,
                Some("file id src/deleted.rs removed"),
                vec!["src/deleted.rs"],
            ),
        ]
    }

    fn store_with_status(
        &self,
        id: &str,
        status: MemoryVerificationStatus,
        stale_reason: Option<&str>,
        linked_files: Vec<&str>,
    ) -> String {
        let is_stale = matches!(status, MemoryVerificationStatus::Stale);
        let memory_id = self.store_memory(Memory {
            stale_reason: stale_reason.map(str::to_string),
            is_stale,
            verification_status: status,
            linked_files: linked_files.into_iter().map(str::to_string).collect(),
            ..base_memory(id, &format!("memory {id}"), MemoryScope::Branch, vec![])
        });
        let mut fields = self
            .memory_store
            .get_structured_fields(&memory_id)
            .expect("fields load")
            .expect("fields exist");
        fields.verification_status = status;
        self.set_fields(&memory_id, fields);
        memory_id
    }

    fn store_memory(&self, memory: Memory) -> String {
        self.memory_store.store(memory).expect("memory stores")
    }

    fn set_fields(&self, id: &str, fields: MemoryStructuredFields) {
        self.memory_store
            .update_structured_fields(id, &fields)
            .expect("fields update");
    }

    fn bundle_for_ids(&self, ids: &[&str]) -> crate::verification::SurfacedBundle {
        let memories = ids
            .iter()
            .map(|id| {
                self.memory_store
                    .get_by_id(id)
                    .expect("memory loads")
                    .expect("memory exists")
            })
            .collect::<Vec<_>>();
        SurfacingPipeline::partition(memories)
    }

    fn scoring_context_for(&self, ids: &[String]) -> ScoringContext {
        let mut memory_metadata = BTreeMap::new();
        for id in ids {
            let memory = self
                .memory_store
                .get_by_id(id)
                .expect("memory loads")
                .expect("memory exists");
            memory_metadata.insert(
                memory_identity(id).to_string(),
                MemoryScoringMetadata {
                    verification_status: memory.verification_status,
                    scope: memory.scope,
                    confidence: memory.confidence as f32,
                    evidence_count: 1,
                    created_at: Some(memory.created_at),
                    last_accessed: Some(memory.last_accessed),
                    access_count: memory.access_count,
                    is_stale: memory.is_stale,
                    superseded_by_memory_id: None,
                    contradicted_by_memory_ids: Vec::new(),
                },
            );
        }
        ScoringContext {
            memory_metadata,
            ..ScoringContext::default()
        }
    }

    fn reverify_changed_file(
        &self,
        changed: &FileId,
        new_contents: &str,
        observer: ObservationRecorder,
    ) -> crate::verification::IncrementalReport {
        self.write_file(&changed.repo_relative_path, new_contents);
        let graph = self.graph_with_files(&[(changed, "authFlag", 0)]);
        let file_index = self.file_index(&[(changed, &changed.content_hash, 200)]);
        self.run_incremental(
            changed,
            &graph,
            &file_index,
            &HashMap::new(),
            Some(&observer),
        )
    }

    fn reverify_deleted_file(
        &self,
        changed: &FileId,
        observer: ObservationRecorder,
    ) -> crate::verification::IncrementalReport {
        self.run_incremental(
            changed,
            &CodeGraph::new(),
            &HashMap::new(),
            &HashMap::new(),
            Some(&observer),
        )
    }

    fn reverify_unchanged_content(
        &self,
        changed: &FileId,
        files: &[FileId],
        observer: &ObservationRecorder,
    ) -> crate::verification::IncrementalReport {
        let graph = self.graph_with_files(
            &files
                .iter()
                .map(|file| (file, "fact", 0_usize))
                .collect::<Vec<_>>(),
        );
        let tuples = files
            .iter()
            .map(|file| (file, file.content_hash.as_str(), 200_i64))
            .collect::<Vec<_>>();
        let file_index = self.file_index(&tuples);
        self.run_incremental(
            changed,
            &graph,
            &file_index,
            &HashMap::new(),
            Some(observer),
        )
    }

    fn run_incremental(
        &self,
        changed: &FileId,
        graph: &CodeGraph,
        file_index: &HashMap<String, FileIndexEntry>,
        parsed_files: &HashMap<String, ParsedFile>,
        observer: Option<&ObservationRecorder>,
    ) -> crate::verification::IncrementalReport {
        let mut runtime = self.runtime();
        let reader = WorkspaceFileReader::new(self.workspace_root.clone());
        let operator = operator();
        let mut verifier = IncrementalVerifier::new(
            &self.memory_store,
            &mut runtime,
            graph,
            file_index,
            parsed_files,
            &reader,
            &self.event_writer,
            &operator,
            "workspace-main",
            512,
        );
        if let Some(observer) = observer {
            verifier = verifier.with_observer(observer);
        }
        verifier
            .on_graph_delta(1, 2, vec![changed.clone()])
            .expect("incremental verification succeeds")
    }

    fn scan_expiry(&self, now: DateTime<Utc>) {
        let mut runtime = self.runtime();
        let operator = operator();
        let mut scanner = ExpiryScanner::new(
            &self.memory_store,
            &mut runtime,
            &self.event_writer,
            &operator,
        );
        let _ = scanner.scan("workspace-main", now).expect("expiry scan");
    }

    fn apply_contradiction(&mut self, contradicted_id: &str, contradicting_id: &str) {
        self.driver.push_ok(contradiction_response());
        let pair = ContradictionCandidatePair {
            first: self.load_memory(contradicting_id),
            second: self.load_memory(contradicted_id),
            deterministic_reason: "overlapping build verification assertions".to_string(),
        };
        let ctx = LlmJobContext {
            workspace_id: "workspace-main".to_string(),
            mode: ConsolidationMode::Background,
            budget_catalog: BudgetCatalog::default(),
        };
        let mut runtime = self.runtime();
        let mut services = LlmJobServices {
            driver: &self.driver,
            runtime: &mut runtime,
            memory_store: &self.memory_store,
            event_writer: &self.event_writer,
        };
        let proposal = ContradictionDetectionJob::run(&ctx, &mut services, &pair)
            .expect("contradiction job succeeds")
            .expect("proposal emitted");
        let _ = runtime
            .decide(
                &proposal.proposal_id,
                ProposalDecision::Applied,
                &self.memory_store,
                &self.event_writer,
                operator().value.as_str(),
            )
            .expect("proposal applies");
    }

    fn load_memory(&self, id: &str) -> Memory {
        self.memory_store
            .get_by_id(id)
            .expect("memory loads")
            .expect("memory exists")
    }

    fn graph_with_files(&self, nodes: &[(&FileId, &str, usize)]) -> CodeGraph {
        let mut graph = CodeGraph::new();
        for (file_id, name, byte_offset) in nodes {
            graph.add_node(
                crate::symbols::SymbolId {
                    file: file_id.repo_relative_path.clone(),
                    name: (*name).to_string(),
                    byte_offset: *byte_offset,
                },
                crate::symbols::SymbolKind::Function,
                (*name).to_string(),
                "function".to_string(),
                String::new(),
                file_id.repo_relative_path.clone(),
                1,
                1,
                true,
                Language::TypeScript,
            );
        }
        graph
    }

    fn file_index(&self, files: &[(&FileId, &str, i64)]) -> HashMap<String, FileIndexEntry> {
        files
            .iter()
            .map(|(file_id, content_hash, last_indexed_at)| {
                (
                    file_id.repo_relative_path.clone(),
                    FileIndexEntry {
                        file: file_id.repo_relative_path.clone(),
                        content_hash: (*content_hash).to_string(),
                        mtime_ns: 0,
                        size_bytes: 128,
                        parser_version: FILE_INDEX_PARSER_VERSION,
                        schema_version: FILE_INDEX_SCHEMA_VERSION,
                        last_indexed_at: *last_indexed_at,
                    },
                )
            })
            .collect()
    }
}

#[derive(Default)]
struct ObservationRecorder {
    memory_ids: Mutex<Vec<String>>,
}

impl ObservationRecorder {
    fn memory_ids(&self) -> Vec<String> {
        self.memory_ids.lock().expect("observer lock").clone()
    }
}

impl VerificationObserver for ObservationRecorder {
    fn on_verify(&self, memory_id: &str) {
        self.memory_ids
            .lock()
            .expect("observer lock")
            .push(memory_id.to_string());
    }
}

#[derive(Clone)]
struct FakeDriver {
    responses: Arc<Mutex<Vec<Result<String, LlmDriverError>>>>,
}

impl FakeDriver {
    fn new() -> Self {
        Self {
            responses: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn push_ok(&self, response: String) {
        self.responses
            .lock()
            .expect("driver lock")
            .push(Ok(response));
    }
}

impl LlmDriver for FakeDriver {
    fn complete(&self, _request: LlmRequest) -> Result<LlmResponse, LlmDriverError> {
        self.responses
            .lock()
            .expect("driver lock")
            .pop()
            .unwrap_or_else(|| Err(LlmDriverError::Failed("missing fake response".to_string())))
            .map(|content| LlmResponse { content })
    }

    fn name(&self) -> &str {
        "fake-llm"
    }
}

struct BufferWriter(Arc<Mutex<Vec<u8>>>);

impl<'a> MakeWriter<'a> for BufferWriter {
    type Writer = BufferGuard;

    fn make_writer(&'a self) -> Self::Writer {
        BufferGuard(self.0.clone())
    }
}

struct BufferGuard(Arc<Mutex<Vec<u8>>>);

impl Write for BufferGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn operator() -> OperatorId {
    OperatorId {
        value: "verification-bot".to_string(),
    }
}

fn branch(name: &str) -> BranchRef {
    BranchRef {
        name: name.to_string(),
    }
}

fn base_memory(id: &str, content: &str, scope: MemoryScope, linked_files: Vec<String>) -> Memory {
    Memory {
        id: id.to_string(),
        session_id: "session-main".to_string(),
        content: content.to_string(),
        memory_type: MemoryType::Observation,
        scope,
        confidence: 0.9,
        linked_symbols: Vec::new(),
        linked_files,
        workspace_id: Some("workspace-main".to_string()),
        branch: Some("main".to_string()),
        scope_organization_id: None,
        refresh_key: Some(format!("refresh-{id}")),
        source_query: Some("verification test".to_string()),
        created_at: 100,
        last_accessed: 100,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: MemoryVerificationStatus::Unverified,
    }
}

fn branch_memory(id: &str, branch_name: &str, org: Option<&str>) -> Memory {
    Memory {
        branch: Some(branch_name.to_string()),
        scope_organization_id: org.map(str::to_string),
        ..base_memory(id, id, MemoryScope::Branch, vec![])
    }
}

fn repo_memory(id: &str, workspace_id: &str, org: Option<&str>) -> Memory {
    Memory {
        workspace_id: Some(workspace_id.to_string()),
        scope_organization_id: org.map(str::to_string),
        ..base_memory(id, id, MemoryScope::Repo, vec![])
    }
}

fn org_memory(id: &str, org: &str) -> Memory {
    Memory {
        workspace_id: None,
        branch: None,
        scope_organization_id: Some(org.to_string()),
        ..base_memory(id, id, MemoryScope::Organization, vec![])
    }
}

fn fields_with_evidence(evidence: Vec<MemoryEvidence>) -> MemoryStructuredFields {
    MemoryStructuredFields {
        evidence,
        ..MemoryStructuredFields::default()
    }
}

fn file_fields(path: &str) -> MemoryStructuredFields {
    fields_with_evidence(vec![MemoryEvidence {
        kind: "file".to_string(),
        reference: Some(path.to_string()),
        detail: None,
        captured_at: Some(100),
        span: None,
        evidence_content_hash: None,
    }])
}

fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(&digest);
    hash
}

fn contradiction_response() -> String {
    json!({
        "is_contradiction": true,
        "contradicted_memory_id": "mem-b",
        "contradicting_memory_id": "mem-a",
        "rationale": "One asserts cargo check is never used while the other says it is used."
    })
    .to_string()
}

fn ranked_memory(id: &str, score: f32) -> RankedCandidate {
    RankedCandidate {
        candidate: Candidate {
            identity: memory_identity(id),
            source: CandidateSource::MemoryLinks,
            seed_anchor: None,
            raw_score: score as f64,
            preliminary_inclusion_reason: format!("memory {id}"),
            expansion_handle_hint: None,
            traversal_path: Vec::new(),
            budget_exhausted: false,
        },
        total_score: score,
        signal_scores: Vec::new(),
        inclusion_reason: format!("ranked memory {id}"),
    }
}

fn memory_identity(id: &str) -> Identity {
    Identity::Memory(MemoryId {
        workspace_id: "workspace-main".to_string(),
        ulid: id.to_string(),
    })
}

fn assert_sections(
    bundle: &crate::verification::SurfacedBundle,
    memory_id: &str,
    expected: BundleSection,
) {
    assert!(!bundle
        .trusted
        .iter()
        .any(|memory| memory.memory_id.ulid == memory_id));
    assert_eq!(
        bundle
            .advisory
            .iter()
            .find(|memory| memory.memory_id.ulid == memory_id)
            .expect("memory in advisory")
            .section,
        expected
    );
}

fn assert_reason_contains(
    bundle: &crate::verification::SurfacedBundle,
    memory_id: &str,
    expected: &str,
) {
    let reason = &bundle
        .advisory
        .iter()
        .find(|memory| memory.memory_id.ulid == memory_id)
        .expect("memory in advisory")
        .label_reason;
    assert!(!reason.is_empty());
    assert!(
        reason.contains(expected),
        "missing `{expected}` in `{reason}`"
    );
}
