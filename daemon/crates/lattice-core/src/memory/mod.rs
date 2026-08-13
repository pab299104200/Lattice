pub mod model;
pub mod router;
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
    MemoryRecallTier, MemoryStoreRole, MemoryStoreRouter,
};
pub use session_digest::{
    extract_default_session_digest_candidates, extract_session_digest_candidates,
    parse_session_digest, CheckOutcome, ErrorStatus, NormalizedSessionDigest, SessionDigest,
    SessionDigestCandidate, SessionDigestCandidateKind, SessionDigestCheckEvidence,
    SessionDigestError, SessionDigestErrorEvidence, SessionDigestEvidence,
    SessionDigestObservation, MAX_CHECK_LABEL_BYTES, MAX_EDITED_PATHS, MAX_EDITED_PATH_BYTES,
    MAX_ERROR_SUMMARY_BYTES, MAX_FINAL_SUMMARY_BYTES, MAX_OBSERVATIONS, MAX_OBSERVATION_BYTES,
    MAX_SESSION_DIGEST_BYTES, MAX_SESSION_ID_BYTES, SESSION_DIGEST_EXTRACTOR_VERSION,
    SESSION_DIGEST_SCHEMA_VERSION,
};
pub use store::MemoryStore;
