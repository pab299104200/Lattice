//! Manual review queue for high-scope consolidation proposals.
//!
//! This module implements the scope-gating rule from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 6. Consolidation Engine` "Consolidation modes": proposals whose target
//! memory scope is `repo` or `organization` must remain pending for explicit
//! human review instead of flowing through low-scope auto-apply policy.
//! It also provides the durable queue backing required by
//! `## 10. Human Review Surface` "Required operator views" so the future memory
//! inbox and consolidation queue can read, inspect, and decide pending items
//! without reimplementing scope logic.

use std::convert::TryFrom;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::info_span;

use crate::consolidation::llm::LlmProvenance;
use crate::consolidation::proposal::{load_row, ConsolidationProposalRow};
use crate::consolidation::{
    ApplyOutcome, ConsolidationProposal, ProposalDecision, ProposalKind, RejectOutcome,
};
use crate::error::LatticeError;
use crate::events::EventWriter;
use crate::identity::OperatorId;
use crate::memory::MemoryStore;
use crate::memory_graph::MemoryScope;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewQueueFilter {
    pub scope: Option<MemoryScope>,
    pub kind: Option<ProposalKind>,
    pub older_than: Option<i64>,
    pub workspace_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewItem {
    pub proposal_id: String,
    pub job_id: String,
    pub workspace_id: String,
    pub kind: ProposalKind,
    pub scope: MemoryScope,
    pub target_memory_id: Option<String>,
    pub prior_state: Value,
    pub proposed_state: Value,
    pub evidence: Value,
    pub provenance: Option<LlmProvenance>,
    pub enqueued_at: i64,
    pub decided_at: Option<i64>,
    pub decision: ProposalDecision,
    pub decided_by: Option<String>,
    pub decision_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewDecisionOutcome {
    Applied { outcome: ApplyOutcome },
    Rejected { outcome: RejectOutcome },
    AlreadyDecided { decision: ProposalDecision },
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewQueueError {
    #[error(transparent)]
    Storage(#[from] LatticeError),
    #[error("review proposal '{proposal_id}' was not found")]
    ProposalNotFound { proposal_id: String },
    #[error("proposal '{proposal_id}' is not eligible for manual review (scope {scope})")]
    ScopeMismatch { proposal_id: String, scope: String },
    #[error("proposal '{proposal_id}' does not encode a target scope")]
    MissingScope { proposal_id: String },
}

pub struct ReviewQueue<'a> {
    conn: &'a Connection,
    memory_store: &'a MemoryStore,
    event_writer: &'a EventWriter,
}

impl<'a> ReviewQueue<'a> {
    pub fn new(
        conn: &'a Connection,
        memory_store: &'a MemoryStore,
        event_writer: &'a EventWriter,
    ) -> Self {
        Self {
            conn,
            memory_store,
            event_writer,
        }
    }

    pub fn should_gate(proposal: &ConsolidationProposal) -> bool {
        proposal_scope_value(&proposal.proposed_state, &proposal.prior_state)
            .and_then(parse_scope_value)
            .is_some_and(is_manual_review_scope)
    }

    pub fn enqueue(
        &self,
        proposal: &ConsolidationProposal,
    ) -> Result<Option<ReviewItem>, ReviewQueueError> {
        if !Self::should_gate(proposal) {
            return Ok(None);
        }
        self.inspect(&proposal.proposal_id).map(Some)
    }

    pub fn list_pending(
        &self,
        workspace_id: &str,
        filter: &ReviewQueueFilter,
    ) -> Result<Vec<ReviewItem>, ReviewQueueError> {
        let requested_workspace = filter.workspace_id.as_deref().unwrap_or(workspace_id);
        let mut statement = self
            .conn
            .prepare(
                "SELECT p.proposal_id,
                    p.job_id,
                    j.workspace_id,
                    j.enqueued_at,
                    p.target_memory_id,
                    p.proposal_kind,
                    p.prior_state,
                    p.proposed_state,
                    p.evidence,
                    p.provenance_json,
                    p.decided_at,
                    p.decision,
                    p.decided_by,
                    p.decision_reason
             FROM consolidation_proposals p
             INNER JOIN consolidation_jobs j ON j.job_id = p.job_id
             WHERE p.decision = 'pending'
               AND j.workspace_id = ?1
             ORDER BY j.enqueued_at ASC",
            )
            .map_err(storage_error)?;
        let rows = statement
            .query_map(params![requested_workspace], map_review_row)
            .map_err(storage_error)?;
        let mut items = Vec::new();
        for row in rows {
            let item = ReviewItem::try_from(row.map_err(storage_error)?)?;
            if !is_manual_review_scope(item.scope) {
                continue;
            }
            if let Some(scope) = filter.scope {
                if item.scope != scope {
                    continue;
                }
            }
            if let Some(kind) = filter.kind {
                if item.kind != kind {
                    continue;
                }
            }
            if let Some(older_than) = filter.older_than {
                if item.enqueued_at > older_than {
                    continue;
                }
            }
            items.push(item);
        }
        Ok(items)
    }

    /// Internal review-queue API for the Phase 10 memory inbox described in
    /// `## 10. Human Review Surface` "Required operator views".
    pub fn inspect(&self, proposal_id: &str) -> Result<ReviewItem, ReviewQueueError> {
        let row = load_row(self.conn, proposal_id)?.ok_or_else(|| {
            ReviewQueueError::ProposalNotFound {
                proposal_id: proposal_id.to_string(),
            }
        })?;
        let item = ReviewItem::try_from(row)?;
        if !is_manual_review_scope(item.scope) {
            return Err(ReviewQueueError::ScopeMismatch {
                proposal_id: proposal_id.to_string(),
                scope: item.scope.as_str().to_string(),
            });
        }
        Ok(item)
    }

    pub fn decide(
        &self,
        proposal_id: &str,
        decision: ProposalDecision,
        decided_by: &OperatorId,
        reason: Option<String>,
    ) -> Result<ReviewDecisionOutcome, ReviewQueueError> {
        let item = self.inspect(proposal_id)?;
        let span = info_span!(
            "manual_review_decision",
            proposal_id = item.proposal_id.as_str(),
            scope = item.scope.as_str(),
            decision = decision.as_str(),
            decided_by = decided_by.value.as_str()
        );
        let _entered = span.enter();
        if item.decision != ProposalDecision::Pending {
            return Ok(ReviewDecisionOutcome::AlreadyDecided {
                decision: item.decision,
            });
        }
        let proposal = ConsolidationProposal::load(self.conn, proposal_id)?.ok_or_else(|| {
            ReviewQueueError::ProposalNotFound {
                proposal_id: proposal_id.to_string(),
            }
        })?;
        let reason_ref = reason.as_deref();
        match decision {
            ProposalDecision::Applied => {
                let outcome = proposal.apply(
                    self.conn,
                    self.memory_store,
                    self.event_writer,
                    decided_by.value.as_str(),
                    reason_ref,
                )?;
                Ok(ReviewDecisionOutcome::Applied { outcome })
            }
            ProposalDecision::Rejected => {
                let outcome = proposal.reject(
                    self.conn,
                    self.event_writer,
                    decided_by.value.as_str(),
                    reason_ref,
                )?;
                Ok(ReviewDecisionOutcome::Rejected { outcome })
            }
            ProposalDecision::Pending | ProposalDecision::Reverted => {
                Ok(ReviewDecisionOutcome::AlreadyDecided { decision })
            }
        }
    }
}

fn storage_error(error: rusqlite::Error) -> ReviewQueueError {
    ReviewQueueError::Storage(LatticeError::Storage(format!(
        "Review queue storage failed: {error}"
    )))
}

impl TryFrom<ConsolidationProposalRow> for ReviewItem {
    type Error = ReviewQueueError;

    fn try_from(row: ConsolidationProposalRow) -> Result<Self, Self::Error> {
        let scope = proposal_scope_from_strings(
            row.proposal_id.as_str(),
            &row.proposed_state,
            &row.prior_state,
        )?;
        Ok(Self {
            proposal_id: row.proposal_id,
            job_id: row.job_id,
            workspace_id: row.workspace_id,
            kind: ProposalKind::from_str(&row.proposal_kind)?,
            scope,
            target_memory_id: row.target_memory_id,
            prior_state: serde_json::from_str(&row.prior_state).map_err(json_error)?,
            proposed_state: serde_json::from_str(&row.proposed_state).map_err(json_error)?,
            evidence: serde_json::from_str(&row.evidence).map_err(json_error)?,
            provenance: row
                .provenance_json
                .map(|value| serde_json::from_str(&value).map_err(json_error))
                .transpose()?,
            enqueued_at: row.enqueued_at,
            decided_at: row.decided_at,
            decision: ProposalDecision::from_str(&row.decision)?,
            decided_by: row.decided_by,
            decision_reason: row.decision_reason,
        })
    }
}

fn map_review_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConsolidationProposalRow> {
    Ok(ConsolidationProposalRow {
        proposal_id: row.get(0)?,
        job_id: row.get(1)?,
        workspace_id: row.get(2)?,
        enqueued_at: row.get(3)?,
        target_memory_id: row.get(4)?,
        proposal_kind: row.get(5)?,
        prior_state: row.get(6)?,
        proposed_state: row.get(7)?,
        evidence: row.get(8)?,
        provenance_json: row.get(9)?,
        decided_at: row.get(10)?,
        decision: row.get(11)?,
        decided_by: row.get(12)?,
        decision_reason: row.get(13)?,
    })
}

fn proposal_scope_from_strings(
    proposal_id: &str,
    proposed_state: &str,
    prior_state: &str,
) -> Result<MemoryScope, ReviewQueueError> {
    let proposed = serde_json::from_str(proposed_state).map_err(json_error)?;
    let prior = serde_json::from_str(prior_state).map_err(json_error)?;
    proposal_scope(&proposed, &prior).ok_or_else(|| ReviewQueueError::MissingScope {
        proposal_id: proposal_id.to_string(),
    })
}

fn proposal_scope(proposed_state: &Value, prior_state: &Value) -> Option<MemoryScope> {
    proposal_scope_value(proposed_state, prior_state).and_then(parse_scope_value)
}

fn proposal_scope_value<'a>(proposed_state: &'a Value, prior_state: &'a Value) -> Option<&'a str> {
    proposed_state
        .pointer("/memory/scope")
        .and_then(Value::as_str)
        .or_else(|| proposed_state.get("scope").and_then(Value::as_str))
        .or_else(|| prior_state.pointer("/memory/scope").and_then(Value::as_str))
        .or_else(|| prior_state.get("scope").and_then(Value::as_str))
}

fn parse_scope_value(value: &str) -> Option<MemoryScope> {
    match value.trim().trim_matches('"').to_ascii_lowercase().as_str() {
        "session" => Some(MemoryScope::Session),
        "branch" => Some(MemoryScope::Branch),
        "repo" => Some(MemoryScope::Repo),
        "organization" => Some(MemoryScope::Organization),
        "user" => Some(MemoryScope::User),
        _ => None,
    }
}

fn is_manual_review_scope(scope: MemoryScope) -> bool {
    matches!(scope, MemoryScope::Repo | MemoryScope::Organization)
}

fn json_error(error: serde_json::Error) -> ReviewQueueError {
    ReviewQueueError::Storage(LatticeError::Storage(format!(
        "Failed to process review queue JSON: {error}"
    )))
}
