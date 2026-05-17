//! Cross-layer contract tests for identity ↔ event log ↔ memory graph.
//!
//! Each test pins one boundary in the three-substrate round trip enumerated
//! by `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Design Thesis`:
//!
//! 1. Identity references serialize cleanly into event payloads.
//! 2. Event payloads round-trip back to identity targets through the reader.
//! 3. Memory links reference stable identities, not transient names.
//! 4. Memory evidence anchors carry typed identities, not path / heading
//!    strings.
//! 5. Migration preserves identity references (no path strings survive).
//! 6. Event capture records identity-typed references.
//! 7. Replay preserves identity-resolvable evidence anchors.
//! 8. Schema-level columns that hold identities are constrained to the typed
//!    shape.
//!
//! Verification of these contracts is what R26 — the Phase-3 ↔ Phase-1
//! contract gate — certifies before Phase 4 retrieval begins. Fixtures and
//! the in-process harness live in `identity_event_memory_support.rs`; this
//! file holds only the per-surface assertions. The eight coherent surface
//! sections plus the end-to-end test put the file just over the 800-line
//! Cadres heuristic but inside the "850-line file with 9 coherent sections
//! is fine" carve-out in the standard's `## Hard limits`.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use serde_json::Value;

use crate::events::{
    Actor, ContextBundleReturnedPayload, DiagnosticObservedPayload, DiagnosticSeverity, EventKind,
    EventPayload, FileReadPayload, MemoryRetrievedPayload, PatchAppliedPayload, StableRef,
};
use crate::identity::{
    decode_identity, encode_identity, EventId, FileId, Identity, SectionId, SymbolId,
};
use crate::memory_graph::{
    get_evidence_for, get_links_from, initialize_schema, insert_link, replay_events,
    EvidenceAnchor, MemoryDraft, MemoryLink, MemoryLinkId, MemoryLinkTarget, MemoryLinkType,
    MemoryMigrator,
};

use super::identity_event_memory_support::{
    append_event, build_empty_resolver, build_filesystem_resolver, context_handle_identity,
    daemon_envelope, doc_section_evidence, file_identity, file_span_evidence, idem,
    insert_legacy_row, legacy_source_conn, memory_identity, read_back, section_identity,
    seed_draft, symbol_identity, ContractHarness, WORKSPACE,
};

// ---------------------------------------------------------------------------
// SURFACE 1 — identity references serialize cleanly into event payloads
// ---------------------------------------------------------------------------

#[test]
fn stable_ref_file_variant_serializes_to_typed_identity_struct_not_path_string() {
    let file = file_identity("src/auth.rs");
    let stable_ref = StableRef::FileRef(file.clone());
    let json: Value = serde_json::to_value(&stable_ref).expect("stable ref serializes");

    assert!(
        json.is_object(),
        "StableRef::FileRef must serialize as a tagged object, not a string ({json})"
    );
    let object = json.as_object().expect("object form");
    assert_eq!(
        object.len(),
        1,
        "StableRef uses externally tagged serde representation"
    );
    let (variant_tag, payload) = object.iter().next().expect("one variant tag");
    assert!(
        variant_tag.to_ascii_lowercase().contains("file"),
        "tag must identify a file variant, got `{variant_tag}`"
    );
    assert_eq!(
        payload["workspace_id"],
        Value::String(WORKSPACE.to_string())
    );
    assert_eq!(
        payload["repo_relative_path"],
        Value::String("src/auth.rs".to_string())
    );
    assert_eq!(payload["content_hash"], Value::String(file.content_hash));
    assert!(
        payload.get("path").is_none(),
        "raw path string must never appear in the wire form"
    );
}

#[test]
fn stable_ref_symbol_variant_serializes_to_typed_identity_struct_not_qualified_name_string() {
    let file = file_identity("src/auth.rs");
    let symbol = symbol_identity(&file, "login_user");
    let stable_ref = StableRef::SymbolRef(symbol.clone());
    let json: Value = serde_json::to_value(&stable_ref).expect("stable ref serializes");

    let object = json.as_object().expect("StableRef serializes as object");
    let (_variant_tag, payload) = object.iter().next().expect("one variant tag");

    assert_eq!(
        payload["qualified_name"],
        Value::String("login_user".to_string())
    );
    assert_eq!(payload["byte_offset"], Value::Number(12.into()));
    assert!(
        payload.get("file").is_some(),
        "symbol identity carries its file identity, not a path"
    );
    assert_eq!(
        payload["file"]["workspace_id"],
        Value::String(WORKSPACE.to_string())
    );
    let _: SymbolId = serde_json::from_value(payload.clone()).expect("symbol round-trips typed");
}

