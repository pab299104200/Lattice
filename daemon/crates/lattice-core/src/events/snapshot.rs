//! Versioned event-log compaction snapshots.
//!
//! Format and recovery rules are documented in
//! `docs/architecture/2026-05-16-event-log-compaction.md`
//! `## Snapshot format` and
//! `## Recovery from corrupt snapshot`.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::graph::model::{CodeGraph, EdgeKind, GraphNode};
use crate::memory::{Memory, MemoryStore};
use crate::storage::SecureDir;
use crate::{DateTime, LatticeError, Utc};

pub const SNAPSHOT_FORMAT_VERSION: u16 = 2;
pub const SNAPSHOT_HEADER_LEN: usize = 64;
pub const SNAPSHOT_MAGIC: [u8; 8] = *b"LATTSNP1";

pub type SnapshotHash = [u8; 32];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub format_version: u16,
    pub taken_at: DateTime<Utc>,
    pub up_to_event_id: i64,
    pub graph_state: GraphSnapshot,
    pub memory_state: MemorySnapshot,
    pub content_hash: SnapshotHash,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphSnapshot {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphSnapshotEdge>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphSnapshotEdge {
    pub from: crate::symbols::SymbolId,
    pub to: crate::symbols::SymbolId,
    pub kind: EdgeKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemorySnapshot {
    pub memories: Vec<MemorySnapshotEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemorySnapshotEntry {
    pub memory: Memory,
    pub structured_fields: crate::memory::MemoryStructuredFields,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotHandle {
    pub path: PathBuf,
    pub format_version: u16,
    pub up_to_event_id: i64,
    pub taken_at: DateTime<Utc>,
    pub content_hash: SnapshotHash,
    pub bytes_written: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotHeader {
    pub format_version: u16,
    pub flags: u16,
    pub header_len: u32,
    pub content_offset: u64,
    pub content_len: u64,
    pub content_hash: SnapshotHash,
}

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("snapshot I/O failed for {path}: {source}")]
    IoFailed {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("snapshot header magic is unrecognized: {magic:?}")]
    HeaderUnrecognized { magic: [u8; 8] },
    #[error("snapshot format version {found} is unsupported; max supported is {max}")]
    FormatVersionUnsupported { found: u16, max: u16 },
    #[error("snapshot content hash mismatch")]
    ContentHashMismatch,
    #[error("snapshot serialization failed: {0}")]
    Serde(String),
    #[error("snapshot memory capture failed: {0}")]
    Memory(String),
}

impl Snapshot {
    pub fn capture(graph: &CodeGraph, up_to_event_id: i64) -> Result<Self, SnapshotError> {
        let graph_state = GraphSnapshot::from_graph(graph);
        // Durable memory belongs to the memory SQLite store. Event snapshots
        // are graph replay checkpoints and must not retain copied memory text.
        let memory_state = MemorySnapshot {
            memories: Vec::new(),
        };
        let taken_at = DateTime::from_unix_seconds(now_unix_seconds());
        Ok(Self::with_hash(
            taken_at,
            up_to_event_id,
            graph_state,
            memory_state,
        )?)
    }

    pub fn write(
        path: &Path,
        graph: &CodeGraph,
        up_to_event_id: i64,
    ) -> Result<SnapshotHandle, SnapshotError> {
        let snapshot = Self::capture(graph, up_to_event_id)?;
        snapshot.write_to(path)
    }

    pub fn write_to(&self, path: &Path) -> Result<SnapshotHandle, SnapshotError> {
        let parent = path
            .parent()
            .ok_or_else(|| SnapshotError::Serde("snapshot path has no parent directory".into()))?;
        fs::create_dir_all(parent).map_err(|source| SnapshotError::IoFailed {
            path: parent.into(),
            source,
        })?;
        let dir = SecureDir::open(parent).map_err(|source| SnapshotError::IoFailed {
            path: parent.into(),
            source,
        })?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| SnapshotError::Serde("snapshot filename is not valid UTF-8".into()))?;
        self.write_to_dir(&dir, name)
    }

    pub(crate) fn write_to_dir(
        &self,
        dir: &SecureDir,
        name: &str,
    ) -> Result<SnapshotHandle, SnapshotError> {
        let body = encode_body(self)?;
        let header = SnapshotHeader::new(self.format_version, body.len(), self.content_hash);
        let tmp_name = temp_name(name);
        let display = dir.path().join(name);
        let mut file = dir
            .open_new_file(&tmp_name)
            .map_err(|source| SnapshotError::IoFailed {
                path: dir.path().join(&tmp_name),
                source,
            })?;
        let write_result = file
            .write_all(&header.encode())
            .and_then(|_| file.write_all(&body))
            .and_then(|_| file.sync_all());
        drop(file);
        if let Err(source) = write_result {
            if let Ok(Some(entry)) = dir.metadata(&tmp_name) {
                let _ = dir.remove_file(&tmp_name, entry.identity);
            }
            return Err(SnapshotError::IoFailed {
                path: dir.path().join(&tmp_name),
                source,
            });
        }
        let source = dir
            .metadata(&tmp_name)
            .map_err(|source| SnapshotError::IoFailed {
                path: dir.path().join(&tmp_name),
                source,
            })?
            .ok_or_else(|| SnapshotError::Serde("snapshot temporary file disappeared".into()))?;
        let destination = dir
            .metadata(name)
            .map_err(|source| SnapshotError::IoFailed {
                path: display.clone(),
                source,
            })?;
        dir.replace_from(
            &tmp_name,
            dir,
            name,
            source.identity,
            destination.map(|e| e.identity),
        )
        .map_err(|source| SnapshotError::IoFailed {
            path: display.clone(),
            source,
        })?;
        let bytes_written = (SNAPSHOT_HEADER_LEN + body.len()) as u64;
        Ok(SnapshotHandle {
            path: display,
            format_version: self.format_version,
            up_to_event_id: self.up_to_event_id,
            taken_at: self.taken_at,
            content_hash: self.content_hash,
            bytes_written,
        })
    }

    pub fn read(path: &Path) -> Result<Self, SnapshotError> {
        let (dir, name) = open_snapshot_parent(path)?;
        Self::read_from_dir(&dir, &name)
    }

    pub(crate) fn read_from_dir(dir: &SecureDir, name: &str) -> Result<Self, SnapshotError> {
        let path = dir.path().join(name);
        let mut file = dir
            .open_file(name, false)
            .map_err(|source| SnapshotError::IoFailed {
                path: path.clone(),
                source,
            })?;
        let header = read_header_from(&mut file, &path)?;
        validate_header(&header)?;
        let body = read_body_from(&mut file, &path, &header)?;
        verify_hash(&body, &header.content_hash)?;
        let snapshot: Snapshot =
            ciborium::from_reader(body.as_slice()).map_err(cbor_decode_error)?;
        validate_decoded_snapshot(snapshot, &header)
    }

    pub fn read_header(path: &Path) -> Result<SnapshotHeader, SnapshotError> {
        let (dir, name) = open_snapshot_parent(path)?;
        let mut file = dir
            .open_file(&name, false)
            .map_err(|source| SnapshotError::IoFailed {
                path: path.into(),
                source,
            })?;
        let header = read_header_from(&mut file, path)?;
        validate_header(&header)?;
        Ok(header)
    }

    /// Rewrite a historical replay checkpoint without retaining knowledge payloads.
    pub fn without_memory(&self) -> Result<Self, SnapshotError> {
        Self::with_hash(
            self.taken_at,
            self.up_to_event_id,
            self.graph_state.clone(),
            MemorySnapshot {
                memories: Vec::new(),
            },
        )
    }

    fn with_hash(
        taken_at: DateTime<Utc>,
        up_to_event_id: i64,
        graph_state: GraphSnapshot,
        memory_state: MemorySnapshot,
    ) -> Result<Self, SnapshotError> {
        let mut snapshot = Self {
            format_version: SNAPSHOT_FORMAT_VERSION,
            taken_at,
            up_to_event_id,
            graph_state,
            memory_state,
            content_hash: [0; 32],
        };
        snapshot.content_hash = hash_bytes(&encode_cbor(&snapshot)?);
        Ok(snapshot)
    }
}

fn open_snapshot_parent(path: &Path) -> Result<(SecureDir, String), SnapshotError> {
    let parent = path
        .parent()
        .ok_or_else(|| SnapshotError::Serde("snapshot path has no parent directory".into()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| SnapshotError::Serde("snapshot filename is not valid UTF-8".into()))?
        .to_owned();
    let dir = SecureDir::open(parent).map_err(|source| SnapshotError::IoFailed {
        path: parent.into(),
        source,
    })?;
    Ok((dir, name))
}

/// Verify and rewrite a legacy snapshot without materializing its CBOR body.
/// Graph data is copied byte-for-byte and memory payloads are skipped with a
/// fixed-size buffer. The original remains authoritative until the replacement
/// has been hashed, synced, and atomically published.
pub(crate) fn rewrite_without_memory_streaming(
    dir: &SecureDir,
    name: &str,
) -> Result<u64, SnapshotError> {
    const BUFFER: usize = 64 * 1024;
    let path = dir.path().join(name);
    let original_identity = dir
        .metadata(name)
        .map_err(|source| SnapshotError::IoFailed {
            path: path.clone(),
            source,
        })?
        .ok_or_else(|| SnapshotError::Serde("snapshot disappeared before rewrite".into()))?
        .identity;
    let mut source = dir
        .open_file(name, false)
        .map_err(|source| SnapshotError::IoFailed {
            path: path.clone(),
            source,
        })?;
    let old_header = read_header_from(&mut source, &path)?;
    validate_header(&old_header)?;
    let available = source
        .metadata()
        .map_err(|source| SnapshotError::IoFailed {
            path: path.clone(),
            source,
        })?
        .len()
        .saturating_sub(SNAPSHOT_HEADER_LEN as u64);
    if old_header.content_len > available {
        return Err(SnapshotError::ContentHashMismatch);
    }
    let mut remaining = old_header.content_len;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; BUFFER];
    while remaining != 0 {
        let amount = usize::try_from(remaining.min(BUFFER as u64)).unwrap();
        source
            .read_exact(&mut buffer[..amount])
            .map_err(|source| SnapshotError::IoFailed {
                path: path.clone(),
                source,
            })?;
        hash.update(&buffer[..amount]);
        remaining -= amount as u64;
    }
    if hash.finalize().as_slice() != old_header.content_hash {
        return Err(SnapshotError::ContentHashMismatch);
    }

    source
        .seek(SeekFrom::Start(old_header.content_offset))
        .map_err(|source| SnapshotError::IoFailed {
            path: path.clone(),
            source,
        })?;
    let mut sizing = HashingWriter::new(std::io::sink());
    {
        let mut limited = (&mut source).take(old_header.content_len);
        transform_snapshot_map(&mut limited, &mut sizing, 0).map_err(|source| {
            SnapshotError::IoFailed {
                path: path.clone(),
                source,
            }
        })?;
        if limited.limit() != 0 {
            return Err(SnapshotError::Serde(
                "snapshot CBOR body has trailing bytes".into(),
            ));
        }
    }
    let (expected_len, expected_hash) = sizing.finish();
    ensure_rewrite_space(dir, expected_len + SNAPSHOT_HEADER_LEN as u64).map_err(|source| {
        SnapshotError::IoFailed {
            path: dir.path().into(),
            source,
        }
    })?;
    source
        .seek(SeekFrom::Start(old_header.content_offset))
        .map_err(|source| SnapshotError::IoFailed {
            path: path.clone(),
            source,
        })?;
    let tmp_name = temp_name(name);
    let mut output = dir
        .open_new_file(&tmp_name)
        .map_err(|source| SnapshotError::IoFailed {
            path: dir.path().join(&tmp_name),
            source,
        })?;
    let result = (|| {
        output.write_all(&[0; SNAPSHOT_HEADER_LEN])?;
        let mut limited = (&mut source).take(old_header.content_len);
        let mut hashing = HashingWriter::new(&mut output);
        transform_snapshot_map(&mut limited, &mut hashing, 0)?;
        if limited.limit() != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "snapshot CBOR body has trailing bytes",
            ));
        }
        let (content_len, content_hash) = hashing.finish();
        if content_len != expected_len || content_hash != expected_hash {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "snapshot changed during streaming rewrite",
            ));
        }
        let header = SnapshotHeader::new(
            SNAPSHOT_FORMAT_VERSION,
            usize::try_from(content_len).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "snapshot is too large")
            })?,
            content_hash,
        );
        output.seek(SeekFrom::Start(0))?;
        output.write_all(&header.encode())?;
        output.sync_all()?;
        Ok::<u64, std::io::Error>(content_len + SNAPSHOT_HEADER_LEN as u64)
    })();
    drop(output);
    let bytes = match result {
        Ok(bytes) => bytes,
        Err(source) => {
            if let Ok(Some(entry)) = dir.metadata(&tmp_name) {
                let _ = dir.remove_file(&tmp_name, entry.identity);
            }
            return Err(SnapshotError::IoFailed {
                path: dir.path().join(&tmp_name),
                source,
            });
        }
    };
    let temporary = dir
        .metadata(&tmp_name)
        .map_err(|source| SnapshotError::IoFailed {
            path: dir.path().join(&tmp_name),
            source,
        })?
        .ok_or_else(|| SnapshotError::Serde("snapshot rewrite temporary disappeared".into()))?;
    let publish = (|| {
        let destination = dir
            .metadata(name)
            .map_err(|source| SnapshotError::IoFailed {
                path: path.clone(),
                source,
            })?
            .ok_or_else(|| SnapshotError::Serde("snapshot changed during rewrite".into()))?;
        if destination.identity != original_identity {
            return Err(SnapshotError::Serde(
                "snapshot identity changed during rewrite".into(),
            ));
        }
        dir.replace_from(
            &tmp_name,
            dir,
            name,
            temporary.identity,
            Some(destination.identity),
        )
        .map_err(|source| SnapshotError::IoFailed {
            path: path.clone(),
            source,
        })
    })();
    if let Err(error) = publish {
        if let Ok(Some(entry)) = dir.metadata(&tmp_name) {
            let _ = dir.remove_file(&tmp_name, entry.identity);
        }
        return Err(error);
    }
    Ok(bytes)
}

