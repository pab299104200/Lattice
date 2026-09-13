use thiserror::Error;

use super::kinds::{
    ContextHandleId, DocId, EventId, FileId, Identity, MemoryId, SectionId, SymbolId,
};

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum IdentityDecodeError {
    #[error("identity string is empty")]
    Empty,
    #[error("identity string is missing a recognized prefix")]
    MissingPrefix,
    #[error("identity string has malformed {field}: {reason}")]
    MalformedField {
        field: &'static str,
        reason: &'static str,
    },
}

pub fn encode_identity(identity: &Identity) -> String {
    match identity {
        Identity::File(value) => encode_file(value),
        Identity::Symbol(value) => encode_symbol(value),
        Identity::Doc(value) => encode_doc(value),
        Identity::Section(value) => encode_section(value),
        Identity::Event(value) => encode_workspace_ulid("event", &value.workspace_id, &value.ulid),
        Identity::Memory(value) => {
            encode_workspace_ulid("memory", &value.workspace_id, &value.ulid)
        }
        Identity::ContextHandle(value) => encode_context_handle(value),
    }
}

pub fn decode_identity(value: &str) -> Result<Identity, IdentityDecodeError> {
    if value.is_empty() {
        return Err(IdentityDecodeError::Empty);
    }
    let Some((prefix, payload)) = value.split_once(':') else {
        return Err(IdentityDecodeError::MissingPrefix);
    };
    match prefix {
        "file" => decode_file(payload).map(Identity::File),
        "symbol" => decode_symbol(payload).map(Identity::Symbol),
        "doc" => decode_doc(payload).map(Identity::Doc),
        "section" => decode_section(payload).map(Identity::Section),
        "event" => decode_event(payload).map(Identity::Event),
        "memory" => decode_memory(payload).map(Identity::Memory),
        "handle" => decode_context_handle(payload).map(Identity::ContextHandle),
        _ => Err(IdentityDecodeError::MissingPrefix),
    }
}

fn encode_file(value: &FileId) -> String {
    format!(
        "file:{}/{}@{}",
        escape(&value.workspace_id),
        escape(&value.repo_relative_path),
        value.content_hash
    )
}

fn encode_doc(value: &DocId) -> String {
    format!(
        "doc:{}/{}@{}",
        escape(&value.workspace_id),
        escape(&value.repo_relative_path),
        value.content_hash
    )
}

fn encode_symbol(value: &SymbolId) -> String {
    format!(
        "symbol:{}/{}@{}#{}@{}:{}",
        escape(&value.file.workspace_id),
        escape(&value.file.repo_relative_path),
        value.file.content_hash,
        escape(&value.qualified_name),
        value.byte_offset,
        escape(&value.kind)
    )
}

fn encode_section(value: &SectionId) -> String {
    let headings = value
        .heading_path
        .iter()
        .map(|heading| escape(heading))
        .collect::<Vec<_>>()
        .join("/");
    format!(
        "section:{}/{}@{}#{}@{}",
        escape(&value.doc.workspace_id),
        escape(&value.doc.repo_relative_path),
        value.doc.content_hash,
        headings,
        value.byte_offset
    )
}

fn encode_workspace_ulid(prefix: &str, workspace_id: &str, ulid: &str) -> String {
    format!("{prefix}:{}/{}", escape(workspace_id), ulid)
}

fn encode_context_handle(value: &ContextHandleId) -> String {
    format!(
        "handle:{}/{}/{}",
        escape(&value.workspace_id),
        escape(&value.session_id),
        value.ulid
    )
}

fn decode_file(payload: &str) -> Result<FileId, IdentityDecodeError> {
    let (workspace_id, path, content_hash) = decode_workspace_path_hash(payload)?;
    Ok(FileId {
        workspace_id,
        repo_relative_path: path,
        content_hash,
    })
}

fn decode_doc(payload: &str) -> Result<DocId, IdentityDecodeError> {
    let (workspace_id, path, content_hash) = decode_workspace_path_hash(payload)?;
    Ok(DocId {
        workspace_id,
        repo_relative_path: path,
        content_hash,
    })
}

fn decode_symbol(payload: &str) -> Result<SymbolId, IdentityDecodeError> {
    let (file_payload, symbol_payload) = split_once(payload, '#', "symbol")?;
    let (workspace_id, path, content_hash) = decode_workspace_path_hash(file_payload)?;
    let (name_payload, kind_payload) = split_once(symbol_payload, ':', "symbol kind")?;
    let (qualified_name, offset) = decode_named_offset(name_payload)?;
    Ok(SymbolId {
        file: FileId {
            workspace_id,
            repo_relative_path: path,
            content_hash,
        },
        qualified_name,
        byte_offset: offset,
        kind: unescape(kind_payload, "symbol kind")?,
    })
}

fn decode_section(payload: &str) -> Result<SectionId, IdentityDecodeError> {
    let (doc_payload, section_payload) = split_once(payload, '#', "section")?;
    let (workspace_id, path, content_hash) = decode_workspace_path_hash(doc_payload)?;
    let (headings_payload, byte_offset) = decode_offset_suffix(section_payload)?;
    let heading_path = decode_heading_path(headings_payload)?;
    Ok(SectionId {
        doc: DocId {
            workspace_id,
            repo_relative_path: path,
            content_hash,
        },
        heading_path,
        byte_offset,
    })
}

