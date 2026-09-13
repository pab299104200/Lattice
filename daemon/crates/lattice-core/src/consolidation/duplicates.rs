//! Deterministic consolidation job for "detect duplicate memories".
//!
//! This implements the `## 6. Consolidation Engine` consolidation-jobs bullet
//! "detect duplicate memories" from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use serde_json::json;
use tracing::info_span;

use super::{
    capture_memory_state, encode_memory_state, ConsolidationJobMode, ConsolidationJobRuntime,
    PendingProposalSpec, ProposalKind, ScanError, ScanReport,
};
use crate::memory::{Memory, MemoryStore};

pub struct DuplicateDetector<'a> {
    store: &'a MemoryStore,
    runtime: &'a mut ConsolidationJobRuntime,
}

impl<'a> DuplicateDetector<'a> {
    pub fn new(store: &'a MemoryStore, runtime: &'a mut ConsolidationJobRuntime) -> Self {
        Self { store, runtime }
    }

    pub fn scan(
        &mut self,
        authority: &super::EvolutionAuthority<'_>,
        kind_filter: Option<ProposalKind>,
    ) -> Result<ScanReport, ScanError> {
        let workspace_id = authority.repository_id;
        let started = Instant::now();
        let span = info_span!(
            "consolidation.duplicates.scan",
            workspace_id,
            kind = "detect duplicate memories"
        );
        let _entered = span.enter();
        let memories: Vec<Memory> = self
            .store
            .list_applicable_workspace_memories(workspace_id, authority.checkout_id)?
            .into_iter()
            .filter(|memory| {
                memory.scope != crate::memory::MemoryScope::Branch
                    || memory.branch.as_deref() == Some(authority.branch)
            })
            .collect();
        if memories.is_empty() {
            return Ok(ScanReport::from_counts(0, 0, started));
        }

        let mut proposals_enqueued = 0u32;
        let mut skipped = 0u32;
        for bucket in bucket_memories(&memories).into_values() {
            for (left_index, memory) in bucket.iter().enumerate() {
                for candidate in bucket.iter().skip(left_index + 1) {
                    if !shares_typed_evidence(memory, candidate) {
                        skipped += 1;
                        continue;
                    }

                    let older = older_memory(memory, candidate);
                    let newer = newer_memory(memory, candidate);
                    let both_stale = older.is_stale && newer.is_stale;
                    let proposal_kind = if both_stale {
                        ProposalKind::Demote
                    } else {
                        ProposalKind::Supersede
                    };
                    if kind_filter.is_some() && kind_filter != Some(proposal_kind) {
                        skipped += 1;
                        continue;
                    }

                    let prior_state = capture_memory_state(self.store, older)?;
                    let proposed_state = if proposal_kind == ProposalKind::Demote {
                        let mut next = prior_state.clone();
                        next.memory.confidence = (next.memory.confidence - 0.15).max(0.1);
                        if next.structured_fields.verification_status
                            == crate::memory::MemoryVerificationStatus::Verified
                        {
                            next.structured_fields.verification_status =
                                crate::memory::MemoryVerificationStatus::Unverified;
                        }
                        next
                    } else {
                        let mut next = prior_state.clone();
                        next.structured_fields.verification_status =
                            crate::memory::MemoryVerificationStatus::Superseded;
                        next.structured_fields.superseded_by_memory_id = Some(newer.id.clone());

                        next
                    };

                    enqueue(
                        self.runtime,
                        workspace_id,
                        format!("duplicate-detection-{}-{}", older.id, newer.id),
                        PendingProposalSpec {
                            proposal_id: format!("duplicate-proposal-{}-{}", older.id, newer.id),
                            target_memory_id: Some(older.id.clone()),
                            proposal_kind,
                            prior_state: encode_memory_state(&prior_state),
                            proposed_state: encode_memory_state(&proposed_state),
                            evidence: json!({
                                "source_memory_ids": [older.id.clone(), newer.id.clone()],
                                "superseded_by_memory_id": newer.id.clone(),
                                "replacement_state_hash": super::state_hash_for_memory(self.store, &newer.id)?,
                                "shared_files": shared_values(&older.linked_files, &newer.linked_files),
                                "shared_symbols": shared_values(&older.linked_symbols, &newer.linked_symbols),
                                "decision_basis": "typed_evidence_overlap",
                            }),
                            provenance: None,
                        },
                    )?;
                    proposals_enqueued += 1;
                }
            }
        }

        let _ = self.runtime.run_due(self.store, authority)?;
        Ok(ScanReport::from_counts(
            proposals_enqueued,
            skipped,
            started,
        ))
    }
}

fn bucket_memories(memories: &[Memory]) -> BTreeMap<(String, String), Vec<Memory>> {
    let mut buckets = BTreeMap::new();
    for memory in memories {
        buckets
            .entry((
                memory.memory_type.as_str().to_string(),
                memory.scope.as_str().to_string(),
            ))
            .or_insert_with(Vec::new)
            .push(memory.clone());
    }
    buckets
}

fn older_memory<'a>(left: &'a Memory, right: &'a Memory) -> &'a Memory {
    if left.created_at <= right.created_at {
        left
    } else {
        right
    }
}

fn newer_memory<'a>(left: &'a Memory, right: &'a Memory) -> &'a Memory {
    if left.created_at >= right.created_at {
        left
    } else {
        right
    }
}

fn shares_typed_evidence(left: &Memory, right: &Memory) -> bool {
    !shared_values(&left.linked_files, &right.linked_files).is_empty()
        || !shared_values(&left.linked_symbols, &right.linked_symbols).is_empty()
}

fn shared_values(left: &[String], right: &[String]) -> Vec<String> {
    let right_values = right.iter().collect::<BTreeSet<_>>();
    left.iter()
        .filter(|value| right_values.contains(value))
        .cloned()
        .collect()
}

fn enqueue(
    runtime: &mut ConsolidationJobRuntime,
    workspace_id: &str,
    job_id: String,
    proposal: PendingProposalSpec,
) -> Result<(), ScanError> {
    runtime
        .submit(crate::consolidation::ConsolidationJobSpec {
            job_id,
            workspace_id: workspace_id.to_string(),
            kind: "detect duplicate memories".to_string(),
            mode: ConsolidationJobMode::Background,
            proposal: Some(proposal),
        })
        .map_err(ScanError::from)
        .map(|_| ())
}
