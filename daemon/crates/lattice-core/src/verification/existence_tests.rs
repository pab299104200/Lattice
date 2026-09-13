use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::Connection;
use tempfile::tempdir;

use super::{VerificationStatus, VerifierCore, WorkspaceFileReader};
use crate::consolidation::{
    ConsolidationConfig, ConsolidationJobRuntime, ConsolidationProposal, EvolutionAuthority,
    ProposalKind,
};
use crate::events::{EventStore, EventWriter, FlushPolicy};
use crate::graph::CodeGraph;
use crate::identity::{encode_identity, FileId, Identity, SymbolId};
use crate::memory::{
    BehavioralValidationRecord, BehavioralValidationStatus, EvidenceFreshnessStatus, Memory,
    MemoryEvidence, MemoryScope, MemoryStore, MemoryStructuredFields, MemoryType,
    MemoryVerificationStatus,
};
use crate::storage::graph_store::{
    FileIndexEntry, FILE_INDEX_PARSER_VERSION, FILE_INDEX_SCHEMA_VERSION,
};
use crate::symbols::{Language, ParsedFile, Symbol, SymbolKind};

#[test]
fn live_file_evidence_emits_verified_proposal() {
    let fixture = Fixture::new();
    let file_id = fixture.file_id("src/auth.ts", "hash-auth");
    let memory_id = fixture.seed_memory(
        "memory-file",
        vec![MemoryEvidence {
            kind: "file".to_string(),
            reference: Some(file_id.repo_relative_path.clone()),
            detail: None,
            captured_at: Some(100),
            span: None,
            evidence_content_hash: None,
        }],
    );
    let graph = fixture.graph_with_auth(false);
    let file_index = fixture.file_index(&[("src/auth.ts", "hash-auth", 120)]);
    let parsed_files = HashMap::new();

    let verdict = fixture.verify(&memory_id, &graph, &file_index, &parsed_files);

    assert_eq!(verdict.status, VerificationStatus::Verified);
    assert_eq!(fixture.latest_proposal_kind(), ProposalKind::MarkVerified);
    assert_eq!(
        fixture
            .memory_store
            .get_structured_fields(&memory_id)
            .unwrap()
            .unwrap()
            .verification_status,
        MemoryVerificationStatus::Unverified
    );
}

#[test]
fn deleting_file_emits_invalidated_proposal() {
    let fixture = Fixture::new();
    let memory_id = fixture.seed_memory(
        "memory-delete-file",
        vec![MemoryEvidence {
            kind: "file".to_string(),
            reference: Some("src/auth.ts".to_string()),
            detail: None,
            captured_at: Some(100),
            span: None,
            evidence_content_hash: None,
        }],
    );
    let file_index = fixture.file_index(&[]);
    let parsed_files = HashMap::new();

    let verdict = fixture.verify(&memory_id, &CodeGraph::new(), &file_index, &parsed_files);

    assert_eq!(verdict.status, VerificationStatus::Invalidated);
    assert_eq!(
        fixture.latest_proposal_kind(),
        ProposalKind::MarkInvalidated
    );
}

#[test]
fn deleted_symbol_emits_invalidated_proposal() {
    let fixture = Fixture::new();
    let file_id = fixture.file_id("src/auth.ts", "hash-auth");
    let symbol_id = fixture.symbol_id(&file_id, "loginUser", 12, "function");
    let memory_id = fixture.seed_memory(
        "memory-symbol",
        vec![MemoryEvidence {
            kind: "symbol".to_string(),
            reference: Some(symbol_id.to_string()),
            detail: None,
            captured_at: Some(100),
            span: None,
            evidence_content_hash: None,
        }],
    );
    let file_index = fixture.file_index(&[("src/auth.ts", "hash-auth", 120)]);
    let parsed_files = HashMap::new();

    let verdict = fixture.verify(&memory_id, &CodeGraph::new(), &file_index, &parsed_files);

    assert_eq!(verdict.status, VerificationStatus::Invalidated);
    assert_eq!(
        fixture.latest_proposal_kind(),
        ProposalKind::MarkInvalidated
    );
}

