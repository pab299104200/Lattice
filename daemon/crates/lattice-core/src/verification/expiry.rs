//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 8. Verification Engine`:
//!
//! Verification checks:
//!
//! - time-bound memory has expired
//!
//! Verification outputs:
//!
//! - `expired`

use std::time::Instant;

use serde_json::json;

use super::VerificationStatus;
use crate::consolidation::{
    capture_memory_state, encode_memory_state, now_unix_micros, ConsolidationJobMode,
    ConsolidationJobRuntime, PendingProposalSpec, ProposalDecision, ProposalKind,
};
use crate::error::LatticeError;
use crate::events::EventWriter;
use crate::identity::OperatorId;
use crate::memory::{MemoryStore, MemoryVerificationStatus};
use crate::{DateTime, Utc};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExpiryReport {
    pub expired: u32,
    pub elapsed_millis: u128,
}

#[derive(Debug, thiserror::Error)]
pub enum ExpiryError {
    #[error(transparent)]
    Storage(#[from] LatticeError),
}

pub struct ExpiryScanner<'a> {
    store: &'a MemoryStore,
    runtime: &'a mut ConsolidationJobRuntime,
    event_writer: &'a EventWriter,
    decided_by: &'a OperatorId,
    authority: &'a crate::consolidation::EvolutionAuthority<'a>,
}

impl<'a> ExpiryScanner<'a> {
    pub fn new(
        store: &'a MemoryStore,
        runtime: &'a mut ConsolidationJobRuntime,
        event_writer: &'a EventWriter,
        decided_by: &'a OperatorId,
        authority: &'a crate::consolidation::EvolutionAuthority<'a>,
    ) -> Self {
        Self {
            store,
            runtime,
            event_writer,
            decided_by,
            authority,
        }
    }

    pub fn scan(
        &mut self,
        workspace_id: &str,
        now: DateTime<Utc>,
    ) -> Result<ExpiryReport, ExpiryError> {
        let started = Instant::now();
        let memories = self.store.list_memories_expired_before(workspace_id, now)?;
        let mut report = ExpiryReport::default();

        for memory in memories {
            if self.expire_memory(&memory.id, workspace_id, now)?.is_some() {
                report.expired += 1;
            }
        }

        report.elapsed_millis = started.elapsed().as_millis();
        Ok(report)
    }

    pub fn expire_memory(
        &mut self,
        memory_id: &str,
        workspace_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<String>, ExpiryError> {
        let Some(memory) = self.store.get_by_id(memory_id)? else {
            return Ok(None);
        };
        let prior_state = capture_memory_state(self.store, &memory)?;
        if prior_state.structured_fields.verification_status == MemoryVerificationStatus::Expired {
            return Ok(None);
        }
        let proposal_id =
            self.submit_expiry_proposal(memory_id, workspace_id, now, &prior_state)?;
        self.apply_proposal(&proposal_id)?;
        Ok(Some(proposal_id))
    }

    fn submit_expiry_proposal(
        &mut self,
        memory_id: &str,
        workspace_id: &str,
        now: DateTime<Utc>,
        prior_state: &crate::consolidation::ConsolidationMemoryState,
    ) -> Result<String, ExpiryError> {
        let job_id = format!("expiry-{}-{}", memory_id, now_unix_micros());
        let proposal_id = format!("expiry-proposal-{}-{}", memory_id, now_unix_micros());
        let proposed_state = expired_state(prior_state);

        let _ = self.runtime.submit_inline(
            crate::consolidation::ConsolidationJobSpec {
                job_id,
                workspace_id: workspace_id.to_string(),
                kind: "expire time-bound durable memory".to_string(),
                mode: ConsolidationJobMode::Background,
                proposal: Some(PendingProposalSpec {
                    proposal_id: proposal_id.clone(),
                    target_memory_id: Some(memory_id.to_string()),
                    proposal_kind: ProposalKind::MarkExpired,
                    prior_state: encode_memory_state(prior_state),
                    proposed_state: encode_memory_state(&proposed_state),
                    evidence: json!({
                        "source_memory_ids": [memory_id],
                        "verification_status": VerificationStatus::Expired.as_str(),
                        "expired_at": now,
                        "prior_state": {
                            "expires_at": prior_state.expires_at,
                        },
                    }),
                    provenance: None,
                }),
            },
            self.store,
            self.authority,
        )?;

        Ok(proposal_id)
    }

    fn apply_proposal(&self, proposal_id: &str) -> Result<(), ExpiryError> {
        let _ = self.runtime.decide(
            proposal_id,
            ProposalDecision::Applied,
            self.store,
            self.event_writer,
            self.decided_by.value.as_str(),
            self.authority,
        )?;
        Ok(())
    }
}

fn expired_state(
    prior_state: &crate::consolidation::ConsolidationMemoryState,
) -> crate::consolidation::ConsolidationMemoryState {
    let mut proposed_state = prior_state.clone();
    proposed_state.memory.is_stale = false;
    proposed_state.memory.stale_reason = None;
    proposed_state.structured_fields.verification_status = MemoryVerificationStatus::Expired;
    proposed_state
}