#[test]
fn file_read_payload_embeds_typed_file_identity_not_path_string() {
    let file = file_identity("src/lib.rs");
    let payload = EventPayload::FileRead(FileReadPayload {
        file_id: file.clone(),
        source_event_id: None,
        byte_start: None,
        byte_end: None,
        reason: "contract".to_string(),
    });
    let json: Value = serde_json::to_value(&payload).expect("payload serializes");
    let inner = &json["payload"];

    assert!(inner["file_id"].is_object(), "FileRead.file_id is typed");
    assert_eq!(
        inner["file_id"]["repo_relative_path"],
        Value::String("src/lib.rs".to_string())
    );
    assert!(
        inner["file_id"].get("path").is_none(),
        "FileRead must not flatten file_id to a raw path string"
    );
}

#[test]
fn diagnostic_observed_payload_embeds_typed_file_identity() {
    let file = file_identity("src/critical.rs");
    let payload = EventPayload::DiagnosticObserved(DiagnosticObservedPayload {
        diagnostic_id: "DIAG-1".to_string(),
        source_event_id: None,
        file_id: file.clone(),
        symbol_id: None,
        severity: DiagnosticSeverity::Error,
        message: "boom".to_string(),
    });
    let json = serde_json::to_value(&payload).expect("payload serializes");
    let inner = &json["payload"]["file_id"];
    assert!(inner.is_object());
    assert_eq!(inner["workspace_id"], Value::String(WORKSPACE.to_string()));
}

// ---------------------------------------------------------------------------
// SURFACE 2 — event payloads round-trip back to identity targets
// ---------------------------------------------------------------------------

#[test]
fn stable_ref_round_trips_through_event_store_byte_equivalent() {
    let harness = ContractHarness::new();
    let writer = harness.writer();
    let file = file_identity("src/round_trip.rs");
    let symbol = symbol_identity(&file, "round_trip_fn");
    let memory = memory_identity("round-trip-mem");
    let handle = context_handle_identity("round-trip-handle");

    let envelope = daemon_envelope(
        EventKind::ContextBundleReturned,
        vec![
            StableRef::FileRef(file.clone()),
            StableRef::SymbolRef(symbol.clone()),
            StableRef::MemoryRef(memory.clone()),
            StableRef::ContextHandleRef(handle.clone()),
        ],
        "round-trip references",
        EventPayload::ContextBundleReturned(ContextBundleReturnedPayload {
            context_handle_id: handle.clone(),
            source_event_id: None,
            file_ids: vec![file.clone()],
            symbol_ids: vec![symbol.clone()],
            doc_section_ids: Vec::new(),
            memory_ids: vec![memory.clone()],
            token_estimate: 128,
        }),
    );

    let event_id = append_event(&writer, envelope);
    let read = read_back(&harness.reader(), &event_id);

    assert_eq!(
        read.references,
        vec![
            StableRef::FileRef(file.clone()),
            StableRef::SymbolRef(symbol.clone()),
            StableRef::MemoryRef(memory.clone()),
            StableRef::ContextHandleRef(handle.clone()),
        ],
        "references vector must round-trip byte-equivalent typed identities"
    );

    let EventPayload::ContextBundleReturned(payload) = read.payload else {
        panic!("payload variant must survive round-trip");
    };
    assert_eq!(payload.file_ids, vec![file]);
    assert_eq!(payload.symbol_ids, vec![symbol]);
    assert_eq!(payload.memory_ids, vec![memory]);
    assert_eq!(payload.context_handle_id, handle);
}