#[test]
fn existing_doc_section_emits_verified_proposal() {
    let fixture = Fixture::new();
    let memory_id = fixture.seed_memory(
        "memory-doc",
        vec![MemoryEvidence {
            kind: "doc_section".to_string(),
            reference: Some("docs/spec.md#Verification Engine".to_string()),
            detail: None,
            captured_at: Some(100),
            span: None,
            evidence_content_hash: None,
        }],
    );
    let graph = CodeGraph::new();
    let file_index = fixture.file_index(&[("docs/spec.md", "hash-doc", 120)]);
    let parsed_files = fixture.doc_parsed_files(&[("docs/spec.md", "Verification Engine", 0)]);

    let verdict = fixture.verify(&memory_id, &graph, &file_index, &parsed_files);

    assert_eq!(verdict.status, VerificationStatus::Verified);
    assert_eq!(fixture.latest_proposal_kind(), ProposalKind::MarkVerified);
}

#[test]
fn deleted_test_emits_invalidated_proposal() {
    let fixture = Fixture::new();
    let file_id = fixture.file_id("tests/auth.test.ts", "abcdef12");
    let test_symbol_id = fixture.symbol_id(&file_id, "loginUserTest", 4, "test");
    let memory_id = fixture.seed_memory(
        "memory-test",
        vec![MemoryEvidence {
            kind: "test".to_string(),
            reference: Some(test_symbol_id.to_string()),
            detail: None,
            captured_at: Some(100),
            span: None,
            evidence_content_hash: None,
        }],
    );
    let graph = fixture.graph_with_auth(false);
    let file_index = fixture.file_index(&[("tests/auth.test.ts", "abcdef12", 120)]);
    let parsed_files = HashMap::new();

    let verdict = fixture.verify(&memory_id, &graph, &file_index, &parsed_files);

    assert_eq!(verdict.status, VerificationStatus::Invalidated);
    assert_eq!(
        fixture.latest_proposal_kind(),
        ProposalKind::MarkInvalidated
    );
}

