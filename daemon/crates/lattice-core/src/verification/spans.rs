//! Exact-span verification for durable memory evidence.
//!
//! This module implements the verifier contract from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 8. Verification Engine` and
//! `## Non-Negotiable Product Properties`.
//!
//! Verification checks:
//!
//! - evidence text still matches when exact spans were captured
//!
//! Non-negotiable product property:
//!
//! - Every stale or contradicted memory is surfaced as such, not hidden behind recency.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{VerificationStatus, VerificationVerdict};
use crate::error::LatticeError;
use crate::identity::FileId;
use crate::memory::{EvidenceSpan, MemoryEvidence};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpanMismatch {
    ContentChanged,
    FileShrunkBelowSpan,
    EncodingChanged,
    FileMissing,
}

impl SpanMismatch {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContentChanged => "ContentChanged",
            Self::FileShrunkBelowSpan => "FileShrunkBelowSpan",
            Self::EncodingChanged => "EncodingChanged",
            Self::FileMissing => "FileMissing",
        }
    }

    fn status(self) -> VerificationStatus {
        match self {
            Self::FileMissing => VerificationStatus::Invalidated,
            Self::ContentChanged | Self::FileShrunkBelowSpan | Self::EncodingChanged => {
                VerificationStatus::Stale
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanMismatchReason {
    pub kind: String,
    pub subkind: String,
    pub evidence_id: String,
}

impl SpanMismatchReason {
    pub fn new(evidence_id: impl Into<String>, subkind: SpanMismatch) -> Self {
        Self {
            kind: "SpanMismatch".to_string(),
            subkind: subkind.as_str().to_string(),
            evidence_id: evidence_id.into(),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            format!(
                r#"{{"kind":"SpanMismatch","subkind":"{}","evidence_id":"{}"}}"#,
                self.subkind, self.evidence_id
            )
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpanValidationError {
    #[error("invalid evidence span `{byte_start}..{byte_end}`")]
    InvalidSpan { byte_start: u32, byte_end: u32 },
    #[error("path `{repo_relative_path}` resolves outside workspace root `{workspace_root}`")]
    OutsideWorkspace {
        workspace_root: String,
        repo_relative_path: String,
    },
    #[error("failed to read `{path}`: {source}")]
    ReadFailed {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Storage(#[from] LatticeError),
}

pub trait SpanReader {
    fn file_len(&self, file_id: &FileId) -> Result<Option<u64>, SpanValidationError>;
    fn read_range(
        &self,
        file_id: &FileId,
        start: u32,
        end: u32,
    ) -> Result<Option<Vec<u8>>, SpanValidationError>;
    fn read_line_span(
        &self,
        file_id: &FileId,
        line_start: u32,
        line_end: u32,
    ) -> Result<Option<Vec<u8>>, SpanValidationError>;
}

pub struct WorkspaceFileReader {
    workspace_root: PathBuf,
}

impl WorkspaceFileReader {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
        }
    }

    fn path_for(&self, file_id: &FileId) -> Result<PathBuf, SpanValidationError> {
        let candidate = self.workspace_root.join(&file_id.repo_relative_path);
        let normalized = normalize_candidate_path(&candidate);
        if !normalized.starts_with(&self.workspace_root) {
            tracing::warn!(
                target: "security",
                workspace_root = self.workspace_root.display().to_string(),
                repo_relative_path = file_id.repo_relative_path.as_str(),
                "workspace_file_reader_rejected_outside_workspace_path"
            );
            return Err(SpanValidationError::OutsideWorkspace {
                workspace_root: self.workspace_root.display().to_string(),
                repo_relative_path: file_id.repo_relative_path.clone(),
            });
        }
        Ok(normalized)
    }
}

impl SpanReader for WorkspaceFileReader {
    fn file_len(&self, file_id: &FileId) -> Result<Option<u64>, SpanValidationError> {
        let path = self.path_for(file_id)?;
        match crate::security::workspace::open_source(
            &self.workspace_root,
            Path::new(&file_id.repo_relative_path),
        )
        .and_then(|file| file.metadata())
        {
            Ok(metadata) => Ok(Some(metadata.len())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(read_failed(&path, error)),
        }
    }

    fn read_range(
        &self,
        file_id: &FileId,
        start: u32,
        end: u32,
    ) -> Result<Option<Vec<u8>>, SpanValidationError> {
        let path = self.path_for(file_id)?;
        let mut file = match crate::security::workspace::open_source(
            &self.workspace_root,
            Path::new(&file_id.repo_relative_path),
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(read_failed(&path, error)),
        };
        let length = usize::try_from(end - start).expect("u32 range fits into usize");
        file.seek(SeekFrom::Start(u64::from(start)))
            .map_err(|error| read_failed(&path, error))?;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)
            .map_err(|error| read_failed(&path, error))?;
        Ok(Some(bytes))
    }

    fn read_line_span(
        &self,
        file_id: &FileId,
        line_start: u32,
        line_end: u32,
    ) -> Result<Option<Vec<u8>>, SpanValidationError> {
        let path = self.path_for(file_id)?;
        let file = match crate::security::workspace::open_source(
            &self.workspace_root,
            Path::new(&file_id.repo_relative_path),
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(read_failed(&path, error)),
        };
        let mut reader = BufReader::new(file);
        let mut current_line = 1_u32;
        let mut line = Vec::new();
        let mut collected = Vec::new();
        loop {
            line.clear();
            let read = reader
                .read_until(b'\n', &mut line)
                .map_err(|error| read_failed(&path, error))?;
            if read == 0 {
                break;
            }
            if current_line >= line_start && current_line <= line_end {
                collected.extend_from_slice(&line);
            }
            if current_line >= line_end {
                break;
            }
            current_line += 1;
        }
        if collected.is_empty() && line_start > current_line {
            return Ok(None);
        }
        Ok(Some(collected))
    }
}

pub struct SpanValidator;

impl SpanValidator {
    pub fn validate(evidence: &MemoryEvidence, current_file_bytes: &[u8]) -> VerificationVerdict {
        let Some(span) = evidence.span.as_ref() else {
            return VerificationVerdict::new(
                VerificationStatus::Verified,
                "span validation skipped because evidence has no exact span",
            );
        };
        let start = usize::try_from(span.byte_start).expect("u32 span start fits into usize");
        let end = usize::try_from(span.byte_end).expect("u32 span end fits into usize");
        if start > end {
            return VerificationVerdict::new(
                VerificationStatus::Unverified,
                "span validation skipped because evidence span is invalid",
            );
        }
        let span_len = end - start;
        let candidate = if current_file_bytes.len() == span_len {
            current_file_bytes
        } else if end <= current_file_bytes.len() {
            &current_file_bytes[start..end]
        } else {
            return Self::mismatch_verdict("unknown", SpanMismatch::FileShrunkBelowSpan);
        };
        let Some(expected_hash) = evidence.evidence_content_hash else {
            return VerificationVerdict::new(
                VerificationStatus::Unverified,
                "span validation skipped because evidence has no captured content hash",
            );
        };
        let actual = hash_bytes(candidate);
        if actual == expected_hash {
            return VerificationVerdict::new(
                VerificationStatus::Verified,
                "captured span bytes still match the recorded hash",
            );
        }
        if is_encoding_only_change(evidence, candidate) {
            return Self::mismatch_verdict("unknown", SpanMismatch::EncodingChanged);
        }
        Self::mismatch_verdict("unknown", SpanMismatch::ContentChanged)
    }

    pub fn validate_with_reader(
        evidence_id: &str,
        evidence: &MemoryEvidence,
        reader: &dyn SpanReader,
    ) -> Result<VerificationVerdict, SpanValidationError> {
        let Some(span) = evidence.span.as_ref() else {
            return Ok(Self::validate(evidence, &[]));
        };
        validate_span_bounds(span)?;
        let Some(file_len) = reader.file_len(&span.file_id)? else {
            return Ok(Self::mismatch_verdict(
                evidence_id,
                SpanMismatch::FileMissing,
            ));
        };
        if u64::from(span.byte_end) > file_len {
            return Ok(Self::mismatch_verdict(
                evidence_id,
                SpanMismatch::FileShrunkBelowSpan,
            ));
        }
        let Some(range_bytes) = reader.read_range(&span.file_id, span.byte_start, span.byte_end)?
        else {
            return Ok(Self::mismatch_verdict(
                evidence_id,
                SpanMismatch::FileMissing,
            ));
        };
        let basic = Self::validate(evidence, &range_bytes);
        if basic.status == VerificationStatus::Verified {
            return Ok(basic);
        }
        if reason_subkind(&basic.reason).as_deref() != Some("ContentChanged") {
            return Ok(Self::with_evidence_id(evidence_id, basic));
        }
        let Some(line_bytes) =
            reader.read_line_span(&span.file_id, span.line_start, span.line_end)?
        else {
            return Ok(Self::mismatch_verdict(
                evidence_id,
                SpanMismatch::FileMissing,
            ));
        };
        if is_encoding_only_change(evidence, &line_bytes) {
            return Ok(Self::mismatch_verdict(
                evidence_id,
                SpanMismatch::EncodingChanged,
            ));
        }
        Ok(Self::mismatch_verdict(
            evidence_id,
            SpanMismatch::ContentChanged,
        ))
    }

    fn with_evidence_id(evidence_id: &str, verdict: VerificationVerdict) -> VerificationVerdict {
        match reason_subkind(&verdict.reason).as_deref() {
            Some("ContentChanged") => {
                Self::mismatch_verdict(evidence_id, SpanMismatch::ContentChanged)
            }
            Some("FileShrunkBelowSpan") => {
                Self::mismatch_verdict(evidence_id, SpanMismatch::FileShrunkBelowSpan)
            }
            Some("EncodingChanged") => {
                Self::mismatch_verdict(evidence_id, SpanMismatch::EncodingChanged)
            }
            Some("FileMissing") => Self::mismatch_verdict(evidence_id, SpanMismatch::FileMissing),
            _ => verdict,
        }
    }

    pub fn mismatch_verdict(evidence_id: &str, mismatch: SpanMismatch) -> VerificationVerdict {
        VerificationVerdict::new(
            mismatch.status(),
            SpanMismatchReason::new(evidence_id, mismatch).to_json(),
        )
    }
}

fn read_failed(path: &Path, source: std::io::Error) -> SpanValidationError {
    SpanValidationError::ReadFailed {
        path: path.display().to_string(),
        source,
    }
}

fn normalize_candidate_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn validate_span_bounds(span: &EvidenceSpan) -> Result<(), SpanValidationError> {
    if span.byte_start <= span.byte_end {
        return Ok(());
    }
    Err(SpanValidationError::InvalidSpan {
        byte_start: span.byte_start,
        byte_end: span.byte_end,
    })
}

fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut hash = [0; 32];
    hash.copy_from_slice(&digest);
    hash
}

fn is_encoding_only_change(evidence: &MemoryEvidence, current_bytes: &[u8]) -> bool {
    let Some(detail) = evidence.detail.as_ref() else {
        return false;
    };
    normalize_text(detail.as_bytes()) == normalize_text(current_bytes)
}

fn normalize_text(bytes: &[u8]) -> Vec<u8> {
    let without_bom = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    without_bom
        .iter()
        .copied()
        .filter(|byte| *byte != b'\r')
        .collect()
}

fn reason_subkind(reason: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(reason)
        .ok()
        .and_then(|value| {
            value
                .get("subkind")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
}
