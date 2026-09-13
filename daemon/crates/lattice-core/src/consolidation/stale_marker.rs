//! Deterministic consolidation job for "mark stale memories after graph changes".
//!
//! This implements the `## 6. Consolidation Engine` consolidation-jobs bullet
//! "mark stale memories after graph changes" from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`.

use std::collections::BTreeSet;
use std::time::Instant;

use serde_json::json;
use tracing::info_span;

use super::{
    capture_memory_state, encode_memory_state, mark_state_stale, ConsolidationJobMode,
    ConsolidationJobRuntime, PendingProposalSpec, ProposalKind, ScanError, ScanReport,
};
use crate::graph::CodeGraph;
use crate::memory::MemoryStore;

pub struct StaleMarker<'a> {
    store: &'a MemoryStore,
    runtime: &'a mut ConsolidationJobRuntime,
    graph: &'a CodeGraph,
    workspace_id: &'a str,
}

impl<'a> StaleMarker<'a> {
    pub fn new(
        store: &'a MemoryStore,
        runtime: &'a mut ConsolidationJobRuntime,
        graph: &'a CodeGraph,
        workspace_id: &'a str,
    ) -> Self {
        Self {
            store,
            runtime,
            graph,
            workspace_id,
        }
    }

    pub fn on_graph_change(
        &mut self,
        authority: &super::EvolutionAuthority<'_>,
        changed_files: Vec<String>,
    ) -> Result<ScanReport, ScanError> {
        if authority.repository_id != self.workspace_id {
            return Err(ScanError::Storage(crate::LatticeError::Storage(
                "stale marker authority repository mismatch".into(),
            )));
        }
        let started = Instant::now();
        let span = info_span!(
            "consolidation.stale_marker.scan",
            workspace_id = self.workspace_id,
            kind = "mark stale memories after graph changes"
        );
        let _entered = span.enter();
        let changed_files = changed_files.into_iter().collect::<BTreeSet<_>>();
        let changed_symbols = self
            .graph
            .all_nodes()
            .into_iter()
            .filter(|node| changed_files.contains(node.file.as_str()))
            .map(|node| node.name.clone())
            .collect::<BTreeSet<_>>();
        let memories = self.store.list_workspace_memories(self.workspace_id)?;
        let mut proposals_enqueued = 0u32;
        let mut skipped = 0u32;

        for memory in memories {
            let file_match = memory
                .linked_files
                .iter()
                .any(|file| changed_files.contains(file.as_str()));
            let symbol_match = memory
                .linked_symbols
                .iter()
                .any(|symbol| changed_symbols.contains(symbol.as_str()));
            if !file_match && !symbol_match {
                skipped += 1;
                continue;
            }

            let prior_state = capture_memory_state(self.store, &memory)?;
            let reason = if file_match {
                format!("graph changed for {}", memory.linked_files.join(", "))
            } else {
                format!("graph changed for {}", memory.linked_symbols.join(", "))
            };
            let proposed_state = mark_state_stale(&prior_state, reason.clone());

            self.runtime
                .submit(crate::consolidation::ConsolidationJobSpec {
                    job_id: format!("stale-marker-{}", memory.id),
                    workspace_id: self.workspace_id.to_string(),
                    kind: "mark stale memories after graph changes".to_string(),
                    mode: ConsolidationJobMode::Background,
                    proposal: Some(PendingProposalSpec {
                        proposal_id: format!("stale-marker-proposal-{}", memory.id),
                        target_memory_id: Some(memory.id.clone()),
                        proposal_kind: ProposalKind::MarkStale,
                        prior_state: encode_memory_state(&prior_state),
                        proposed_state: encode_memory_state(&proposed_state),
                        evidence: json!({
                            "source_memory_ids": [memory.id.clone()],
                            "changed_files": changed_files.iter().cloned().collect::<Vec<_>>(),
                            "changed_symbols": changed_symbols.iter().cloned().collect::<Vec<_>>(),
                            "staleness_reason": reason,
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