struct HashingWriter<W> {
    inner: W,
    hash: Sha256,
    count: u64,
}
impl<W> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hash: Sha256::new(),
            count: 0,
        }
    }
    fn finish(self) -> (u64, SnapshotHash) {
        let mut out = [0; 32];
        out.copy_from_slice(&self.hash.finalize());
        (self.count, out)
    }
}
impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(bytes)?;
        self.hash.update(&bytes[..n]);
        self.count += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(unix)]
fn ensure_rewrite_space(dir: &SecureDir, required: u64) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(dir.path().as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "snapshot directory contains NUL",
        )
    })?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    let available = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
    if available < required {
        return Err(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            format!(
                "snapshot rewrite requires {required} bytes but only {available} are available"
            ),
        ));
    }
    Ok(())
}
#[cfg(not(unix))]
fn ensure_rewrite_space(_: &SecureDir, _: u64) -> std::io::Result<()> {
    Ok(())
}

fn transform_snapshot_map<R: Read, W: Write>(
    input: &mut R,
    output: &mut W,
    depth: usize,
) -> std::io::Result<()> {
    let (head, major, count) = read_cbor_head(input)?;
    if major != 5 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "snapshot CBOR root is not a definite map",
        ));
    }
    let count = count.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "indefinite snapshot maps are unsupported",
        )
    })?;
    output.write_all(&head)?;
    let mut saw_format = false;
    let mut saw_memory = false;
    for _ in 0..count {
        let (key_head, major, key_len) = read_cbor_head(input)?;
        let key_len = key_len.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "indefinite snapshot map key is unsupported",
            )
        })?;
        if major != 3 || key_len > 120 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "snapshot map key is not bounded text",
            ));
        }
        let mut payload = vec![0; key_len as usize];
        input.read_exact(&mut payload)?;
        let decoded = String::from_utf8(payload.clone())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut key = key_head;
        key.extend_from_slice(&payload);
        output.write_all(&key)?;
        match decoded.as_str() {
            "format_version" => {
                saw_format = true;
                skip_cbor_value(input, depth + 1)?;
                output.write_all(&[2])?;
            }
            "memory_state" => {
                saw_memory = true;
                skip_cbor_value(input, depth + 1)?;
                output.write_all(&[0xa1, 0x68])?;
                output.write_all(b"memories")?;
                output.write_all(&[0x80])?;
            }
            _ => copy_cbor_value(input, output, depth + 1)?,
        }
    }
    if !saw_format || !saw_memory {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "snapshot CBOR lacks required format or memory field",
        ));
    }
    Ok(())
}

