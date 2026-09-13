//! Typed event log records for the cognitive workspace fork plan.
//!
//! This module implements the event model described in
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `### 3. Event Log`.

pub mod compaction;
pub mod envelope;
pub mod hashing;
pub mod kinds;
pub mod migrations;
pub mod query;
pub mod reader;
pub mod snapshot;
pub mod store;
pub mod writer;

#[cfg(test)]
mod budget_tests;
#[cfg(test)]
mod corruption_tests;
#[cfg(test)]
mod reader_tests;
#[cfg(test)]
mod replay_tests;
#[cfg(test)]
mod snapshot_tests;
#[cfg(test)]
mod store_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod writer_tests;

pub type DocSectionId = crate::identity::SectionId;

pub use compaction::{
    expire_managed_snapshots, expire_managed_snapshots_dir, expire_managed_snapshots_dir_page,
    Bootstrap, BootstrapError, BootstrappedState, CompactionConfig, CompactionError,
    CompactionReport, Compactor, GraphHandle, SchedulerHandle, SnapshotExpiryCursor,
    SnapshotExpiryReport,
};
pub use envelope::{
    Actor, BranchRef, CompactSummary, EventEnvelope, EventModelError, PayloadLocation, SessionId,
    StableRef, TaskId,
};
pub use hashing::{
    canonical_json_bytes, canonicalize_json_value, hash_canonical_payload_bytes, hash_payload,
    PayloadHash, HASH_ALGORITHM,
};
pub use kinds::{
    AssistantTaskStartedPayload, ConsolidationFailedPayload, ContextBundleReturnedPayload,
    DiagnosticObservedPayload, DiagnosticSeverity, EventKind, EventPayload, ExcludedContextDelta,
    FileReadPayload, IncludedContextDelta, MemoryConsolidatedPayload, MemoryCreatedPayload,
    MemoryExpandedPayload, MemoryInvalidatedPayload, MemoryRetrievedPayload,
    MemoryScopeFilteredPayload, MemoryUpdatedPayload, PatchAppliedPayload, PlanCreatedPayload,
    TestRunCompletedPayload, TestRunStartedPayload, TestRunStatus, ToolCalledPayload,
    ToolResultPayload, ToolResultStatus, UserCorrectionPayload, UserPreferenceObservedPayload,
    WorkflowFailedPayload, WorkflowSucceededPayload,
};
pub use query::{EventQuery, EventQueryError, QueryOrder, SessionScope, TaskScope};
pub use reader::{EventPage, EventReader};
pub use snapshot::{GraphSnapshot, MemorySnapshot, Snapshot, SnapshotError, SnapshotHandle};
pub use store::{
    EventCursor, EventEnvelopeRow, EventPayloadRow, EventStore, EventStoreError, InsertEnvelopeRow,
};
pub use writer::{EventWriteError, EventWriter, FlushPolicy, IdentityError, PartialEnvelope};
