//! Deterministic, privacy-preserving session digest admission and extraction.
//!
//! This module deliberately accepts a narrow, versioned payload.  It never
//! keeps the input JSON (or an error containing a value from it), so callers
//! cannot accidentally turn a session digest into a transcript archive.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::MemoryClass;
use crate::{DateTime, Utc};

pub const SESSION_DIGEST_SCHEMA_VERSION: u32 = 1;
pub const SESSION_DIGEST_EXTRACTOR_VERSION: &str = "session-digest-v1";
pub const MAX_SESSION_DIGEST_BYTES: usize = 256 * 1024;
pub const MAX_SESSION_ID_BYTES: usize = 128;
pub const MAX_EDITED_PATHS: usize = 128;
pub const MAX_EDITED_PATH_BYTES: usize = 512;
pub const MAX_OBSERVATIONS: usize = 64;
pub const MAX_OBSERVATION_BYTES: usize = 1024;
pub const MAX_FINAL_SUMMARY_BYTES: usize = 2_000;
pub const MAX_CHECK_LABEL_BYTES: usize = 256;
pub const MAX_ERROR_SUMMARY_BYTES: usize = 512;
const MAX_FUTURE_SKEW_SECONDS: i64 = 300;

/// A normalized, safe-to-store session digest.  This type contains no raw
/// JSON, transcript location, command output, or unfiltered text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDigest {
    pub schema_version: u32,
    pub session_id: String,
    pub repository_id: String,
    pub checkout_id: Option<String>,
    pub branch: Option<String>,
    pub ended_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub edited_paths: Vec<String>,
    pub final_summary: Option<String>,
    pub observations: Vec<SessionDigestObservation>,
    /// SHA-256 of the normalized, sanitized payload.  This is safe provenance,
    /// not a hash of the original caller input.
    pub payload_hash: String,
    /// Count only: no rejected value or path is retained.
    pub dropped_observation_count: usize,
}

/// Compatibility-oriented name that emphasizes that a parsed digest is always
/// normalized before it can be extracted or persisted.
pub type NormalizedSessionDigest = SessionDigest;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionDigestObservation {
    Check {
        label: String,
        outcome: CheckOutcome,
    },
    Error {
        category: String,
        fingerprint: String,
        status: ErrorStatus,
        summary: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    Passed,
    Failed,
    Skipped,
}

impl CheckOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorStatus {
    Observed,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDigestCandidate {
    pub kind: SessionDigestCandidateKind,
    pub memory_class: MemoryClass,
    /// A bounded, sanitized statement appropriate for a memory assertion.
    pub claim: String,
    pub evidence: SessionDigestEvidence,
    pub claim_fingerprint: String,
    pub assertion_fingerprint: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDigestCandidateKind {
    EditedPaths,
    CheckOutcome,
    ResolvedFailure,
    Narrative,
}

impl SessionDigestCandidateKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::EditedPaths => "edited_paths",
            Self::CheckOutcome => "check_outcome",
            Self::ResolvedFailure => "resolved_failure",
            Self::Narrative => "narrative",
        }
    }
}

