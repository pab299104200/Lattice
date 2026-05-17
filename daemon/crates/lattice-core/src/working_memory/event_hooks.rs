//! Event hooks for working-memory state mutations.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 3. Event Log` requires `MemoryRetrieved` and `MemoryExpanded` events,
//! and `## Phase 5: Working Memory` requires included and excluded context to
//! be observable without feeding the full event stream back into prompts.

use rusqlite::ErrorCode;

use crate::events::{
    Actor, BranchRef, CompactSummary, EventKind, EventPayload, EventWriteError, EventWriter,
    ExcludedContextDelta, FlushPolicy, IncludedContextDelta, MemoryExpandedPayload,
    MemoryRetrievedPayload, PartialEnvelope, SessionId, StableRef, TaskId,
};
use crate::identity::{EventId, Identity, MemoryId, WorkspaceId};

use super::state::WorkingMemoryState;

const HOT_PATH_FLUSH: FlushPolicy = FlushPolicy::Batched { interval_ms: 250 };

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationEventSummary {
    pub op_name: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingMemoryEventContext {
    pub workspace_id: WorkspaceId,
    pub branch: BranchRef,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub actor: Actor,
}

pub trait WorkingMemoryEventAppender {
    fn append(
        &self,
        envelope: PartialEnvelope,
        flush_policy: FlushPolicy,
    ) -> Result<EventId, EventWriteError>;
}

impl WorkingMemoryEventAppender for EventWriter {
    fn append(
        &self,
        envelope: PartialEnvelope,
        flush_policy: FlushPolicy,
    ) -> Result<EventId, EventWriteError> {
        self.append_with_flush_policy(envelope, flush_policy)
    }
}

pub fn emit_memory_retrieved(
    state_before: &WorkingMemoryState,
    state_after: &WorkingMemoryState,
    op_summary: &MutationEventSummary,
    ctx: &WorkingMemoryEventContext,
    writer: &dyn WorkingMemoryEventAppender,
) -> Result<Option<EventId>, EventWriteError> {
    let included = diff_selected_memories(state_before, state_after);
    let excluded = diff_excluded_memories(state_before, state_after);
    if included.is_empty() && excluded.is_empty() {
        return Ok(None);
    }
    let payload = EventPayload::MemoryRetrieved(MemoryRetrievedPayload {
        retrieval_query: op_summary.summary.clone(),
        context_handle_id: None,
        memory_ids: collect_memory_ids(&included),
        supporting_event_ids: Vec::new(),
        included_context: included.clone(),
        excluded_context: excluded.clone(),
    });
    let references = build_references(&included, &excluded);
    let summary = compact_summary(
        "Working memory retrieved",
        &op_summary.op_name,
        included.len(),
        excluded.len(),
    )?;
    writer
        .append(
            PartialEnvelope {
                workspace_id: Some(ctx.workspace_id.clone()),
                branch: ctx.branch.clone(),
                session_id: ctx.session_id.clone(),
                task_id: Some(ctx.task_id.clone()),
                actor: ctx.actor.clone(),
                kind: EventKind::MemoryRetrieved,
                references,
                summary,
                payload,
            },
            HOT_PATH_FLUSH,
        )
        .map(Some)
}

pub fn emit_memory_expanded(
    state_before: &WorkingMemoryState,
    state_after: &WorkingMemoryState,
    op_summary: &MutationEventSummary,
    ctx: &WorkingMemoryEventContext,
    writer: &dyn WorkingMemoryEventAppender,
) -> Result<Option<EventId>, EventWriteError> {
    let included = diff_selected_memories(state_before, state_after);
    let excluded = diff_excluded_memories(state_before, state_after);
    if included.is_empty() && excluded.is_empty() {
        return Ok(None);
    }
    let memory_id = included
        .iter()
        .find_map(|item| memory_id_from_identity(&item.identity))
        .or_else(|| first_memory_id(state_after));
    let payload = EventPayload::MemoryExpanded(MemoryExpandedPayload {
        memory_id,
        source_event_id: None,
        linked_memory_ids: collect_memory_ids(&included),
        linked_symbol_ids: Vec::new(),
        linked_doc_section_ids: Vec::new(),
        expansion_identity: included
            .first()
            .map(|item| item.identity.clone())
            .or_else(|| first_selected_identity(state_after)),
        included_context: included.clone(),
        excluded_context: excluded.clone(),
    });
    let references = build_references(&included, &excluded);
    let summary = compact_summary(
        "Working memory expanded",
        &op_summary.op_name,
        included.len(),
        excluded.len(),
    )?;
    writer
        .append(
            PartialEnvelope {
                workspace_id: Some(ctx.workspace_id.clone()),
                branch: ctx.branch.clone(),
                session_id: ctx.session_id.clone(),
                task_id: Some(ctx.task_id.clone()),
                actor: ctx.actor.clone(),
                kind: EventKind::MemoryExpanded,
                references,
                summary,
                payload,
            },
            HOT_PATH_FLUSH,
        )
        .map(Some)
}

pub fn is_backpressure_error(error: &EventWriteError) -> bool {
    match error {
        EventWriteError::Storage(crate::events::EventStoreError::Sqlite(
            rusqlite::Error::SqliteFailure(sqlite_error, _),
        )) => matches!(
            sqlite_error.code,
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
        ),
        EventWriteError::Storage(crate::events::EventStoreError::Sqlite(error)) => {
            let text = error.to_string();
            text.contains("database is locked") || text.contains("database is busy")
        }
        _ => false,
    }
}

fn diff_selected_memories(
    state_before: &WorkingMemoryState,
    state_after: &WorkingMemoryState,
) -> Vec<IncludedContextDelta> {
    state_after
        .selected_memories
        .iter()
        .filter(|candidate| {
            !state_before
                .selected_memories
                .iter()
                .any(|previous| previous.identity == candidate.identity)
        })
        .map(|result| IncludedContextDelta {
            identity: result.identity.clone(),
            headline: result.headline.clone(),
            inclusion_reason: result.inclusion_reason.clone(),
        })
        .collect()
}

fn diff_excluded_memories(
    state_before: &WorkingMemoryState,
    state_after: &WorkingMemoryState,
) -> Vec<ExcludedContextDelta> {
    state_after
        .excluded_memories
        .iter()
        .filter(|candidate| {
            !state_before.excluded_memories.iter().any(|previous| {
                previous.result.identity == candidate.result.identity
                    && previous.exclusion_reason == candidate.exclusion_reason
            })
        })
        .map(|excluded| ExcludedContextDelta {
            identity: excluded.result.identity.clone(),
            headline: excluded.result.headline.clone(),
            exclusion_reason: excluded.exclusion_reason.clone(),
        })
        .collect()
}

fn collect_memory_ids(included: &[IncludedContextDelta]) -> Vec<MemoryId> {
    included
        .iter()
        .filter_map(|item| memory_id_from_identity(&item.identity))
        .collect()
}

fn build_references(
    included: &[IncludedContextDelta],
    excluded: &[ExcludedContextDelta],
) -> Vec<StableRef> {
    let mut references = Vec::with_capacity(included.len() + excluded.len());
    for item in included {
        if let Some(reference) = stable_ref_from_identity(&item.identity) {
            references.push(reference);
        }
    }
    for item in excluded {
        if let Some(reference) = stable_ref_from_identity(&item.identity) {
            references.push(reference);
        }
    }
    references
}

fn stable_ref_from_identity(identity: &Identity) -> Option<StableRef> {
    Some(match identity {
        Identity::File(file) => StableRef::FileRef(file.clone()),
        Identity::Symbol(symbol) => StableRef::SymbolRef(symbol.clone()),
        Identity::Doc(_) => return None,
        Identity::Section(section) => StableRef::DocSectionRef(section.clone()),
        Identity::Event(event) => StableRef::EventRef(event.clone()),
        Identity::Memory(memory) => StableRef::MemoryRef(memory.clone()),
        Identity::ContextHandle(handle) => StableRef::ContextHandleRef(handle.clone()),
    })
}

fn memory_id_from_identity(identity: &Identity) -> Option<MemoryId> {
    match identity {
        Identity::Memory(memory) => Some(memory.clone()),
        _ => None,
    }
}

fn first_memory_id(state: &WorkingMemoryState) -> Option<MemoryId> {
    state
        .selected_memories
        .iter()
        .find_map(|result| memory_id_from_identity(&result.identity))
}

fn first_selected_identity(state: &WorkingMemoryState) -> Option<Identity> {
    state
        .selected_memories
        .first()
        .map(|result| result.identity.clone())
}

fn compact_summary(
    prefix: &str,
    op_name: &str,
    included: usize,
    excluded: usize,
) -> Result<CompactSummary, EventWriteError> {
    CompactSummary::new(format!(
        "{prefix}: op={op_name} included={included} excluded={excluded}"
    ))
    .map_err(|error| {
        crate::events::EventStoreError::EnvelopeInvalid {
            reason: error.to_string(),
        }
        .into()
    })
}
