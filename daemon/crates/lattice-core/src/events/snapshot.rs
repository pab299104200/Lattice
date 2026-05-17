//! Versioned event-log compaction snapshots.
//!
//! Format and recovery rules are documented in
//! `docs/architecture/2026-05-16-event-log-compaction.md`
//! `## Snapshot format` and
//! `## Recovery from corrupt snapshot`.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::graph::model::{CodeGraph, EdgeKind, GraphNode};
use crate::memory::{Memory, MemoryStore};
use crate::{DateTime, LatticeError, Utc};

pub const SNAPSHOT_FORMAT_VERSION: u16 = 1;
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
    pub fn capture(
        graph: &CodeGraph,
        memory: &MemoryStore,
        up_to_event_id: i64,
    ) -> Result<Self, SnapshotError> {
        let graph_state = GraphSnapshot::from_graph(graph);
        let memory_state = MemorySnapshot::from_store(memory)?;
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
        memory: &MemoryStore,
        up_to_event_id: i64,
    ) -> Result<SnapshotHandle, SnapshotError> {
        let snapshot = Self::capture(graph, memory, up_to_event_id)?;
        snapshot.write_to(path)
    }

    pub fn write_to(&self, path: &Path) -> Result<SnapshotHandle, SnapshotError> {
        let body = encode_body(self)?;
        let header = SnapshotHeader::new(self.format_version, body.len(), self.content_hash);
        let tmp_path = temp_path(path);
        write_snapshot_file(&tmp_path, &header, &body)?;
        fs::rename(&tmp_path, path).map_err(|source| SnapshotError::IoFailed {
            path: path.to_path_buf(),
            source,
        })?;
        let bytes_written = (SNAPSHOT_HEADER_LEN + body.len()) as u64;
        Ok(SnapshotHandle {
            path: path.to_path_buf(),
            format_version: self.format_version,
            up_to_event_id: self.up_to_event_id,
            taken_at: self.taken_at,
            content_hash: self.content_hash,
            bytes_written,
        })
    }

    pub fn read(path: &Path) -> Result<Self, SnapshotError> {
        let mut file = fs::File::open(path).map_err(|source| SnapshotError::IoFailed {
            path: path.to_path_buf(),
            source,
        })?;
        let header = read_header_from(&mut file, path)?;
        validate_header(&header)?;
        let body = read_body_from(&mut file, path, &header)?;
        verify_hash(&body, &header.content_hash)?;
        let snapshot: Snapshot =
            ciborium::from_reader(body.as_slice()).map_err(cbor_decode_error)?;
        validate_decoded_snapshot(snapshot, &header)
    }

    pub fn read_header(path: &Path) -> Result<SnapshotHeader, SnapshotError> {
        let mut file = fs::File::open(path).map_err(|source| SnapshotError::IoFailed {
            path: path.to_path_buf(),
            source,
        })?;
        let header = read_header_from(&mut file, path)?;
        validate_header(&header)?;
        Ok(header)
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

fn write_snapshot_file(
    path: &Path,
    header: &SnapshotHeader,
    body: &[u8],
) -> Result<(), SnapshotError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| SnapshotError::IoFailed {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut file = fs::File::create(path).map_err(|source| SnapshotError::IoFailed {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(&header.encode())
        .and_then(|_| file.write_all(body))
        .and_then(|_| file.sync_all())
        .map_err(|source| SnapshotError::IoFailed {
            path: path.to_path_buf(),
            source,
        })
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

fn temp_path(path: &Path) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_extension(format!("tmp-{suffix}"))
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
