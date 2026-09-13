//! Bounded session-capture admission and deterministic segment reduction.
//!
//! Transport payloads contain facts only. Session, repository, checkout, Git,
//! and memory-scope authority is supplied separately by the daemon after hook
//! capability verification. The reducer is pure and never reads a workspace,
//! transcript, command, or tool payload.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value};

use super::session_digest::{
    bind_session_digest_authority, extract_session_digest_candidates, is_safe_category,
    is_sha256_fingerprint, normalize_paths, normalized_payload_hash, sanitize_text, CheckOutcome,
    ErrorStatus, SessionDigest, SessionDigestAuthority, SessionDigestCandidate,
    SessionDigestContent, SessionDigestError, SessionDigestObservation, MAX_CHECK_LABEL_BYTES,
    MAX_ERROR_SUMMARY_BYTES, MAX_FINAL_SUMMARY_BYTES, SESSION_DIGEST_SCHEMA_VERSION,
};
use crate::{DateTime, Utc};

pub const SESSION_CAPTURE_SCHEMA_VERSION: u32 = 1;
pub const MAX_SESSION_CAPTURE_EVENT_BYTES: usize = 4 * 1024;
pub const MAX_SESSION_CAPTURE_CLOSE_BYTES: usize = 4 * 1024;
pub const MAX_SESSION_CAPTURE_SELECTOR_BYTES: usize = 512;
/// Maximum admitted size of a sanitized host-provided assistant turn summary.
pub const MAX_TURN_SUMMARY_BYTES: usize = MAX_FINAL_SUMMARY_BYTES;

/// Repository-local lifecycle bounds for the sanitized capture journal.
///
/// Both bounds are mandatory. A delivery is eligible for pruning when it is
/// older than `max_age` or falls outside the newest `max_captures` deliveries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionCaptureRetentionPolicy {
    max_age: Duration,
    max_captures: usize,
}

impl SessionCaptureRetentionPolicy {
    pub fn new(max_age: Duration, max_captures: usize) -> Result<Self, SessionCaptureError> {
        if max_age.is_zero() || max_captures == 0 {
            return Err(SessionCaptureError::InvalidRetentionPolicy);
        }
        Ok(Self {
            max_age,
            max_captures,
        })
    }

    pub(crate) fn max_age(self) -> Duration {
        self.max_age
    }

    pub(crate) fn max_captures(self) -> usize {
        self.max_captures
    }
}

/// An authority-qualified opaque selector for operator deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCaptureSelector {
    repository_id: String,
    opaque_id: String,
    kind: SessionCaptureSelectorKind,
}

impl SessionCaptureSelector {
    pub fn session(
        repository_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Result<Self, SessionCaptureError> {
        Self::new(
            repository_id.into(),
            session_id.into(),
            SessionCaptureSelectorKind::Session,
        )
    }

    pub fn capture(
        repository_id: impl Into<String>,
        capture_id: impl Into<String>,
    ) -> Result<Self, SessionCaptureError> {
        Self::new(
            repository_id.into(),
            capture_id.into(),
            SessionCaptureSelectorKind::Capture,
        )
    }

    fn new(
        repository_id: String,
        opaque_id: String,
        kind: SessionCaptureSelectorKind,
    ) -> Result<Self, SessionCaptureError> {
        if !valid_opaque_selector_component(&repository_id)
            || !valid_opaque_selector_component(&opaque_id)
        {
            return Err(SessionCaptureError::InvalidLifecycleSelector);
        }
        Ok(Self {
            repository_id,
            opaque_id,
            kind,
        })
    }

    pub(crate) fn repository_id(&self) -> &str {
        &self.repository_id
    }

    pub(crate) fn opaque_id(&self) -> &str {
        &self.opaque_id
    }

    pub(crate) fn kind(&self) -> SessionCaptureSelectorKind {
        self.kind
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionCaptureSelectorKind {
    Session,
    Capture,
}

/// Content-free outcome of retention or operator deletion.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionCaptureDeletionResult {
    pub deleted_capture_ids: Vec<String>,
    pub deleted_memory_count: usize,
    pub retained_derived_memory_count: usize,
    pub retained_proposal_count: usize,
}

/// A normalized fact admitted from one authenticated hook event.
///
/// This value deliberately has no session, repository, checkout, branch,
/// revision, scope, organization, cwd, prompt, tool, command, diff, transcript,
/// or free-form metadata field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCaptureEvent {
    pub schema_version: u32,
    pub fact: SessionCaptureFact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionCaptureFact {
    EditedPath {
        path: String,
    },
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
    /// Non-authoritative prose from the host's `last_assistant_message` field.
    /// It can supplement typed observations at session end but cannot certify
    /// an outcome by itself.
    TurnSummary {
        summary: String,
    },
}

/// A normalized authenticated close payload. Capture and end time are supplied
/// by the daemon, not selected by the hook payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCaptureClose {
    pub schema_version: u32,
    pub received_at: DateTime<Utc>,
    pub final_summary: Option<String>,
}

/// One normalized event paired with authority sampled by the daemon at event
/// admission. The pairing is an internal reduction input, not a wire schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonSessionCaptureEvent {
    pub authority: SessionDigestAuthority,
    pub event: SessionCaptureEvent,
}