/// Typed, bounded evidence retained by all extracted candidates.  The fields
/// are copies of normalized digest data and never refer to the input envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDigestEvidence {
    pub session_id: String,
    pub repository_id: String,
    pub checkout_id: Option<String>,
    pub branch: Option<String>,
    pub captured_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub edited_paths: Vec<String>,
    pub check: Option<SessionDigestCheckEvidence>,
    pub resolved_error: Option<SessionDigestErrorEvidence>,
    pub summary_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDigestCheckEvidence {
    pub label: String,
    pub outcome: CheckOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDigestErrorEvidence {
    pub category: String,
    pub fingerprint: String,
    pub summary: Option<String>,
}

/// Errors intentionally name only the violated contract.  They must never
/// include caller-provided values, which could be a secret or transcript path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SessionDigestError {
    #[error("invalid session digest JSON")]
    InvalidJson,
    #[error("session digest exceeds the input size limit")]
    InputTooLarge,
    #[error("unsupported session digest schema version")]
    UnsupportedSchemaVersion,
    #[error("invalid session digest session identifier")]
    InvalidSessionId,
    #[error("invalid session digest repository identity")]
    InvalidRepositoryId,
    #[error("invalid session digest checkout identity")]
    InvalidCheckoutId,
    #[error("invalid session digest branch")]
    InvalidBranch,
    #[error("invalid session digest timestamp")]
    InvalidTimestamp,
    #[error("too many edited paths in session digest")]
    TooManyEditedPaths,
    #[error("invalid edited path in session digest")]
    InvalidEditedPath,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSessionDigest {
    schema_version: u32,
    session_id: String,
    repository_id: String,
    #[serde(default)]
    checkout_id: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    ended_at: String,
    #[serde(default)]
    edited_paths: Vec<String>,
    #[serde(default)]
    final_summary: Option<String>,
    #[serde(default)]
    observations: Vec<Value>,
}

/// Parse, normalize, and sanitize a version-one session digest.
///
/// `received_at` is supplied by the daemon so a remote or malformed caller
/// cannot move capture time forward.  Timestamps more than five minutes in the
/// future are clamped to this value.
pub fn parse_session_digest(
    input: &str,
    received_at: DateTime<Utc>,
) -> Result<SessionDigest, SessionDigestError> {
    if input.len() > MAX_SESSION_DIGEST_BYTES {
        return Err(SessionDigestError::InputTooLarge);
    }

    let raw: RawSessionDigest =
        serde_json::from_str(input).map_err(|_| SessionDigestError::InvalidJson)?;
    if raw.schema_version != SESSION_DIGEST_SCHEMA_VERSION {
        return Err(SessionDigestError::UnsupportedSchemaVersion);
    }
    if !is_safe_opaque_id(&raw.session_id, MAX_SESSION_ID_BYTES) {
        return Err(SessionDigestError::InvalidSessionId);
    }
    if !is_safe_opaque_id(&raw.repository_id, MAX_SESSION_ID_BYTES) {
        return Err(SessionDigestError::InvalidRepositoryId);
    }
    let checkout_id = match raw.checkout_id {
        Some(value) if is_safe_opaque_id(&value, MAX_SESSION_ID_BYTES) => Some(value),
        Some(_) => return Err(SessionDigestError::InvalidCheckoutId),
        None => None,
    };
    let branch = match raw.branch {
        Some(value) if is_safe_branch(&value) => Some(value),
        Some(_) => return Err(SessionDigestError::InvalidBranch),
        None => None,
    };

    let ended_at =
        DateTime::parse_rfc3339(&raw.ended_at).map_err(|_| SessionDigestError::InvalidTimestamp)?;
    let ended_at = if ended_at.unix_seconds() > received_at.unix_seconds() + MAX_FUTURE_SKEW_SECONDS
    {
        received_at
    } else {
        ended_at
    };

    if raw.edited_paths.len() > MAX_EDITED_PATHS {
        return Err(SessionDigestError::TooManyEditedPaths);
    }
    let edited_paths = normalize_paths(raw.edited_paths)?;
    let final_summary = raw
        .final_summary
        .as_deref()
        .and_then(|value| sanitize_text(value, MAX_FINAL_SUMMARY_BYTES));

    let (observations, dropped_observation_count) = normalize_observations(raw.observations);
    let mut digest = SessionDigest {
        schema_version: SESSION_DIGEST_SCHEMA_VERSION,
        session_id: raw.session_id,
        repository_id: raw.repository_id,
        checkout_id,
        branch,
        ended_at,
        received_at,
        edited_paths,
        final_summary,
        observations,
        payload_hash: String::new(),
        dropped_observation_count,
    };
    digest.payload_hash = normalized_payload_hash(&digest);
    Ok(digest)
}

/// Deterministically derive the bounded memory candidates for a digest.
/// Extraction performs no I/O, command execution, file reads, or model calls.
pub fn extract_session_digest_candidates(
    digest: &SessionDigest,
    extractor_version: &str,
) -> Vec<SessionDigestCandidate> {
    let extractor_version = sanitize_extractor_version(extractor_version);
    let mut candidates = Vec::new();

    if !digest.edited_paths.is_empty() {
        let claim = format!(
            "Session edited {} repository file(s).",
            digest.edited_paths.len()
        );
        candidates.push(candidate(
            digest,
            SessionDigestCandidateKind::EditedPaths,
            MemoryClass::WorkflowOutcome,
            claim,
            SessionDigestEvidence {
                session_id: digest.session_id.clone(),
                repository_id: digest.repository_id.clone(),
                checkout_id: digest.checkout_id.clone(),
                branch: digest.branch.clone(),
                captured_at: digest.received_at,
                ended_at: digest.ended_at,
                edited_paths: digest.edited_paths.clone(),
                check: None,
                resolved_error: None,
                summary_hash: None,
            },
            &extractor_version,
        ));
    }

    for observation in &digest.observations {
        if let SessionDigestObservation::Check { label, outcome } = observation {
            let claim = format!("Check `{label}` {}.", outcome.as_str());
            candidates.push(candidate(
                digest,
                SessionDigestCandidateKind::CheckOutcome,
                MemoryClass::WorkflowOutcome,
                claim,
                base_evidence(
                    digest,
                    SessionDigestEvidence {
                        session_id: String::new(),
                        repository_id: String::new(),
                        checkout_id: None,
                        branch: None,
                        captured_at: digest.received_at,
                        ended_at: digest.ended_at,
                        edited_paths: Vec::new(),
                        check: Some(SessionDigestCheckEvidence {
                            label: label.clone(),
                            outcome: *outcome,
                        }),
                        resolved_error: None,
                        summary_hash: None,
                    },
                ),
                &extractor_version,
            ));
        }
    }

    for error in matched_resolved_errors(&digest.observations) {
        let claim = format!("Resolved {} failure.", error.category);
        candidates.push(candidate(
            digest,
            SessionDigestCandidateKind::ResolvedFailure,
            MemoryClass::FailurePattern,
            claim,
            base_evidence(
                digest,
                SessionDigestEvidence {
                    session_id: String::new(),
                    repository_id: String::new(),
                    checkout_id: None,
                    branch: None,
                    captured_at: digest.received_at,
                    ended_at: digest.ended_at,
                    edited_paths: Vec::new(),
                    check: None,
                    resolved_error: Some(error),
                    summary_hash: None,
                },
            ),
            &extractor_version,
        ));
    }

    // Narrative prose is evidence only when paired with at least one typed
    // observation.  It cannot on its own claim a check result or resolution.
    if let (Some(summary), true) = (&digest.final_summary, !digest.observations.is_empty()) {
        candidates.push(candidate(
            digest,
            SessionDigestCandidateKind::Narrative,
            MemoryClass::WorkflowOutcome,
            summary.clone(),
            base_evidence(
                digest,
                SessionDigestEvidence {
                    session_id: String::new(),
                    repository_id: String::new(),
                    checkout_id: None,
                    branch: None,
                    captured_at: digest.received_at,
                    ended_at: digest.ended_at,
                    edited_paths: Vec::new(),
                    check: None,
                    resolved_error: None,
                    summary_hash: Some(hash_text(summary)),
                },
            ),
            &extractor_version,
        ));
    }

    candidates.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.idempotency_key.cmp(&right.idempotency_key))
    });
    candidates
}

