use super::{
    bundle_from_task, emit_standard_events, WorkflowBundle, WorkflowEventSink, WorkflowRequest,
};
use lattice_core::intelligence::TaskBundle;
use lattice_core::query::ContextCapsule;

/// Compose the redesigned `prepare_change` bundle from ranked graph context,
/// memory evidence, event episodes, working-memory metadata, and verification
/// commands.
pub fn run(
    workspace_id: &str,
    request: &WorkflowRequest,
    task: &TaskBundle,
    capsule: &ContextCapsule,
    sink: &mut dyn WorkflowEventSink,
) -> WorkflowBundle {
    emit_standard_events(sink, "prepare_change");
    bundle_from_task(workspace_id, request, task, capsule)
}