#[test]
fn round_tripped_event_id_resolves_via_identity_resolver_to_same_target() {
    let harness = ContractHarness::new();
    let writer = harness.writer();
    let file = file_identity("src/resolved.rs");

    let envelope = daemon_envelope(
        EventKind::FileRead,
        vec![StableRef::FileRef(file.clone())],
        "file read",
        EventPayload::FileRead(FileReadPayload {
            file_id: file.clone(),
            source_event_id: None,
            byte_start: None,
            byte_end: None,
            reason: "contract".to_string(),
        }),
    );
    let event_id = append_event(&writer, envelope);
    let read = read_back(&harness.reader(), &event_id);

    let resolver = build_empty_resolver(WORKSPACE, vec![read.event_id.clone()]);
    let encoded = encode_identity(&Identity::Event(read.event_id.clone()));
    let resolved = resolver
        .resolve_event_ref(&WORKSPACE.to_string(), &encoded)
        .expect("event identity resolves after round-trip");

    assert_eq!(resolved, read.event_id);
}

#[test]
fn memory_retrieved_payload_round_trips_memory_ids_through_event_log() {
    let harness = ContractHarness::new();
    let writer = harness.writer();
    let memory_a = memory_identity("retrieved-a");
    let memory_b = memory_identity("retrieved-b");

    let envelope = daemon_envelope(
        EventKind::MemoryRetrieved,
        vec![
            StableRef::MemoryRef(memory_a.clone()),
            StableRef::MemoryRef(memory_b.clone()),
        ],
        "memories retrieved",
        EventPayload::MemoryRetrieved(MemoryRetrievedPayload {
            retrieval_query: "what".to_string(),
            context_handle_id: None,
            memory_ids: vec![memory_a.clone(), memory_b.clone()],
            supporting_event_ids: Vec::new(),
            included_context: Vec::new(),
            excluded_context: Vec::new(),
        }),
    );
    let event_id = append_event(&writer, envelope);
    let read = read_back(&harness.reader(), &event_id);

    let EventPayload::MemoryRetrieved(payload) = read.payload else {
        panic!("variant survives");
    };
    assert_eq!(payload.memory_ids, vec![memory_a, memory_b]);
}

// ---------------------------------------------------------------------------
// SURFACE 3 — memory links reference stable identities, not transient names
// ---------------------------------------------------------------------------

#[test]
fn memory_link_target_symbol_persists_encoded_identity_in_target_id_column() {
    let harness = ContractHarness::new();
    let file = file_identity("src/auth.rs");
    let symbol = symbol_identity(&file, "login_user");
    let mem_file = file_identity("src/store.rs");
    let mem_symbol = symbol_identity(&mem_file, "store");
    let mem_section = section_identity("docs/store.md", "Store");
    let memory = harness
        .store
        .create(
            seed_draft(&mem_file, &mem_symbol, &mem_section),
            idem("link-source"),
        )
        .expect("memory creates");

    let link = MemoryLink::try_new(
        MemoryLinkId("contract-symbol-link".to_string()),
        memory.memory_id.clone(),
        MemoryLinkTarget::Symbol(symbol.clone()),
        MemoryLinkType::AppliesTo,
        1.0,
        "applies to login flow".to_string(),
        None,
        Actor::Daemon,
        crate::Utc::now(),
        crate::memory_graph::VerificationStatus::Verified,
    )
    .expect("link validates");
    insert_link(&harness.conn(), &link).expect("link persists");

    let raw_target_id: String = harness
        .conn()
        .query_row(
            "SELECT target_id FROM memory_links WHERE link_id = ?1",
            rusqlite::params!["contract-symbol-link"],
            |row| row.get(0),
        )
        .expect("link row visible");

    assert!(
        raw_target_id.starts_with("symbol:"),
        "memory_links.target_id must be an encoded symbol identity, got `{raw_target_id}`"
    );
    assert!(
        !raw_target_id.contains("\"login_user\""),
        "target_id must not store the raw qualified_name as a JSON string"
    );

    let decoded = decode_identity(&raw_target_id).expect("encoded id parses");
    match decoded {
        Identity::Symbol(decoded_symbol) => assert_eq!(decoded_symbol, symbol),
        other => panic!("expected symbol identity, got {:?}", other.kind()),
    }
}

