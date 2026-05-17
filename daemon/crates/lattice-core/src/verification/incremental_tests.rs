use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tempfile::tempdir;

use super::{ExpiryScanner, IncrementalVerifier, VerificationObserver, WorkspaceFileReader};
use crate::consolidation::{ConsolidationConfig, ConsolidationJobRuntime};
use crate::events::{EventStore, EventWriter, FlushPolicy};
use crate::graph::CodeGraph;
use crate::identity::{FileId, OperatorId};
use crate::memory::{
    Memory, MemoryEvidence, MemoryScope, MemoryStore, MemoryStructuredFields, MemoryType,
    MemoryVerificationStatus,
};
use crate::storage::graph_store::{
    FileIndexEntry, FILE_INDEX_PARSER_VERSION, FILE_INDEX_SCHEMA_VERSION,
};
use crate::symbols::Language;
use crate::DateTime;

#[test]
fn graph_delta_reverifies_only_impacted_memories() {
    let fixture = Fixture::new();
    let changed_file = fixture.file_id("src/auth.ts", "hash-auth");
    let unchanged_file = fixture.file_id("src/billing.ts", "hash-billing");
    let graph = fixture.graph_with_files(&[
        (&changed_file, "loginUser", 12),
        (&unchanged_file, "chargeInvoice", 24),
    ]);
    let file_index = fixture.file_index(&[
        (&changed_file, "hash-auth", 200),
        (&unchanged_file, "hash-billing", 200),
    ]);
    let parsed_files = HashMap::new();
    let reader = WorkspaceFileReader::new(fixture.workspace_root.clone());
    let observer = CountingObserver::default();

    fixture.seed_memory("mem-file", file_evidence(&changed_file.repo_relative_path));
    fixture.seed_memory("mem-symbol", symbol_evidence("loginUser"));
    fixture.seed_memory(
        "mem-other",
        file_evidence(&unchanged_file.repo_relative_path),
    );

    let mut runtime = fixture.runtime();
    let operator = operator();
    let mut verifier = IncrementalVerifier::new(
        &fixture.memory_store,
        &mut runtime,
        &graph,
        &file_index,
        &parsed_files,
        &reader,
        &fixture.event_writer,
        &operator,
        "workspace-main",
        32,
    )
    .with_observer(&observer);

    let report = verifier
        .on_graph_delta(1, 2, vec![changed_file.clone()])
        .expect("delta verification succeeds");

    assert_eq!(report.impacted, 2);
    assert_eq!(observer.memory_ids(), vec!["mem-file", "mem-symbol"]);
}

#[test]
fn expiry_scanner_marks_expired_and_is_idempotent() {
    let fixture = Fixture::new();
    let expired_memory = fixture.seed_memory("mem-expired", file_evidence("src/auth.ts"));
    fixture
        .memory_store
        .set_expires_at(&expired_memory, DateTime::from_unix_seconds(10))
        .expect("expiry stores");
    let operator = operator();
    let mut runtime = fixture.runtime();
    let mut scanner = ExpiryScanner::new(
        &fixture.memory_store,
        &mut runtime,
        &fixture.event_writer,
        &operator,
    );

    let first = scanner
        .scan("workspace-main", DateTime::from_unix_seconds(20))
        .expect("first expiry scan succeeds");
    let second = scanner
        .scan("workspace-main", DateTime::from_unix_seconds(20))
        .expect("second expiry scan succeeds");

    assert_eq!(first.expired, 1);
    assert_eq!(second.expired, 0);
    assert_eq!(
        fixture
            .memory_store
            .get_structured_fields(&expired_memory)
            .expect("fields load")
            .expect("memory exists")
            .verification_status,
        MemoryVerificationStatus::Expired
    );
}

#[test]
fn successful_verification_advances_last_verified_graph_snapshot_id() {
    let fixture = Fixture::new();
    let changed_file = fixture.file_id("src/auth.ts", "hash-auth");
    let graph = fixture.graph_with_files(&[(&changed_file, "loginUser", 12)]);
    let file_index = fixture.file_index(&[(&changed_file, "hash-auth", 200)]);
    let parsed_files = HashMap::new();
    let reader = WorkspaceFileReader::new(fixture.workspace_root.clone());

    let memory_id = fixture.seed_memory(
        "mem-verified",
        file_evidence(&changed_file.repo_relative_path),
    );
    let mut runtime = fixture.runtime();
    let operator = operator();
    let mut verifier = IncrementalVerifier::new(
        &fixture.memory_store,
        &mut runtime,
        &graph,
        &file_index,
        &parsed_files,
        &reader,
        &fixture.event_writer,
        &operator,
        "workspace-main",
        8,
    );

    let report = verifier
        .on_graph_delta(4, 8, vec![changed_file])
        .expect("verification succeeds");

    assert_eq!(report.verified, 1);
    assert_eq!(
        fixture
            .memory_store
            .get_last_verified_graph_snapshot_id(&memory_id)
            .expect("snapshot id loads"),
        Some(8)
    );
}