/// Errors name only a contract category and never echo rejected input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SessionCaptureError {
    #[error("invalid session capture JSON")]
    InvalidJson,
    #[error("session capture input exceeds the size limit")]
    InputTooLarge,
    #[error("unsupported session capture schema version")]
    UnsupportedSchemaVersion,
    #[error("invalid session capture event kind")]
    InvalidEventKind,
    #[error("invalid session capture edited path")]
    InvalidEditedPath,
    #[error("invalid session capture check")]
    InvalidCheck,
    #[error("invalid session capture error observation")]
    InvalidError,
    #[error("invalid session capture turn summary")]
    InvalidTurnSummary,
    #[error("session capture authority does not match the closing session")]
    AuthorityMismatch,
    #[error("session capture segments are not ordered")]
    SegmentOrder,
    #[error("invalid daemon-owned session capture authority")]
    InvalidAuthority,
    #[error("invalid session capture retention policy")]
    InvalidRetentionPolicy,
    #[error("invalid session capture lifecycle selector")]
    InvalidLifecycleSelector,
}

/// Parse one narrow event envelope.
///
/// Accepted shapes are `edited_path`, `check`, `error`, and `turn_summary`.
/// Exact keys are enforced for every shape, so authority-like fields and
/// unknown metadata are rejected instead of being retained or silently
/// interpreted.
pub fn parse_session_capture_event(
    input: &str,
) -> Result<SessionCaptureEvent, SessionCaptureError> {
    if input.len() > MAX_SESSION_CAPTURE_EVENT_BYTES {
        return Err(SessionCaptureError::InputTooLarge);
    }
    let value: Value = serde_json::from_str(input).map_err(|_| SessionCaptureError::InvalidJson)?;
    let object = value.as_object().ok_or(SessionCaptureError::InvalidJson)?;
    parse_schema_version(object)?;
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or(SessionCaptureError::InvalidEventKind)?;

    let fact = match kind {
        "edited_path" => {
            require_exact_keys(object, &["schema_version", "kind", "path"])?;
            let path = object
                .get("path")
                .and_then(Value::as_str)
                .ok_or(SessionCaptureError::InvalidEditedPath)?;
            let mut paths = normalize_paths(vec![path.to_owned()])
                .map_err(|_| SessionCaptureError::InvalidEditedPath)?;
            SessionCaptureFact::EditedPath {
                path: paths.pop().expect("one admitted path remains one path"),
            }
        }
        "check" => {
            require_exact_keys(object, &["schema_version", "kind", "label", "outcome"])?;
            let label = object
                .get("label")
                .and_then(Value::as_str)
                .and_then(|value| sanitize_text(value, MAX_CHECK_LABEL_BYTES))
                .ok_or(SessionCaptureError::InvalidCheck)?;
            let outcome = match object.get("outcome").and_then(Value::as_str) {
                Some("passed") => CheckOutcome::Passed,
                Some("failed") => CheckOutcome::Failed,
                Some("skipped") => CheckOutcome::Skipped,
                _ => return Err(SessionCaptureError::InvalidCheck),
            };
            SessionCaptureFact::Check { label, outcome }
        }
        "error" => {
            let has_summary = object.contains_key("summary");
            let expected = if has_summary {
                &[
                    "schema_version",
                    "kind",
                    "category",
                    "fingerprint",
                    "status",
                    "summary",
                ][..]
            } else {
                &[
                    "schema_version",
                    "kind",
                    "category",
                    "fingerprint",
                    "status",
                ][..]
            };
            require_exact_keys(object, expected)?;
            let category = object
                .get("category")
                .and_then(Value::as_str)
                .filter(|value| is_safe_category(value))
                .ok_or(SessionCaptureError::InvalidError)?
                .to_owned();
            let fingerprint = object
                .get("fingerprint")
                .and_then(Value::as_str)
                .filter(|value| is_sha256_fingerprint(value))
                .ok_or(SessionCaptureError::InvalidError)?
                .to_owned();
            let status = match object.get("status").and_then(Value::as_str) {
                Some("observed") => ErrorStatus::Observed,
                Some("resolved") => ErrorStatus::Resolved,
                _ => return Err(SessionCaptureError::InvalidError),
            };
            let summary = match object.get("summary") {
                Some(Value::String(value)) => sanitize_text(value, MAX_ERROR_SUMMARY_BYTES),
                Some(_) => return Err(SessionCaptureError::InvalidError),
                None => None,
            };
            SessionCaptureFact::Error {
                category,
                fingerprint,
                status,
                summary,
            }
        }
        "turn_summary" => {
            require_exact_keys(object, &["schema_version", "kind", "summary"])?;
            let summary = object
                .get("summary")
                .and_then(Value::as_str)
                .and_then(normalize_turn_summary)
                .ok_or(SessionCaptureError::InvalidTurnSummary)?;
            SessionCaptureFact::TurnSummary { summary }
        }
        _ => return Err(SessionCaptureError::InvalidEventKind),
    };

    Ok(SessionCaptureEvent {
        schema_version: SESSION_CAPTURE_SCHEMA_VERSION,
        fact,
    })
}