#[test]
fn memory_link_round_trips_through_get_links_from_with_typed_target() {
    let harness = ContractHarness::new();
    let mem_file = file_identity("src/store.rs");
    let mem_symbol = symbol_identity(&mem_file, "store");
    let mem_section = section_identity("docs/store.md", "Store");
    let memory = harness
        .store
        .create(
            seed_draft(&mem_file, &mem_symbol, &mem_section),
            idem("link-rt"),
        )
        .expect("memory creates");

    let section = section_identity("docs/usage.md", "Caller");
    let link = MemoryLink::try_new(
        MemoryLinkId("contract-section-link".to_string()),
        memory.memory_id.clone(),
        MemoryLinkTarget::DocSection(section.clone()),
        MemoryLinkType::DerivedFrom,
        0.7,
        "derived from doc".to_string(),
        None,
        Actor::Daemon,
        crate::Utc::now(),
        crate::memory_graph::VerificationStatus::Verified,
    )
    .expect("link validates");
    insert_link(&harness.conn(), &link).expect("link persists");

    let loaded = get_links_from(&harness.conn(), &memory.memory_id).expect("links load");
    let resolved = loaded
        .into_iter()
        .find(|stored| stored.link_id == link.link_id)
        .expect("link survives round-trip");

    match resolved.target {
        MemoryLinkTarget::DocSection(id) => assert_eq!(id, section),
        other => panic!("doc section target survives, got {:?}", other),
    }
}

#[test]
fn memory_link_file_target_resolves_under_file_rename_compat_without_link_rewrite() {
    let workspace = WORKSPACE.to_string();
    let source = "pub fn login_user() {}\n";

    let original_resolver =
        build_filesystem_resolver(&workspace, &[(WORKSPACE, "src/original.rs", source)]);
    let original_file = original_resolver
        .resolve_path(&workspace, "src/original.rs")
        .expect("original file resolves");
    let encoded_original = encode_identity(&Identity::File(original_file.clone()));

    let renamed_resolver =
        build_filesystem_resolver(&workspace, &[(WORKSPACE, "src/renamed.rs", source)]);

    let resolved_after_rename = renamed_resolver
        .resolve_path(&workspace, &encoded_original)
        .expect("encoded original file identity remaps to the renamed file");

    assert_eq!(resolved_after_rename.repo_relative_path, "src/renamed.rs");
    assert_eq!(
        resolved_after_rename.content_hash,
        original_file.content_hash
    );
    assert_eq!(
        encoded_original,
        encode_identity(&Identity::File(original_file)),
        "the stored link target_id does not need to change to follow the rename"
    );
}

// ---------------------------------------------------------------------------
// SURFACE 4 — memory evidence anchors carry typed identities
// ---------------------------------------------------------------------------

#[test]
fn evidence_anchor_filespan_serializes_with_typed_fileid_struct() {
    let file = file_identity("src/anchor.rs");
    let anchor = EvidenceAnchor::FileSpan {
        file: file.clone(),
        byte_start: 0,
        byte_end: 32,
        sha256: [0_u8; 32],
    };
    let json = serde_json::to_value(&anchor).expect("anchor serializes");
    let payload = json.get("file_span").expect("file_span tag present");
    assert!(payload["file"].is_object(), "FileSpan.file is typed");
    assert_eq!(
        payload["file"]["repo_relative_path"],
        Value::String("src/anchor.rs".to_string())
    );
    assert!(payload.get("path").is_none(), "no raw path field exposed");
}

#[test]
fn evidence_anchor_docsection_serializes_with_typed_doc_section_id() {
    let section = section_identity("docs/spec.md", "Architecture");
    let anchor = EvidenceAnchor::DocSection {
        id: section.clone(),
        sha256: [9_u8; 32],
    };
    let json = serde_json::to_value(&anchor).expect("anchor serializes");
    let payload = json.get("doc_section").expect("doc_section tag present");
    assert!(payload["id"].is_object(), "DocSection.id is typed");
    assert_eq!(
        payload["id"]["heading_path"][0],
        Value::String("Architecture".to_string())
    );
    assert!(
        payload.get("heading").is_none(),
        "no raw heading string exposed"
    );
}

