use std::cell::Cell;

use sha2::{Digest, Sha256};

use super::{
    SpanMismatchReason, SpanReader, SpanValidationError, SpanValidator, VerificationStatus,
};
use crate::identity::FileId;
use crate::memory::{EvidenceSpan, MemoryEvidence};

#[test]
fn unchanged_content_at_span_returns_verified() {
    let file_id = file_id("src/auth.ts");
    let source = b"prefix\nlet token = issue();\nsuffix\n";
    let span_bytes = b"let token = issue();";
    let start = source
        .windows(span_bytes.len())
        .position(|window| window == span_bytes)
        .expect("span start");
    let evidence = evidence_with_span(
        file_id,
        start as u32,
        (start + span_bytes.len()) as u32,
        2,
        2,
        span_bytes,
        "let token = issue();",
    );

    let verdict = SpanValidator::validate(&evidence, source);

    assert_eq!(verdict.status, VerificationStatus::Verified);
}

#[test]
fn modified_content_at_span_returns_stale_content_changed() {
    let reader = MockSpanReader::new(
        Some(32),
        Some(b"let token = refresh();".to_vec()),
        Some(b"let token = refresh();\n".to_vec()),
    );
    let evidence = evidence_with_span(
        file_id("src/auth.ts"),
        0,
        20,
        1,
        1,
        b"let token = issue();",
        "let token = issue();",
    );

    let verdict = SpanValidator::validate_with_reader("memory-1:evidence:0", &evidence, &reader)
        .expect("span validation succeeds");

    assert_eq!(verdict.status, VerificationStatus::Stale);
    assert_eq!(reason(&verdict.reason).subkind, "ContentChanged");
}

#[test]
fn truncated_file_below_span_returns_file_shrunk_below_span() {
    let reader = MockSpanReader::new(Some(8), None, None);
    let evidence = evidence_with_span(
        file_id("src/auth.ts"),
        0,
        20,
        1,
        1,
        b"let token = issue();",
        "let token = issue();",
    );

    let verdict = SpanValidator::validate_with_reader("memory-1:evidence:0", &evidence, &reader)
        .expect("span validation succeeds");

    assert_eq!(verdict.status, VerificationStatus::Stale);
    assert_eq!(reason(&verdict.reason).subkind, "FileShrunkBelowSpan");
}

#[test]
fn crlf_flip_is_reported_as_encoding_changed() {
    let reader = MockSpanReader::new(
        Some(32),
        Some(b"alpha\r\nbeta".to_vec()),
        Some(b"\xef\xbb\xbfalpha\r\nbeta\r\n".to_vec()),
    );
    let evidence = evidence_with_span(
        file_id("src/auth.ts"),
        0,
        10,
        1,
        2,
        b"alpha\nbeta",
        "alpha\nbeta\n",
    );

    let verdict = SpanValidator::validate_with_reader("memory-1:evidence:0", &evidence, &reader)
        .expect("span validation succeeds");

    assert_eq!(verdict.status, VerificationStatus::Stale);
    assert_eq!(reason(&verdict.reason).subkind, "EncodingChanged");
}

#[test]
fn evidence_without_span_is_a_verified_no_op() {
    let verdict = SpanValidator::validate(
        &MemoryEvidence {
            kind: "file".to_string(),
            reference: Some("src/auth.ts".to_string()),
            detail: None,
            captured_at: Some(100),
            span: None,
            evidence_content_hash: None,
        },
        b"irrelevant",
    );

    assert_eq!(verdict.status, VerificationStatus::Verified);
}

#[test]
fn span_validation_never_reads_the_whole_file() {
    let reader = MockSpanReader::new(
        Some(10_000),
        Some(b"issue".to_vec()),
        Some(b"issue\n".to_vec()),
    );
    let evidence = evidence_with_span(file_id("src/auth.ts"), 120, 125, 10, 10, b"issue", "issue");

    let verdict = SpanValidator::validate_with_reader("memory-1:evidence:0", &evidence, &reader)
        .expect("span validation succeeds");

    assert_eq!(verdict.status, VerificationStatus::Verified);
    assert_eq!(reader.bytes_read(), 5);
    assert_eq!(reader.line_bytes_read(), 0);
}

fn file_id(path: &str) -> FileId {
    FileId {
        workspace_id: "workspace-main".to_string(),
        repo_relative_path: path.to_string(),
        content_hash: "deadbeef".to_string(),
    }
}

fn evidence_with_span(
    file_id: FileId,
    byte_start: u32,
    byte_end: u32,
    line_start: u32,
    line_end: u32,
    bytes: &[u8],
    detail: &str,
) -> MemoryEvidence {
    MemoryEvidence {
        kind: "file".to_string(),
        reference: Some(file_id.repo_relative_path.clone()),
        detail: Some(detail.to_string()),
        captured_at: Some(100),
        span: Some(EvidenceSpan {
            file_id,
            byte_start,
            byte_end,
            line_start,
            line_end,
        }),
        evidence_content_hash: Some(hash(bytes)),
    }
}

fn hash(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut value = [0; 32];
    value.copy_from_slice(&digest);
    value
}

fn reason(value: &str) -> SpanMismatchReason {
    serde_json::from_str(value).expect("reason payload parses")
}

struct MockSpanReader {
    file_len: Option<u64>,
    range_bytes: Option<Vec<u8>>,
    line_bytes: Option<Vec<u8>>,
    bytes_read: Cell<usize>,
    line_bytes_read: Cell<usize>,
}

impl MockSpanReader {
    fn new(
        file_len: Option<u64>,
        range_bytes: Option<Vec<u8>>,
        line_bytes: Option<Vec<u8>>,
    ) -> Self {
        Self {
            file_len,
            range_bytes,
            line_bytes,
            bytes_read: Cell::new(0),
            line_bytes_read: Cell::new(0),
        }
    }

    fn bytes_read(&self) -> usize {
        self.bytes_read.get()
    }

    fn line_bytes_read(&self) -> usize {
        self.line_bytes_read.get()
    }
}

impl SpanReader for MockSpanReader {
    fn file_len(&self, _file_id: &FileId) -> Result<Option<u64>, SpanValidationError> {
        Ok(self.file_len)
    }

    fn read_range(
        &self,
        _file_id: &FileId,
        _start: u32,
        _end: u32,
    ) -> Result<Option<Vec<u8>>, SpanValidationError> {
        if let Some(bytes) = self.range_bytes.as_ref() {
            self.bytes_read.set(self.bytes_read.get() + bytes.len());
        }
        Ok(self.range_bytes.clone())
    }

    fn read_line_span(
        &self,
        _file_id: &FileId,
        _line_start: u32,
        _line_end: u32,
    ) -> Result<Option<Vec<u8>>, SpanValidationError> {
        if let Some(bytes) = self.line_bytes.as_ref() {
            self.line_bytes_read
                .set(self.line_bytes_read.get() + bytes.len());
        }
        Ok(self.line_bytes.clone())
    }
}