fn read_cbor_head<R: Read>(input: &mut R) -> std::io::Result<(Vec<u8>, u8, Option<u64>)> {
    let mut first = [0u8; 1];
    input.read_exact(&mut first)?;
    let major = first[0] >> 5;
    let additional = first[0] & 0x1f;
    let width = match additional {
        0..=23 => 0,
        24 => 1,
        25 => 2,
        26 => 4,
        27 => 8,
        31 => 0,
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid CBOR additional information",
            ))
        }
    };
    let mut head = vec![first[0]];
    head.resize(1 + width, 0);
    input.read_exact(&mut head[1..])?;
    let value = match additional {
        0..=23 => Some(additional as u64),
        24 => Some(head[1] as u64),
        25 => Some(u16::from_be_bytes(head[1..3].try_into().unwrap()) as u64),
        26 => Some(u32::from_be_bytes(head[1..5].try_into().unwrap()) as u64),
        27 => Some(u64::from_be_bytes(head[1..9].try_into().unwrap())),
        31 => None,
        _ => unreachable!(),
    };
    Ok((head, major, value))
}

fn copy_cbor_value<R: Read, W: Write>(
    input: &mut R,
    output: &mut W,
    depth: usize,
) -> std::io::Result<()> {
    if depth > 128 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "CBOR nesting exceeds limit",
        ));
    }
    let (head, major, value) = read_cbor_head(input)?;
    output.write_all(&head)?;
    match major {
        0 | 1 | 7 => {}
        2 | 3 => copy_exact(
            input,
            output,
            value.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "indefinite CBOR is unsupported",
                )
            })?,
        )?,
        4 => {
            let count = value.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "indefinite CBOR is unsupported",
                )
            })?;
            for _ in 0..count {
                copy_cbor_value(input, output, depth + 1)?;
            }
        }
        5 => {
            let count = value.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "indefinite CBOR is unsupported",
                )
            })?;
            for _ in 0..count {
                copy_cbor_value(input, output, depth + 1)?;
                copy_cbor_value(input, output, depth + 1)?;
            }
        }
        6 => copy_cbor_value(input, output, depth + 1)?,
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid CBOR major type",
            ))
        }
    }
    Ok(())
}
fn skip_cbor_value<R: Read>(input: &mut R, depth: usize) -> std::io::Result<()> {
    copy_cbor_value(input, &mut std::io::sink(), depth)
}
fn copy_exact<R: Read, W: Write>(
    input: &mut R,
    output: &mut W,
    mut remaining: u64,
) -> std::io::Result<()> {
    let mut buffer = [0u8; 64 * 1024];
    while remaining != 0 {
        let n = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
        input.read_exact(&mut buffer[..n])?;
        output.write_all(&buffer[..n])?;
        remaining -= n as u64;
    }
    Ok(())
}

