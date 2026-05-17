//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 8. Verification Engine`:
//!
//! Verification checks:
//!
//! - linked files still exist
//! - linked symbols still exist
//! - cited docs still exist
//! - linked tests still exist
//! - evidence text still matches when exact spans were captured
//! - implementation still matches memory claim where deterministic checks are possible
//! - contradicted/superseded states remain coherent
//! - branch-scoped memory is not leaking into unrelated branches
//! - time-bound memory has expired
//!
//! Verification outputs:
//!
//! - `verified`
//! - `unverified`
//! - `in_review`
//! - `stale`
//! - `contradicted`
//! - `superseded`
//! - `expired`
//! - `invalidated`
//!
//! Verification must be incremental and tied to workspace changes. Large repos
//! cannot tolerate full rescans for every memory.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use serde_json::json;
use tracing::{field, info_span};

use super::{ExpiryError, ExpiryScanner, SpanReader, VerificationStatus, VerifierCore};
use crate::consolidation::{
    capture_memory_state, encode_memory_state, now_unix_micros, ConsolidationJobMode,
    ConsolidationJobRuntime, PendingProposalSpec, ProposalDecision, ProposalKind,
};
use crate::error::LatticeError;
use crate::events::EventWriter;
use crate::graph::CodeGraph;
use crate::identity::{FileId, OperatorId};
use crate::memory::{MemoryStore, MemoryVerificationStatus};
use crate::storage::graph_store::FileIndexEntry;
use crate::symbols::ParsedFile;
use crate::{DateTime, Utc};

pub trait VerificationObserver {
    fn on_verify(&self, memory_id: &str);
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IncrementalReport {
    pub impacted: usize,
    pub verified: u32,
    pub stale: u32,
    pub invalidated: u32,
    pub expired: u32,
    pub queued_for_retry: usize,
    pub elapsed_millis: u128,
}

#[derive(Debug, thiserror::Error)]
pub enum IncrementalError {
    #[error(transparent)]
    Storage(#[from] LatticeError),
    #[error(transparent)]
    Verification(#[from] super::VerificationError),
    #[error(transparent)]
    Expiry(#[from] ExpiryError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingVerification {
    memory_id: String,
    new_snapshot_id: u64,
}

pub struct IncrementalVerifier<'a> {
    store: &'a MemoryStore,
    runtime: &'a mut ConsolidationJobRuntime,
    graph: &'a CodeGraph,
    file_index: &'a HashMap<String, FileIndexEntry>,
    parsed_files: &'a HashMap<String, ParsedFile>,
    span_reader: &'a dyn SpanReader,
    event_writer: &'a EventWriter,
    decided_by: &'a OperatorId,
    workspace_id: &'a str,
    work_budget: usize,
    observer: Option<&'a dyn VerificationObserver>,
    pending: VecDeque<PendingVerification>,
}

impl<'a> IncrementalVerifier<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: &'a MemoryStore,
        runtime: &'a mut ConsolidationJobRuntime,
        graph: &'a CodeGraph,
        file_index: &'a HashMap<String, FileIndexEntry>,
        parsed_files: &'a HashMap<String, ParsedFile>,
        span_reader: &'a dyn SpanReader,
        event_writer: &'a EventWriter,
        decided_by: &'a OperatorId,
        workspace_id: &'a str,
        work_budget: usize,
    ) -> Self {
        Self {
            store,
            runtime,
            graph,
            file_index,
            parsed_files,
            span_reader,
            event_writer,
            decided_by,
            workspace_id,
            work_budget,
            observer: None,
            pending: VecDeque::new(),
        }
    }