/// Extract with the version shipped by this crate.
pub fn extract_default_session_digest_candidates(
    digest: &SessionDigest,
) -> Vec<SessionDigestCandidate> {
    extract_session_digest_candidates(digest, SESSION_DIGEST_EXTRACTOR_VERSION)
}

fn candidate(
    digest: &SessionDigest,
    kind: SessionDigestCandidateKind,
    memory_class: MemoryClass,
    claim: String,
    evidence: SessionDigestEvidence,
    extractor_version: &str,
) -> SessionDigestCandidate {
    let evidence_fingerprint = hash_json(&evidence);
    let claim_fingerprint = hash_parts(&[
        "session-digest-claim",
        extractor_version,
        memory_class.as_str(),
        &claim,
    ]);
    let assertion_fingerprint = hash_parts(&[
        "session-digest-assertion",
        extractor_version,
        kind.as_str(),
        &evidence_fingerprint,
    ]);
    let idempotency_key = hash_parts(&[
        "session-digest-idempotency",
        extractor_version,
        &digest.repository_id,
        &digest.session_id,
        kind.as_str(),
        &evidence_fingerprint,
    ]);
    SessionDigestCandidate {
        kind,
        memory_class,
        claim,
        evidence,
        claim_fingerprint,
        assertion_fingerprint,
        idempotency_key,
    }
}