impl GraphSnapshot {
    pub fn from_graph(graph: &CodeGraph) -> Self {
        let mut nodes: Vec<GraphNode> = graph.all_nodes().into_iter().cloned().collect();
        nodes.sort_by(|left, right| {
            left.id
                .name
                .cmp(&right.id.name)
                .then(left.file.cmp(&right.file))
        });
        let mut edges = graph
            .all_edges()
            .into_iter()
            .map(|(from, to, kind)| GraphSnapshotEdge {
                from: from.id.clone(),
                to: to.id.clone(),
                kind,
            })
            .collect::<Vec<_>>();
        edges.sort_by(|left, right| {
            left.from
                .name
                .cmp(&right.from.name)
                .then(left.to.name.cmp(&right.to.name))
                .then(format!("{:?}", left.kind).cmp(&format!("{:?}", right.kind)))
        });
        Self { nodes, edges }
    }

    pub fn into_graph(self) -> CodeGraph {
        let mut graph = CodeGraph::new();
        for node in self.nodes {
            graph.add_node(
                node.id,
                node.kind,
                node.name,
                node.signature,
                node.body,
                node.file,
                node.line,
                node.end_line,
                node.is_exported,
                node.language,
            );
        }
        for edge in self.edges {
            graph.add_edge(&edge.from, &edge.to, edge.kind);
        }
        graph
    }
}

