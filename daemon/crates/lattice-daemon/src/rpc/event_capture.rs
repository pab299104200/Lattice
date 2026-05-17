use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use lattice_core::events::{
    Actor, AssistantTaskStartedPayload, BranchRef, ContextBundleReturnedPayload,
    DiagnosticObservedPayload, EventPayload, EventWriteError, EventWriter, FileReadPayload,
    FlushPolicy, MemoryConsolidatedPayload, MemoryCreatedPayload, MemoryExpandedPayload,
    MemoryInvalidatedPayload, MemoryRetrievedPayload, MemoryUpdatedPayload, PartialEnvelope,
    PatchAppliedPayload, PlanCreatedPayload, SessionId, StableRef, TaskId, TestRunCompletedPayload,
    TestRunStartedPayload, ToolCalledPayload, ToolResultPayload, UserCorrectionPayload,
    UserPreferenceObservedPayload, WorkflowFailedPayload, WorkflowSucceededPayload,
};
use lattice_core::identity::{ContextHandleId, EventId, FileId, MemoryId};
use serde_json::Value;

use super::event_capture_support::{
    call_id_from_parent, compact_summary, compact_text, context_handle_id_from_value,
    created_memory_ids, doc_section_refs, file_refs, lock_error, memory_ids_from_value,
    memory_refs, outcome_summary, output_context_handle_id, parse_tool_payload, status_label,
    symbol_refs, tool_result_status, workflow_summary,
};

pub use super::event_capture_support::{
    DiagnosticRecord, EventCaptureError, PatchId, PlanId, PreferenceScope, PreferenceValue,
    TestRunPair, ToolOutcome,
};

const HOT_PATH_FLUSH: FlushPolicy = FlushPolicy::Batched { interval_ms: 250 };
const TERMINAL_FLUSH: FlushPolicy = FlushPolicy::Sync;

pub struct EventCapture {
    writer: Arc<EventWriter>,
    workspace_id: String,
    branch: BranchRef,
    session_id: SessionId,
    current_task: Mutex<Option<TaskId>>,
    active_calls: Mutex<HashMap<String, String>>,
    call_sequence: AtomicU64,
}

