use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tempfile::TempDir;

use crate::events::{
    EventPayload, EventReader, EventStore, EventWriter, FlushPolicy, SessionId, SessionScope,
};
use crate::identity::{DocId, EventId, FileId, MemoryId, SectionId, SymbolId};
use crate::memory_graph::store::MemoryScopeState;
use crate::memory_graph::{
    initialize_schema, AssertionType, EvidenceAnchor, FreshnessKind, FreshnessPolicy,
    IdempotencyKey, MemoryClass, MemoryEvidence, MemoryEvidenceId, MemoryLinkReference,
    MemoryScope, MemoryStore, ScopeFilter,
};
use crate::DateTime;

pub struct TestHarness {
    _tempdir: TempDir,
    conn: Arc<Mutex<Connection>>,
    pub event_store: Arc<EventStore>,
    pub store: MemoryStore,
}

impl TestHarness {
    pub fn new() -> Self {
        let tempdir = TempDir::new().expect("tempdir creates");
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
            EventWriter::new(event_store.clone(), "workspace".to_string(), 4096)
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

    pub fn draft(&self, suffix: &str) -> TestDraftBuilder {
        TestDraftBuilder {
            suffix: suffix.to_string(),
            class: MemoryClass::Observation,
            assertion_type: AssertionType::Observation,
            confidence: 0.7,
            scope: branch_scope_state(),
            initial_evidence: Vec::new(),
            linked_files: vec![file_id("src/lib.rs")],
            linked_symbols: vec![symbol_id("memory::store")],
            linked_docs: vec![section_id("docs/spec.md")],
            supersession_links: Vec::new(),
        }
    }

    pub fn branch_scope(&self) -> ScopeFilter {
        ScopeFilter::branch("workspace", "main")
    }

    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("memory graph DB lock")
    }

    pub fn latest_event_payload(&self) -> EventPayload {
        self.tail_event_payloads(1)
            .into_iter()
            .next()
            .expect("tail event")
    }

    pub fn tail_event_payloads(&self, n: usize) -> Vec<EventPayload> {
        let reader = EventReader::new(self.event_store.clone());
        reader
            .tail(
                SessionScope {
                    session_id: SessionId {
                        value: "memory-graph".to_string(),
                    },
                },
                n,
            )
            .expect("tail query succeeds")
            .into_iter()
            .map(|event| event.payload)
            .collect()
    }
}

pub struct TestDraftBuilder {
    suffix: String,
    class: MemoryClass,
    assertion_type: AssertionType,
    confidence: f64,
    scope: MemoryScopeState,
    initial_evidence: Vec<MemoryEvidence>,
    linked_files: Vec<FileId>,
    linked_symbols: Vec<SymbolId>,
    linked_docs: Vec<SectionId>,
    supersession_links: Vec<MemoryLinkReference>,
}

impl TestDraftBuilder {
    pub fn with_class(mut self, class: MemoryClass, assertion_type: AssertionType) -> Self {
        self.class = class;
        self.assertion_type = assertion_type;
        self
    }

    pub fn with_confidence(mut self, confidence: f64) -> Self {
        self.confidence = confidence;
        self
    }

    pub fn with_initial_evidence(mut self, evidence: MemoryEvidence) -> Self {
        self.initial_evidence.push(evidence);
        self
    }

    pub fn with_linked_file(mut self, file: FileId) -> Self {
        self.linked_files.push(file);
        self
    }

    pub fn with_supersession_link(mut self, memory_id: MemoryId, reason: &str) -> Self {
        self.supersession_links.push(MemoryLinkReference {
            memory_id,
            reason: reason.to_string(),
        });
        self
    }

    pub fn build(self) -> crate::memory_graph::MemoryDraft {
        crate::memory_graph::MemoryDraft {
            content: format!("memory {}", self.suffix),
            class: self.class,
            assertion_type: self.assertion_type,
            scope: self.scope,
            confidence: self.confidence,
            confidence_reason: "seed".to_string(),
            freshness_policy: FreshnessPolicy {
                kind: FreshnessKind::BranchScoped,
                ttl: None,
                recheck_interval: None,
            },
            validity_conditions: Vec::new(),
            invalidation_triggers: Vec::new(),
            provenance_event_ids: Vec::new(),
            evidence_references: Vec::new(),
            linked_files: self.linked_files,
            linked_symbols: self.linked_symbols,
            linked_docs: self.linked_docs,
            linked_tests: Vec::new(),
            linked_memories: Vec::new(),
            contradiction_links: Vec::new(),
            supersession_links: self.supersession_links,
            access_history: Vec::new(),
            created_by: "assistant".to_string(),
            updated_by: "assistant".to_string(),
            superseded_by: None,
            schema_version: 1,
            initial_evidence: self.initial_evidence,
        }
    }
}

impl From<TestDraftBuilder> for crate::memory_graph::MemoryDraft {
    fn from(value: TestDraftBuilder) -> Self {
        value.build()
    }
}

pub fn branch_scope_state() -> MemoryScopeState {
    MemoryScopeState {
        scope: MemoryScope::Branch,
        scope_session_id: None,
        scope_branch: Some("main".to_string()),
        scope_workspace_id: Some("workspace".to_string()),
        scope_user_id: None,
        scope_org_id: None,
    }
}

pub fn idem(value: &str) -> IdempotencyKey {
    IdempotencyKey::new(value.to_string()).expect("idempotency key")
}

pub fn memory_evidence(id: &str) -> MemoryEvidence {
    MemoryEvidence {
        evidence_id: MemoryEvidenceId(id.to_string()),
        memory_id: memory_id("placeholder"),
        event_id: Some(event_id(id)),
        anchor: EvidenceAnchor::EventReference(event_id(id)),
        captured_at: DateTime::from_unix_seconds(100),
        captured_by: crate::events::Actor::Daemon,
    }
}

pub fn row_count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .expect("row count query")
}

pub fn file_id(path: &str) -> FileId {
    FileId {
        workspace_id: "workspace".to_string(),
        repo_relative_path: path.to_string(),
        content_hash: "deadbeef".to_string(),
    }
}

pub fn symbol_id(name: &str) -> SymbolId {
    SymbolId {
        file: file_id("src/lib.rs"),
        qualified_name: name.to_string(),
        byte_offset: 12,
        kind: "function".to_string(),
    }
}

pub fn section_id(path: &str) -> SectionId {
    SectionId {
        doc: DocId {
            workspace_id: "workspace".to_string(),
            repo_relative_path: path.to_string(),
            content_hash: "feedface".to_string(),
        },
        heading_path: vec!["Guide".to_string(), "Section".to_string()],
        byte_offset: 44,
    }
}

pub fn event_id(seed: &str) -> EventId {
    EventId {
        workspace_id: "workspace".to_string(),
        ulid: stable_ulid(seed),
    }
}

pub fn memory_id(seed: &str) -> MemoryId {
    MemoryId {
        workspace_id: "workspace".to_string(),
        ulid: stable_ulid(seed),
    }
}

pub fn stable_ulid(seed: &str) -> String {
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