fn base_evidence(
    digest: &SessionDigest,
    mut evidence: SessionDigestEvidence,
) -> SessionDigestEvidence {
    evidence.session_id = digest.session_id.clone();
    evidence.repository_id = digest.repository_id.clone();
    evidence.checkout_id = digest.checkout_id.clone();
    evidence.branch = digest.branch.clone();
    evidence
}

fn normalize_paths(paths: Vec<String>) -> Result<Vec<String>, SessionDigestError> {
    let mut normalized = BTreeSet::new();
    for path in paths {
        if !is_safe_repository_path(&path) {
            return Err(SessionDigestError::InvalidEditedPath);
        }
        normalized.insert(path);
    }
    Ok(normalized.into_iter().collect())
}

fn normalize_observations(raw: Vec<Value>) -> (Vec<SessionDigestObservation>, usize) {
    let mut observations = Vec::new();
    let mut dropped = raw.len().saturating_sub(MAX_OBSERVATIONS);
    for value in raw.into_iter().take(MAX_OBSERVATIONS) {
        let record_is_bounded = serde_json::to_vec(&value)
            .map(|encoded| encoded.len() <= MAX_OBSERVATION_BYTES)
            .unwrap_or(false);
        if !record_is_bounded {
            dropped += 1;
            continue;
        }
        match normalize_observation(value) {
            Some(observation) => observations.push(observation),
            None => dropped += 1,
        }
    }
    observations.sort_by_key(observation_sort_key);
    observations.dedup();
    (observations, dropped)
}

fn normalize_observation(value: Value) -> Option<SessionDigestObservation> {
    let object = value.as_object()?;
    let kind = object.get("kind")?.as_str()?;
    match kind {
        "check" => {
            if !has_exact_keys(object, &["kind", "label", "outcome"]) {
                return None;
            }
            let label = sanitize_text(object.get("label")?.as_str()?, MAX_CHECK_LABEL_BYTES)?;
            let outcome = match object.get("outcome")?.as_str()? {
                "passed" => CheckOutcome::Passed,
                "failed" => CheckOutcome::Failed,
                "skipped" => CheckOutcome::Skipped,
                _ => return None,
            };
            Some(SessionDigestObservation::Check { label, outcome })
        }
        "error" => {
            if !has_exact_keys(
                object,
                &["kind", "category", "fingerprint", "status", "summary"],
            ) && !has_exact_keys(object, &["kind", "category", "fingerprint", "status"])
            {
                return None;
            }
            let category = object.get("category")?.as_str()?;
            let fingerprint = object.get("fingerprint")?.as_str()?;
            let status = match object.get("status")?.as_str()? {
                "observed" => ErrorStatus::Observed,
                "resolved" => ErrorStatus::Resolved,
                _ => return None,
            };
            if !is_safe_category(category) || !is_sha256_fingerprint(fingerprint) {
                return None;
            }
            let summary = object
                .get("summary")
                .and_then(Value::as_str)
                .and_then(|value| sanitize_text(value, MAX_ERROR_SUMMARY_BYTES));
            Some(SessionDigestObservation::Error {
                category: category.to_owned(),
                fingerprint: fingerprint.to_owned(),
                status,
                summary,
            })
        }
        _ => None,
    }
}

