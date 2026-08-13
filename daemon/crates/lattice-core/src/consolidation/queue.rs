use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::error::LatticeError;
use crate::events::{
    Actor, BranchRef, CompactSummary, ConsolidationFailedPayload, EventKind, EventPayload,
    EventWriter, PartialEnvelope, SessionId,
};

static CONTENT_FREE_SKIP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidationJobSpec {
    pub job_id: String,
    pub workspace_id: String,
    pub kind: String,
    pub mode: ConsolidationJobMode,
    pub proposal: Option<PendingProposalSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidationJobRecord {
    pub job_id: String,
    pub workspace_id: String,
    pub kind: String,
    pub mode: ConsolidationJobMode,
    pub status: ConsolidationJobStatus,
    pub enqueued_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub proposal_id: Option<String>,
    pub error_kind: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingProposalSpec {
    pub proposal_id: String,
    pub target_memory_id: Option<String>,
    pub proposal_kind: crate::consolidation::ProposalKind,
    pub prior_state: serde_json::Value,
    pub proposed_state: serde_json::Value,
    pub evidence: serde_json::Value,
    pub provenance: Option<crate::consolidation::llm::LlmProvenance>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidationJobMode {
    SynchronousPostTask,
    Background,
    ManualReview,
    Replay,
}

impl ConsolidationJobMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SynchronousPostTask => "synchronous_post_task",
            Self::Background => "background",
            Self::ManualReview => "manual_review",
            Self::Replay => "replay",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsolidationJobStatus {
    Queued,
    Running,
    Proposed,
    Applied,
    Rejected,
    Failed,
    Dropped,
}

impl ConsolidationJobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Proposed => "proposed",
            Self::Applied => "applied",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
            Self::Dropped => "dropped",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Queued { depth: usize },
    Dropped { reason: String },
}

/// Content-free reason recorded when a consolidation job is not admitted.
///
/// Keeping this as a closed enum prevents callers from accidentally storing
/// capture content, provider secrets, or model input in skip telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidationSkipReason {
    Disabled,
    MissingProviderKey,
    QueueFull,
    NoEligibleCaptureFacts,
    BudgetExceeded,
}

impl ConsolidationSkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::MissingProviderKey => "missing_provider_key",
            Self::QueueFull => "queue_full",
            Self::NoEligibleCaptureFacts => "no_eligible_capture_facts",
            Self::BudgetExceeded => "budget_exceeded",
        }
    }
}

pub struct BoundedJobQueue {
    max_depth: usize,
    per_kind_max_depth: Option<usize>,
    jobs: VecDeque<ConsolidationJobSpec>,
    conn: Arc<Mutex<Connection>>,
}

impl BoundedJobQueue {
    pub fn new(max_depth: usize, conn: Arc<Mutex<Connection>>) -> Self {
        Self {
            max_depth,
            per_kind_max_depth: None,
            jobs: VecDeque::new(),
            conn,
        }
    }

    pub fn with_per_kind_max_depth(mut self, max_depth: usize) -> Self {
        self.per_kind_max_depth = Some(max_depth);
        self
    }

    pub fn depth(&self) -> usize {
        self.jobs.len()
    }

    pub fn has_capacity(&self) -> bool {
        self.jobs.len() < self.max_depth
    }

    pub fn enqueue(&mut self, job: ConsolidationJobSpec) -> Result<EnqueueOutcome, LatticeError> {
        if self.jobs.len() >= self.max_depth {
            self.persist_job(&job, ConsolidationJobStatus::Dropped)?;
            warn!(
                workspace_id = job.workspace_id.as_str(),
                kind = job.kind.as_str(),
                depth = self.jobs.len(),
                max_depth = self.max_depth,
                "consolidation queue full; dropping job"
            );
            return Ok(EnqueueOutcome::Dropped {
                reason: format!("consolidation queue depth {} reached", self.max_depth),
            });
        }

        self.persist_job(&job, ConsolidationJobStatus::Queued)?;
        self.jobs.push_back(job);
        Ok(EnqueueOutcome::Queued {
            depth: self.depth(),
        })
    }

