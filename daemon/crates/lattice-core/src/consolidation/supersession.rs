//! Deterministic consolidation job for "detect supersession candidates".
//!
//! This implements the `## 6. Consolidation Engine` consolidation-jobs bullet
//! "detect supersession candidates" from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`.

use std::collections::BTreeMap;
use std::time::Instant;

use serde_json::json;
use tracing::info_span;

use super::{
    capture_memory_state, encode_memory_state, ConsolidationJobMode, ConsolidationJobRuntime,
    PendingProposalSpec, ProposalKind, ScanError, ScanReport,
};
use crate::memory::{Memory, MemoryLinkRecord, MemoryStore, MemoryVerificationStatus};

pub struct SupersessionCandidates<'a> {
    store: &'a MemoryStore,
    runtime: &'a mut ConsolidationJobRuntime,
}

impl<'a> SupersessionCandidates<'a> {
    pub fn new(store: &'a MemoryStore, runtime: &'a mut ConsolidationJobRuntime) -> Self {
        Self { store, runtime }
    }

    pub fn scan(&mut self, workspace_id: &str) -> Result<ScanReport, ScanError> {
        let started = Instant::now();
        let span = info_span!(
            "consolidation.supersession.scan",
            workspace_id,
            kind = "detect supersession candidates"
        );
        let _entered = span.enter();
        let memories = self.store.list_workspace_memories(workspace_id)?;
        let mut proposals_enqueued = 0u32;
        let mut skipped = 0u32;

        for bucket in anchor_buckets(&memories).into_values() {
            if bucket.len() < 2 {
                skipped += bucket.len() as u32;
                continue;
            }
            let mut ordered = bucket;
            ordered.sort_by_key(|memory| memory.created_at);
            let Some(newer) = ordered.last().cloned() else {
                continue;
            };
            for older in ordered
                .into_iter()
                .take_while(|memory| memory.id != newer.id)
            {
                let prior_state = capture_memory_state(self.store, &older)?;
                if prior_state
                    .structured_fields
                    .superseded_by_memory_id
                    .is_some()
                {
                    skipped += 1;
                    continue;
                }
                let mut proposed_state = prior_state.clone();
                proposed_state.structured_fields.verification_status =
                    MemoryVerificationStatus::Superseded;
                proposed_state.structured_fields.superseded_by_memory_id = Some(newer.id.clone());
                proposed_state.memory_links.push(MemoryLinkRecord {
                    link_id: format!("supersession:{}:{}", older.id, newer.id),
                    source_memory_id: older.id.clone(),
                    target_memory_id: newer.id.clone(),
                    link_type: "supersedes".to_string(),
                    reason: "newer memory replaced overlapping anchors".to_string(),
                    created_at: newer.created_at,
                    verification_status: "verified".to_string(),
                });

                self.runtime.submit(crate::consolidation::ConsolidationJobSpec {
                    job_id: format!("supersession-{}-{}", older.id, newer.id),
                    workspace_id: workspace_id.to_string(),
                    kind: "detect supersession candidates".to_string(),
                    mode: ConsolidationJobMode::Background,
                    proposal: Some(PendingProposalSpec {
                        proposal_id: format!("supersession-proposal-{}-{}", older.id, newer.id),
                        target_memory_id: Some(older.id.clone()),
                        proposal_kind: ProposalKind::Supersede,
                        prior_state: encode_memory_state(&prior_state),
                        proposed_state: encode_memory_state(&proposed_state),
                        evidence: json!({
                            "source_memory_ids": [older.id.clone(), newer.id.clone()],
                            "shared_files": shared_values(&older.linked_files, &newer.linked_files),
                            "shared_symbols": shared_values(&older.linked_symbols, &newer.linked_symbols),
                        }),
                        provenance: None,
                    }),
                })?;
                proposals_enqueued += 1;
            }
        }

        let _ = self.runtime.run_due()?;
        Ok(ScanReport::from_counts(
            proposals_enqueued,
            skipped,
            started,
        ))
    }
}

fn anchor_buckets(memories: &[Memory]) -> BTreeMap<String, Vec<Memory>> {
    let mut buckets = BTreeMap::new();
    for memory in memories {
        for file in &memory.linked_files {
            buckets
                .entry(format!("file:{file}"))
                .or_insert_with(Vec::new)
                .push(memory.clone());
        }
        for symbol in &memory.linked_symbols {
            buckets
                .entry(format!("symbol:{symbol}"))
                .or_insert_with(Vec::new)
                .push(memory.clone());
        }
    }
    buckets
}

fn shared_values(left: &[String], right: &[String]) -> Vec<String> {
    let right_values = right.iter().collect::<std::collections::BTreeSet<_>>();
    left.iter()
        .filter(|value| right_values.contains(value))
        .cloned()
        .collect()
}
