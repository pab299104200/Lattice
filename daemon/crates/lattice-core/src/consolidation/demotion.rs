//! Deterministic consolidation job for "demote unused or low-value memories".
//!
//! This implements the `## 6. Consolidation Engine` consolidation-jobs bullet
//! "demote unused or low-value memories" from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`.

use std::time::Instant;

use serde_json::json;
use tracing::info_span;

use super::{
    capture_memory_state, encode_memory_state, ConsolidationJobMode, ConsolidationJobRuntime,
    PendingProposalSpec, ProposalKind, ScanError, ScanReport,
};
use crate::memory::{MemoryScoreKind, MemoryStore, MemoryVerificationStatus};

const DEFAULT_DEMOTION_THRESHOLD: f32 = 0.35;

pub struct DemotionScanner<'a> {
    store: &'a MemoryStore,
    runtime: &'a mut ConsolidationJobRuntime,
    demotion_threshold: f32,
}

impl<'a> DemotionScanner<'a> {
    pub fn new(store: &'a MemoryStore, runtime: &'a mut ConsolidationJobRuntime) -> Self {
        Self {
            store,
            runtime,
            demotion_threshold: DEFAULT_DEMOTION_THRESHOLD,
        }
    }

    pub fn scan(&mut self, workspace_id: &str, cutoff: u64) -> Result<ScanReport, ScanError> {
        let started = Instant::now();
        let span = info_span!(
            "consolidation.demotion.scan",
            workspace_id,
            kind = "demote unused or low-value memories"
        );
        let _entered = span.enter();
        let memories = self.store.list_workspace_memories(workspace_id)?;
        let mut proposals_enqueued = 0u32;
        let mut skipped = 0u32;

        for memory in memories {
            let access_count = self.store.count_memory_accesses_since(&memory.id, cutoff)?;
            let usefulness = self
                .store
                .latest_memory_score(&memory.id, MemoryScoreKind::UsefulnessPrior)?
                .map(|score| score.value)
                .unwrap_or(memory.confidence as f32);
            if access_count > 0 || usefulness >= self.demotion_threshold {
                skipped += 1;
                continue;
            }

            let prior_state = capture_memory_state(self.store, &memory)?;
            let mut proposed_state = prior_state.clone();
            proposed_state.memory.confidence = (proposed_state.memory.confidence - 0.2).max(0.1);
            if proposed_state.structured_fields.verification_status
                == MemoryVerificationStatus::Verified
            {
                proposed_state.structured_fields.verification_status =
                    MemoryVerificationStatus::Unverified;
            }

            self.runtime
                .submit(crate::consolidation::ConsolidationJobSpec {
                    job_id: format!("demotion-{}", memory.id),
                    workspace_id: workspace_id.to_string(),
                    kind: "demote unused or low-value memories".to_string(),
                    mode: ConsolidationJobMode::Background,
                    proposal: Some(PendingProposalSpec {
                        proposal_id: format!("demotion-proposal-{}", memory.id),
                        target_memory_id: Some(memory.id.clone()),
                        proposal_kind: ProposalKind::Demote,
                        prior_state: encode_memory_state(&prior_state),
                        proposed_state: encode_memory_state(&proposed_state),
                        evidence: json!({
                            "source_memory_ids": [memory.id.clone()],
                            "access_count_since_cutoff": access_count,
                            "usefulness_score": usefulness,
                            "cutoff": cutoff,
                        }),
                        provenance: None,
                    }),
                })?;
            proposals_enqueued += 1;
        }

        let _ = self.runtime.run_due()?;
        Ok(ScanReport::from_counts(
            proposals_enqueued,
            skipped,
            started,
        ))
    }
}
