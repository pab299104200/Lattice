pub mod model;
pub mod router;
pub mod session_capture;
pub mod session_digest;
pub mod store;

#[cfg(test)]
mod tests;

pub use model::{
    EvidenceSpan, Memory, MemoryAccessRecord, MemoryClass, MemoryEvidence, MemoryLinkRecord,
    MemoryScope, MemoryScoreKind, MemoryScoreRecord, MemoryStructuredFields, MemoryType,
    MemoryVerificationStatus,
};
pub use router::{
    AuthorityQualifiedMemoryId, MemoryAuthority, MemoryQueryAuthority, MemoryRecallResult,
    MemoryRecallTier, MemoryStoreRole, MemoryStoreRouter, SessionDigestCaptureResult,
};
pub use session_capture::{
    parse_session_capture_close, parse_session_capture_event, reduce_session_capture,
    reduce_session_capture_candidates, session_capture_turn_summary_from_host,
    DaemonSessionCaptureEvent, SessionCaptureClose, SessionCaptureError, SessionCaptureEvent,
    SessionCaptureFact, MAX_SESSION_CAPTURE_CLOSE_BYTES, MAX_SESSION_CAPTURE_EVENT_BYTES,
    MAX_TURN_SUMMARY_BYTES, SESSION_CAPTURE_SCHEMA_VERSION,
};
pub use session_digest::{
    bind_session_digest_authority, extract_default_session_digest_candidates,
    extract_session_digest_candidates, parse_session_digest, CheckOutcome, ErrorStatus,
    NormalizedSessionDigest, SessionDigest, SessionDigestAuthority, SessionDigestCandidate,
    SessionDigestCandidateKind, SessionDigestCheckEvidence, SessionDigestContent,
    SessionDigestError, SessionDigestErrorEvidence, SessionDigestEvidence,
    SessionDigestObservation, MAX_BRANCH_BYTES, MAX_CHECK_LABEL_BYTES, MAX_EDITED_PATHS,
    MAX_EDITED_PATH_BYTES, MAX_ERROR_SUMMARY_BYTES, MAX_FINAL_SUMMARY_BYTES, MAX_OBSERVATIONS,
    MAX_OBSERVATION_BYTES, MAX_REVISION_BYTES, MAX_SESSION_DIGEST_BYTES, MAX_SESSION_ID_BYTES,
    SESSION_DIGEST_EXTRACTOR_VERSION, SESSION_DIGEST_SCHEMA_VERSION,
};
pub use store::MemoryStore;