#[test]
fn deleted_file_invalidates_verified_memory_on_next_delta() {
    let fixture = Fixture::new();
    let changed_file = fixture.file_id("src/auth.ts", "hash-auth");
    let parsed_files = HashMap::new();
    let reader = WorkspaceFileReader::new(fixture.workspace_root.clone());

    let memory_id = fixture.seed_memory(
        "mem-delete",
        file_evidence(&changed_file.repo_relative_path),
    );
    fixture.set_verification_status(&memory_id, MemoryVerificationStatus::Verified);
    let empty_graph = CodeGraph::new();
    let empty_file_index = HashMap::new();

    let mut runtime = fixture.runtime();
    let operator = operator();
    let mut verifier = IncrementalVerifier::new(
        &fixture.memory_store,
        &mut runtime,
        &empty_graph,
        &empty_file_index,
        &parsed_files,
        &reader,
        &fixture.event_writer,
        &operator,
        "workspace-main",
        8,
    );

    let report = verifier
        .on_graph_delta(8, 9, vec![changed_file])
        .expect("verification succeeds");

    assert_eq!(report.invalidated, 1);
    assert_eq!(
        fixture
            .memory_store
            .get_structured_fields(&memory_id)
            .expect("fields load")
            .expect("memory exists")
            .verification_status,
        MemoryVerificationStatus::Invalidated
    );
}

#[test]
fn bounded_work_budget_reenqueues_remaining_memories() {
    let fixture = Fixture::new();
    let changed_file = fixture.file_id("src/auth.ts", "hash-auth");
    let graph = fixture.graph_with_files(&[(&changed_file, "loginUser", 12)]);
    let file_index = fixture.file_index(&[(&changed_file, "hash-auth", 200)]);
    let parsed_files = HashMap::new();
    let reader = WorkspaceFileReader::new(fixture.workspace_root.clone());
    let observer = CountingObserver::default();

    for index in 0..1000 {
        fixture.seed_memory(
            &format!("mem-{index}"),
            file_evidence(&changed_file.repo_relative_path),
        );
    }

    let mut runtime = fixture.runtime();
    let operator = operator();
    let mut verifier = IncrementalVerifier::new(
        &fixture.memory_store,
        &mut runtime,
        &graph,
        &file_index,
        &parsed_files,
        &reader,
        &fixture.event_writer,
        &operator,
        "workspace-main",
        50,
    )
    .with_observer(&observer);

    let report = verifier
        .on_graph_delta(1, 2, vec![changed_file])
        .expect("delta verification succeeds");

    assert_eq!(report.impacted, 1000);
    assert_eq!(report.verified, 50);
    assert_eq!(report.queued_for_retry, 950);
    assert_eq!(observer.memory_ids().len(), 50);
}

#[derive(Default)]
struct CountingObserver {
    memory_ids: Mutex<Vec<String>>,
}

impl CountingObserver {
    fn memory_ids(&self) -> Vec<String> {
        self.memory_ids.lock().expect("observer lock").clone()
    }
}

impl VerificationObserver for CountingObserver {
    fn on_verify(&self, memory_id: &str) {
        self.memory_ids
            .lock()
            .expect("observer lock")
            .push(memory_id.to_string());
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    workspace_root: PathBuf,
    memory_store: MemoryStore,
    runtime_db: PathBuf,
    _event_store: Arc<EventStore>,
    event_writer: EventWriter,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempdir().expect("tempdir");
        let event_store = Arc::new(EventStore::open_in_memory().expect("event store"));
        let event_writer =
            EventWriter::new(event_store.clone(), "workspace-main".to_string(), 4096)
                .with_flush_policy(FlushPolicy::Sync);
        Self {
            workspace_root: dir.path().to_path_buf(),
            memory_store: MemoryStore::open(&dir.path().join("memory.sqlite"))
                .expect("memory store"),
            runtime_db: dir.path().join("verification.sqlite"),
            _event_store: event_store,
            event_writer,
            _dir: dir,
        }
    }

    fn runtime(&self) -> ConsolidationJobRuntime {
        ConsolidationJobRuntime::new(
            Connection::open(&self.runtime_db).expect("verification db opens"),
            ConsolidationConfig::default(),
        )
        .expect("runtime opens")
    }

    fn seed_memory(&self, id: &str, evidence: MemoryEvidence) -> String {
        let memory = Memory {
            id: id.to_string(),
            session_id: "session-main".to_string(),
            content: format!("memory {id}"),
            memory_type: MemoryType::Observation,
            scope: MemoryScope::Branch,
            confidence: 0.9,
            linked_symbols: Vec::new(),
            linked_files: Vec::new(),
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
            verification_status: crate::memory::MemoryVerificationStatus::Unverified,
        };
        let stored_id = self.memory_store.store(memory).expect("memory stores");
        let mut fields = MemoryStructuredFields::default();
        fields.evidence = vec![evidence];
        self.memory_store
            .update_structured_fields(&stored_id, &fields)
            .expect("fields update");
        stored_id
    }

    fn set_verification_status(&self, memory_id: &str, status: MemoryVerificationStatus) {
        let mut fields = self
            .memory_store
            .get_structured_fields(memory_id)
            .expect("fields load")
            .expect("memory exists");
        fields.verification_status = status;
        self.memory_store
            .update_structured_fields(memory_id, &fields)
            .expect("fields update");
    }

    fn file_id(&self, path: &str, content_hash: &str) -> FileId {
        FileId {
            workspace_id: "workspace-main".to_string(),
            repo_relative_path: path.to_string(),
            content_hash: content_hash.to_string(),
        }
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

fn operator() -> OperatorId {
    OperatorId {
        value: "verification-bot".to_string(),
    }
}

fn file_evidence(path: &str) -> MemoryEvidence {
    MemoryEvidence {
        kind: "file".to_string(),
        reference: Some(path.to_string()),
        detail: None,
        captured_at: Some(100),
        span: None,
        evidence_content_hash: None,
    }
}

fn symbol_evidence(symbol_name: &str) -> MemoryEvidence {
    MemoryEvidence {
        kind: "symbol".to_string(),
        reference: Some(symbol_name.to_string()),
        detail: None,
        captured_at: Some(100),
        span: None,
        evidence_content_hash: None,
    }
}
