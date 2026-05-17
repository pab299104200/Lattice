use std::collections::HashSet;
use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::events::{
    EventEnvelope, EventKind, EventPayload, StableRef, TaskId, ToolCalledPayload, ToolResultPayload,
};
use crate::identity::EventId;

pub type ToolName = String;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeOutcome {
    Success,
    Failure,
    Abandoned,
}

impl EpisodeOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Abandoned => "abandoned",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EpisodeError {
    #[error("task slice for `{task_id}` was not found")]
    MissingTaskSlice { task_id: String },
    #[error("task slice was empty")]
    EmptyTaskSlice,
    #[error("event `{event_id}` is missing a task id")]
    EventMissingTaskId { event_id: String },
    #[error("task slice mixed task ids; expected `{expected}`, got `{actual}`")]
    MixedTaskIds { expected: String, actual: String },
    #[error("failed to read consolidation storage: {0}")]
    StoreUnavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpisodeTemplate {
    pub task_id: TaskId,
    pub session_id: crate::events::SessionId,
    pub outcome: EpisodeOutcome,
    pub event_window: Range<EventId>,
    pub salient_anchors: Vec<StableRef>,
    pub tools_used: Vec<ToolName>,
    pub summary_text: String,
}

impl EpisodeTemplate {
    pub fn from_task_slice(events: &[EventEnvelope]) -> Result<EpisodeTemplate, EpisodeError> {
        let first = events.first().ok_or(EpisodeError::EmptyTaskSlice)?;
        let task_id = first
            .task_id
            .clone()
            .ok_or_else(|| EpisodeError::EventMissingTaskId {
                event_id: first.event_id.to_string(),
            })?;
        let session_id = first.session_id.clone();
        let last = events
            .last()
            .expect("non-empty events slice has last element");

        for event in events.iter().skip(1) {
            let Some(event_task_id) = event.task_id.as_ref() else {
                return Err(EpisodeError::EventMissingTaskId {
                    event_id: event.event_id.to_string(),
                });
            };
            if event_task_id != &task_id {
                return Err(EpisodeError::MixedTaskIds {
                    expected: task_id.value.clone(),
                    actual: event_task_id.value.clone(),
                });
            }
        }

        let salient_anchors = collect_salient_anchors(events);
        let tools_used = collect_tools_used(events);
        let outcome = detect_outcome(events);
        let file_count = salient_anchors
            .iter()
            .filter(|reference| matches!(reference, StableRef::FileRef(_)))
            .count();
        let summary_text = format!(
            "Task {}: {} tools, {} files, outcome={}",
            task_id.value,
            tools_used.len(),
            file_count,
            outcome.as_str()
        );

        Ok(EpisodeTemplate {
            task_id,
            session_id,
            outcome,
            event_window: first.event_id.clone()..last.event_id.clone(),
            salient_anchors,
            tools_used,
            summary_text,
        })
    }
}

fn collect_salient_anchors(events: &[EventEnvelope]) -> Vec<StableRef> {
    let mut seen = HashSet::new();
    let mut anchors = Vec::new();
    for event in events {
        for reference in &event.references {
            if !matches!(
                reference,
                StableRef::FileRef(_) | StableRef::SymbolRef(_) | StableRef::DocSectionRef(_)
            ) {
                continue;
            }
            if seen.insert(reference.clone()) {
                anchors.push(reference.clone());
            }
        }
    }
    anchors
}

fn collect_tools_used(events: &[EventEnvelope]) -> Vec<ToolName> {
    let mut seen = HashSet::new();
    let mut tools = Vec::new();
    for event in events {
        let tool_name = match &event.payload {
            EventPayload::ToolCalled(ToolCalledPayload { tool_name, .. })
            | EventPayload::ToolResult(ToolResultPayload { tool_name, .. }) => Some(tool_name),
            _ => None,
        };
        if let Some(tool_name) = tool_name {
            if seen.insert(tool_name.clone()) {
                tools.push(tool_name.clone());
            }
        }
    }
    tools
}

fn detect_outcome(events: &[EventEnvelope]) -> EpisodeOutcome {
    if events
        .iter()
        .any(|event| event.kind == EventKind::WorkflowFailed)
    {
        EpisodeOutcome::Failure
    } else if events
        .iter()
        .any(|event| event.kind == EventKind::WorkflowSucceeded)
    {
        EpisodeOutcome::Success
    } else {
        EpisodeOutcome::Abandoned
    }
}