    pub fn with_observer(mut self, observer: &'a dyn VerificationObserver) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn on_graph_delta(
        &mut self,
        prior_snapshot_id: u64,
        new_snapshot_id: u64,
        changed_files: Vec<FileId>,
    ) -> Result<IncrementalReport, IncrementalError> {
        let started = Instant::now();
        let changed_file_paths = changed_file_paths(&changed_files);
        let changed_symbols = changed_symbols(self.graph, &changed_file_paths);
        let delta_impacted = self.store.find_impacted_memory_ids_for_graph_delta(
            self.workspace_id,
            &changed_file_paths,
            &changed_symbols,
        )?;
        #[cfg(debug_assertions)]
        {
            let row_count = self.store.count_memory_evidence_rows_for_graph_delta(
                self.workspace_id,
                &changed_file_paths,
                &changed_symbols,
            )?;
            debug_assert!(delta_impacted.len() <= row_count);
        }

        self.enqueue_impacted(delta_impacted, new_snapshot_id);
        let impacted = self.pending.len();
        let span = info_span!(
            "incremental_verification",
            workspace_id = self.workspace_id,
            prior_snapshot = prior_snapshot_id,
            new_snapshot = new_snapshot_id,
            impacted = impacted,
            verified = field::Empty,
            stale = field::Empty,
            invalidated = field::Empty,
            expired = field::Empty,
            elapsed_ms = field::Empty
        );
        let _entered = span.enter();

        let mut report = IncrementalReport {
            impacted,
            ..IncrementalReport::default()
        };
        let budget = self.work_budget.min(self.pending.len());
        for _ in 0..budget {
            let Some(task) = self.pending.pop_front() else {
                break;
            };
            self.process_memory(task, &mut report)?;
        }

        report.queued_for_retry = self.pending.len();
        report.elapsed_millis = started.elapsed().as_millis();
        span.record("verified", report.verified);
        span.record("stale", report.stale);
        span.record("invalidated", report.invalidated);
        span.record("expired", report.expired);
        span.record("elapsed_ms", report.elapsed_millis as i64);
        Ok(report)
    }

    pub fn run_pending_expiry(
        &mut self,
        workspace_id: &str,
        now: DateTime<Utc>,
    ) -> Result<super::expiry::ExpiryReport, IncrementalError> {
        let mut scanner =
            ExpiryScanner::new(self.store, self.runtime, self.event_writer, self.decided_by);
        Ok(scanner.scan(workspace_id, now)?)
    }

    fn enqueue_impacted(&mut self, memory_ids: Vec<String>, new_snapshot_id: u64) {
        let mut seen = self
            .pending
            .iter()
            .map(|item| item.memory_id.clone())
            .collect::<HashSet<_>>();
        for memory_id in memory_ids {
            if !seen.insert(memory_id.clone()) {
                continue;
            }
            self.pending.push_back(PendingVerification {
                memory_id,
                new_snapshot_id,
            });
        }
    }

    fn process_memory(
        &mut self,
        task: PendingVerification,
        report: &mut IncrementalReport,
    ) -> Result<(), IncrementalError> {
        if let Some(observer) = self.observer {
            observer.on_verify(&task.memory_id);
        }
        if self.try_expire(&task.memory_id, task.new_snapshot_id)? {
            report.expired += 1;
            return Ok(());
        }

        let mut verifier = VerifierCore::new(
            self.store,
            self.runtime,
            self.graph,
            self.file_index,
            self.parsed_files,
            self.span_reader,
            self.workspace_id,
        );
        let outcome = verifier.verify_memory_with_outcome(&task.memory_id)?;
        if let Some(proposal_id) = outcome.proposal_id.as_deref() {
            self.apply_proposal(proposal_id)?;
        }
        match outcome.verdict.status {
            VerificationStatus::Verified => {
                self.refresh_verified_memory(&task.memory_id, task.new_snapshot_id)?;
                report.verified += 1;
            }
            VerificationStatus::Stale => report.stale += 1,
            VerificationStatus::Invalidated => report.invalidated += 1,
            VerificationStatus::Expired => report.expired += 1,
            _ => {}
        }
        Ok(())
    }