#[test]
fn no_evidence_does_not_promote_claim_even_when_scope_holds() {
    let fixture = Fixture::new();
    let memory_id = fixture.seed_memory("memory-no-evidence", Vec::new());

    let verdict = fixture.verify(
        &memory_id,
        &CodeGraph::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    assert_eq!(verdict.status, VerificationStatus::Unverified);
    assert_eq!(verdict.evidence_freshness, EvidenceFreshnessStatus::Unknown);
    assert_eq!(
        verdict.behavioral_validation,
        BehavioralValidationStatus::Unverified
    );
    assert_eq!(fixture.proposal_count(), 0);
}

#[test]
fn unsupported_evidence_does_not_promote_claim() {
    let fixture = Fixture::new();
    let memory_id = fixture.seed_memory(
        "memory-unsupported",
        vec![MemoryEvidence {
            kind: "shell_command".to_string(),
            reference: Some("cargo test".to_string()),
            detail: None,
            captured_at: Some(100),
            span: None,
            evidence_content_hash: None,
        }],
    );

    let verdict = fixture.verify(
        &memory_id,
        &CodeGraph::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    assert_eq!(verdict.status, VerificationStatus::Unverified);
    assert_eq!(
        verdict.behavioral_validation,
        BehavioralValidationStatus::Unverified
    );
    assert_eq!(fixture.proposal_count(), 0);
}

#[test]
fn existing_test_requires_current_bound_passing_result() {
    let fixture = Fixture::new();
    let file_id = fixture.file_id("tests/auth.test.ts", "abcdef12");
    let test_symbol_id = fixture.symbol_id(&file_id, "loginUserTest", 4, "test");
    let evidence = MemoryEvidence {
        kind: "test".to_string(),
        reference: Some(encode_identity(&Identity::Symbol(test_symbol_id))),
        detail: None,
        captured_at: Some(100),
        span: None,
        evidence_content_hash: None,
    };
    let reference = evidence.reference.clone().unwrap();
    let memory_id = fixture.seed_memory("memory-live-test", vec![evidence]);
    let graph = fixture.graph_with_auth(true);
    assert!(graph
        .get_node(&crate::symbols::SymbolId {
            file: "tests/auth.test.ts".to_string(),
            name: "loginUserTest".to_string(),
            byte_offset: 4,
        })
        .is_some());
    let file_index = fixture.file_index(&[("tests/auth.test.ts", "abcdef12", 120)]);

    let no_result = fixture.verify(&memory_id, &graph, &file_index, &HashMap::new());
    assert_eq!(
        no_result.status,
        VerificationStatus::Unverified,
        "{}",
        no_result.reason
    );
    assert_eq!(no_result.evidence_freshness, EvidenceFreshnessStatus::Fresh);

    let failed = BehavioralValidationRecord {
        repository_id: "repository-main".to_string(),
        checkout_id: "checkout-main".to_string(),
        evidence_reference: reference.clone(),
        status: BehavioralValidationStatus::Failed,
        revision: Some("rev-current".to_string()),
        graph_generation: Some(42),
        observed_at: 200,
    };
    let failed_result = fixture.verify_with_validations(
        &memory_id,
        &graph,
        &file_index,
        &HashMap::new(),
        std::slice::from_ref(&failed),
        Some("rev-current"),
        Some(42),
    );
    assert_eq!(failed_result.status, VerificationStatus::Invalidated);
    assert_eq!(
        failed_result.behavioral_validation,
        BehavioralValidationStatus::Failed
    );

    let stale_pass = BehavioralValidationRecord {
        repository_id: "repository-main".to_string(),
        checkout_id: "checkout-main".to_string(),
        evidence_reference: reference.clone(),
        status: BehavioralValidationStatus::Passed,
        revision: Some("rev-old".to_string()),
        graph_generation: Some(42),
        observed_at: 150,
    };
    let stale_result = fixture.verify_with_validations(
        &memory_id,
        &graph,
        &file_index,
        &HashMap::new(),
        &[stale_pass],
        Some("rev-current"),
        None,
    );
    assert_eq!(stale_result.status, VerificationStatus::Unverified);

    let current_pass = BehavioralValidationRecord {
        repository_id: "repository-main".to_string(),
        checkout_id: "checkout-main".to_string(),
        evidence_reference: reference,
        status: BehavioralValidationStatus::Passed,
        revision: Some("rev-current".to_string()),
        graph_generation: Some(42),
        observed_at: 250,
    };
    let passed_result = fixture.verify_with_validations(
        &memory_id,
        &graph,
        &file_index,
        &HashMap::new(),
        std::slice::from_ref(&current_pass),
        Some("rev-current"),
        Some(42),
    );
    assert_eq!(passed_result.status, VerificationStatus::Verified);
    assert_eq!(
        passed_result.behavioral_validation,
        BehavioralValidationStatus::Passed
    );

    let wrong_checkout = BehavioralValidationRecord {
        checkout_id: "checkout-other".to_string(),
        ..current_pass.clone()
    };
    assert_eq!(
        fixture
            .verify_with_validations(
                &memory_id,
                &graph,
                &file_index,
                &HashMap::new(),
                &[wrong_checkout],
                Some("rev-current"),
                Some(42),
            )
            .behavioral_validation,
        BehavioralValidationStatus::Unverified
    );

    let revision_only = BehavioralValidationRecord {
        graph_generation: None,
        ..current_pass.clone()
    };
    assert_eq!(
        fixture
            .verify_with_validations(
                &memory_id,
                &graph,
                &file_index,
                &HashMap::new(),
                &[revision_only],
                Some("rev-current"),
                Some(42),
            )
            .behavioral_validation,
        BehavioralValidationStatus::Unverified,
        "same revision without dirty-content generation cannot certify a check"
    );

    let expired = BehavioralValidationRecord {
        observed_at: 100,
        ..current_pass.clone()
    };
    assert_eq!(
        fixture
            .verify_with_validations(
                &memory_id,
                &graph,
                &file_index,
                &HashMap::new(),
                &[expired],
                Some("rev-current"),
                Some(42),
            )
            .behavioral_validation,
        BehavioralValidationStatus::Unverified
    );

    let later_failure = BehavioralValidationRecord {
        status: BehavioralValidationStatus::Failed,
        observed_at: 260,
        ..current_pass.clone()
    };
    for records in [
        vec![current_pass.clone(), later_failure.clone()],
        vec![later_failure.clone(), current_pass.clone()],
    ] {
        assert_eq!(
            fixture
                .verify_with_validations(
                    &memory_id,
                    &graph,
                    &file_index,
                    &HashMap::new(),
                    &records,
                    Some("rev-current"),
                    Some(42),
                )
                .behavioral_validation,
            BehavioralValidationStatus::Failed,
            "newest bound observation wins independently of input order"
        );
    }
}

#[test]
fn verification_schema_round_trips_under_runtime_migration() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("verification.sqlite");
    let conn = Connection::open(&path).expect("verification db opens");
    crate::consolidation::initialize_schema(&conn).expect("workflow schemas initialize");
    conn.execute(
        "INSERT INTO verification_jobs
            (job_id, workspace_id, target_memory_id, check_kind, status, verdict, reason, queued_at)
         VALUES ('verify-job', 'workspace-main', 'memory-id', 'existence', 'completed', 'verified', 'ok', 1)",
        [],
    )
    .expect("verification job inserts");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM verification_jobs", [], |row| {
            row.get(0)
        })
        .expect("verification job count reads");
    assert_eq!(count, 1);
}