    pub fn enqueue_llm(
        &mut self,
        job: ConsolidationJobSpec,
        event_writer: &EventWriter,
        model_name: &str,
    ) -> Result<EnqueueOutcome, LatticeError> {
        if let Some(max_depth) = self.per_kind_max_depth {
            let current_depth = self.kind_depth(&job.kind);
            if current_depth >= max_depth {
                self.persist_job(&job, ConsolidationJobStatus::Dropped)?;
                warn!(
                    workspace_id = job.workspace_id.as_str(),
                    kind = job.kind.as_str(),
                    current_depth,
                    max_depth,
                    "LLM consolidation queue slice full; dropping job"
                );
                emit_queue_full(event_writer, &job, model_name, current_depth, max_depth)?;
                return Ok(EnqueueOutcome::Dropped {
                    reason: format!(
                        "LLM consolidation queue slice {} reached for kind {}",
                        max_depth, job.kind
                    ),
                });
            }
        }
        self.enqueue(job)
    }

    pub(crate) fn persist_enqueued_only(
        &self,
        job: &ConsolidationJobSpec,
    ) -> Result<EnqueueOutcome, LatticeError> {
        self.persist_job(job, ConsolidationJobStatus::Queued)?;
        Ok(EnqueueOutcome::Queued {
            depth: self.depth(),
        })
    }

    pub(crate) fn persist_content_free_skip(
        &self,
        workspace_id: &str,
        kind: &str,
        reason: ConsolidationSkipReason,
    ) -> Result<String, LatticeError> {
        let recorded_at = now_unix_micros();
        let sequence = CONTENT_FREE_SKIP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let job_id = format!("{kind}-skip-{recorded_at}-{sequence}");
        let conn = self.conn.lock().map_err(|_| {
            LatticeError::Storage("consolidation queue storage lock was poisoned".to_string())
        })?;
        conn.execute(
            "INSERT INTO consolidation_jobs
                (job_id, workspace_id, kind, mode, status, enqueued_at, finished_at, error_kind)
             VALUES (?1, ?2, ?3, 'background', 'dropped', ?4, ?4, ?5)",
            params![job_id, workspace_id, kind, recorded_at, reason.as_str()],
        )
        .map_err(|error| {
            LatticeError::Storage(format!(
                "Failed to persist content-free consolidation skip: {error}"
            ))
        })?;
        Ok(job_id)
    }

    pub fn pop(&mut self) -> Option<ConsolidationJobSpec> {
        self.jobs.pop_front()
    }

    pub fn drain_for_shutdown(&mut self) -> Vec<ConsolidationJobSpec> {
        self.jobs.drain(..).collect()
    }

    fn kind_depth(&self, kind: &str) -> usize {
        self.jobs.iter().filter(|job| job.kind == kind).count()
    }

    fn persist_job(
        &self,
        job: &ConsolidationJobSpec,
        status: ConsolidationJobStatus,
    ) -> Result<(), LatticeError> {
        let conn = self.conn.lock().map_err(|_| {
            LatticeError::Storage("consolidation queue storage lock was poisoned".to_string())
        })?;
        conn.execute(
            "INSERT OR REPLACE INTO consolidation_jobs
                (job_id, workspace_id, kind, mode, status, enqueued_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                job.job_id,
                job.workspace_id,
                job.kind,
                job.mode.as_str(),
                status.as_str(),
                now_unix_micros()
            ],
        )
        .map_err(|e| LatticeError::Storage(format!("Failed to persist consolidation job: {e}")))?;
        Ok(())
    }
}

fn emit_queue_full(
    event_writer: &EventWriter,
    job: &ConsolidationJobSpec,
    model_name: &str,
    current_depth: usize,
    max_depth: usize,
) -> Result<(), LatticeError> {
    let payload = ConsolidationFailedPayload {
        job_id: job.job_id.clone(),
        job_kind: job.kind.clone(),
        mode: job.mode.as_str().to_string(),
        model_name: model_name.to_string(),
        error_kind: "queue_full".to_string(),
        error_message: format!(
            "LLM consolidation queue slice full for kind {} ({current_depth}/{max_depth})",
            job.kind
        ),
    };
    event_writer
        .append(PartialEnvelope {
            workspace_id: Some(job.workspace_id.clone()),
            branch: BranchRef {
                name: "main".to_string(),
            },
            session_id: SessionId {
                value: "llm-consolidation".to_string(),
            },
            task_id: None,
            actor: Actor::Daemon,
            kind: EventKind::ConsolidationFailed,
            references: Vec::new(),
            summary: CompactSummary::new(format!(
                "LLM consolidation {} failed: queue_full",
                job.kind
            ))
            .map_err(|e| LatticeError::Storage(e.to_string()))?,
            payload: EventPayload::ConsolidationFailed(payload),
        })
        .map_err(|e| LatticeError::Storage(format!("Failed to write queue-full event: {e}")))?;
    Ok(())
}

pub(crate) fn now_unix_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(i64::MAX as u128) as i64
}
