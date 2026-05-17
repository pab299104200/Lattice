//! Shared fixtures, harness, and identity factories for the R26 cross-layer
//! contract tests in `identity_event_memory.rs`.
//!
//! These helpers exist so the test file can stay focused on the contract
//! surfaces it pins. Per the Cadres coding standard `## Hard limits`, this
//! support module keeps the test file under the 800-line ceiling without
//! losing the per-surface organization that makes the gate readable.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tempfile::TempDir;

use crate::events::{
    Actor, EventEnvelope, EventQuery, EventReader, EventStore, EventWriter, FlushPolicy,
    PartialEnvelope, QueryOrder,
};
use crate::graph::CodeGraph;
use crate::identity::{
    ContextHandleId, DocId, EventId, FileId, IdentityResolver, MemoryId, SectionId, SymbolId,
};
use crate::memory_graph::store::MemoryScopeState;
use crate::memory_graph::{
    initialize_schema, AssertionType, EvidenceAnchor, FreshnessKind, FreshnessPolicy,
    IdempotencyKey, MemoryClass, MemoryDraft, MemoryEvidence, MemoryEvidenceId, MemoryScope,
    MemoryStore,
};
use crate::parser::parse_file;
use crate::storage::graph_store::{
    FileIndexEntry, FILE_INDEX_PARSER_VERSION, FILE_INDEX_SCHEMA_VERSION,
};
use crate::DateTime;

pub(super) const WORKSPACE: &str = "workspace";
pub(super) const BRANCH: &str = "main";
pub(super) const SESSION: &str = "contract-session";

// ---------------------------------------------------------------------------
// Identity factories
// ---------------------------------------------------------------------------

pub(super) fn file_identity(path: &str) -> FileId {
    FileId {
        workspace_id: WORKSPACE.to_string(),
        repo_relative_path: path.to_string(),
        content_hash: "feedface00000001".to_string(),
    }
}

pub(super) fn doc_identity(path: &str) -> DocId {
    DocId {
        workspace_id: WORKSPACE.to_string(),
        repo_relative_path: path.to_string(),
        content_hash: "deadbeef00000002".to_string(),
    }
}

pub(super) fn section_identity(path: &str, heading: &str) -> SectionId {
    SectionId {
        doc: doc_identity(path),
        heading_path: vec![heading.to_string()],
        byte_offset: 64,
    }
}

pub(super) fn symbol_identity(file: &FileId, qualified: &str) -> SymbolId {
    SymbolId {
        file: file.clone(),
        qualified_name: qualified.to_string(),
        byte_offset: 12,
        kind: "function".to_string(),
    }
}

pub(super) fn context_handle_identity(seed: &str) -> ContextHandleId {
    ContextHandleId {
        workspace_id: WORKSPACE.to_string(),
        session_id: SESSION.to_string(),
        ulid: stable_ulid(seed),
    }
}

pub(super) fn memory_identity(seed: &str) -> MemoryId {
    MemoryId {
        workspace_id: WORKSPACE.to_string(),
        ulid: stable_ulid(seed),
    }
}

pub(super) fn stable_ulid(seed: &str) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut bytes = [b'0'; 26];
    bytes[..24].copy_from_slice(b"01ARZ3NDEKTSV4RRFFQ69G5F");
    let checksum = seed.bytes().fold(0_u32, |acc, byte| {
        acc.wrapping_mul(33).wrapping_add(u32::from(byte))
    });
    bytes[24] = ALPHABET[((checksum >> 5) & 31) as usize];
    bytes[25] = ALPHABET[(checksum & 31) as usize];
    String::from_utf8(bytes.to_vec()).expect("stable ULID is valid ASCII")
}

pub(super) fn branch_scope_state() -> MemoryScopeState {
    MemoryScopeState {
        scope: MemoryScope::Branch,
        scope_session_id: None,
        scope_branch: Some(BRANCH.to_string()),
        scope_workspace_id: Some(WORKSPACE.to_string()),
        scope_user_id: None,
        scope_org_id: None,
    }
}

pub(super) fn idem(value: &str) -> IdempotencyKey {
    IdempotencyKey::new(value.to_string()).expect("idempotency key")
}

// ---------------------------------------------------------------------------
// In-process collaborator harness
// ---------------------------------------------------------------------------

/// Wiring that mirrors `memory_graph::tests_common::TestHarness` but lives
/// inside `contract_tests` so the cross-layer surface is exercised without
/// re-exporting a private per-substrate helper.
pub(super) struct ContractHarness {
    _tempdir: TempDir,
    pub(super) conn: Arc<Mutex<Connection>>,
    pub(super) event_store: Arc<EventStore>,
    pub(super) store: MemoryStore,
}