impl MemorySnapshot {
    pub fn from_store(store: &MemoryStore) -> Result<Self, SnapshotError> {
        let mut entries = Vec::new();
        for memory in store
            .query_unscoped_admin(None, usize::MAX)
            .map_err(memory_error)?
        {
            let structured_fields = store
                .get_structured_fields(&memory.id)
                .map_err(memory_error)?
                .unwrap_or_default();
            entries.push(MemorySnapshotEntry {
                memory,
                structured_fields,
            });
        }
        entries.sort_by(|left, right| left.memory.id.cmp(&right.memory.id));
        Ok(Self { memories: entries })
    }
}

impl SnapshotHeader {
    fn new(format_version: u16, content_len: usize, content_hash: SnapshotHash) -> Self {
        Self {
            format_version,
            flags: 0,
            header_len: SNAPSHOT_HEADER_LEN as u32,
            content_offset: SNAPSHOT_HEADER_LEN as u64,
            content_len: content_len as u64,
            content_hash,
        }
    }

    fn encode(&self) -> [u8; SNAPSHOT_HEADER_LEN] {
        let mut bytes = [0; SNAPSHOT_HEADER_LEN];
        bytes[0..8].copy_from_slice(&SNAPSHOT_MAGIC);
        bytes[8..10].copy_from_slice(&self.format_version.to_le_bytes());
        bytes[10..12].copy_from_slice(&self.flags.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.header_len.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.content_offset.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.content_len.to_le_bytes());
        bytes[32..64].copy_from_slice(&self.content_hash);
        bytes
    }