/// Admit the host's bounded `last_assistant_message` as non-authoritative
/// narrative prose.
///
/// Adapters should call this function only for that dedicated host field. An
/// absent, oversized, or unsafe value is omitted rather than truncated or
/// retained. The returned fact still cannot produce memory without a typed
/// observation in the same daemon-owned segment.
pub fn session_capture_turn_summary_from_host(
    last_assistant_message: Option<&str>,
) -> Option<SessionCaptureEvent> {
    let summary = normalize_turn_summary(last_assistant_message?)?;
    Some(SessionCaptureEvent {
        schema_version: SESSION_CAPTURE_SCHEMA_VERSION,
        fact: SessionCaptureFact::TurnSummary { summary },
    })
}

/// Parse a close marker. Its only optional content is a bounded safe summary;
/// receive time is injected by the daemon.
pub fn parse_session_capture_close(
    input: &str,
    received_at: DateTime<Utc>,
) -> Result<SessionCaptureClose, SessionCaptureError> {
    if input.len() > MAX_SESSION_CAPTURE_CLOSE_BYTES {
        return Err(SessionCaptureError::InputTooLarge);
    }
    let value: Value = serde_json::from_str(input).map_err(|_| SessionCaptureError::InvalidJson)?;
    let object = value.as_object().ok_or(SessionCaptureError::InvalidJson)?;
    parse_schema_version(object)?;
    let expected = if object.contains_key("final_summary") {
        &["schema_version", "final_summary"][..]
    } else {
        &["schema_version"][..]
    };
    require_exact_keys(object, expected)?;
    let final_summary = match object.get("final_summary") {
        Some(Value::String(value)) => sanitize_text(value, MAX_FINAL_SUMMARY_BYTES),
        Some(_) => return Err(SessionCaptureError::InvalidJson),
        None => None,
    };
    Ok(SessionCaptureClose {
        schema_version: SESSION_CAPTURE_SCHEMA_VERSION,
        received_at,
        final_summary,
    })
}