impl std::fmt::Debug for EventCapture {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EventCapture")
            .field("workspace_id", &self.workspace_id)
            .field("branch", &self.branch)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl EventCapture {
    pub fn new(
        writer: Arc<EventWriter>,
        workspace_id: String,
        branch: String,
        session_id: SessionId,
    ) -> Result<Self, EventCaptureError> {
        if writer.workspace_id() != &workspace_id {
            return Err(EventCaptureError::WorkspaceMismatch {
                expected: writer.workspace_id().clone(),
                actual: workspace_id,
            });
        }

        Ok(Self {
            writer,
            workspace_id,
            branch: BranchRef { name: branch },
            session_id,
            current_task: Mutex::new(None),
            active_calls: Mutex::new(HashMap::new()),
            call_sequence: AtomicU64::new(0),
        })
    }

    pub fn begin_task(&self, task_id: TaskId, statement: &str) -> Result<EventId, EventWriteError> {
        {
            let mut current = self.current_task.lock().map_err(lock_error)?;
            *current = Some(task_id);
        }
        let payload = EventPayload::AssistantTaskStarted(AssistantTaskStartedPayload {
            context_handle_id: None,
            seed_event_ids: Vec::new(),
            initial_memory_ids: Vec::new(),
            objective: compact_text(statement, 240),
        });
        self.append(
            Actor::Assistant {
                model: "mcp-client".to_string(),
            },
            payload,
            Vec::new(),
            format!("Assistant task started: {}", compact_text(statement, 160)),
            HOT_PATH_FLUSH,
        )
    }

    pub fn ensure_task_started(&self, statement: &str) -> Result<Option<EventId>, EventWriteError> {
        let task_id = TaskId {
            value: format!("{}:default", self.session_id.value),
        };
        if self.current_task.lock().map_err(lock_error)?.is_some() {
            return Ok(None);
        }
        self.begin_task(task_id, statement).map(Some)
    }

    pub fn record_tool_called(
        &self,
        tool: &str,
        inputs: &Value,
    ) -> Result<EventId, EventWriteError> {
        let call_id = self.next_call_id(tool);
        let payload = EventPayload::ToolCalled(ToolCalledPayload {
            call_id: call_id.clone(),
            tool_name: tool.to_string(),
            context_handle_id: context_handle_id_from_value(inputs),
            source_event_id: None,
            input_summary: compact_text(&inputs.to_string(), 300),
        });
        let event_id = self.append(
            Actor::Tool {
                name: tool.to_string(),
            },
            payload,
            Vec::new(),
            format!("Tool called: {tool}"),
            HOT_PATH_FLUSH,
        )?;
        self.active_calls
            .lock()
            .map_err(lock_error)?
            .insert(event_id.ulid.clone(), call_id);
        Ok(event_id)
    }

    pub fn record_tool_result(
        &self,
        tool: &str,
        result: &ToolOutcome,
        parent: EventId,
    ) -> Result<EventId, EventWriteError> {
        let payload = EventPayload::ToolResult(ToolResultPayload {
            call_id: self.call_id_for_parent(tool, &parent)?,
            tool_name: tool.to_string(),
            status: tool_result_status(result),
            tool_call_event_id: Some(parent.clone()),
            output_context_handle_id: output_context_handle_id(result),
            created_memory_ids: created_memory_ids(result, &self.workspace_id),
            output_summary: outcome_summary(result),
        });
        self.append(
            Actor::Tool {
                name: tool.to_string(),
            },
            payload,
            vec![StableRef::EventRef(parent)],
            format!("Tool result: {tool} {}", status_label(result)),
            HOT_PATH_FLUSH,
        )
    }

    pub fn record_workflow_events(
        &self,
        tool: &str,
        outcome: &ToolOutcome,
        terminal_event: EventId,
    ) -> Result<Vec<EventId>, EventWriteError> {
        match outcome {
            ToolOutcome::Success(value) => self.record_success_events(tool, value, terminal_event),
            ToolOutcome::Error { message, .. } => {
                let event =
                    self.record_workflow_outcome(false, tool, message, &[terminal_event])?;
                Ok(vec![event])
            }
        }
    }

    pub fn record_context_bundle(
        &self,
        handle: ContextHandleId,
        anchors: &[StableRef],
        _inclusion_reasons: &[String],
    ) -> Result<EventId, EventWriteError> {
        let payload = EventPayload::ContextBundleReturned(ContextBundleReturnedPayload {
            context_handle_id: handle.clone(),
            source_event_id: None,
            file_ids: file_refs(anchors),
            symbol_ids: symbol_refs(anchors),
            doc_section_ids: doc_section_refs(anchors),
            memory_ids: memory_refs(anchors),
            token_estimate: 0,
        });
        self.append(
            Actor::Daemon,
            payload,
            vec![StableRef::ContextHandleRef(handle)],
            "Context bundle returned".to_string(),
            HOT_PATH_FLUSH,
        )
    }

    pub fn record_memory_retrieved(
        &self,
        memory_ids: &[MemoryId],
        reasons: &[String],
    ) -> Result<EventId, EventWriteError> {
        let payload = EventPayload::MemoryRetrieved(MemoryRetrievedPayload {
            retrieval_query: compact_text(&reasons.join("; "), 240),
            context_handle_id: None,
            memory_ids: memory_ids.to_vec(),
            supporting_event_ids: Vec::new(),
            included_context: Vec::new(),
            excluded_context: Vec::new(),
        });
        self.append(
            Actor::Daemon,
            payload,
            memory_ids
                .iter()
                .cloned()
                .map(StableRef::MemoryRef)
                .collect(),
            format!("Retrieved {} memories", memory_ids.len()),
            HOT_PATH_FLUSH,
        )
    }

    // T15 defines helper contracts before all future call sites exist.
    #[allow(dead_code)]
    pub fn record_memory_expanded(
        &self,
        memory_id: MemoryId,
        _depth: u8,
    ) -> Result<EventId, EventWriteError> {
        let payload = EventPayload::MemoryExpanded(MemoryExpandedPayload {
            memory_id: Some(memory_id.clone()),
            source_event_id: None,
            linked_memory_ids: Vec::new(),
            linked_symbol_ids: Vec::new(),
            linked_doc_section_ids: Vec::new(),
            expansion_identity: None,
            included_context: Vec::new(),
            excluded_context: Vec::new(),
        });
        self.append(
            Actor::Daemon,
            payload,
            vec![StableRef::MemoryRef(memory_id)],
            "Memory expanded".to_string(),
            HOT_PATH_FLUSH,
        )
    }

    pub fn record_plan_created(
        &self,
        plan_id: PlanId,
        target_files: &[FileId],
    ) -> Result<EventId, EventWriteError> {
        let payload = EventPayload::PlanCreated(PlanCreatedPayload {
            context_handle_id: None,
            source_event_id: None,
            memory_ids: Vec::new(),
            step_count: 0,
            plan_summary: plan_id,
        });
        self.append(
            Actor::Daemon,
            payload,
            target_files
                .iter()
                .cloned()
                .map(StableRef::FileRef)
                .collect(),
            "Plan created".to_string(),
            HOT_PATH_FLUSH,
        )
    }

    // T15 defines helper contracts before all future call sites exist.
    #[allow(dead_code)]
    pub fn record_file_read(
        &self,
        file_id: FileId,
        byte_range: Option<(u64, u64)>,
    ) -> Result<EventId, EventWriteError> {
        let (byte_start, byte_end) = byte_range
            .map(|(start, end)| (u32::try_from(start).ok(), u32::try_from(end).ok()))
            .unwrap_or((None, None));
        let payload = EventPayload::FileRead(FileReadPayload {
            file_id: file_id.clone(),
            source_event_id: None,
            byte_start,
            byte_end,
            reason: "MCP workflow context read".to_string(),
        });
        self.append(
            Actor::Daemon,
            payload,
            vec![StableRef::FileRef(file_id)],
            "File read".to_string(),
            HOT_PATH_FLUSH,
        )
    }

    // T15 defines helper contracts before all future call sites exist.
    #[allow(dead_code)]
    pub fn record_patch_applied(
        &self,
        patch_id: PatchId,
        files: &[FileId],
    ) -> Result<EventId, EventWriteError> {
        let payload = EventPayload::PatchApplied(PatchAppliedPayload {
            patch_id,
            source_event_id: None,
            file_ids: files.to_vec(),
            symbol_ids: Vec::new(),
            lines_added: 0,
            lines_removed: 0,
        });
        self.append(
            Actor::Daemon,
            payload,
            files.iter().cloned().map(StableRef::FileRef).collect(),
            "Patch applied".to_string(),
            TERMINAL_FLUSH,
        )
    }

    // T15 defines helper contracts before all future call sites exist.
    #[allow(dead_code)]
    pub fn record_test_run(
        &self,
        events: TestRunPair,
    ) -> Result<(EventId, EventId), EventWriteError> {
        let started = self.append(
            Actor::Daemon,
            EventPayload::TestRunStarted(TestRunStartedPayload {
                run_id: events.run_id.clone(),
                source_event_id: None,
                file_ids: events.file_ids.clone(),
                symbol_ids: events.symbol_ids,
                command: events.command,
            }),
            events
                .file_ids
                .iter()
                .cloned()
                .map(StableRef::FileRef)
                .collect(),
            "Test run started".to_string(),
            HOT_PATH_FLUSH,
        )?;
        let completed = self.append(
            Actor::Daemon,
            EventPayload::TestRunCompleted(TestRunCompletedPayload {
                run_id: events.run_id,
                started_event_id: Some(started.clone()),
                status: events.status,
                passed: events.passed,
                failed: events.failed,
                skipped: events.skipped,
                diagnostic_event_ids: Vec::new(),
            }),
            vec![StableRef::EventRef(started.clone())],
            "Test run completed".to_string(),
            TERMINAL_FLUSH,
        )?;
        Ok((started, completed))
    }

    // T15 defines helper contracts before all future call sites exist.
    #[allow(dead_code)]
    pub fn record_diagnostic(&self, diag: DiagnosticRecord) -> Result<EventId, EventWriteError> {
        let payload = EventPayload::DiagnosticObserved(DiagnosticObservedPayload {
            diagnostic_id: diag.diagnostic_id,
            source_event_id: None,
            file_id: diag.file_id.clone(),
            symbol_id: diag.symbol_id,
            severity: diag.severity,
            message: diag.message,
        });
        self.append(
            Actor::Daemon,
            payload,
            vec![StableRef::FileRef(diag.file_id)],
            "Diagnostic observed".to_string(),
            HOT_PATH_FLUSH,
        )
    }

    // T15 defines helper contracts before all future call sites exist.
    #[allow(dead_code)]
    pub fn record_user_correction(
        &self,
        prior_event: EventId,
        correction: &str,
    ) -> Result<EventId, EventWriteError> {
        let payload = EventPayload::UserCorrection(UserCorrectionPayload {
            corrected_event_id: prior_event.clone(),
            file_ids: Vec::new(),
            superseded_memory_ids: Vec::new(),
            correction_summary: compact_text(correction, 240),
        });
        self.append(
            Actor::User,
            payload,
            vec![StableRef::EventRef(prior_event)],
            "User correction observed".to_string(),
            TERMINAL_FLUSH,
        )
    }

    // T15 defines helper contracts before all future call sites exist.
    #[allow(dead_code)]
    pub fn record_user_preference(
        &self,
        scope: PreferenceScope,
        value: PreferenceValue,
    ) -> Result<EventId, EventWriteError> {
        let payload = EventPayload::UserPreferenceObserved(UserPreferenceObservedPayload {
            preference_key: format!("{:?}:{}", scope, value.key),
            preference_value: value.value,
            observed_from_event_id: None,
            memory_id: None,
        });
        self.append(
            Actor::Daemon,
            payload,
            Vec::new(),
            "User preference observed".to_string(),
            TERMINAL_FLUSH,
        )
    }

    pub fn record_workflow_outcome(
        &self,
        success: bool,
        workflow_name: &str,
        summary: &str,
        evidence: &[EventId],
    ) -> Result<EventId, EventWriteError> {
        self.record_workflow_outcome_detailed(
            success,
            workflow_name,
            summary,
            evidence,
            &[],
            None,
            &[],
            false,
        )
    }

    pub fn record_workflow_outcome_detailed(
        &self,
        success: bool,
        workflow_name: &str,
        summary: &str,
        evidence: &[EventId],
        additional_references: &[StableRef],
        output_context_handle_id: Option<ContextHandleId>,
        memory_ids: &[MemoryId],
        retryable: bool,
    ) -> Result<EventId, EventWriteError> {
        let mut references = Vec::new();
        let mut seen = HashSet::new();
        for reference in evidence
            .iter()
            .cloned()
            .map(StableRef::EventRef)
            .chain(additional_references.iter().cloned())
        {
            let key = format!("{reference:?}");
            if seen.insert(key) {
                references.push(reference);
            }
        }
        let payload = if success {
            EventPayload::WorkflowSucceeded(WorkflowSucceededPayload {
                workflow_name: workflow_name.to_string(),
                terminal_event_id: evidence.last().cloned(),
                output_context_handle_id,
                memory_ids: memory_ids.to_vec(),
                result_summary: compact_text(summary, 1600),
            })
        } else {
            EventPayload::WorkflowFailed(WorkflowFailedPayload {
                workflow_name: workflow_name.to_string(),
                terminal_event_id: evidence.last().cloned(),
                diagnostic_event_ids: Vec::new(),
                retryable,
                failure_summary: compact_text(summary, 1600),
            })
        };
        self.append(
            Actor::Daemon,
            payload,
            references,
            workflow_summary(success, workflow_name),
            HOT_PATH_FLUSH,
        )
    }

    // T15 defines helper contracts before all future call sites exist.
    #[allow(dead_code)]
    pub fn record_memory_created(
        &self,
        memory_id: MemoryId,
        evidence: &[EventId],
    ) -> Result<EventId, EventWriteError> {
        let payload = EventPayload::MemoryCreated(MemoryCreatedPayload {
            memory_id: memory_id.clone(),
            class: "observation".to_string(),
            stream: "semantic_repo_claims".to_string(),
            scope: "repo".to_string(),
            idempotency_key: format!("event-capture:{}", memory_id.ulid),
            source_event_id: evidence.last().cloned(),
            evidence_event_ids: evidence.to_vec(),
            symbol_ids: Vec::new(),
            doc_section_ids: Vec::new(),
            replay_snapshot_json: None,
        });
        self.append(
            Actor::Daemon,
            payload,
            vec![StableRef::MemoryRef(memory_id)],
            "Memory created".to_string(),
            TERMINAL_FLUSH,
        )
    }

    // T15 defines helper contracts before all future call sites exist.
    #[allow(dead_code)]
    pub fn record_memory_updated(
        &self,
        memory_id: MemoryId,
        summary: &str,
    ) -> Result<EventId, EventWriteError> {
        let payload = EventPayload::MemoryUpdated(MemoryUpdatedPayload {
            memory_id: memory_id.clone(),
            previous_event_id: None,
            evidence_event_ids: Vec::new(),
            superseded_memory_id: None,
            update_summary: compact_text(summary, 240),
            replay_snapshot_json: None,
        });
        self.append(
            Actor::Daemon,
            payload,
            vec![StableRef::MemoryRef(memory_id)],
            "Memory updated".to_string(),
            TERMINAL_FLUSH,
        )
    }

    // T15 defines helper contracts before all future call sites exist.
    #[allow(dead_code)]
    pub fn record_memory_invalidated(
        &self,
        memory_id: MemoryId,
        reason: &str,
    ) -> Result<EventId, EventWriteError> {
        let payload = EventPayload::MemoryInvalidated(MemoryInvalidatedPayload {
            memory_id: memory_id.clone(),
            invalidated_by_event_id: None,
            contradicting_memory_ids: Vec::new(),
            reason: compact_text(reason, 240),
            replay_snapshot_json: None,
        });
        self.append(
            Actor::Daemon,
            payload,
            vec![StableRef::MemoryRef(memory_id)],
            "Memory invalidated".to_string(),
            TERMINAL_FLUSH,
        )
    }

    // T15 defines helper contracts before all future call sites exist.
    #[allow(dead_code)]
    pub fn record_memory_consolidated(
        &self,
        source_memory_ids: &[MemoryId],
        consolidated_memory_id: MemoryId,
        source_event_ids: &[EventId],
        summary: &str,
    ) -> Result<EventId, EventWriteError> {
        let payload = EventPayload::MemoryConsolidated(MemoryConsolidatedPayload {
            source_memory_ids: source_memory_ids.to_vec(),
            consolidated_memory_id: consolidated_memory_id.clone(),
            source_event_ids: source_event_ids.to_vec(),
            consolidation_summary: compact_text(summary, 240),
            proposal_id: None,
            prior_state_json: None,
            proposed_state_json: None,
            post_apply_state_hash: [0; 32],
            decided_by: None,
            decision_reason: None,
        });
        self.append(
            Actor::Daemon,
            payload,
            vec![StableRef::MemoryRef(consolidated_memory_id)],
            "Memory consolidated".to_string(),
            TERMINAL_FLUSH,
        )
    }

    pub fn record_memory_consolidation_proposed(
        &self,
        source_memory_ids: &[MemoryId],
        consolidated_memory_id: MemoryId,
        source_event_ids: &[EventId],
        summary: &str,
        proposal_id: &str,
        prior_state_json: Option<String>,
        proposed_state_json: Option<String>,
    ) -> Result<EventId, EventWriteError> {
        let payload = EventPayload::MemoryConsolidated(MemoryConsolidatedPayload {
            source_memory_ids: source_memory_ids.to_vec(),
            consolidated_memory_id: consolidated_memory_id.clone(),
            source_event_ids: source_event_ids.to_vec(),
            consolidation_summary: compact_text(summary, 240),
            proposal_id: Some(proposal_id.to_string()),
            prior_state_json,
            proposed_state_json,
            post_apply_state_hash: [0; 32],
            decided_by: None,
            decision_reason: Some("proposal_only".to_string()),
        });
        self.append(
            Actor::Daemon,
            payload,
            vec![StableRef::MemoryRef(consolidated_memory_id)],
            "Memory consolidation proposed".to_string(),
            TERMINAL_FLUSH,
        )
    }

    fn record_success_events(
        &self,
        tool: &str,
        value: &Value,
        terminal_event: EventId,
    ) -> Result<Vec<EventId>, EventWriteError> {
        let mut events = Vec::new();
        let parsed = parse_tool_payload(value);
        if let Some(handle) = parsed.as_ref().and_then(context_handle_id_from_value) {
            events.push(self.record_context_bundle(handle, &[], &[])?);
        }
        if let Some(memory_ids) = parsed
            .as_ref()
            .map(|item| memory_ids_from_value(item, &self.workspace_id))
        {
            if !memory_ids.is_empty() {
                events.push(self.record_memory_retrieved(&memory_ids, &[tool.to_string()])?);
            }
        }
        if matches!(
            tool,
            "prepare_change" | "plan_edit" | "trace_scenario" | "diagnose_failure"
        ) {
            events.push(self.record_plan_created(tool.to_string(), &[])?);
        }
        events.push(self.record_workflow_outcome(
            true,
            tool,
            &outcome_summary(&ToolOutcome::Success(value.clone())),
            &[terminal_event],
        )?);
        Ok(events)
    }

    pub fn writer(&self) -> Arc<EventWriter> {
        self.writer.clone()
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn current_task_id(&self) -> Option<TaskId> {
        self.current_task
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn next_call_id(&self, tool: &str) -> String {
        let sequence = self.call_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{}:{}:{sequence}", self.session_id.value, tool)
    }

    fn call_id_for_parent(&self, tool: &str, parent: &EventId) -> Result<String, EventWriteError> {
        Ok(self
            .active_calls
            .lock()
            .map_err(lock_error)?
            .remove(&parent.ulid)
            .unwrap_or_else(|| call_id_from_parent(tool, parent)))
    }

    fn append(
        &self,
        actor: Actor,
        payload: EventPayload,
        references: Vec<StableRef>,
        summary: String,
        flush_policy: FlushPolicy,
    ) -> Result<EventId, EventWriteError> {
        let kind = payload.kind();
        let envelope = PartialEnvelope {
            workspace_id: Some(self.workspace_id.clone()),
            branch: self.branch.clone(),
            session_id: self.session_id.clone(),
            task_id: self.current_task.lock().map_err(lock_error)?.clone(),
            actor,
            kind,
            references,
            summary: compact_summary(summary)?,
            payload,
        };
        self.writer.append_with_flush_policy(envelope, flush_policy)
    }
}