fn matched_resolved_errors(
    observations: &[SessionDigestObservation],
) -> Vec<SessionDigestErrorEvidence> {
    let observed: BTreeSet<&str> = observations
        .iter()
        .filter_map(|observation| match observation {
            SessionDigestObservation::Error {
                fingerprint,
                status: ErrorStatus::Observed,
                ..
            } => Some(fingerprint.as_str()),
            _ => None,
        })
        .collect();
    let mut errors = observations
        .iter()
        .filter_map(|observation| match observation {
            SessionDigestObservation::Error {
                category,
                fingerprint,
                status: ErrorStatus::Resolved,
                summary,
            } if observed.contains(fingerprint.as_str()) => Some(SessionDigestErrorEvidence {
                category: category.clone(),
                fingerprint: fingerprint.clone(),
                summary: summary.clone(),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    errors.sort_by(|left, right| {
        left.fingerprint
            .cmp(&right.fingerprint)
            .then_with(|| left.category.cmp(&right.category))
            .then_with(|| left.summary.cmp(&right.summary))
    });
    errors.dedup();
    errors
}

fn observation_sort_key(observation: &SessionDigestObservation) -> String {
    match observation {
        SessionDigestObservation::Check { label, outcome } => {
            format!("check\0{}\0{}", label, outcome.as_str())
        }
        SessionDigestObservation::Error {
            category,
            fingerprint,
            status,
            summary,
        } => format!(
            "error\0{category}\0{fingerprint}\0{}\0{}",
            match status {
                ErrorStatus::Observed => "observed",
                ErrorStatus::Resolved => "resolved",
            },
            summary.as_deref().unwrap_or_default()
        ),
    }
}

fn has_exact_keys(object: &serde_json::Map<String, Value>, expected: &[&str]) -> bool {
    object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
}

fn is_safe_opaque_id(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
}

fn is_safe_branch(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains("..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
}

fn is_safe_repository_path(path: &str) -> bool {
    if path.is_empty()
        || path.len() > MAX_EDITED_PATH_BYTES
        || path.contains('\0')
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains('\\')
        || is_windows_drive_path(path)
    {
        return false;
    }
    let mut components = path.split('/');
    let Some(first) = components.next() else {
        return false;
    };
    if is_excluded_component(first) {
        return false;
    }
    for component in std::iter::once(first).chain(components) {
        if component.is_empty()
            || component == "."
            || component == ".."
            || is_excluded_component(component)
        {
            return false;
        }
    }
    true
}

fn is_excluded_component(component: &str) -> bool {
    matches!(
        component,
        ".git" | ".lattice" | "target" | "node_modules" | "vendor" | ".env"
    ) || component.starts_with(".env.")
}

fn is_windows_drive_path(value: &str) -> bool {
    value.as_bytes().get(1) == Some(&b':')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
}

fn is_safe_category(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn is_sha256_fingerprint(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Normalize plain text and drop it whenever it contains a secret, a home
/// directory, a likely external path, or a shell/control construct.  Dropping
/// an ambiguous field is intentional: the caller can still retain unrelated,
/// safe observations from the same digest.
fn sanitize_text(value: &str, max_bytes: usize) -> Option<String> {
    if value.is_empty() || value.contains('\0') || value.chars().any(char::is_control) {
        return None;
    }
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty()
        || contains_secret_material(&normalized)
        || contains_external_path(&normalized)
        || contains_shell_construct(&normalized)
    {
        return None;
    }
    Some(truncate_utf8(&normalized, max_bytes))
}

fn contains_secret_material(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let sensitive_assignments = [
        "password=",
        "password =",
        "passwd=",
        "secret=",
        "secret =",
        "token=",
        "token =",
        "api_key=",
        "api_key =",
        "apikey=",
        "authorization:",
        "bearer ",
        "aws_secret_access_key=",
        "database_url=",
        "connection_string=",
    ];
    lower.contains("-----begin ") && lower.contains("private key-----")
        || sensitive_assignments
            .iter()
            .any(|needle| lower.contains(needle))
        || (lower.contains("://") && lower.contains('@'))
        || has_prefixed_secret(value, "ghp_", 24)
        || has_prefixed_secret(value, "gho_", 24)
        || has_prefixed_secret(value, "github_pat_", 30)
        || has_prefixed_secret(value, "sk-", 20)
        || has_prefixed_secret(value, "xox", 20)
        || has_prefixed_secret(value, "AKIA", 20)
        || looks_like_jwt(value)
}

fn has_prefixed_secret(value: &str, prefix: &str, minimum_len: usize) -> bool {
    value
        .split(|character: char| {
            !character.is_ascii_alphanumeric() && character != '_' && character != '-'
        })
        .any(|token| token.starts_with(prefix) && token.len() >= minimum_len)
}

fn looks_like_jwt(value: &str) -> bool {
    value.split_whitespace().any(|token| {
        token.starts_with("eyJ")
            && token.len() >= 32
            && token.bytes().filter(|byte| *byte == b'.').count() == 2
    })
}

fn contains_external_path(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("~/")
        || lower.contains("/home/")
        || lower.contains("/users/")
        || lower.contains("/private/")
        || lower.contains("/tmp/")
        || lower.contains("/var/")
        || lower.contains("/etc/")
        || lower.contains("/opt/")
        || lower.contains("/usr/")
        || lower.contains("/mnt/")
        || lower.contains("file://")
        || lower.contains("\\users\\")
        || lower.contains("\\home\\")
        || is_windows_drive_path(value)
}

fn contains_shell_construct(value: &str) -> bool {
    value.contains('`')
        || value.contains("$(")
        || value.contains(";")
        || value.contains("&&")
        || value.contains("||")
        || value.contains('|')
        || value.contains('>')
        || value.contains('<')
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn normalized_payload_hash(digest: &SessionDigest) -> String {
    #[derive(Serialize)]
    struct Payload<'a> {
        schema_version: u32,
        session_id: &'a str,
        repository_id: &'a str,
        checkout_id: &'a Option<String>,
        branch: &'a Option<String>,
        ended_at: DateTime<Utc>,
        received_at: DateTime<Utc>,
        edited_paths: &'a [String],
        final_summary: &'a Option<String>,
        observations: &'a [SessionDigestObservation],
    }
    hash_json(&Payload {
        schema_version: digest.schema_version,
        session_id: &digest.session_id,
        repository_id: &digest.repository_id,
        checkout_id: &digest.checkout_id,
        branch: &digest.branch,
        ended_at: digest.ended_at,
        received_at: digest.received_at,
        edited_paths: &digest.edited_paths,
        final_summary: &digest.final_summary,
        observations: &digest.observations,
    })
}

fn sanitize_extractor_version(value: &str) -> String {
    if is_safe_opaque_id(value, 128) {
        value.to_owned()
    } else {
        SESSION_DIGEST_EXTRACTOR_VERSION.to_owned()
    }
}

fn hash_json(value: &impl Serialize) -> String {
    let bytes =
        serde_json::to_vec(value).expect("session digest types serialize deterministically");
    hash_bytes(&bytes)
}

fn hash_text(value: &str) -> String {
    hash_bytes(value.as_bytes())
}

fn hash_parts(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FINGERPRINT: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn received_at() -> DateTime<Utc> {
        DateTime::from_unix_seconds(1_800_000_000)
    }

    fn parse(input: &str) -> SessionDigest {
        parse_session_digest(input, received_at()).expect("digest should parse")
    }

    #[test]
    fn parses_sanitizes_sorts_and_extracts_deterministically() {
        let input = format!(
            r#"{{
                "schema_version":1,
                "session_id":"session-1",
                "repository_id":"repo-1",
                "checkout_id":"checkout-1",
                "branch":"feature/digest",
                "ended_at":"2026-05-17T14:30:00Z",
                "edited_paths":["src/z.rs","src/a.rs","src/a.rs"],
                "final_summary":"  Implemented digest capture.  ",
                "observations":[
                  {{"kind":"error","category":"compiler","fingerprint":"{FINGERPRINT}","status":"resolved","summary":"unresolved import"}},
                  {{"kind":"check","label":"cargo test -p lattice-core","outcome":"passed"}},
                  {{"kind":"error","category":"compiler","fingerprint":"{FINGERPRINT}","status":"observed","summary":"unresolved import"}}
                ]
            }}"#
        );
        let digest = parse(&input);
        assert_eq!(digest.edited_paths, ["src/a.rs", "src/z.rs"]);
        assert_eq!(
            digest.final_summary.as_deref(),
            Some("Implemented digest capture.")
        );
        assert_eq!(digest.observations.len(), 3);

        let first = extract_default_session_digest_candidates(&digest);
        let second = extract_default_session_digest_candidates(&digest);
        assert_eq!(first, second);
        assert_eq!(first.len(), 4);
        assert!(first
            .iter()
            .any(|candidate| candidate.kind == SessionDigestCandidateKind::ResolvedFailure));
        assert!(first
            .iter()
            .all(|candidate| candidate.idempotency_key.starts_with("sha256:")));
    }

    #[test]
    fn rejects_unbounded_or_out_of_workspace_paths() {
        for path in [
            "/tmp/source.rs",
            "../outside.rs",
            "src/../outside.rs",
            ".git/config",
            "target/debug/app",
            "C:\\Users\\pete\\x.rs",
        ] {
            let path_json = serde_json::to_string(path).unwrap();
            let input = format!(
                r#"{{"schema_version":1,"session_id":"s","repository_id":"r","ended_at":"2026-05-17T14:30:00Z","edited_paths":[{path_json}],"observations":[]}}"#
            );
            assert_eq!(
                parse_session_digest(&input, received_at()),
                Err(SessionDigestError::InvalidEditedPath),
                "path {path:?}"
            );
        }
    }

    #[test]
    fn never_retains_secret_or_external_path_text() {
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let input = format!(
            r#"{{
              "schema_version":1,"session_id":"s","repository_id":"r",
              "ended_at":"2026-05-17T14:30:00Z",
              "final_summary":"token={secret}",
              "observations":[
                {{"kind":"check","label":"cargo test; echo {secret}","outcome":"passed"}},
                {{"kind":"error","category":"compiler","fingerprint":"{FINGERPRINT}","status":"observed","summary":"/Users/pete/secret.rs"}}
              ]
            }}"#
        );
        let digest = parse(&input);
        let serialized = serde_json::to_string(&digest).unwrap();
        assert!(digest.final_summary.is_none());
        assert!(digest
            .observations
            .iter()
            .all(|observation| !matches!(observation, SessionDigestObservation::Check { .. })));
        assert!(!serialized.contains(secret));
        assert!(!serialized.contains("/Users/pete"));
    }

    #[test]
    fn ignores_unknown_or_malformed_optional_observations() {
        let input = r#"{
          "schema_version":1,"session_id":"s","repository_id":"r",
          "ended_at":"2026-05-17T14:30:00Z",
          "observations":[
            {"kind":"tool_output","content":"not admitted"},
            {"kind":"check","label":"cargo test","outcome":"passed","extra":"nope"},
            {"kind":"check","label":"cargo test","outcome":"passed"}
          ]
        }"#;
        let digest = parse(input);
        assert_eq!(digest.observations.len(), 1);
        assert_eq!(digest.dropped_observation_count, 2);
    }

    #[test]
    fn prose_and_unmatched_resolution_cannot_assert_outcomes() {
        let only_prose = parse(
            r#"{
          "schema_version":1,"session_id":"s","repository_id":"r",
          "ended_at":"2026-05-17T14:30:00Z",
          "final_summary":"All tests passed and compiler error is resolved."
        }"#,
        );
        assert!(extract_default_session_digest_candidates(&only_prose).is_empty());

        let unmatched = parse(&format!(
            r#"{{
          "schema_version":1,"session_id":"s","repository_id":"r",
          "ended_at":"2026-05-17T14:30:00Z",
          "observations":[{{"kind":"error","category":"compiler","fingerprint":"{FINGERPRINT}","status":"resolved"}}]
        }}"#
        ));
        assert!(extract_default_session_digest_candidates(&unmatched)
            .iter()
            .all(|candidate| candidate.kind != SessionDigestCandidateKind::ResolvedFailure));
    }

    #[test]
    fn future_timestamp_is_clamped_and_unknown_envelope_fields_are_rejected() {
        let clamped = parse(
            r#"{
          "schema_version":1,"session_id":"s","repository_id":"r",
          "ended_at":"2099-01-01T00:00:00Z"
        }"#,
        );
        assert_eq!(clamped.ended_at, received_at());
        let invalid = r#"{
          "schema_version":1,"session_id":"s","repository_id":"r",
          "ended_at":"2026-05-17T14:30:00Z","transcript_path":"/tmp/agent.jsonl"
        }"#;
        assert_eq!(
            parse_session_digest(invalid, received_at()),
            Err(SessionDigestError::InvalidJson)
        );
    }
}