    fn decode(bytes: [u8; SNAPSHOT_HEADER_LEN]) -> Result<Self, SnapshotError> {
        let mut magic = [0; 8];
        magic.copy_from_slice(&bytes[0..8]);
        if magic != SNAPSHOT_MAGIC {
            return Err(SnapshotError::HeaderUnrecognized { magic });
        }
        let mut content_hash = [0; 32];
        content_hash.copy_from_slice(&bytes[32..64]);
        Ok(Self {
            format_version: u16::from_le_bytes([bytes[8], bytes[9]]),
            flags: u16::from_le_bytes([bytes[10], bytes[11]]),
            header_len: u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
            content_offset: u64::from_le_bytes(bytes[16..24].try_into().expect("slice len")),
            content_len: u64::from_le_bytes(bytes[24..32].try_into().expect("slice len")),
            content_hash,
        })
    }
}

fn encode_body(snapshot: &Snapshot) -> Result<Vec<u8>, SnapshotError> {
    let mut body_snapshot = snapshot.clone();
    body_snapshot.content_hash = [0; 32];
    let body = encode_cbor(&body_snapshot)?;
    let actual_hash = hash_bytes(&body);
    if actual_hash != snapshot.content_hash {
        return Err(SnapshotError::ContentHashMismatch);
    }
    Ok(body)
}

fn read_header_from(file: &mut fs::File, path: &Path) -> Result<SnapshotHeader, SnapshotError> {
    let mut header = [0; SNAPSHOT_HEADER_LEN];
    file.read_exact(&mut header)
        .map_err(|source| SnapshotError::IoFailed {
            path: path.to_path_buf(),
            source,
        })?;
    SnapshotHeader::decode(header)
}

