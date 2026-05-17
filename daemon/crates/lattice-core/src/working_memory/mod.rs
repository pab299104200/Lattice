//! Explicit per-task working memory for the cognitive workspace.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 5: Working Memory` requires state, operations, event hooks, and
//! checkpoint restore. This module owns the state and storage substrate first;
//! operation and event-hook behavior is layered on top by the follow-up tasks.

pub mod event_hooks;
pub mod operations;
pub mod state;

#[cfg(test)]
mod budget_tests;
#[cfg(test)]
mod checkpoint_tests;
#[cfg(test)]
mod operations_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
mod tests_common;

pub use crate::events::{ExcludedContextDelta, IncludedContextDelta};
pub use event_hooks::{
    emit_memory_expanded, emit_memory_retrieved, MutationEventSummary, WorkingMemoryEventAppender,
    WorkingMemoryEventContext,
};
pub use operations::{
    checkpoint, compress, evict, expand, filter, pin, retrieve, summarize, summarize_state,
    CheckpointArgs, CheckpointOutcome, CompressArgs, CompressOutcome, EvictArgs, EvictOutcome,
    ExpandArgs, ExpandOutcome, FilterPredicate, OpContext, PhaseFourResources, PhaseFourRetriever,
    PinArgs, PinOutcome, RetrieveArgs, RetrieveOutcome, StateMutationObserver, StateMutationRecord,
    SummarizeArgs, WorkingMemoryOp, WorkingMemoryRetriever, WorkingMemorySummary,
};
pub use state::{
    initialize_schema, load_checkpoint, load_latest_checkpoint_for_scope, save_checkpoint,
    save_checkpoint_for_scope, BudgetDecisions, CheckpointId, CheckpointScope, ExcludedMemory,
    FailureRecord, FileIdentity, Hypothesis, PlanRef, SymbolIdentity, WorkingMemoryState,
    WorkingMemoryVerification, WORKING_MEMORY_STATE_VERSION,
};