fn decode_event(payload: &str) -> Result<EventId, IdentityDecodeError> {
    let (workspace_id, ulid) = decode_workspace_ulid(payload)?;
    Ok(EventId { workspace_id, ulid })
}

fn decode_memory(payload: &str) -> Result<MemoryId, IdentityDecodeError> {
    let (workspace_payload, local_id) = split_once(payload, '/', "workspace memory id")?;
    validate_memory_local_id(local_id)?;
    Ok(MemoryId {
        workspace_id: unescape(workspace_payload, "workspace_id")?,
        ulid: local_id.to_string(),
    })
}

fn decode_context_handle(payload: &str) -> Result<ContextHandleId, IdentityDecodeError> {
    let parts = payload.split('/').collect::<Vec<_>>();
    if parts.len() != 3 {
        return malformed("handle", "expected workspace/session/ulid");
    }
    let workspace_id = unescape(parts[0], "workspace_id")?;
    let session_id = unescape(parts[1], "session_id")?;
    validate_ulid(parts[2])?;
    Ok(ContextHandleId {
        workspace_id,
        session_id,
        ulid: parts[2].to_string(),
    })
}

fn decode_workspace_path_hash(
    payload: &str,
) -> Result<(String, String, String), IdentityDecodeError> {
    let (workspace_payload, rest) = split_once(payload, '/', "workspace path")?;
    let (path_payload, hash) = split_once(rest, '@', "content hash")?;
    validate_content_hash(hash)?;
    Ok((
        unescape(workspace_payload, "workspace_id")?,
        unescape(path_payload, "repo_relative_path")?,
        hash.to_string(),
    ))
}

fn decode_workspace_ulid(payload: &str) -> Result<(String, String), IdentityDecodeError> {
    let (workspace_payload, ulid) = split_once(payload, '/', "workspace ulid")?;
    validate_ulid(ulid)?;
    Ok((
        unescape(workspace_payload, "workspace_id")?,
        ulid.to_string(),
    ))
}

fn decode_named_offset(payload: &str) -> Result<(String, usize), IdentityDecodeError> {
    let (name_payload, byte_offset) = decode_offset_suffix(payload)?;
    Ok((unescape(name_payload, "qualified_name")?, byte_offset))
}

fn decode_offset_suffix(payload: &str) -> Result<(&str, usize), IdentityDecodeError> {
    let (head, offset_payload) = split_once(payload, '@', "byte_offset")?;
    let byte_offset =
        offset_payload
            .parse::<usize>()
            .map_err(|_| IdentityDecodeError::MalformedField {
                field: "byte_offset",
                reason: "expected unsigned integer",
            })?;
    Ok((head, byte_offset))
}

fn decode_heading_path(payload: &str) -> Result<Vec<String>, IdentityDecodeError> {
    if payload.is_empty() {
        return malformed("heading_path", "expected at least one heading");
    }
    payload
        .split('/')
        .map(|heading| unescape(heading, "heading_path"))
        .collect()
}

fn split_once<'a>(
    payload: &'a str,
    delimiter: char,
    field: &'static str,
) -> Result<(&'a str, &'a str), IdentityDecodeError> {
    let Some(parts) = payload.split_once(delimiter) else {
        return malformed(field, "missing delimiter");
    };
    if parts.0.is_empty() || parts.1.is_empty() {
        return malformed(field, "empty component");
    }
    Ok(parts)
}

fn validate_content_hash(value: &str) -> Result<(), IdentityDecodeError> {
    let is_valid =
        (8..=128).contains(&value.len()) && value.chars().all(|item| item.is_ascii_hexdigit());
    if is_valid {
        return Ok(());
    }
    malformed("content_hash", "expected 8 to 128 hexadecimal characters")
}

fn validate_ulid(value: &str) -> Result<(), IdentityDecodeError> {
    let is_valid = value.len() == 26
        && value
            .chars()
            .all(|item| matches!(item, '0'..='9' | 'A'..='H' | 'J'..='K' | 'M'..='N' | 'P'..='T' | 'V'..='Z'));
    if is_valid {
        return Ok(());
    }
    malformed("ulid", "expected canonical Crockford base32 ULID")
}

fn validate_memory_local_id(value: &str) -> Result<(), IdentityDecodeError> {
    let is_valid = (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if is_valid {
        return Ok(());
    }
    malformed(
        "memory_id",
        "expected 1 to 128 ASCII letters, digits, hyphens, or underscores",
    )
}

fn escape(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn unescape(value: &str, field: &'static str) -> Result<String, IdentityDecodeError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let byte = decode_hex_escape(bytes, index, field)?;
        decoded.push(byte);
        index += 3;
    }
    String::from_utf8(decoded).map_err(|_| IdentityDecodeError::MalformedField {
        field,
        reason: "expected utf-8",
    })
}

fn decode_hex_escape(
    bytes: &[u8],
    index: usize,
    field: &'static str,
) -> Result<u8, IdentityDecodeError> {
    if index + 2 >= bytes.len() {
        return malformed(field, "incomplete percent escape");
    }
    let high = hex_value(bytes[index + 1], field)?;
    let low = hex_value(bytes[index + 2], field)?;
    Ok((high << 4) | low)
}

fn hex_value(value: u8, field: &'static str) -> Result<u8, IdentityDecodeError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => malformed(field, "invalid percent escape"),
    }
}

fn malformed<T>(field: &'static str, reason: &'static str) -> Result<T, IdentityDecodeError> {
    Err(IdentityDecodeError::MalformedField { field, reason })
}
