pub mod attribution;
pub mod conflict_query;
pub mod identity_migration;
pub mod model;
pub mod repository_owner;
pub mod retention;
pub mod retrieval;
pub mod router;
pub mod session_capture;
pub mod session_digest;
pub mod store;

#[cfg(test)]
mod attribution_tests;
#[cfg(test)]
mod conflict_query_tests;
#[cfg(test)]
mod retention_acceptance_tests;
#[cfg(test)]
mod tests;

pub use attribution::{
    AttributionAccessInput, AttributionDisposition, AttributionEventFact, AttributionEventKind,
    AttributionPruneOutcome, AttributionPrunePolicy, AttributionRecordOutcome,
    AttributionResolutionOutcome, AttributionResolutionStatus, AttributionRetrievalInput,
    PendingAttributionCursor, PendingAttributionMetric, PendingAttributionMetrics,
    ResolvedAttributionAccess, StoredAttributionRetrieval, MAX_ATTRIBUTION_ACCESSES,
    MAX_ATTRIBUTION_METRIC_BATCH, MAX_ATTRIBUTION_PRUNE_BATCH, MAX_ATTRIBUTION_RETAINED,
    MAX_ATTRIBUTION_STRING_BYTES,
};
pub use conflict_query::{
    query_conflicts, ConflictAnchorQuery, ConflictQueryPage, ConflictQueryRecord,
    MAX_CONFLICT_PAGE_SIZE,
};
pub use model::{
    BehavioralValidationRecord, BehavioralValidationStatus, EvidenceFreshnessStatus, EvidenceSpan,
    Memory, MemoryAccessRecord, MemoryClass, MemoryEvidence, MemoryLinkRecord, MemoryScope,
    MemoryScoreKind, MemoryScoreRecord, MemoryStructuredFields, MemoryType,
    MemoryVerificationStatus,
};
pub use repository_owner::RepositoryMemoryOwner;
pub use retention::{
    DeliveryBinding, ReclamationReport, RetentionHealth, RetentionPolicy, SweepReport,
};
pub use retrieval::RecallOptions;
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
pub use store::{MemoryStoreAvailability, MemoryStoreFailureKind};