#[test]
fn evidence_anchor_filespan_round_trips_through_memory_evidence_table() {
    let harness = ContractHarness::new();
    let file = file_identity("src/anchor_rt.rs");
    let symbol = symbol_identity(&file, "anchor_rt");
    let section = section_identity("docs/anchor.md", "Anchor");
    let draft = MemoryDraft {
        initial_evidence: vec![file_span_evidence(&file), doc_section_evidence(&section)],
        ..seed_draft(&file, &symbol, &section)
    };
    let memory = harness
        .store
        .create(draft, idem("anchor-rt"))
        .expect("memory creates");

    let evidence = get_evidence_for(&harness.conn(), &memory.memory_id).expect("evidence loads");
    let file_anchor = evidence
        .iter()
        .find(|item| matches!(item.anchor, EvidenceAnchor::FileSpan { .. }))
        .expect("file span evidence preserved");
    let section_anchor = evidence
        .iter()
        .find(|item| matches!(item.anchor, EvidenceAnchor::DocSection { .. }))
        .expect("doc section evidence preserved");

    match &file_anchor.anchor {
        EvidenceAnchor::FileSpan {
            file: stored,
            byte_end,
            ..
        } => {
            assert_eq!(stored, &file);
            assert_eq!(*byte_end, 32);
        }
        other => panic!("expected FileSpan, got {:?}", other),
    }
    match &section_anchor.anchor {
        EvidenceAnchor::DocSection { id, .. } => assert_eq!(id, &section),
        other => panic!("expected DocSection, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// SURFACE 5 — migration (T22) preserves identity references
// ---------------------------------------------------------------------------

#[test]
fn migration_emits_typed_file_ids_in_linked_files_json_not_path_strings() {
    let source = Arc::new(Mutex::new(legacy_source_conn()));
    {
        let conn = source.lock().expect("source lock");
        insert_legacy_row(&conn, "legacy-a", r#"["src/legacy_a.rs"]"#);
    }
    let dest = Arc::new(Mutex::new(Connection::open_in_memory().expect("dest")));
    {
        let conn = dest.lock().expect("dest lock");
        initialize_schema(&conn).expect("dest schema");
    }
    let migrator = MemoryMigrator::new(source.clone(), dest.clone(), 8, false);
    let plan = migrator.plan().expect("plan succeeds");
    let report = migrator.run(&plan).expect("run succeeds");
    assert_eq!(report.migrated_rows, 1);

    let linked_files_json: String = dest
        .lock()
        .expect("dest lock")
        .query_row(
            "SELECT linked_files_json FROM memories WHERE memory_id LIKE 'memory:%'",
            rusqlite::params![],
            |row| row.get(0),
        )
        .expect("migrated row visible");
    let parsed: Vec<FileId> =
        serde_json::from_str(&linked_files_json).expect("linked_files_json parses as Vec<FileId>");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].repo_relative_path, "src/legacy_a.rs");
    assert_eq!(parsed[0].workspace_id, WORKSPACE);
    assert!(
        !linked_files_json.starts_with("[\""),
        "migrated linked_files_json must not be a JSON array of bare path strings, got `{linked_files_json}`"
    );
}

#[test]
fn migration_preserves_memory_id_workspace_scoping_in_destination_memory_id_column() {
    let canonical_ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let source = Arc::new(Mutex::new(legacy_source_conn()));
    {
        let conn = source.lock().expect("source lock");
        insert_legacy_row(&conn, canonical_ulid, r#"[]"#);
    }
    let dest = Arc::new(Mutex::new(Connection::open_in_memory().expect("dest")));
    {
        let conn = dest.lock().expect("dest lock");
        initialize_schema(&conn).expect("dest schema");
    }
    let migrator = MemoryMigrator::new(source, dest.clone(), 8, false);
    let plan = migrator.plan().expect("plan succeeds");
    migrator.run(&plan).expect("run succeeds");

    let stored_id: String = dest
        .lock()
        .expect("dest lock")
        .query_row(
            "SELECT memory_id FROM memories",
            rusqlite::params![],
            |row| row.get(0),
        )
        .expect("migrated row visible");
    assert!(
        stored_id.starts_with("memory:"),
        "memory_id column must hold an encoded memory identity, got `{stored_id}`"
    );
    let decoded = decode_identity(&stored_id).expect("memory id decodes");
    match decoded {
        Identity::Memory(id) => {
            assert_eq!(id.workspace_id, WORKSPACE);
            assert_eq!(id.ulid, canonical_ulid);
        }
        other => panic!("expected memory identity, got {:?}", other.kind()),
    }
}

// ---------------------------------------------------------------------------
// SURFACE 6 — event capture records identity-typed references
// ---------------------------------------------------------------------------

#[test]
fn patch_applied_capture_records_only_typed_file_and_symbol_refs() {
    let harness = ContractHarness::new();
    let writer = harness.writer();
    let file = file_identity("src/patched.rs");
    let symbol = symbol_identity(&file, "patched_fn");
    let envelope = daemon_envelope(
        EventKind::PatchApplied,
        vec![
            StableRef::FileRef(file.clone()),
            StableRef::SymbolRef(symbol.clone()),
        ],
        "patch",
        EventPayload::PatchApplied(PatchAppliedPayload {
            patch_id: "patch-1".to_string(),
            source_event_id: None,
            file_ids: vec![file.clone()],
            symbol_ids: vec![symbol.clone()],
            lines_added: 5,
            lines_removed: 1,
        }),
    );
    let event_id = append_event(&writer, envelope);
    let read = read_back(&harness.reader(), &event_id);

    assert_eq!(read.references.len(), 2);
    for stable_ref in &read.references {
        match stable_ref {
            StableRef::FileRef(_) | StableRef::SymbolRef(_) => {}
            other => panic!("expected typed file/symbol ref, got {:?}", other),
        }
    }
    let raw_json: String = harness
        .event_store
        .lock_conn()
        .expect("event conn")
        .query_row(
            "SELECT references_json FROM events WHERE event_uuid = ?1",
            rusqlite::params![read.event_id.ulid.clone()],
            |row| row.get(0),
        )
        .expect("event row visible");
    let parsed: Vec<StableRef> = serde_json::from_str(&raw_json).expect("references parse typed");
    assert_eq!(parsed, read.references);
}

// ---------------------------------------------------------------------------
// SURFACE 7 — replay preserves the contract (identity-resolvable evidence)
// ---------------------------------------------------------------------------

#[test]
fn replay_preserves_identity_typed_evidence_anchor_after_full_replay() {
    let harness = ContractHarness::new();
    let file = file_identity("src/replay_anchor.rs");
    let symbol = symbol_identity(&file, "replay_anchor");
    let section = section_identity("docs/replay.md", "Replay");
    let draft = MemoryDraft {
        initial_evidence: vec![file_span_evidence(&file)],
        ..seed_draft(&file, &symbol, &section)
    };
    let memory = harness
        .store
        .create(draft, idem("replay-anchor"))
        .expect("memory creates");
    let original_evidence =
        get_evidence_for(&harness.conn(), &memory.memory_id).expect("evidence loads");

    let replay_conn = Connection::open_in_memory().expect("replay DB");
    replay_conn
        .execute_batch("PRAGMA foreign_keys = ON;")
        .expect("FK on");
    initialize_schema(&replay_conn).expect("replay schema");
    replay_events(&replay_conn, &harness.all_memory_events()).expect("replay succeeds");

    let replayed_evidence =
        get_evidence_for(&replay_conn, &memory.memory_id).expect("replayed evidence");
    let original_file_anchor: Vec<_> = original_evidence
        .iter()
        .filter_map(|item| match &item.anchor {
            EvidenceAnchor::FileSpan { file, .. } => Some(file.clone()),
            _ => None,
        })
        .collect();
    let replayed_file_anchor: Vec<_> = replayed_evidence
        .iter()
        .filter_map(|item| match &item.anchor {
            EvidenceAnchor::FileSpan { file, .. } => Some(file.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        original_file_anchor, replayed_file_anchor,
        "FileSpan evidence anchor survives replay with typed FileId"
    );
    assert!(!replayed_file_anchor.is_empty(), "FileSpan anchor present");
    assert_eq!(replayed_file_anchor[0], file);
}

#[test]
fn replay_emits_memory_id_column_as_encoded_identity_after_full_replay() {
    let harness = ContractHarness::new();
    let file = file_identity("src/replay_id.rs");
    let symbol = symbol_identity(&file, "replay_id");
    let section = section_identity("docs/replay.md", "ID");
    let memory = harness
        .store
        .create(seed_draft(&file, &symbol, &section), idem("replay-id"))
        .expect("memory creates");

    let replay_conn = Connection::open_in_memory().expect("replay DB");
    replay_conn
        .execute_batch("PRAGMA foreign_keys = ON;")
        .expect("FK on");
    initialize_schema(&replay_conn).expect("replay schema");
    replay_events(&replay_conn, &harness.all_memory_events()).expect("replay succeeds");

    let stored_id: String = replay_conn
        .query_row(
            "SELECT memory_id FROM memories",
            rusqlite::params![],
            |row| row.get(0),
        )
        .expect("replayed memory row");
    assert!(stored_id.starts_with("memory:"));
    let decoded = decode_identity(&stored_id).expect("memory id decodes");
    match decoded {
        Identity::Memory(id) => assert_eq!(id, memory.memory_id),
        other => panic!("expected memory identity, got {:?}", other.kind()),
    }
}

// ---------------------------------------------------------------------------
// SURFACE 8 — schema-level enforcement: typed identity columns
// ---------------------------------------------------------------------------

#[test]
fn memory_columns_holding_identity_references_store_typed_shapes_not_raw_strings() {
    let harness = ContractHarness::new();
    let file = file_identity("src/schema_check.rs");
    let symbol = symbol_identity(&file, "schema_check");
    let section = section_identity("docs/schema.md", "Schema");
    let memory = harness
        .store
        .create(seed_draft(&file, &symbol, &section), idem("schema-check"))
        .expect("memory creates");

    let (memory_id_str, linked_files_json, linked_symbols_json, linked_docs_json, provenance_json) =
        harness
            .conn()
            .query_row(
                "SELECT memory_id, linked_files_json, linked_symbols_json, linked_docs_json, \
                 provenance_event_ids_json FROM memories",
                rusqlite::params![],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .expect("memory row visible");

    assert!(memory_id_str.starts_with("memory:"));
    assert_eq!(
        decode_identity(&memory_id_str).expect("memory id decodes"),
        Identity::Memory(memory.memory_id.clone())
    );

    let files: Vec<FileId> = serde_json::from_str(&linked_files_json).expect("file ids");
    assert_eq!(files, vec![file.clone()]);
    let symbols: Vec<SymbolId> = serde_json::from_str(&linked_symbols_json).expect("symbols");
    assert_eq!(symbols, vec![symbol]);
    let sections: Vec<SectionId> = serde_json::from_str(&linked_docs_json).expect("sections");
    assert_eq!(sections, vec![section]);
    let provenance: Vec<EventId> =
        serde_json::from_str(&provenance_json).expect("provenance event ids");
    assert!(
        !provenance.is_empty(),
        "memory_created always populates provenance with the typed event id"
    );
    for event in &provenance {
        assert_eq!(event.workspace_id, WORKSPACE);
    }
}

#[test]
fn event_table_references_json_column_only_holds_typed_stable_refs() {
    let harness = ContractHarness::new();
    let writer = harness.writer();
    let file = file_identity("src/schema_event.rs");
    let envelope = daemon_envelope(
        EventKind::FileRead,
        vec![StableRef::FileRef(file.clone())],
        "file read",
        EventPayload::FileRead(FileReadPayload {
            file_id: file.clone(),
            source_event_id: None,
            byte_start: None,
            byte_end: None,
            reason: "schema".to_string(),
        }),
    );
    let event_id = append_event(&writer, envelope);

    let references_json: String = harness
        .event_store
        .lock_conn()
        .expect("event conn")
        .query_row(
            "SELECT references_json FROM events WHERE event_uuid = ?1",
            rusqlite::params![event_id.ulid.clone()],
            |row| row.get(0),
        )
        .expect("event row visible");
    let parsed: Vec<StableRef> = serde_json::from_str(&references_json).expect("refs parse typed");
    assert_eq!(parsed, vec![StableRef::FileRef(file)]);
    let raw: Value = serde_json::from_str(&references_json).expect("refs parse value");
    let first = &raw[0];
    assert!(
        first.is_object(),
        "references_json must be an array of typed identity objects, not strings: {references_json}"
    );
}

// ---------------------------------------------------------------------------
// END-TO-END — identity → event → store → reader → memory → identity
// ---------------------------------------------------------------------------

#[test]
fn end_to_end_identity_event_memory_round_trip_preserves_typed_references_at_every_boundary() {
    let harness = ContractHarness::new();
    let file = file_identity("src/end_to_end.rs");
    let symbol = symbol_identity(&file, "end_to_end_fn");
    let section = section_identity("docs/e2e.md", "Walkthrough");

    // Step 1 — write a file_read event that carries the typed file identity.
    let event_id = append_event(
        &harness.writer(),
        daemon_envelope(
            EventKind::FileRead,
            vec![StableRef::FileRef(file.clone())],
            "end-to-end seed",
            EventPayload::FileRead(FileReadPayload {
                file_id: file.clone(),
                source_event_id: None,
                byte_start: None,
                byte_end: None,
                reason: "e2e".to_string(),
            }),
        ),
    );

    // Step 2 — read it back through EventReader and confirm payload + references.
    let event_envelope = read_back(&harness.reader(), &event_id);
    let EventPayload::FileRead(file_read) = event_envelope.payload.clone() else {
        panic!("payload survives");
    };
    assert_eq!(file_read.file_id, file);
    assert_eq!(
        event_envelope.references,
        vec![StableRef::FileRef(file.clone())]
    );

    // Step 3 — write a memory whose evidence anchor cites the same file identity.
    let draft = MemoryDraft {
        initial_evidence: vec![file_span_evidence(&file)],
        ..seed_draft(&file, &symbol, &section)
    };
    let memory = harness
        .store
        .create(draft, idem("end-to-end"))
        .expect("memory creates");

    // Step 4 — read evidence back through MemoryStore-side queries.
    let stored_evidence =
        get_evidence_for(&harness.conn(), &memory.memory_id).expect("evidence loads");
    let stored_file = stored_evidence
        .iter()
        .find_map(|item| match &item.anchor {
            EvidenceAnchor::FileSpan { file, .. } => Some(file.clone()),
            _ => None,
        })
        .expect("file span survives");
    assert_eq!(stored_file, file);

    // Step 5 — resolve every reference back through the IdentityResolver.
    let event_ids: Vec<EventId> = memory.provenance_event_ids.clone();
    assert!(!event_ids.is_empty());
    let resolver = build_filesystem_resolver(
        &WORKSPACE.to_string(),
        &[(
            WORKSPACE,
            "src/end_to_end.rs",
            "pub fn end_to_end_fn() {}\n",
        )],
    );
    let by_path = resolver
        .resolve_path(&WORKSPACE.to_string(), "src/end_to_end.rs")
        .expect("path resolves");
    assert_eq!(by_path.repo_relative_path, "src/end_to_end.rs");
    let encoded = encode_identity(&Identity::File(by_path.clone()));
    let by_encoded = resolver
        .resolve_path(&WORKSPACE.to_string(), &encoded)
        .expect("encoded identity resolves");
    assert_eq!(by_encoded, by_path);

    // Step 6 — round-trip the memory's evidence event into a typed identity.
    let event_resolver = build_empty_resolver(WORKSPACE, event_ids.clone());
    for provenance in &event_ids {
        let encoded_event = encode_identity(&Identity::Event(provenance.clone()));
        let resolved = event_resolver
            .resolve_event_ref(&WORKSPACE.to_string(), &encoded_event)
            .expect("event identity resolves");
        assert_eq!(resolved, *provenance);
    }

    // Step 7 — the canonical wire encodings are stable across the trip.
    let event_envelope_encoding = encode_identity(&Identity::Event(event_envelope.event_id));
    assert!(event_envelope_encoding.starts_with("event:"));
    let memory_encoding = encode_identity(&Identity::Memory(memory.memory_id));
    assert!(memory_encoding.starts_with("memory:"));
}
