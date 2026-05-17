//! Synchronous post-task consolidation for small task traces.
//!
//! This module implements the `## 6. Consolidation Engine` "Consolidation modes"
//! definition from `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`:
//! synchronous post-task consolidation runs immediately after a workflow
//! concludes for small traces, while larger histories are redirected to
//! background consolidation. It also enforces the `## Phase 2: Event Log
//! Substrate` hot-path rule that event writes stay bounded; no LLM inference is
//! allowed here, and slices above the configured ceiling short-circuit into a
//! background job before the synchronous path can grow unbounded.

use std::path::Path;
use std::time::Instant;

use rusqlite::Connection;
use serde_json::{json, Value};
use tracing::{field, info_span};

use super::{
    empty_state, now_unix_micros, ConsolidationConfig, ConsolidationJobMode,
    ConsolidationJobRuntime, ConsolidationJobSpec, EnqueueOutcome, EpisodeError, EpisodeOutcome,
    EpisodeTemplate, PendingProposalSpec, ProposalKind,
};
use crate::events::{EventQuery, EventReader, EventStore, QueryOrder, StableRef, TaskId};
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType};

const DEFAULT_SYNCHRONOUS_MAX_EVENTS: usize = 200;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionConsolidationConfig {
    pub synchronous_max_events: usize,
}