/// Reduce ordered admitted events into one digest per daemon-observed Git
/// segment. The close authority identifies the final segment. Each segment
/// retains its latest safe turn summary until the true close, then attaches it
/// only when the segment also contains a typed observation. Summary prose does
/// not create typed checks or resolutions.
pub fn reduce_session_capture(
    ordered_events: &[DaemonSessionCaptureEvent],
    close: &SessionCaptureClose,
    close_authority: &SessionDigestAuthority,
) -> Result<Vec<SessionDigest>, SessionCaptureError> {
    #[derive(Default)]
    struct SegmentFacts {
        authority: Option<SessionDigestAuthority>,
        edited_paths: Vec<String>,
        observations: Vec<SessionDigestObservation>,
        observed_error_fingerprints: BTreeSet<String>,
        latest_turn_summary: Option<String>,
    }

    if close.schema_version != SESSION_CAPTURE_SCHEMA_VERSION {
        return Err(SessionCaptureError::UnsupportedSchemaVersion);
    }

    let mut segments = BTreeMap::<u64, SegmentFacts>::new();
    let mut previous_segment = None;
    for admitted in ordered_events {
        if admitted.event.schema_version != SESSION_CAPTURE_SCHEMA_VERSION {
            return Err(SessionCaptureError::UnsupportedSchemaVersion);
        }
        ensure_same_session(&admitted.authority, close_authority)?;
        if previous_segment.is_some_and(|previous| admitted.authority.segment < previous) {
            return Err(SessionCaptureError::SegmentOrder);
        }
        previous_segment = Some(admitted.authority.segment);
        let segment = segments.entry(admitted.authority.segment).or_default();
        match &segment.authority {
            Some(existing) if existing != &admitted.authority => {
                return Err(SessionCaptureError::AuthorityMismatch);
            }
            None => segment.authority = Some(admitted.authority.clone()),
            _ => {}
        }
        match &admitted.event.fact {
            SessionCaptureFact::EditedPath { path } => segment.edited_paths.push(path.clone()),
            SessionCaptureFact::Check { label, outcome } => {
                segment.observations.push(SessionDigestObservation::Check {
                    label: label.clone(),
                    outcome: *outcome,
                });
            }
            SessionCaptureFact::Error {
                category,
                fingerprint,
                status: ErrorStatus::Observed,
                summary,
            } => {
                segment
                    .observed_error_fingerprints
                    .insert(fingerprint.clone());
                segment.observations.push(SessionDigestObservation::Error {
                    category: category.clone(),
                    fingerprint: fingerprint.clone(),
                    status: ErrorStatus::Observed,
                    summary: summary.clone(),
                });
            }
            SessionCaptureFact::Error {
                category,
                fingerprint,
                status: ErrorStatus::Resolved,
                summary,
            } => {
                // Only a preceding observation with the same fingerprint in
                // this exact segment corroborates a resolution.
                if segment.observed_error_fingerprints.contains(fingerprint) {
                    segment.observations.push(SessionDigestObservation::Error {
                        category: category.clone(),
                        fingerprint: fingerprint.clone(),
                        status: ErrorStatus::Resolved,
                        summary: summary.clone(),
                    });
                }
            }
            SessionCaptureFact::TurnSummary { summary } => {
                // Defend the reduction boundary even if a caller constructed
                // the public fact directly instead of using the parser.
                if let Some(summary) = normalize_turn_summary(summary) {
                    segment.latest_turn_summary = Some(summary);
                }
            }
        }
    }

    if previous_segment.is_some_and(|previous| close_authority.segment < previous) {
        return Err(SessionCaptureError::SegmentOrder);
    }
    let final_segment = segments.entry(close_authority.segment).or_default();
    match &final_segment.authority {
        Some(existing) if existing != close_authority => {
            return Err(SessionCaptureError::AuthorityMismatch);
        }
        None => final_segment.authority = Some(close_authority.clone()),
        _ => {}
    }
    if let Some(summary) = &close.final_summary {
        final_segment.latest_turn_summary = Some(summary.clone());
    }

    let mut digests = Vec::with_capacity(segments.len());
    for (_, mut facts) in segments {
        let authority = facts.authority.take().expect("every segment has authority");
        facts.edited_paths = normalize_paths(facts.edited_paths)
            .map_err(|_| SessionCaptureError::InvalidEditedPath)?;
        facts.observations.sort_by(|left, right| {
            serde_json::to_string(left)
                .expect("observation serialization")
                .cmp(&serde_json::to_string(right).expect("observation serialization"))
        });
        facts.observations.dedup();
        // Narrative prose is retained only when a typed observation from the
        // same daemon-owned segment corroborates that work occurred. Edited
        // paths alone are not check or error evidence.
        let final_summary = if facts.observations.is_empty() {
            None
        } else {
            facts.latest_turn_summary
        };
        let mut content = SessionDigestContent {
            schema_version: SESSION_DIGEST_SCHEMA_VERSION,
            ended_at: close.received_at,
            received_at: close.received_at,
            edited_paths: facts.edited_paths,
            final_summary,
            observations: facts.observations,
            payload_hash: String::new(),
            dropped_observation_count: 0,
        };
        content.payload_hash = normalized_payload_hash(&content);
        let digest = bind_session_digest_authority(content, &authority)
            .map_err(map_digest_authority_error)?;
        digests.push(digest);
    }
    Ok(digests)
}