impl ContractHarness {
    pub(super) fn new() -> Self {
        let tempdir = TempDir::new().expect("contract tempdir");
        let conn = Connection::open(tempdir.path().join("memory-graph.db"))
            .expect("memory graph DB opens");
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .expect("foreign keys enable");
        initialize_schema(&conn).expect("memory graph schema initializes");
        let conn = Arc::new(Mutex::new(conn));
        let event_store = Arc::new(
            EventStore::open(&tempdir.path().join("events.db")).expect("event store opens"),
        );
        let writer = Arc::new(
            EventWriter::new(event_store.clone(), WORKSPACE.to_string(), 4096)
                .with_flush_policy(FlushPolicy::Sync),
        );
        let store = MemoryStore::open(conn.clone(), writer);
        Self {
            _tempdir: tempdir,
            conn,
            event_store,
            store,
        }
    }

    pub(super) fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("contract harness DB lock")
    }

    pub(super) fn writer(&self) -> EventWriter {
        EventWriter::new(self.event_store.clone(), WORKSPACE.to_string(), 4096)
            .with_flush_policy(FlushPolicy::Sync)
    }

    pub(super) fn reader(&self) -> EventReader {
        EventReader::new(self.event_store.clone())
    }

    pub(super) fn all_memory_events(&self) -> Vec<EventEnvelope> {
        self.reader()
            .execute(
                EventQuery::new()
                    .session("memory-graph")
                    .workspace(WORKSPACE)
                    .branch(BRANCH)
                    .order(QueryOrder::OldestFirst)
                    .limit(200),
            )
            .expect("memory graph events load")
    }
}

pub(super) fn append_event(writer: &EventWriter, partial: PartialEnvelope) -> EventId {
    writer.append(partial).expect("event appends")
}

/// Convenience constructor for the workspace/branch/session-scoped envelopes
/// every cross-layer test in this gate uses. Surfaces stay readable when the
/// test body shows only the kind, references, and typed payload that
/// distinguish it.
pub(super) fn daemon_envelope(
    kind: crate::events::EventKind,
    references: Vec<crate::events::StableRef>,
    summary: &str,
    payload: crate::events::EventPayload,
) -> PartialEnvelope {
    PartialEnvelope {
        workspace_id: Some(WORKSPACE.to_string()),
        branch: crate::events::BranchRef {
            name: BRANCH.to_string(),
        },
        session_id: crate::events::SessionId {
            value: SESSION.to_string(),
        },
        task_id: None,
        actor: Actor::Daemon,
        kind,
        references,
        summary: crate::events::CompactSummary::new(summary.to_string())
            .expect("contract summary fits"),
        payload,
    }
}

pub(super) fn read_back(reader: &EventReader, event_id: &EventId) -> EventEnvelope {
    let envelopes = reader
        .execute(
            EventQuery::new()
                .workspace(WORKSPACE)
                .branch(BRANCH)
                .order(QueryOrder::OldestFirst)
                .limit(100),
        )
        .expect("workspace-scoped read succeeds");
    envelopes
        .into_iter()
        .find(|envelope| envelope.event_id == *event_id)
        .expect("appended event is visible to the reader")
}

pub(super) fn seed_draft(file: &FileId, symbol: &SymbolId, section: &SectionId) -> MemoryDraft {
    MemoryDraft {
        content: "round-trip seed".to_string(),
        class: MemoryClass::Observation,
        assertion_type: AssertionType::Observation,
        scope: branch_scope_state(),
        confidence: 0.8,
        confidence_reason: "contract seed".to_string(),
        freshness_policy: FreshnessPolicy {
            kind: FreshnessKind::BranchScoped,
            ttl: None,
            recheck_interval: None,
        },
        validity_conditions: Vec::new(),
        invalidation_triggers: Vec::new(),
        provenance_event_ids: Vec::new(),
        evidence_references: Vec::new(),
        linked_files: vec![file.clone()],
        linked_symbols: vec![symbol.clone()],
        linked_docs: vec![section.clone()],
        linked_tests: Vec::new(),
        linked_memories: Vec::new(),
        contradiction_links: Vec::new(),
        supersession_links: Vec::new(),
        access_history: Vec::new(),
        created_by: "assistant".to_string(),
        updated_by: "assistant".to_string(),
        superseded_by: None,
        schema_version: 1,
        initial_evidence: vec![file_span_evidence(file)],
    }
}

pub(super) fn file_span_evidence(file: &FileId) -> MemoryEvidence {
    MemoryEvidence {
        evidence_id: MemoryEvidenceId(String::new()),
        memory_id: memory_identity("placeholder"),
        event_id: None,
        anchor: EvidenceAnchor::FileSpan {
            file: file.clone(),
            byte_start: 0,
            byte_end: 32,
            sha256: [0_u8; 32],
        },
        captured_at: DateTime::from_unix_seconds(1_700_000_000),
        captured_by: Actor::Daemon,
    }
}

pub(super) fn doc_section_evidence(section: &SectionId) -> MemoryEvidence {
    MemoryEvidence {
        evidence_id: MemoryEvidenceId(String::new()),
        memory_id: memory_identity("placeholder"),
        event_id: None,
        anchor: EvidenceAnchor::DocSection {
            id: section.clone(),
            sha256: [1_u8; 32],
        },
        captured_at: DateTime::from_unix_seconds(1_700_000_010),
        captured_by: Actor::Daemon,
    }
}