struct Fixture {
    _dir: tempfile::TempDir,
    workspace_root: PathBuf,
    memory_store: MemoryStore,
    runtime_db: PathBuf,
    _event_store: Arc<EventStore>,
    _event_writer: EventWriter,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempdir().expect("tempdir");
        let memory_store =
            MemoryStore::open(&dir.path().join("memory.sqlite")).expect("memory store");
        let event_store = Arc::new(EventStore::open_in_memory().expect("event store"));
        let event_writer =
            EventWriter::new(event_store.clone(), "workspace-main".to_string(), 4096)
                .with_flush_policy(FlushPolicy::Sync);
        Self {
            workspace_root: dir.path().to_path_buf(),
            runtime_db: dir.path().join("verification.sqlite"),
            _dir: dir,
            memory_store,
            _event_store: event_store,
            _event_writer: event_writer,
        }
    }

    fn file_id(&self, path: &str, content_hash: &str) -> FileId {
        FileId {
            workspace_id: "workspace-main".to_string(),
            repo_relative_path: path.to_string(),
            content_hash: content_hash.to_string(),
        }
    }

    fn symbol_id(
        &self,
        file: &FileId,
        qualified_name: &str,
        byte_offset: usize,
        kind: &str,
    ) -> SymbolId {
        SymbolId {
            file: file.clone(),
            qualified_name: qualified_name.to_string(),
            byte_offset,
            kind: kind.to_string(),
        }
    }

    fn seed_memory(&self, id: &str, evidence: Vec<MemoryEvidence>) -> String {
        let memory = Memory {
            id: id.to_string(),
            session_id: "session-main".to_string(),
            content: format!("memory {id}"),
            memory_type: MemoryType::Observation,
            scope: MemoryScope::Branch,
            confidence: 0.8,
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
        fields.evidence = evidence;
        self.memory_store
            .update_structured_fields(&stored_id, &fields)
            .expect("structured fields update");
        stored_id
    }

    fn verify(
        &self,
        memory_id: &str,
        graph: &CodeGraph,
        file_index: &HashMap<String, FileIndexEntry>,
        parsed_files: &HashMap<String, ParsedFile>,
    ) -> super::VerificationVerdict {
        let conn = Connection::open(&self.runtime_db).expect("runtime db opens");
        let mut runtime = ConsolidationJobRuntime::new(
            conn,
            ConsolidationConfig {
                max_queue_depth: 16,
                llm_budget_catalog: None,
            },
        )
        .expect("runtime opens");
        let reader = WorkspaceFileReader::new(self.workspace_root.clone());
        let authority = self.authority();
        let mut verifier = VerifierCore::new(
            &self.memory_store,
            &mut runtime,
            graph,
            file_index,
            parsed_files,
            &reader,
            "workspace-main",
            &authority,
        );
        verifier
            .verify_memory(memory_id)
            .expect("verification succeeds")
    }

    fn latest_proposal_kind(&self) -> ProposalKind {
        self.memory_store
            .with_connection(|conn| {
                let proposal_id: String = conn
                    .query_row(
                        "SELECT proposal_id
                 FROM consolidation_proposals
                 ORDER BY rowid DESC
                 LIMIT 1",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(|error| crate::LatticeError::Storage(error.to_string()))?;
                ConsolidationProposal::load(&conn, &proposal_id)
                    .map(|proposal| proposal.expect("proposal exists").proposal_kind)
            })
            .expect("canonical proposal loads")
    }

    fn proposal_count(&self) -> i64 {
        self.memory_store
            .with_connection(|conn| {
                conn.query_row("SELECT COUNT(*) FROM consolidation_proposals", [], |row| {
                    row.get(0)
                })
                .map_err(|error| crate::LatticeError::Storage(error.to_string()))
            })
            .expect("canonical proposal count reads")
    }

    #[allow(clippy::too_many_arguments)]
    fn verify_with_validations(
        &self,
        memory_id: &str,
        graph: &CodeGraph,
        file_index: &HashMap<String, FileIndexEntry>,
        parsed_files: &HashMap<String, ParsedFile>,
        validations: &[BehavioralValidationRecord],
        revision: Option<&str>,
        generation: Option<u64>,
    ) -> super::VerificationVerdict {
        let conn = Connection::open(&self.runtime_db).expect("runtime db opens");
        let mut runtime = ConsolidationJobRuntime::new(
            conn,
            ConsolidationConfig {
                max_queue_depth: 16,
                llm_budget_catalog: None,
            },
        )
        .expect("runtime opens");
        let reader = WorkspaceFileReader::new(self.workspace_root.clone());
        let authority = self.authority();
        VerifierCore::new(
            &self.memory_store,
            &mut runtime,
            graph,
            file_index,
            parsed_files,
            &reader,
            "workspace-main",
            &authority,
        )
        .with_behavioral_validations(
            validations,
            "repository-main",
            "checkout-main",
            revision,
            generation,
            300,
            120,
        )
        .verify_memory(memory_id)
        .expect("verification succeeds")
    }

    fn authority(&self) -> EvolutionAuthority<'static> {
        EvolutionAuthority {
            repository_id: "workspace-main",
            checkout_id: "checkout-main",
            branch: "main",
        }
    }

    fn graph_with_auth(&self, include_test: bool) -> CodeGraph {
        let mut graph = CodeGraph::new();
        let auth_symbol = crate::symbols::SymbolId {
            file: "src/auth.ts".to_string(),
            name: "loginUser".to_string(),
            byte_offset: 12,
        };
        graph.add_node(
            auth_symbol,
            SymbolKind::Function,
            "loginUser".to_string(),
            "fn loginUser()".to_string(),
            "fn loginUser() {}".to_string(),
            "src/auth.ts".to_string(),
            1,
            3,
            true,
            Language::TypeScript,
        );
        if include_test {
            let test_symbol = crate::symbols::SymbolId {
                file: "tests/auth.test.ts".to_string(),
                name: "loginUserTest".to_string(),
                byte_offset: 4,
            };
            graph.add_node(
                test_symbol,
                SymbolKind::Function,
                "loginUserTest".to_string(),
                "fn loginUserTest()".to_string(),
                "fn loginUserTest() {}".to_string(),
                "tests/auth.test.ts".to_string(),
                1,
                3,
                false,
                Language::TypeScript,
            );
        }
        graph
    }

    fn file_index(&self, entries: &[(&str, &str, i64)]) -> HashMap<String, FileIndexEntry> {
        entries
            .iter()
            .map(|(file, content_hash, last_indexed_at)| {
                (
                    (*file).to_string(),
                    FileIndexEntry {
                        file: (*file).to_string(),
                        content_hash: (*content_hash).to_string(),
                        mtime_ns: 0,
                        size_bytes: 0,
                        parser_version: FILE_INDEX_PARSER_VERSION,
                        schema_version: FILE_INDEX_SCHEMA_VERSION,
                        last_indexed_at: *last_indexed_at,
                    },
                )
            })
            .collect()
    }

    fn doc_parsed_files(&self, sections: &[(&str, &str, usize)]) -> HashMap<String, ParsedFile> {
        sections
            .iter()
            .map(|(path, heading, byte_offset)| {
                (
                    (*path).to_string(),
                    ParsedFile {
                        file: (*path).to_string(),
                        language: Language::Markdown,
                        imports: Vec::new(),
                        links: Vec::new(),
                        symbols: vec![Symbol {
                            id: crate::symbols::SymbolId {
                                file: (*path).to_string(),
                                name: (*heading).to_string(),
                                byte_offset: *byte_offset,
                            },
                            kind: SymbolKind::Section,
                            name: (*heading).to_string(),
                            signature: (*heading).to_string(),
                            body: String::new(),
                            file: (*path).to_string(),
                            line: 1,
                            end_line: 1,
                            is_exported: false,
                            language: Language::Markdown,
                            references: Vec::new(),
                            imports: Vec::new(),
                        }],
                    },
                )
            })
            .collect()
    }
}