/// Reduce and extract all segment candidates with stable global ordering.
pub fn reduce_session_capture_candidates(
    ordered_events: &[DaemonSessionCaptureEvent],
    close: &SessionCaptureClose,
    close_authority: &SessionDigestAuthority,
    extractor_version: &str,
) -> Result<Vec<SessionDigestCandidate>, SessionCaptureError> {
    let mut candidates = reduce_session_capture(ordered_events, close, close_authority)?
        .iter()
        .flat_map(|digest| extract_session_digest_candidates(digest, extractor_version))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.idempotency_key.cmp(&right.idempotency_key));
    Ok(candidates)
}

fn parse_schema_version(object: &Map<String, Value>) -> Result<(), SessionCaptureError> {
    match object.get("schema_version").and_then(Value::as_u64) {
        Some(version) if version == u64::from(SESSION_CAPTURE_SCHEMA_VERSION) => Ok(()),
        Some(_) => Err(SessionCaptureError::UnsupportedSchemaVersion),
        None => Err(SessionCaptureError::InvalidJson),
    }
}

fn require_exact_keys(
    object: &Map<String, Value>,
    expected: &[&str],
) -> Result<(), SessionCaptureError> {
    if object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key)) {
        Ok(())
    } else {
        Err(SessionCaptureError::InvalidJson)
    }
}

fn ensure_same_session(
    authority: &SessionDigestAuthority,
    close_authority: &SessionDigestAuthority,
) -> Result<(), SessionCaptureError> {
    if authority.session_id == close_authority.session_id
        && authority.repository_id == close_authority.repository_id
        && authority.checkout_id == close_authority.checkout_id
    {
        Ok(())
    } else {
        Err(SessionCaptureError::AuthorityMismatch)
    }
}

fn map_digest_authority_error(_: SessionDigestError) -> SessionCaptureError {
    SessionCaptureError::InvalidAuthority
}

fn normalize_turn_summary(value: &str) -> Option<String> {
    if value.len() > MAX_TURN_SUMMARY_BYTES {
        return None;
    }
    sanitize_text(value, MAX_TURN_SUMMARY_BYTES)
}

