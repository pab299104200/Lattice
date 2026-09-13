//! Deterministic consolidation job for "refresh memories whose evidence still matches current code".
//!
//! This implements the `## 6. Consolidation Engine` consolidation-jobs bullet
//! "refresh memories whose evidence still matches current code" from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`.

use std::collections::BTreeSet;
use std::time::Instant;

use serde_json::json;
use tracing::info_span;

use super::{
    capture_memory_state, encode_memory_state, ConsolidationJobMode, ConsolidationJobRuntime,
    PendingProposalSpec, ProposalKind, ScanError, ScanReport,
};
use crate::graph::CodeGraph;
use crate::memory::MemoryStore;

pub struct RefreshScanner<'a> {
    store: &'a MemoryStore,
    runtime: &'a mut ConsolidationJobRuntime,
    graph: &'a CodeGraph,
    refresh_interval_secs: u64,
}

impl<'a> RefreshScanner<'a> {
    pub fn new(
        store: &'a MemoryStore,
        runtime: &'a mut ConsolidationJobRuntime,
        graph: &'a CodeGraph,
        refresh_interval_secs: u64,
    ) -> Self {
        Self {
            store,
            runtime,
            graph,
            refresh_interval_secs,
        }
    }

    pub fn scan(
        &mut self,
        authority: &super::EvolutionAuthority<'_>,
        now: u64,
    ) -> Result<ScanReport, ScanError> {
        let workspace_id = authority.repository_id;
        let started = Instant::now();
        let span = info_span!(
            "consolidation.refresh.scan",
            workspace_id,
            kind = "refresh memories whose evidence still matches current code"
        );
        let _entered = span.enter();
        let file_set = self
            .graph
            .all_nodes()
            .into_iter()
            .map(|node| node.file.clone())
            .collect::<BTreeSet<_>>();
        let symbol_set = self
            .graph
            .all_nodes()
            .into_iter()
            .map(|node| node.name.clone())
            .collect::<BTreeSet<_>>();

        let memories = self.store.list_workspace_memories(workspace_id)?;
        let mut proposals_enqueued = 0u32;
        let mut skipped = 0u32;

        for memory in memories {
            let last_verified_at = self.store.get_last_verified_at(&memory.id)?.unwrap_or(0);
            let age = now.saturating_sub(last_verified_at);
            if age < self.refresh_interval_secs {
                skipped += 1;
                continue;
            }
            let evidence_matches = memory
                .linked_files
                .iter()
                .all(|file| file_set.contains(file))
                && memory
                    .linked_symbols
                    .iter()
                    .all(|symbol| symbol_set.contains(symbol));
            if !evidence_matches {
                skipped += 1;
                continue;
            }

            let prior_state = capture_memory_state(self.store, &memory)?;
            let mut proposed_state = prior_state.clone();
            proposed_state.last_verified_at = Some(now);

            self.runtime
                .submit(crate::consolidation::ConsolidationJobSpec {
                    job_id: format!("refresh-{}", memory.id),
                    workspace_id: workspace_id.to_string(),
                    kind: "refresh memories whose evidence still matches current code".to_string(),
                    mode: ConsolidationJobMode::Background,
                    proposal: Some(PendingProposalSpec {
                        proposal_id: format!("refresh-proposal-{}", memory.id),
                        target_memory_id: Some(memory.id.clone()),
                        proposal_kind: ProposalKind::Refresh,
                        prior_state: encode_memory_state(&prior_state),
                        proposed_state: encode_memory_state(&proposed_state),
                        evidence: json!({
                            "source_memory_ids": [memory.id.clone()],
                            "last_verified_at": last_verified_at,
                            "refreshed_at": now,
                        }),
                        provenance: None,
                    }),
                })?;
            proposals_enqueued += 1;
        }

        let _ = self.runtime.run_due(self.store, authority)?;
        Ok(ScanReport::from_counts(
            proposals_enqueued,
            skipped,
            started,
        ))
    }
}
