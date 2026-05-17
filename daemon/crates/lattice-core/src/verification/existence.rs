use std::collections::HashMap;
use std::time::Instant;

use rusqlite::{params, OptionalExtension};
use tracing::{debug_span, field, info, info_span, warn};

use super::{
    ScopeEnforcement, ScopeFilter, SpanReader, SpanValidationError, SpanValidator,
    VerificationStatus, VerificationVerdict,
};
use crate::consolidation::{
    capture_memory_state, encode_memory_state, now_unix_micros, ConsolidationJobMode,
    ConsolidationJobRuntime, PendingProposalSpec, ProposalKind,
};
use crate::error::LatticeError;
use crate::events::BranchRef;
use crate::graph::CodeGraph;
use crate::identity::{
    decode_identity, DocId, FileId, Identity, IdentityResolver, ResolveOutcome, SectionId, SymbolId,
};
use crate::memory::{MemoryEvidence, MemoryStore, MemoryVerificationStatus};
use crate::storage::graph_store::FileIndexEntry;
use crate::symbols::ParsedFile;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifierBudget {
    pub max_memories: usize,
}

impl Default for VerifierBudget {
    fn default() -> Self {
        Self { max_memories: 32 }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerifierReport {
    pub verified: u32,
    pub stale: u32,
    pub invalidated: u32,
    pub expired: u32,
    pub elapsed_millis: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationOutcome {
    pub verdict: VerificationVerdict,
    pub proposal_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum VerificationError {
    #[error(transparent)]
    Storage(#[from] LatticeError),
    #[error(transparent)]
    Span(#[from] SpanValidationError),
    #[error("memory '{memory_id}' not found")]
    MissingMemory { memory_id: String },
    #[error("graph snapshot is unavailable for workspace '{workspace_id}'")]
    MissingGraphSnapshot { workspace_id: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VerificationJobState {
    Queued,
    Running,
    Completed,
    Failed,
}

impl VerificationJobState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct VerificationJobRecord {
    job_id: String,
    target_memory_id: String,
}

pub struct VerifierCore<'a> {
    store: &'a MemoryStore,
    runtime: &'a mut ConsolidationJobRuntime,
    graph: &'a CodeGraph,
    file_index: &'a HashMap<String, FileIndexEntry>,
    parsed_files: &'a HashMap<String, ParsedFile>,
    span_reader: &'a dyn SpanReader,
    workspace_id: &'a str,
}

impl<'a> VerifierCore<'a> {
    pub fn new(
        store: &'a MemoryStore,
        runtime: &'a mut ConsolidationJobRuntime,
        graph: &'a CodeGraph,
        file_index: &'a HashMap<String, FileIndexEntry>,
        parsed_files: &'a HashMap<String, ParsedFile>,
        span_reader: &'a dyn SpanReader,
        workspace_id: &'a str,
    ) -> Self {
        Self {
            store,
            runtime,
            graph,
            file_index,
            parsed_files,
            span_reader,
            workspace_id,
        }
    }

    pub fn verify_memory(
        &mut self,
        memory_id: &str,
    ) -> Result<VerificationVerdict, VerificationError> {
        Ok(self.verify_memory_with_outcome(memory_id)?.verdict)
    }

    pub fn verify_memory_with_outcome(
        &mut self,
        memory_id: &str,
    ) -> Result<VerificationOutcome, VerificationError> {
        let job = self.enqueue_job(memory_id)?;
        self.verify_job(job)
    }

    pub fn run_pending_jobs(
        &mut self,
        workspace_id: &str,
        budget: VerifierBudget,
    ) -> Result<VerifierReport, VerificationError> {
        let started = Instant::now();
        self.seed_jobs(workspace_id, budget.max_memories)?;
        let jobs = self.pending_jobs(workspace_id, budget.max_memories)?;
        let mut report = VerifierReport::default();

        for job in jobs {
            match self.verify_job(job) {
                Ok(outcome) => match outcome.verdict.status {
                    VerificationStatus::Verified => report.verified += 1,
                    VerificationStatus::Stale => report.stale += 1,
                    VerificationStatus::Invalidated => report.invalidated += 1,
                    VerificationStatus::Expired => report.expired += 1,
                    _ => {}
                },
                Err(error) => warn!(error = %error, "verification job failed"),
            }
        }

        report.elapsed_millis = started.elapsed().as_millis();
        info!(
            workspace_id,
            verified = report.verified,
            stale = report.stale,
            invalidated = report.invalidated,
            expired = report.expired,
            elapsed_millis = report.elapsed_millis,
            "verifier run completed"
        );
        Ok(report)
    }

    fn verify_job(
        &mut self,
        job: VerificationJobRecord,
    ) -> Result<VerificationOutcome, VerificationError> {
        self.ensure_graph_snapshot()?;
        self.mark_job_state(&job.job_id, VerificationJobState::Running, None, None)?;
        let result = self.verify_memory_inner(&job.target_memory_id);
        match &result {
            Ok(outcome) => {
                self.mark_job_state(
                    &job.job_id,
                    VerificationJobState::Completed,
                    Some(outcome.verdict.status),
                    Some(outcome.verdict.reason.as_str()),
                )?;
            }
            Err(error) => {
                self.mark_job_state(
                    &job.job_id,
                    VerificationJobState::Failed,
                    None,
                    Some(&error.to_string()),
                )?;
            }
        }
        result
    }

    fn verify_memory_inner(
        &mut self,
        memory_id: &str,
    ) -> Result<VerificationOutcome, VerificationError> {
        let memory =
            self.store
                .get_by_id(memory_id)?
                .ok_or_else(|| VerificationError::MissingMemory {
                    memory_id: memory_id.to_string(),
                })?;
        let prior_state = capture_memory_state(self.store, &memory)?;
        let evidence = prior_state.structured_fields.evidence.clone();
        let resolver = IdentityResolver::new(
            self.graph,
            self.file_index,
            self.parsed_files,
            self.workspace_id.to_string(),
            Vec::new(),
        );
        let span = info_span!(
            "verify_memory",
            memory_id = memory_id,
            evidence_count = evidence.len(),
            verdict = field::Empty
        );
        let _entered = span.enter();

        let mut verdicts = Vec::new();
        for (index, entry) in evidence.iter().enumerate() {
            let outcome = match entry.kind.as_str() {
                "file" => FileExistenceCheck::run(entry, &resolver, self.file_index),
                "symbol" => {
                    SymbolExistenceCheck::run(entry, &resolver, self.file_index, self.graph)
                }
                "doc_section" => DocSectionExistenceCheck::run(
                    entry,
                    &resolver,
                    self.file_index,
                    self.parsed_files,
                ),
                "test" => TestExistenceCheck::run(entry, &resolver, self.file_index, self.graph),
                _ => {
                    warn!(
                        kind = entry.kind.as_str(),
                        "unsupported verification evidence kind"
                    );
                    continue;
                }
            };
            match outcome {
                Ok(verdict) => {
                    let existence_ok = verdict.status == VerificationStatus::Verified;
                    verdicts.push(verdict);
                    if existence_ok {
                        let evidence_id = evidence_row_id(memory_id, index);
                        let span_verdict =
                            self.verify_span_evidence(memory_id, &evidence_id, entry)?;
                        verdicts.push(span_verdict);
                    }
                }
                Err(error) => {
                    warn!(memory_id, evidence_kind = entry.kind.as_str(), error = %error, "verification evidence failed");
                    verdicts.push(VerificationVerdict::new(
                        VerificationStatus::Unverified,
                        error.to_string(),
                    ));
                }
            }
        }

        verdicts.push(self.verify_scope(&memory));

        let verdict = aggregate_verdicts(&verdicts);
        span.record("verdict", verdict.status.as_str());
        let proposal_id = self.emit_proposal(memory_id, &prior_state, &verdict)?;
        Ok(VerificationOutcome {
            verdict,
            proposal_id,
        })
    }

    fn verify_span_evidence(
        &self,
        memory_id: &str,
        evidence_id: &str,
        evidence: &MemoryEvidence,
    ) -> Result<VerificationVerdict, VerificationError> {
        let Some(span) = evidence.span.as_ref() else {
            return Ok(VerificationVerdict::new(
                VerificationStatus::Verified,
                "span validation skipped because evidence has no exact span",
            ));
        };
        let span_guard = debug_span!(
            "validate_span",
            memory_id,
            file_id = %span.file_id,
            byte_start = span.byte_start,
            byte_end = span.byte_end,
            verdict = field::Empty
        );
        let verdict = {
            let _entered = span_guard.enter();
            SpanValidator::validate_with_reader(evidence_id, evidence, self.span_reader)?
        };
        span_guard.record("verdict", verdict.status.as_str());
        Ok(verdict)
    }

    fn emit_proposal(
        &mut self,
        memory_id: &str,
        prior_state: &crate::consolidation::ConsolidationMemoryState,
        verdict: &VerificationVerdict,
    ) -> Result<Option<String>, VerificationError> {
        let Some((proposal_kind, proposed_state)) = project_state(prior_state, verdict) else {
            return Ok(None);
        };
        let job_id = format!("verify-proposal-{}-{}", memory_id, now_unix_micros());
        let proposal_id = format!("verify-proposal-{}-{}", memory_id, now_unix_micros());
        let _ = self
            .runtime
            .submit_inline(crate::consolidation::ConsolidationJobSpec {
                job_id,
                workspace_id: self.workspace_id.to_string(),
                kind: "verify durable memory evidence".to_string(),
                mode: ConsolidationJobMode::Background,
                proposal: Some(PendingProposalSpec {
                    proposal_id: proposal_id.clone(),
                    target_memory_id: Some(memory_id.to_string()),
                    proposal_kind,
                    prior_state: encode_memory_state(prior_state),
                    proposed_state: encode_memory_state(&proposed_state),
                    evidence: serde_json::json!({
                        "source_memory_ids": [memory_id],
                        "verification_status": verdict.status.as_str(),
                        "reason": verdict.reason,
                    }),
                    provenance: None,
                }),
            })?;
        Ok(Some(proposal_id))
    }

    fn verify_scope(&self, memory: &crate::memory::Memory) -> VerificationVerdict {
        let filter = ScopeFilter {
            workspace_id: memory.workspace_id.clone().unwrap_or_default(),
            branch: memory.branch.clone().map(|name| BranchRef { name }),
            organization_id: memory.scope_organization_id.clone(),
            session_id: Some(memory.session_id.clone()),
        };
        ScopeEnforcement::audit_memory(memory, &filter)
    }

    fn ensure_graph_snapshot(&self) -> Result<(), VerificationError> {
        if self.workspace_id.trim().is_empty() {
            return Err(VerificationError::MissingGraphSnapshot {
                workspace_id: self.workspace_id.to_string(),
            });
        }
        Ok(())
    }

    fn enqueue_job(&mut self, memory_id: &str) -> Result<VerificationJobRecord, VerificationError> {
        let job = VerificationJobRecord {
            job_id: format!("verify-job-{}-{}", memory_id, now_unix_micros()),
            target_memory_id: memory_id.to_string(),
        };
        let queued_at = now_unix_micros();
        let conn = self.runtime.lock_conn()?;
        conn.execute(
            "INSERT INTO verification_jobs
                (job_id, workspace_id, target_memory_id, check_kind, status, verdict, reason, queued_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6)",
            params![
                job.job_id,
                self.workspace_id,
                job.target_memory_id,
                "existence",
                VerificationJobState::Queued.as_str(),
                queued_at,
            ],
        )
        .map_err(|error| LatticeError::Storage(format!("Failed to enqueue verification job: {error}")))?;
        Ok(job)
    }

    fn seed_jobs(&mut self, workspace_id: &str, max_jobs: usize) -> Result<(), VerificationError> {
        if max_jobs == 0 {
            return Ok(());
        }
        let memories = self.store.list_workspace_memories(workspace_id)?;
        for memory in memories.into_iter().take(max_jobs) {
            if self.has_active_job(&memory.id)? {
                continue;
            }
            let _ = self.enqueue_job(&memory.id)?;
        }
        Ok(())
    }

    fn has_active_job(&mut self, memory_id: &str) -> Result<bool, VerificationError> {
        let conn = self.runtime.lock_conn()?;
        let status = conn
            .query_row(
                "SELECT 1
                 FROM verification_jobs
                 WHERE workspace_id = ?1
                   AND target_memory_id = ?2
                   AND status IN ('queued', 'running')
                 LIMIT 1",
                params![self.workspace_id, memory_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|error| {
                LatticeError::Storage(format!("Failed to inspect verification queue: {error}"))
            })?;
        Ok(status.is_some())
    }

    fn pending_jobs(
        &mut self,
        workspace_id: &str,
        limit: usize,
    ) -> Result<Vec<VerificationJobRecord>, VerificationError> {
        let conn = self.runtime.lock_conn()?;
        let mut statement = conn
            .prepare(
                "SELECT job_id, target_memory_id
                 FROM verification_jobs
                 WHERE workspace_id = ?1
                   AND status = 'queued'
                 ORDER BY queued_at ASC
                 LIMIT ?2",
            )
            .map_err(|error| {
                LatticeError::Storage(format!("Failed to prepare verification job query: {error}"))
            })?;
        let rows = statement
            .query_map(params![workspace_id, limit as i64], |row| {
                Ok(VerificationJobRecord {
                    job_id: row.get(0)?,
                    target_memory_id: row.get(1)?,
                })
            })
            .map_err(|error| {
                LatticeError::Storage(format!("Failed to query verification jobs: {error}"))
            })?;
        let mut jobs = Vec::new();
        for row in rows {
            jobs.push(row.map_err(|error| {
                LatticeError::Storage(format!("Failed to decode verification job row: {error}"))
            })?);
        }
        Ok(jobs)
    }

    fn mark_job_state(
        &mut self,
        job_id: &str,
        state: VerificationJobState,
        verdict: Option<VerificationStatus>,
        reason: Option<&str>,
    ) -> Result<(), VerificationError> {
        let now = now_unix_micros();
        let (started_at, finished_at) = match state {
            VerificationJobState::Queued => (None, None),
            VerificationJobState::Running => (Some(now), None),
            VerificationJobState::Completed | VerificationJobState::Failed => (None, Some(now)),
        };
        let conn = self.runtime.lock_conn()?;
        conn.execute(
            "UPDATE verification_jobs
             SET status = ?1,
                 verdict = COALESCE(?2, verdict),
                 reason = ?3,
                 started_at = COALESCE(?4, started_at),
                 finished_at = COALESCE(?5, finished_at)
             WHERE job_id = ?6",
            params![
                state.as_str(),
                verdict.map(VerificationStatus::as_str),
                reason,
                started_at,
                finished_at,
                job_id,
            ],
        )
        .map_err(|error| {
            LatticeError::Storage(format!("Failed to update verification job state: {error}"))
        })?;
        Ok(())
    }
}

pub struct FileExistenceCheck;

impl FileExistenceCheck {
    pub fn run(
        evidence: &MemoryEvidence,
        resolver: &IdentityResolver<'_>,
        file_index: &HashMap<String, FileIndexEntry>,
    ) -> Result<VerificationVerdict, VerificationError> {
        let reference = required_reference(evidence, "file")?;
        let file_id = match decode_identity(reference).ok() {
            Some(Identity::File(file_id)) => file_id,
            _ => FileId {
                workspace_id: resolver.default_workspace_id().clone(),
                repo_relative_path: reference.to_string(),
                content_hash: "00000000".to_string(),
            },
        };
        resolve_file_verdict(evidence, &file_id, file_index)
    }
}

pub struct SymbolExistenceCheck;

impl SymbolExistenceCheck {
    pub fn run(
        evidence: &MemoryEvidence,
        resolver: &IdentityResolver<'_>,
        file_index: &HashMap<String, FileIndexEntry>,
        graph: &CodeGraph,
    ) -> Result<VerificationVerdict, VerificationError> {
        let reference = required_reference(evidence, "symbol")?;
        let symbol_id = match resolver.resolve_symbol(resolver.default_workspace_id(), reference) {
            ResolveOutcome::Unique(symbol_id) => symbol_id,
            ResolveOutcome::Ambiguous(report) => {
                return Ok(VerificationVerdict::new(
                    VerificationStatus::Invalidated,
                    report.to_string(),
                ));
            }
            ResolveOutcome::NotFound(error) => {
                return Ok(VerificationVerdict::new(
                    VerificationStatus::Invalidated,
                    error.to_string(),
                ));
            }
        };
        if graph.get_node(&legacy_symbol_id(&symbol_id)).is_none() {
            return Ok(VerificationVerdict::new(
                VerificationStatus::Invalidated,
                format!("symbol `{}` no longer resolves", symbol_id),
            ));
        }
        stale_or_verified(
            evidence,
            &symbol_id.file.repo_relative_path,
            file_index,
            format!("symbol `{}` exists", symbol_id),
        )
    }
}

pub struct DocSectionExistenceCheck;

impl DocSectionExistenceCheck {
    pub fn run(
        evidence: &MemoryEvidence,
        _resolver: &IdentityResolver<'_>,
        file_index: &HashMap<String, FileIndexEntry>,
        parsed_files: &HashMap<String, ParsedFile>,
    ) -> Result<VerificationVerdict, VerificationError> {
        let section_id = section_identity(evidence)?;
        let path = section_id.doc.repo_relative_path.clone();
        let heading = section_id.heading_path.last().cloned().unwrap_or_default();
        let Some(parsed) = parsed_files.get(&path) else {
            return Ok(VerificationVerdict::new(
                VerificationStatus::Invalidated,
                format!("document section `{}` no longer resolves", section_id),
            ));
        };
        let exists = parsed.symbols.iter().any(|symbol| {
            symbol.kind == crate::symbols::SymbolKind::Section
                && symbol.name == heading
                && symbol.id.byte_offset == section_id.byte_offset
        });
        if !exists {
            return Ok(VerificationVerdict::new(
                VerificationStatus::Invalidated,
                format!("document section `{}` no longer resolves", section_id),
            ));
        }
        stale_or_verified(
            evidence,
            &path,
            file_index,
            format!("document section `{}` exists", section_id),
        )
    }
}

pub struct TestExistenceCheck;

impl TestExistenceCheck {
    pub fn run(
        evidence: &MemoryEvidence,
        resolver: &IdentityResolver<'_>,
        file_index: &HashMap<String, FileIndexEntry>,
        graph: &CodeGraph,
    ) -> Result<VerificationVerdict, VerificationError> {
        let reference = required_reference(evidence, "test")?;
        let symbol_id = match resolver.resolve_test(resolver.default_workspace_id(), reference) {
            ResolveOutcome::Unique(symbol_id) => symbol_id,
            ResolveOutcome::Ambiguous(report) => {
                return Ok(VerificationVerdict::new(
                    VerificationStatus::Invalidated,
                    report.to_string(),
                ));
            }
            ResolveOutcome::NotFound(_) => {
                if let Some(identity) = decode_identity(reference).ok() {
                    if let Identity::Symbol(symbol_id) = identity {
                        if graph.get_node(&legacy_symbol_id(&symbol_id)).is_some() {
                            return stale_or_verified(
                                evidence,
                                &symbol_id.file.repo_relative_path,
                                file_index,
                                format!("test `{}` exists", symbol_id),
                            );
                        }
                    }
                }
                return Ok(VerificationVerdict::new(
                    VerificationStatus::Invalidated,
                    format!("test `{reference}` no longer resolves"),
                ));
            }
        };
        if graph.get_node(&legacy_symbol_id(&symbol_id)).is_none() {
            return Ok(VerificationVerdict::new(
                VerificationStatus::Invalidated,
                format!("test `{}` no longer resolves", symbol_id),
            ));
        }
        stale_or_verified(
            evidence,
            &symbol_id.file.repo_relative_path,
            file_index,
            format!("test `{}` exists", symbol_id),
        )
    }
}

fn aggregate_verdicts(verdicts: &[VerificationVerdict]) -> VerificationVerdict {
    if verdicts.is_empty() {
        return VerificationVerdict::new(
            VerificationStatus::Verified,
            "memory has no existence evidence to verify",
        );
    }
    if let Some(verdict) = verdicts
        .iter()
        .find(|verdict| verdict.status == VerificationStatus::Invalidated)
    {
        return verdict.clone();
    }
    if let Some(verdict) = verdicts
        .iter()
        .find(|verdict| verdict.status == VerificationStatus::Stale)
    {
        return verdict.clone();
    }
    if verdicts
        .iter()
        .all(|verdict| verdict.status == VerificationStatus::Verified)
    {
        return VerificationVerdict::new(
            VerificationStatus::Verified,
            "all existence evidence still resolves",
        );
    }
    VerificationVerdict::new(
        VerificationStatus::Unverified,
        "verification completed with non-deterministic evidence results",
    )
}

fn project_state(
    prior_state: &crate::consolidation::ConsolidationMemoryState,
    verdict: &VerificationVerdict,
) -> Option<(ProposalKind, crate::consolidation::ConsolidationMemoryState)> {
    let mut proposed_state = prior_state.clone();
    proposed_state.last_verified_at = Some((now_unix_micros() / 1_000_000) as u64);
    proposed_state.memory.is_stale = verdict.status == VerificationStatus::Stale;
    proposed_state.memory.stale_reason = match verdict.status {
        VerificationStatus::Stale => Some(verdict.reason.clone()),
        _ => None,
    };
    proposed_state.structured_fields.verification_status = map_memory_status(verdict.status);
    let proposal_kind = match verdict.status {
        VerificationStatus::Verified => ProposalKind::MarkVerified,
        VerificationStatus::Stale => ProposalKind::MarkStale,
        VerificationStatus::Invalidated => ProposalKind::MarkInvalidated,
        _ => return None,
    };
    Some((proposal_kind, proposed_state))
}

fn map_memory_status(status: VerificationStatus) -> MemoryVerificationStatus {
    match status {
        VerificationStatus::Verified => MemoryVerificationStatus::Verified,
        VerificationStatus::Unverified => MemoryVerificationStatus::Unverified,
        VerificationStatus::InReview => MemoryVerificationStatus::InReview,
        VerificationStatus::Stale => MemoryVerificationStatus::Stale,
        VerificationStatus::Contradicted => MemoryVerificationStatus::Contradicted,
        VerificationStatus::Superseded => MemoryVerificationStatus::Superseded,
        VerificationStatus::Expired => MemoryVerificationStatus::Expired,
        VerificationStatus::Invalidated => MemoryVerificationStatus::Invalidated,
    }
}

fn required_reference<'a>(
    evidence: &'a MemoryEvidence,
    kind: &'static str,
) -> Result<&'a str, VerificationError> {
    evidence
        .reference
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| LatticeError::Storage(format!("missing {kind} evidence reference")).into())
}

fn evidence_row_id(memory_id: &str, index: usize) -> String {
    format!("{memory_id}:evidence:{index}")
}

fn resolve_file_verdict(
    evidence: &MemoryEvidence,
    file_id: &FileId,
    file_index: &HashMap<String, FileIndexEntry>,
) -> Result<VerificationVerdict, VerificationError> {
    if !file_index.contains_key(&file_id.repo_relative_path) {
        return Ok(VerificationVerdict::new(
            VerificationStatus::Invalidated,
            format!("file `{}` no longer exists", file_id.repo_relative_path),
        ));
    }
    stale_or_verified(
        evidence,
        &file_id.repo_relative_path,
        file_index,
        format!("file `{}` exists", file_id.repo_relative_path),
    )
}

fn stale_or_verified(
    evidence: &MemoryEvidence,
    path: &str,
    file_index: &HashMap<String, FileIndexEntry>,
    verified_reason: String,
) -> Result<VerificationVerdict, VerificationError> {
    let Some(entry) = file_index.get(path) else {
        return Ok(VerificationVerdict::new(
            VerificationStatus::Invalidated,
            format!("path `{path}` is no longer indexed"),
        ));
    };
    if let Some(captured_at) = evidence.captured_at {
        let snapshot_age = entry.last_indexed_at.max(0) as u64;
        if snapshot_age < captured_at {
            return Ok(VerificationVerdict::new(
                VerificationStatus::Stale,
                format!(
                    "graph snapshot for `{path}` ({snapshot_age}) predates evidence capture ({captured_at})"
                ),
            ));
        }
    }
    Ok(VerificationVerdict::new(
        VerificationStatus::Verified,
        verified_reason,
    ))
}

fn legacy_symbol_id(symbol_id: &SymbolId) -> crate::symbols::SymbolId {
    crate::symbols::SymbolId {
        file: symbol_id.file.repo_relative_path.clone(),
        name: symbol_id.qualified_name.clone(),
        byte_offset: symbol_id.byte_offset,
    }
}

fn section_identity(evidence: &MemoryEvidence) -> Result<SectionId, VerificationError> {
    let reference = required_reference(evidence, "doc_section")?;
    if let Some(Identity::Section(section_id)) = decode_identity(reference).ok() {
        return Ok(section_id);
    }
    let (path, heading) = reference.split_once('#').ok_or_else(|| {
        LatticeError::Storage(format!(
            "doc_section evidence `{reference}` must be a stable section identity or path#heading"
        ))
    })?;
    Ok(SectionId {
        doc: DocId {
            workspace_id: "workspace-main".to_string(),
            repo_relative_path: path.to_string(),
            content_hash: "00000000".to_string(),
        },
        heading_path: vec![heading.to_string()],
        byte_offset: 0,
    })
}