fn read_body_from(
    file: &mut fs::File,
    path: &Path,
    header: &SnapshotHeader,
) -> Result<Vec<u8>, SnapshotError> {
    const MAX_SNAPSHOT_BODY_BYTES: u64 = 256 * 1024 * 1024;
    let available = file
        .metadata()
        .map_err(|source| SnapshotError::IoFailed {
            path: path.into(),
            source,
        })?
        .len()
        .saturating_sub(SNAPSHOT_HEADER_LEN as u64);
    if header.content_len > available {
        return Err(SnapshotError::ContentHashMismatch);
    }
    if header.content_len > MAX_SNAPSHOT_BODY_BYTES {
        return Err(SnapshotError::Serde(
            "snapshot content length exceeds available bytes or the 256 MiB read budget".into(),
        ));
    }
    let mut body = vec![0; header.content_len as usize];
    if let Err(source) = file.read_exact(&mut body) {
        if source.kind() == std::io::ErrorKind::UnexpectedEof {
            return Err(SnapshotError::ContentHashMismatch);
        }
        return Err(SnapshotError::IoFailed {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(body)
}

fn validate_header(header: &SnapshotHeader) -> Result<(), SnapshotError> {
    if header.format_version > SNAPSHOT_FORMAT_VERSION {
        return Err(SnapshotError::FormatVersionUnsupported {
            found: header.format_version,
            max: SNAPSHOT_FORMAT_VERSION,
        });
    }
    if header.header_len != SNAPSHOT_HEADER_LEN as u32
        || header.content_offset != SNAPSHOT_HEADER_LEN as u64
    {
        return Err(SnapshotError::HeaderUnrecognized {
            magic: SNAPSHOT_MAGIC,
        });
    }
    Ok(())
}

fn validate_decoded_snapshot(
    mut snapshot: Snapshot,
    header: &SnapshotHeader,
) -> Result<Snapshot, SnapshotError> {
    if snapshot.format_version != header.format_version {
        return Err(SnapshotError::FormatVersionUnsupported {
            found: snapshot.format_version,
            max: SNAPSHOT_FORMAT_VERSION,
        });
    }
    snapshot.content_hash = hash_bytes(&encode_cbor(&snapshot)?);
    if snapshot.content_hash != header.content_hash {
        return Err(SnapshotError::ContentHashMismatch);
    }
    Ok(snapshot)
}

fn verify_hash(body: &[u8], expected: &SnapshotHash) -> Result<(), SnapshotError> {
    if &hash_bytes(body) == expected {
        Ok(())
    } else {
        Err(SnapshotError::ContentHashMismatch)
    }
}

fn encode_cbor(snapshot: &Snapshot) -> Result<Vec<u8>, SnapshotError> {
    let mut bytes = Vec::new();
    ciborium::into_writer(snapshot, &mut bytes).map_err(cbor_encode_error)?;
    Ok(bytes)
}

fn cbor_encode_error(error: ciborium::ser::Error<std::io::Error>) -> SnapshotError {
    SnapshotError::Serde(error.to_string())
}

fn cbor_decode_error(error: ciborium::de::Error<std::io::Error>) -> SnapshotError {
    SnapshotError::Serde(error.to_string())
}

fn hash_bytes(bytes: &[u8]) -> SnapshotHash {
    let digest = Sha256::digest(bytes);
    let mut hash = [0; 32];
    hash.copy_from_slice(&digest);
    hash
}

fn temp_name(name: &str) -> String {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(".{name}.{suffix}.tmp")
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

fn memory_error(error: LatticeError) -> SnapshotError {
    SnapshotError::Memory(error.to_string())
}

#[cfg(test)]
mod retention_regressions {
    use super::*;
    #[test]
    fn final_historical_snapshot_loses_memory_but_preserves_graph_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot-10-10.bin");
        let store = MemoryStore::open_in_memory().unwrap();
        let mut snapshot = Snapshot::capture(&CodeGraph::new(), 10).unwrap();
        store.with_connection(|c| { c.execute("INSERT INTO memories(id,content,memory_type,created_at,last_accessed) VALUES('expired','private expired lesson','fact',1,1)",[]).unwrap(); Ok(()) }).unwrap();
        snapshot.memory_state = MemorySnapshot::from_store(&store).unwrap();
        snapshot.format_version = 1;
        snapshot.taken_at = DateTime::from_unix_seconds(1);
        snapshot.content_hash = [0; 32];
        snapshot.content_hash = hash_bytes(&encode_cbor(&snapshot).unwrap());
        snapshot.write_to(&path).unwrap();
        let report =
            crate::events::expire_managed_snapshots(dir.path(), 100, 20, 1_000_000, 1).unwrap();
        assert_eq!(report.rewritten_without_memory, 1);
        let current = Snapshot::read(&path).unwrap();
        assert_eq!(current.format_version, SNAPSHOT_FORMAT_VERSION);
        assert_eq!(current.up_to_event_id, 10);
        assert!(!fs::read(&path)
            .unwrap()
            .windows(b"private expired lesson".len())
            .any(|w| w == b"private expired lesson"));
        assert!(current.memory_state.memories.is_empty());
    }

    #[test]
    fn streaming_rewrite_preserves_graph_cursor_and_removes_legacy_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot-10-10.bin");
        let store = MemoryStore::open_in_memory().unwrap();
        let mut snapshot = Snapshot::capture(&CodeGraph::new(), 10).unwrap();
        store.with_connection(|c| { c.execute("INSERT INTO memories(id,content,memory_type,created_at,last_accessed) VALUES('expired','stream-only-secret','fact',1,1)",[]).unwrap(); Ok(()) }).unwrap();
        snapshot.memory_state = MemorySnapshot::from_store(&store).unwrap();
        snapshot.format_version = 1;
        snapshot.content_hash = [0; 32];
        snapshot.content_hash = hash_bytes(&encode_cbor(&snapshot).unwrap());
        snapshot.write_to(&path).unwrap();
        let managed = SecureDir::open(dir.path()).unwrap();
        rewrite_without_memory_streaming(&managed, "snapshot-10-10.bin").unwrap();
        let rewritten = Snapshot::read(&path).unwrap();
        assert_eq!(rewritten.format_version, 2);
        assert_eq!(rewritten.up_to_event_id, 10);
        assert!(rewritten.memory_state.memories.is_empty());
        assert!(!fs::read(path)
            .unwrap()
            .windows(18)
            .any(|w| w == b"stream-only-secret"));
    }

    #[test]
    fn oversized_legacy_snapshot_is_streamed_below_the_read_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot-10-1.bin");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(&[0; SNAPSHOT_HEADER_LEN]).unwrap();
        let mut body = HashingWriter::new(&mut file);
        body.write_all(&[0xa6]).unwrap();
        fn value(writer: &mut HashingWriter<&mut fs::File>, key: &str, value: &impl Serialize) {
            ciborium::into_writer(key, &mut *writer).unwrap();
            ciborium::into_writer(value, &mut *writer).unwrap();
        }
        value(&mut body, "format_version", &1u16);
        value(&mut body, "taken_at", &DateTime::from_unix_seconds(1));
        value(&mut body, "up_to_event_id", &10i64);
        value(
            &mut body,
            "graph_state",
            &GraphSnapshot {
                nodes: vec![],
                edges: vec![],
            },
        );
        ciborium::into_writer("memory_state", &mut body).unwrap();
        body.write_all(&[0xa1, 0x68]).unwrap();
        body.write_all(b"memories").unwrap();
        body.write_all(&[0x81, 0x7b]).unwrap();
        let payload_len = 257u64 * 1024 * 1024;
        body.write_all(&payload_len.to_be_bytes()).unwrap();
        let secret = [b'x'; 64 * 1024];
        for _ in 0..(payload_len / secret.len() as u64) {
            body.write_all(&secret).unwrap();
        }
        value(&mut body, "content_hash", &[0u8; 32]);
        let (content_len, content_hash) = body.finish();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&SnapshotHeader::new(1, content_len as usize, content_hash).encode())
            .unwrap();
        file.sync_all().unwrap();
        drop(file);

        let report = crate::events::expire_managed_snapshots(
            dir.path(),
            31 * 86_400,
            30 * 86_400,
            64 * 1024 * 1024,
            8,
        )
        .unwrap();
        assert_eq!(report.rewritten_without_memory, 1);
        let snapshot = Snapshot::read(&path).unwrap();
        assert_eq!(snapshot.up_to_event_id, 10);
        assert!(snapshot.memory_state.memories.is_empty());
        assert!(fs::metadata(path).unwrap().len() < 1024);
    }

    #[test]
    fn streaming_transform_rejects_oversized_map_key_before_allocating_it() {
        let input = [0xa1, 0x7b, 0, 0, 0, 1, 0, 0, 0, 0];
        let error = transform_snapshot_map(&mut input.as_slice(), &mut Vec::new(), 0).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("bounded text"));
    }
}
