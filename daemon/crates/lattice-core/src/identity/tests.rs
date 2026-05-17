use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::{
    decode_identity, encode_identity, ContextHandleId, DocId, EventId, FileId, Identity,
    IdentityDecodeError, IdentityKind, MemoryId, SectionId, SymbolId,
};

const ULID_A: &str = "01HZY8PK7Q0T6K8Y5R2N4M9V3X";
const ULID_B: &str = "01HZY8Q0CV9QGJ5W4P6M8N2K1A";

#[test]
fn round_trip_encoding_for_every_identity_kind() {
    let identities = vec![
        Identity::File(file_id()),
        Identity::Symbol(symbol_id()),
        Identity::Doc(doc_id()),
        Identity::Section(section_id()),
        Identity::Event(EventId {
            workspace_id: "workspace-main".to_string(),
            ulid: ULID_A.to_string(),
        }),
        Identity::Memory(MemoryId {
            workspace_id: "workspace-main".to_string(),
            ulid: ULID_B.to_string(),
        }),
        Identity::ContextHandle(ContextHandleId {
            workspace_id: "workspace-main".to_string(),
            session_id: "session/with delimiter".to_string(),
            ulid: ULID_A.to_string(),
        }),
    ];

    for identity in identities {
        let encoded = encode_identity(&identity);
        assert_eq!(decode_identity(&encoded), Ok(identity));
    }
}

#[test]
fn identity_kind_reports_wrapped_variant() {
    assert_eq!(Identity::File(file_id()).kind(), IdentityKind::File);
    assert_eq!(Identity::Symbol(symbol_id()).kind(), IdentityKind::Symbol);
    assert_eq!(Identity::Doc(doc_id()).kind(), IdentityKind::Doc);
    assert_eq!(
        Identity::Section(section_id()).kind(),
        IdentityKind::Section
    );
}

#[test]
fn display_uses_compact_wire_encoding() {
    assert_eq!(
        file_id().to_string(),
        encode_identity(&Identity::File(file_id()))
    );
    assert_eq!(
        symbol_id().to_string(),
        encode_identity(&Identity::Symbol(symbol_id()))
    );
}

#[test]
fn equality_semantics_include_stability_fields() {
    let mut changed_hash = file_id();
    changed_hash.content_hash = "ffffffff".to_string();

    let mut changed_path = file_id();
    changed_path.repo_relative_path = "src/renamed.rs".to_string();

    assert_eq!(file_id(), file_id());
    assert_ne!(file_id(), changed_hash);
    assert_ne!(file_id(), changed_path);
}

#[test]
fn hash_is_deterministic_for_same_inputs() {
    let first = stable_hash(&Identity::Symbol(symbol_id()));
    let second = stable_hash(&Identity::Symbol(symbol_id()));

    assert_eq!(first, second);
}

#[test]
fn decode_rejects_empty_identity_string() {
    assert_eq!(decode_identity(""), Err(IdentityDecodeError::Empty));
}

#[test]
fn decode_rejects_missing_prefix() {
    assert_eq!(
        decode_identity("workspace/src/lib.rs@abcdef12"),
        Err(IdentityDecodeError::MissingPrefix)
    );
}

#[test]
fn decode_rejects_malformed_hash() {
    let result = decode_identity("file:workspace/src%2Flib.rs@not-a-hash");

    assert!(matches!(
        result,
        Err(IdentityDecodeError::MalformedField {
            field: "content_hash",
            ..
        })
    ));
}

#[test]
fn decode_rejects_malformed_ulid() {
    let result = decode_identity("event:workspace/not-a-ulid");

    assert!(matches!(
        result,
        Err(IdentityDecodeError::MalformedField { field: "ulid", .. })
    ));
}

#[test]
fn legacy_symbol_id_migrates_to_unified_symbol_id() {
    let legacy = crate::symbols::SymbolId {
        file: "src/lib.rs".to_string(),
        name: "parse".to_string(),
        byte_offset: 42,
    };

    let migrated = SymbolId::from(legacy);

    assert_eq!(migrated.file.workspace_id, "legacy");
    assert_eq!(migrated.file.repo_relative_path, "src/lib.rs");
    assert_eq!(migrated.file.content_hash, "00000000");
    assert_eq!(migrated.qualified_name, "parse");
    assert_eq!(migrated.byte_offset, 42);
    assert_eq!(migrated.kind, "unknown");
}

fn stable_hash<T: Hash>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn file_id() -> FileId {
    FileId {
        workspace_id: "workspace-main".to_string(),
        repo_relative_path: "src/lib.rs".to_string(),
        content_hash: "abcdef12".to_string(),
    }
}

fn doc_id() -> DocId {
    DocId {
        workspace_id: "workspace-main".to_string(),
        repo_relative_path: "docs/guide.md".to_string(),
        content_hash: "1234567890abcdef".to_string(),
    }
}

fn symbol_id() -> SymbolId {
    SymbolId {
        file: file_id(),
        qualified_name: "parser::decode@name".to_string(),
        byte_offset: 128,
        kind: "function".to_string(),
    }
}

fn section_id() -> SectionId {
    SectionId {
        doc: doc_id(),
        heading_path: vec![
            "Architecture".to_string(),
            "Stable # Handles".to_string(),
            "Round-trip @ Encoding".to_string(),
        ],
        byte_offset: 256,
    }
}