    fn try_expire(
        &mut self,
        memory_id: &str,
        new_snapshot_id: u64,
    ) -> Result<bool, IncrementalError> {
        let Some(expires_at) = self.store.get_expires_at(memory_id)? else {
            return Ok(false);
        };
        if expires_at.timestamp() > Utc::now().timestamp() {
            return Ok(false);
        }
        let mut scanner =
            ExpiryScanner::new(self.store, self.runtime, self.event_writer, self.decided_by);
        if scanner
            .expire_memory(memory_id, self.workspace_id, Utc::now())?
            .is_some()
        {
            self.bump_graph_snapshot_after_expiry(memory_id, new_snapshot_id)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn bump_graph_snapshot_after_expiry(
        &mut self,
        memory_id: &str,
        new_snapshot_id: u64,
    ) -> Result<(), IncrementalError> {
        let Some(memory) = self.store.get_by_id(memory_id)? else {
            return Ok(());
        };
        let prior_state = capture_memory_state(self.store, &memory)?;
        let mut proposed_state = prior_state.clone();
        proposed_state.last_verified_graph_snapshot_id = Some(new_snapshot_id);
        let proposal_id =
            self.submit_refresh(memory_id, &prior_state, &proposed_state, new_snapshot_id)?;
        self.apply_proposal(&proposal_id)?;
        Ok(())
    }

    fn refresh_verified_memory(
        &mut self,
        memory_id: &str,
        new_snapshot_id: u64,
    ) -> Result<(), IncrementalError> {
        let memory = self
            .store
            .get_by_id(memory_id)?
            .ok_or_else(|| LatticeError::Storage(format!("Memory '{}' not found", memory_id)))?;
        let prior_state = capture_memory_state(self.store, &memory)?;
        let mut proposed_state = prior_state.clone();
        proposed_state.last_verified_at = Some(Utc::now().timestamp().max(0) as u64);
        proposed_state.last_verified_graph_snapshot_id = Some(new_snapshot_id);
        proposed_state.structured_fields.verification_status = MemoryVerificationStatus::Verified;
        let proposal_id =
            self.submit_refresh(memory_id, &prior_state, &proposed_state, new_snapshot_id)?;
        self.apply_proposal(&proposal_id)?;
        Ok(())
    }

    fn submit_refresh(
        &mut self,
        memory_id: &str,
        prior_state: &crate::consolidation::ConsolidationMemoryState,
        proposed_state: &crate::consolidation::ConsolidationMemoryState,
        new_snapshot_id: u64,
    ) -> Result<String, IncrementalError> {
        let job_id = format!("verify-refresh-{}-{}", memory_id, now_unix_micros());
        let proposal_id = format!(
            "verify-refresh-proposal-{}-{}",
            memory_id,
            now_unix_micros()
        );
        let _ = self
            .runtime
            .submit_inline(crate::consolidation::ConsolidationJobSpec {
                job_id,
                workspace_id: self.workspace_id.to_string(),
                kind: "refresh verified memory after incremental verification".to_string(),
                mode: ConsolidationJobMode::Background,
                proposal: Some(PendingProposalSpec {
                    proposal_id: proposal_id.clone(),
                    target_memory_id: Some(memory_id.to_string()),
                    proposal_kind: ProposalKind::Refresh,
                    prior_state: encode_memory_state(prior_state),
                    proposed_state: encode_memory_state(proposed_state),
                    evidence: json!({
                        "source_memory_ids": [memory_id],
                        "last_verified_graph_snapshot_id": new_snapshot_id,
                    }),
                    provenance: None,
                }),
            })?;
        Ok(proposal_id)
    }

    fn apply_proposal(&self, proposal_id: &str) -> Result<(), IncrementalError> {
        let _ = self.runtime.decide(
            proposal_id,
            ProposalDecision::Applied,
            self.store,
            self.event_writer,
            self.decided_by.value.as_str(),
        )?;
        Ok(())
    }
}

fn changed_file_paths(changed_files: &[FileId]) -> Vec<String> {
    let mut paths = changed_files
        .iter()
        .map(|file_id| file_id.repo_relative_path.clone())
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

fn changed_symbols(graph: &CodeGraph, changed_file_paths: &[String]) -> Vec<String> {
    let changed = changed_file_paths.iter().cloned().collect::<HashSet<_>>();
    let mut symbols = graph
        .all_nodes()
        .into_iter()
        .filter(|node| changed.contains(node.file.as_str()))
        .map(|node| node.name.clone())
        .collect::<Vec<_>>();
    symbols.sort();
    symbols.dedup();
    symbols
}