// ---------------------------------------------------------------------------
// Legacy memory source used by the migration round-trip tests.
// ---------------------------------------------------------------------------

pub(super) fn legacy_source_conn() -> Connection {
    let conn = Connection::open_in_memory().expect("legacy source DB");
    conn.execute_batch(
        "CREATE TABLE memories (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL DEFAULT '',
            content TEXT NOT NULL,
            memory_type TEXT NOT NULL,
            scope TEXT NOT NULL DEFAULT 'session',
            confidence REAL NOT NULL DEFAULT 1.0,
            linked_symbols TEXT NOT NULL DEFAULT '[]',
            linked_files TEXT NOT NULL DEFAULT '[]',
            workspace_id TEXT,
            branch TEXT,
            refresh_key TEXT,
            source_query TEXT,
            assertion_type TEXT NOT NULL DEFAULT 'observation',
            verification_status TEXT NOT NULL DEFAULT 'unverified',
            confidence_reason TEXT,
            supersedes_memory_id TEXT,
            superseded_by_memory_id TEXT,
            contradicts_memory_ids TEXT NOT NULL DEFAULT '[]',
            contradicted_by_memory_ids TEXT NOT NULL DEFAULT '[]',
            freshness_policy TEXT NOT NULL DEFAULT 'session_scoped',
            freshness_policy_detail TEXT,
            provenance_json TEXT NOT NULL DEFAULT '[]',
            evidence_json TEXT NOT NULL DEFAULT '[]',
            created_at INTEGER NOT NULL,
            last_accessed INTEGER NOT NULL,
            access_count INTEGER NOT NULL DEFAULT 0,
            is_stale INTEGER NOT NULL DEFAULT 0,
            stale_reason TEXT,
            is_invalidated INTEGER NOT NULL DEFAULT 0
        );",
    )
    .expect("legacy schema");
    conn
}

pub(super) fn insert_legacy_row(conn: &Connection, id: &str, linked_files_json: &str) {
    conn.execute(
        "INSERT INTO memories
            (id, session_id, content, memory_type, scope, confidence, linked_symbols,
             linked_files, workspace_id, branch, refresh_key, source_query, assertion_type,
             verification_status, confidence_reason, supersedes_memory_id,
             superseded_by_memory_id, contradicts_memory_ids, contradicted_by_memory_ids,
             freshness_policy, freshness_policy_detail, provenance_json, evidence_json,
             created_at, last_accessed, access_count, is_stale, stale_reason, is_invalidated)
         VALUES
            (?1, 'legacy-session', ?2, 'observation', 'repo', 0.7, '[]', ?3, ?4, 'main',
             NULL, NULL, 'observation', 'unverified', NULL, NULL, NULL,
             '[]', '[]', 'repo_scoped', NULL, '[]', '[]', 1700000000, 1700000000, 0, 0, NULL, 0)",
        rusqlite::params![id, format!("memory {id}"), linked_files_json, WORKSPACE],
    )
    .expect("legacy row inserts");
}

// ---------------------------------------------------------------------------
// Resolver fixture builders for the rename and end-to-end round-trip tests.
// ---------------------------------------------------------------------------

pub(super) fn build_empty_resolver(
    workspace: &str,
    events: Vec<EventId>,
) -> IdentityResolver<'static> {
    let graph = Box::leak(Box::new(CodeGraph::new()));
    let file_index: HashMap<String, FileIndexEntry> = HashMap::new();
    let parsed_files: HashMap<String, crate::symbols::ParsedFile> = HashMap::new();
    IdentityResolver::new(
        graph,
        Box::leak(Box::new(file_index)),
        Box::leak(Box::new(parsed_files)),
        workspace.to_string(),
        events,
    )
}

pub(super) fn build_filesystem_resolver(
    default_workspace: &str,
    files: &[(&str, &str, &str)],
) -> IdentityResolver<'static> {
    let mut graph = CodeGraph::new();
    let mut file_index: HashMap<String, FileIndexEntry> = HashMap::new();
    let mut parsed_files: HashMap<String, crate::symbols::ParsedFile> = HashMap::new();
    for (workspace, path, source) in files.iter() {
        let stored_path = format!("{}/{}", workspace, path);
        let parsed = parse_file(&stored_path, source).expect("fixture parses");
        for symbol in &parsed.symbols {
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
        parsed_files.insert(stored_path.clone(), parsed);
        file_index.insert(
            stored_path.clone(),
            FileIndexEntry {
                file: stored_path.clone(),
                content_hash: deterministic_hash(source),
                mtime_ns: 0,
                size_bytes: source.len() as i64,
                parser_version: FILE_INDEX_PARSER_VERSION,
                schema_version: FILE_INDEX_SCHEMA_VERSION,
                last_indexed_at: 0,
            },
        );
    }
    IdentityResolver::new(
        Box::leak(Box::new(graph)),
        Box::leak(Box::new(file_index)),
        Box::leak(Box::new(parsed_files)),
        default_workspace.to_string(),
        Vec::new(),
    )
}

fn deterministic_hash(source: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in source.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