impl Default for SessionConsolidationConfig {
    fn default() -> Self {
        Self {
            synchronous_max_events: DEFAULT_SYNCHRONOUS_MAX_EVENTS,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionConsolidationOutcome {
    Proposed {
        proposal_id: String,
        job_id: String,
        proposal_kind: ProposalKind,
    },
    RedirectedToBackground {
        job_id: String,
        slice_len: usize,
        enqueue: EnqueueOutcome,
    },
}

pub struct SessionConsolidator {
    event_reader: EventReader,
    memory_store: MemoryStore,
    runtime: ConsolidationJobRuntime,
    config: SessionConsolidationConfig,
}

impl SessionConsolidator {
    pub fn new(
        event_reader: EventReader,
        memory_store: MemoryStore,
        runtime: ConsolidationJobRuntime,
        config: SessionConsolidationConfig,
    ) -> Self {
        Self {
            event_reader,
            memory_store,
            runtime,
            config,
        }
    }

    pub fn open(
        event_store: std::sync::Arc<EventStore>,
        memory_db_path: &Path,
        config: SessionConsolidationConfig,
    ) -> Result<Self, EpisodeError> {
        let memory_store = MemoryStore::open(memory_db_path)
            .map_err(|error| EpisodeError::StoreUnavailable(error.to_string()))?;
        let runtime = ConsolidationJobRuntime::new(
            Connection::open(memory_db_path)
                .map_err(|error| EpisodeError::StoreUnavailable(error.to_string()))?,
            ConsolidationConfig::default(),
        )
        .map_err(|error| EpisodeError::StoreUnavailable(error.to_string()))?;
        Ok(Self::new(
            EventReader::new(event_store),
            memory_store,
            runtime,
            config,
        ))
    }

    pub fn on_task_complete(
        &mut self,
        workspace_id: &str,
        task_id: &TaskId,
        outcome: EpisodeOutcome,
    ) -> Result<SessionConsolidationOutcome, EpisodeError> {
        let span = info_span!(
            "session_consolidation",
            workspace_id = workspace_id,
            task_id = task_id.value.as_str(),
            slice_len = field::Empty,
            elapsed_us = field::Empty,
            outcome = field::Empty
        );
        let _entered = span.enter();
        let started = Instant::now();
        let slice = self.read_task_slice(workspace_id, task_id)?;
        span.record("slice_len", slice.len() as i64);

        let result = if slice.is_empty() {
            Err(EpisodeError::MissingTaskSlice {
                task_id: task_id.value.clone(),
            })
        } else if slice.len() > self.config.synchronous_max_events {
            let (job_id, enqueue) =
                self.enqueue_background_redirect(workspace_id, task_id, slice.len())?;
            Ok(SessionConsolidationOutcome::RedirectedToBackground {
                job_id,
                slice_len: slice.len(),
                enqueue,
            })
        } else {
            let template = EpisodeTemplate::from_task_slice(&slice)?;
            if template.outcome != outcome {
                tracing::warn!(
                    task_id = task_id.value.as_str(),
                    expected = outcome.as_str(),
                    actual = template.outcome.as_str(),
                    "session consolidation outcome mismatch; using event-derived outcome"
                );
            }
            self.emit_synchronous_proposal(workspace_id, &template)
        };

        let elapsed = started.elapsed();
        span.record("elapsed_us", elapsed.as_micros() as i64);
        match &result {
            Ok(SessionConsolidationOutcome::Proposed { .. }) => {
                span.record("outcome", "proposed");
            }
            Ok(SessionConsolidationOutcome::RedirectedToBackground { .. }) => {
                span.record("outcome", "redirected");
            }
            Err(_) => {
                span.record("outcome", "failed");
            }
        }
        result
    }

    pub fn memory_store(&self) -> &MemoryStore {
        &self.memory_store
    }

    fn read_task_slice(
        &self,
        workspace_id: &str,
        task_id: &TaskId,
    ) -> Result<Vec<crate::events::EventEnvelope>, EpisodeError> {
        EventQuery::new()
            .workspace(workspace_id.to_string())
            .task(task_id.value.clone())
            .limit(self.config.synchronous_max_events.saturating_add(1))
            .order(QueryOrder::OldestFirst)
            .execute(&self.event_reader)
            .map_err(|error| EpisodeError::StoreUnavailable(error.to_string()))
    }

    fn enqueue_background_redirect(
        &mut self,
        workspace_id: &str,
        task_id: &TaskId,
        slice_len: usize,
    ) -> Result<(String, EnqueueOutcome), EpisodeError> {
        let job_id = redirect_job_id(task_id);
        let job = ConsolidationJobSpec {
            job_id: job_id.clone(),
            workspace_id: workspace_id.to_string(),
            kind: format!("session_consolidation_redirect:{}", task_id.value),
            mode: ConsolidationJobMode::Background,
            proposal: None,
        };
        self.runtime
            .submit(job)
            .map_err(|error| EpisodeError::StoreUnavailable(error.to_string()))
            .map(|outcome| {
                tracing::info!(
                    task_id = task_id.value.as_str(),
                    slice_len,
                    "redirected session consolidation to background mode"
                );
                (job_id, outcome)
            })
    }

    fn emit_synchronous_proposal(
        &mut self,
        workspace_id: &str,
        template: &EpisodeTemplate,
    ) -> Result<SessionConsolidationOutcome, EpisodeError> {
        let refresh_key = episode_refresh_key(template);
        let existing = self
            .memory_store
            .find_by_refresh_key(&refresh_key, Some(workspace_id), None)
            .map_err(|error| EpisodeError::StoreUnavailable(error.to_string()))?;
        let proposal_kind = if existing.is_some() {
            ProposalKind::UpdateMemory
        } else {
            ProposalKind::CreateMemory
        };
        let prior_state = existing
            .as_ref()
            .map(memory_with_episode_metadata)
            .unwrap_or_else(empty_state);
        let proposed_memory =
            build_episode_memory(existing.as_ref(), template, &refresh_key, workspace_id);
        let proposed_state = memory_state_with_episode(&proposed_memory, template);
        let proposal_id = format!(
            "episode-proposal-{}-{}",
            task_id_slug(&template.task_id.value),
            template.event_window.end.ulid
        );
        let job_id = format!(
            "session-consolidation-{}-{}",
            task_id_slug(&template.task_id.value),
            template.event_window.end.ulid
        );
        let job = ConsolidationJobSpec {
            job_id: job_id.clone(),
            workspace_id: workspace_id.to_string(),
            kind: format!("session_consolidation:{}", template.task_id.value),
            mode: ConsolidationJobMode::SynchronousPostTask,
            proposal: Some(PendingProposalSpec {
                proposal_id: proposal_id.clone(),
                target_memory_id: existing.as_ref().map(|memory| memory.id.clone()),
                proposal_kind,
                prior_state,
                proposed_state,
                evidence: episode_evidence(template),
                provenance: None,
            }),
        };
        let (_, proposal) = self
            .runtime
            .submit_inline(job)
            .map_err(|error| EpisodeError::StoreUnavailable(error.to_string()))?;
        let proposal = proposal.ok_or_else(|| {
            EpisodeError::StoreUnavailable(
                "synchronous session consolidation did not emit a proposal".to_string(),
            )
        })?;
        Ok(SessionConsolidationOutcome::Proposed {
            proposal_id: proposal.proposal_id,
            job_id,
            proposal_kind,
        })
    }
}

fn build_episode_memory(
    existing: Option<&Memory>,
    template: &EpisodeTemplate,
    refresh_key: &str,
    workspace_id: &str,
) -> Memory {
    Memory {
        id: existing.map(|memory| memory.id.clone()).unwrap_or_else(|| {
            format!(
                "episode-{}-{}",
                task_id_slug(&template.task_id.value),
                template.event_window.end.ulid
            )
        }),
        session_id: template.session_id.value.clone(),
        content: template.summary_text.clone(),
        memory_type: MemoryType::Pattern,
        scope: MemoryScope::Session,
        confidence: match template.outcome {
            EpisodeOutcome::Success => 0.92,
            EpisodeOutcome::Failure => 0.72,
            EpisodeOutcome::Abandoned => 0.55,
        },
        linked_symbols: template
            .salient_anchors
            .iter()
            .filter_map(|reference| match reference {
                StableRef::SymbolRef(symbol) => Some(symbol.to_string()),
                StableRef::DocSectionRef(section) => Some(section.to_string()),
                _ => None,
            })
            .collect(),
        linked_files: template
            .salient_anchors
            .iter()
            .filter_map(|reference| match reference {
                StableRef::FileRef(file) => Some(file.repo_relative_path.clone()),
                _ => None,
            })
            .collect(),
        workspace_id: Some(workspace_id.to_string()),
        branch: None,
        scope_organization_id: None,
        refresh_key: Some(refresh_key.to_string()),
        source_query: Some(format!(
            "episode_summary:{}:{}",
            template.session_id.value, template.task_id.value
        )),
        created_at: existing.map(|memory| memory.created_at).unwrap_or_default(),
        last_accessed: existing
            .map(|memory| memory.last_accessed)
            .unwrap_or_default(),
        access_count: existing
            .map(|memory| memory.access_count)
            .unwrap_or_default(),
        is_stale: false,
        stale_reason: None,
        verification_status: crate::memory::MemoryVerificationStatus::Unverified,
    }
}

fn memory_state_with_episode(memory: &Memory, template: &EpisodeTemplate) -> Value {
    let mut state = serde_json::to_value(memory).expect("memory serializes");
    if let Some(object) = state.as_object_mut() {
        object.insert("episode_task_id".to_string(), json!(template.task_id));
        object.insert("episode_session_id".to_string(), json!(template.session_id));
        object.insert(
            "episode_outcome".to_string(),
            json!(template.outcome.as_str()),
        );
        object.insert(
            "event_window".to_string(),
            json!({
                "start": template.event_window.start,
                "end": template.event_window.end,
            }),
        );
        object.insert(
            "salient_anchors".to_string(),
            serde_json::to_value(&template.salient_anchors).expect("anchors serialize"),
        );
        object.insert(
            "tools_used".to_string(),
            serde_json::to_value(&template.tools_used).expect("tools serialize"),
        );
        object.insert("summary_text".to_string(), json!(template.summary_text));
    }
    state
}

fn memory_with_episode_metadata(memory: &Memory) -> Value {
    serde_json::to_value(memory).expect("memory serializes")
}

fn episode_evidence(template: &EpisodeTemplate) -> Value {
    json!({
        "task_id": template.task_id.value,
        "session_id": template.session_id.value,
        "outcome": template.outcome.as_str(),
        "event_window": {
            "start": &template.event_window.start,
            "end": &template.event_window.end,
        },
        "salient_anchors": &template.salient_anchors,
        "tools_used": &template.tools_used,
        "source_event_ids": [
            &template.event_window.start,
            &template.event_window.end,
        ],
    })
}

fn episode_refresh_key(template: &EpisodeTemplate) -> String {
    format!(
        "workflow_episode::{}::{}",
        template.session_id.value, template.task_id.value
    )
}

fn redirect_job_id(task_id: &TaskId) -> String {
    format!(
        "session-consolidation-background-{}-{}",
        task_id_slug(&task_id.value),
        now_unix_micros()
    )
}

fn task_id_slug(task_id: &str) -> String {
    task_id
        .chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' => character,
            _ => '-',
        })
        .collect()
}