fn valid_opaque_selector_component(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_SESSION_CAPTURE_SELECTOR_BYTES
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryClass, SessionDigestCandidateKind};

    const FINGERPRINT: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_FINGERPRINT: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn now() -> DateTime<Utc> {
        DateTime::from_unix_seconds(1_800_000_000)
    }

    fn authority(segment: u64, branch: Option<&str>, revision: &str) -> SessionDigestAuthority {
        SessionDigestAuthority {
            session_id: "session-1".to_owned(),
            repository_id: "repo-1".to_owned(),
            checkout_id: Some("checkout-1".to_owned()),
            branch: branch.map(str::to_owned),
            revision: revision.to_owned(),
            segment,
        }
    }

    fn event(authority: SessionDigestAuthority, json: &str) -> DaemonSessionCaptureEvent {
        DaemonSessionCaptureEvent {
            authority,
            event: parse_session_capture_event(json).unwrap(),
        }
    }

    #[test]
    fn event_and_close_reject_authority_sensitive_and_unknown_fields() {
        let prohibited = [
            "session_id",
            "repository_id",
            "repo",
            "checkout_id",
            "branch",
            "revision",
            "segment",
            "scope",
            "organization",
            "org",
            "cwd",
            "prompt",
            "tool_input",
            "tool_output",
            "command",
            "diff",
            "transcript",
            "transcript_path",
            "metadata",
        ];
        for field in prohibited {
            let event = format!(
                r#"{{"schema_version":1,"kind":"edited_path","path":"src/lib.rs","{field}":"forged"}}"#
            );
            assert_eq!(
                parse_session_capture_event(&event),
                Err(SessionCaptureError::InvalidJson),
                "event field {field}"
            );
            let close = format!(r#"{{"schema_version":1,"{field}":"forged"}}"#);
            assert_eq!(
                parse_session_capture_close(&close, now()),
                Err(SessionCaptureError::InvalidJson),
                "close field {field}"
            );
        }
    }

    #[test]
    fn host_turn_summary_admission_drops_oversized_and_unsafe_text() {
        let admitted = session_capture_turn_summary_from_host(Some("  Implemented   capture.  "))
            .expect("safe host summary");
        assert_eq!(
            admitted.fact,
            SessionCaptureFact::TurnSummary {
                summary: "Implemented capture.".to_owned(),
            }
        );
        assert!(session_capture_turn_summary_from_host(None).is_none());
        assert!(session_capture_turn_summary_from_host(Some(
            &"x".repeat(MAX_TURN_SUMMARY_BYTES + 1)
        ))
        .is_none());
        assert!(session_capture_turn_summary_from_host(Some(
            "Finished with token=secret-value-that-must-not-be-retained"
        ))
        .is_none());

        let oversized = format!(
            r#"{{"schema_version":1,"kind":"turn_summary","summary":"{}"}}"#,
            "x".repeat(MAX_TURN_SUMMARY_BYTES + 1)
        );
        assert_eq!(
            parse_session_capture_event(&oversized),
            Err(SessionCaptureError::InvalidTurnSummary)
        );
        assert_eq!(
            parse_session_capture_event(
                r#"{"schema_version":1,"kind":"turn_summary","summary":"ran `secret command`"}"#
            ),
            Err(SessionCaptureError::InvalidTurnSummary)
        );
    }

    #[test]
    fn turn_summary_wire_rejects_raw_host_authority_transcript_and_tool_fields() {
        let prohibited = [
            "last_assistant_message",
            "session_id",
            "repository_id",
            "checkout_id",
            "branch",
            "revision",
            "segment",
            "scope",
            "organization",
            "cwd",
            "transcript",
            "transcript_path",
            "tool_name",
            "tool_input",
            "tool_output",
            "command",
            "metadata",
        ];
        for field in prohibited {
            let input = format!(
                r#"{{"schema_version":1,"kind":"turn_summary","summary":"Safe summary.","{field}":"raw"}}"#
            );
            assert_eq!(
                parse_session_capture_event(&input),
                Err(SessionCaptureError::InvalidJson),
                "turn-summary field {field}"
            );
        }
    }

    #[test]
    fn reduction_and_candidate_order_are_deterministic() {
        let auth = authority(0, Some("main"), "abc123");
        let events = vec![
            event(
                auth.clone(),
                r#"{"schema_version":1,"kind":"edited_path","path":"src/z.rs"}"#,
            ),
            event(
                auth.clone(),
                r#"{"schema_version":1,"kind":"edited_path","path":"src/a.rs"}"#,
            ),
            event(
                auth.clone(),
                r#"{"schema_version":1,"kind":"check","label":"core tests","outcome":"passed"}"#,
            ),
        ];
        let close = parse_session_capture_close(
            r#"{"schema_version":1,"final_summary":"Implemented capture."}"#,
            now(),
        )
        .unwrap();
        let first =
            reduce_session_capture_candidates(&events, &close, &auth, "capture-v1").unwrap();
        let second =
            reduce_session_capture_candidates(&events, &close, &auth, "capture-v1").unwrap();
        assert_eq!(first, second);
        assert_eq!(
            reduce_session_capture(&events, &close, &auth).unwrap()[0].edited_paths,
            ["src/a.rs", "src/z.rs"]
        );
    }

    #[test]
    fn daemon_observed_branch_changes_create_distinct_segments() {
        let main = authority(0, Some("main"), "abc123");
        let feature = authority(1, Some("feature/d3"), "def456");
        let events = vec![
            event(
                main,
                r#"{"schema_version":1,"kind":"edited_path","path":"src/main.rs"}"#,
            ),
            event(
                feature.clone(),
                r#"{"schema_version":1,"kind":"edited_path","path":"src/feature.rs"}"#,
            ),
        ];
        let close = parse_session_capture_close(r#"{"schema_version":1}"#, now()).unwrap();
        let digests = reduce_session_capture(&events, &close, &feature).unwrap();
        assert_eq!(digests.len(), 2);
        assert_eq!(digests[0].branch.as_deref(), Some("main"));
        assert_eq!(digests[0].revision, "abc123");
        assert_eq!(digests[0].segment, 0);
        assert_eq!(digests[1].branch.as_deref(), Some("feature/d3"));
        assert_eq!(digests[1].revision, "def456");
        assert_eq!(digests[1].segment, 1);
    }

    #[test]
    fn resolution_requires_matching_fingerprint_in_same_segment() {
        let first = authority(0, Some("main"), "abc123");
        let second = authority(1, Some("feature/d3"), "def456");
        let resolution = format!(
            r#"{{"schema_version":1,"kind":"error","category":"compiler","fingerprint":"{FINGERPRINT}","status":"resolved","summary":"missing import"}}"#
        );
        let observed = format!(
            r#"{{"schema_version":1,"kind":"error","category":"compiler","fingerprint":"{FINGERPRINT}","status":"observed","summary":"missing import"}}"#
        );
        let close = parse_session_capture_close(
            r#"{"schema_version":1,"final_summary":"Everything passed and was resolved."}"#,
            now(),
        )
        .unwrap();

        let cross_segment = vec![event(first, &observed), event(second.clone(), &resolution)];
        let candidates =
            reduce_session_capture_candidates(&cross_segment, &close, &second, "capture-v1")
                .unwrap();
        assert!(candidates
            .iter()
            .all(|candidate| candidate.kind != SessionDigestCandidateKind::ResolvedFailure));

        let different_resolution = resolution.replace(FINGERPRINT, OTHER_FINGERPRINT);
        let mismatched = vec![
            event(second.clone(), &observed),
            event(second.clone(), &different_resolution),
        ];
        let candidates =
            reduce_session_capture_candidates(&mismatched, &close, &second, "capture-v1").unwrap();
        assert!(candidates
            .iter()
            .all(|candidate| candidate.kind != SessionDigestCandidateKind::ResolvedFailure));

        let reversed = vec![
            event(second.clone(), &resolution),
            event(second.clone(), &observed),
        ];
        let candidates =
            reduce_session_capture_candidates(&reversed, &close, &second, "capture-v1").unwrap();
        assert!(candidates
            .iter()
            .all(|candidate| candidate.kind != SessionDigestCandidateKind::ResolvedFailure));

        let same_segment = vec![
            event(second.clone(), &observed),
            event(second.clone(), &resolution),
            event(
                second.clone(),
                r#"{"schema_version":1,"kind":"check","label":"compiler regression","outcome":"passed"}"#,
            ),
        ];
        let candidates =
            reduce_session_capture_candidates(&same_segment, &close, &second, "capture-v1")
                .unwrap();
        assert_eq!(
            candidates
                .iter()
                .filter(|candidate| candidate.kind == SessionDigestCandidateKind::ResolvedFailure)
                .count(),
            1
        );
    }

    #[test]
    fn latest_safe_turn_summary_wins_independently_per_segment() {
        let first = authority(0, Some("main"), "abc123");
        let second = authority(1, Some("feature/d3"), "def456");
        let events = vec![
            event(
                first.clone(),
                r#"{"schema_version":1,"kind":"turn_summary","summary":"First draft."}"#,
            ),
            event(
                first.clone(),
                r#"{"schema_version":1,"kind":"check","label":"core tests","outcome":"passed"}"#,
            ),
            event(
                first,
                r#"{"schema_version":1,"kind":"turn_summary","summary":"First segment final."}"#,
            ),
            event(
                second.clone(),
                r#"{"schema_version":1,"kind":"turn_summary","summary":"Second draft."}"#,
            ),
            event(
                second.clone(),
                r#"{"schema_version":1,"kind":"check","label":"integration tests","outcome":"passed"}"#,
            ),
            event(
                second.clone(),
                r#"{"schema_version":1,"kind":"turn_summary","summary":"Second segment final."}"#,
            ),
        ];
        let close = parse_session_capture_close(r#"{"schema_version":1}"#, now()).unwrap();

        let digests = reduce_session_capture(&events, &close, &second).unwrap();
        assert_eq!(digests.len(), 2);
        assert_eq!(
            digests[0].final_summary.as_deref(),
            Some("First segment final.")
        );
        assert_eq!(
            digests[1].final_summary.as_deref(),
            Some("Second segment final.")
        );
    }

    #[test]
    fn turn_summary_alone_is_discarded_and_generates_no_candidate() {
        let auth = authority(0, Some("main"), "abc123");
        let events = vec![event(
            auth.clone(),
            r#"{"schema_version":1,"kind":"turn_summary","summary":"All checks passed and errors resolved."}"#,
        )];
        let close = parse_session_capture_close(r#"{"schema_version":1}"#, now()).unwrap();

        let digests = reduce_session_capture(&events, &close, &auth).unwrap();
        assert_eq!(digests.len(), 1);
        assert_eq!(digests[0].final_summary, None);
        assert!(extract_session_digest_candidates(&digests[0], "capture-v1").is_empty());
    }

    #[test]
    fn turn_summary_with_typed_observation_adds_narrative_but_no_failure_pattern() {
        let auth = authority(0, Some("main"), "abc123");
        let observed = format!(
            r#"{{"schema_version":1,"kind":"error","category":"compiler","fingerprint":"{FINGERPRINT}","status":"observed"}}"#
        );
        let events = vec![
            event(auth.clone(), &observed),
            event(
                auth.clone(),
                r#"{"schema_version":1,"kind":"turn_summary","summary":"Resolved the compiler failure and all checks passed."}"#,
            ),
        ];
        let close = parse_session_capture_close(r#"{"schema_version":1}"#, now()).unwrap();

        let candidates =
            reduce_session_capture_candidates(&events, &close, &auth, "capture-v1").unwrap();
        assert!(
            candidates.is_empty(),
            "an unresolved observation and prose remain episode evidence, not a lesson"
        );
        let digests = reduce_session_capture(&events, &close, &auth).unwrap();
        assert_eq!(
            digests[0].final_summary.as_deref(),
            Some("Resolved the compiler failure and all checks passed.")
        );
        assert!(candidates.iter().all(|candidate| {
            candidate.kind != SessionDigestCandidateKind::CheckOutcome
                && candidate.kind != SessionDigestCandidateKind::ResolvedFailure
                && candidate.memory_class != MemoryClass::FailurePattern
        }));
    }

    #[test]
    fn summary_alone_cannot_fabricate_checks_or_resolutions() {
        let auth = authority(0, Some("main"), "abc123");
        let close = parse_session_capture_close(
            r#"{"schema_version":1,"final_summary":"All checks passed and errors resolved."}"#,
            now(),
        )
        .unwrap();
        let candidates =
            reduce_session_capture_candidates(&[], &close, &auth, "capture-v1").unwrap();
        assert!(candidates.is_empty());
    }
}
