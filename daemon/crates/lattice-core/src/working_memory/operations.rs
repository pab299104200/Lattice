//! Working-memory operations for the cognitive workspace.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 5. Working Memory` requires explicit `retrieve`, `summarize`,
//! `filter`, `pin`, `evict`, `expand`, `compress`, and `checkpoint`
//! operations with auditable excluded-memory tracking.

use std::collections::{BTreeSet, HashMap};

use rusqlite::Connection;
use serde::Serialize;
use tokio::runtime::Runtime;

use crate::error::LatticeError;
use crate::events::EventEnvelope;
use crate::graph::CodeGraph;
use crate::identity::{decode_identity, Identity, IdentityResolver};
use crate::memory::MemoryStore;
use crate::retrieval_v1::shaper::{compress_bundle_results, shape_retrieval_bundle_detailed};
use crate::retrieval_v1::{
    classify_intent, extract_anchors, resolve_anchors, retrieve_candidates, schema::BudgetReport,
    schema::BundleResult, schema::RetrievalBundle, score_candidates, DiagnosticMode,
    RankingDiagnostics, RetrievalBudget, RetrievalContext, RetrievalProfile, ScoringContext,
    ScoringWeights, ShaperBudget, ShaperContext,
};
use crate::storage::vector_index::VectorIndex;

use super::state::{
    save_checkpoint, state_hash, BudgetDecisions, CheckpointId, ExcludedMemory, FileIdentity,
    SymbolIdentity, WorkingMemoryState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkingMemoryOp {
    Retrieve,
    Summarize,
    Filter,
    Pin,
    Evict,
    Expand,
    Compress,
    Checkpoint,
}

impl WorkingMemoryOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Retrieve => "retrieve",
            Self::Summarize => "summarize",
            Self::Filter => "filter",
            Self::Pin => "pin",
            Self::Evict => "evict",
            Self::Expand => "expand",
            Self::Compress => "compress",
            Self::Checkpoint => "checkpoint",
        }
    }
}

pub trait FilterPredicate {
    fn description(&self) -> &str;
    fn matches(&self, result: &BundleResult) -> bool;
}

pub trait StateMutationObserver {
    fn record(&self, mutation: StateMutationRecord) -> Result<(), LatticeError>;
}

pub trait WorkingMemoryRetriever {
    fn retrieve(
        &self,
        state: &WorkingMemoryState,
        args: &RetrieveArgs,
        ctx: &OpContext<'_>,
    ) -> Result<RetrievalExecution, LatticeError>;

    fn expand(
        &self,
        state: &WorkingMemoryState,
        target: &BundleResult,
        args: &ExpandArgs,
        ctx: &OpContext<'_>,
    ) -> Result<RetrievalExecution, LatticeError>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct StateMutationRecord {
    pub op: WorkingMemoryOp,
    pub op_name: String,
    pub before_hash: String,
    pub after_hash: String,
    pub summary: String,
    pub state_before: WorkingMemoryState,
    pub state_after: WorkingMemoryState,
}

pub struct OpContext<'a> {
    pub conn: &'a Connection,
    pub retrieval_profile: RetrievalProfile,
    pub identity_resolver: &'a IdentityResolver<'a>,
    pub shaper: ShaperContext,
    pub retriever: &'a dyn WorkingMemoryRetriever,
    pub mutation_observer: Option<&'a dyn StateMutationObserver>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetrieveArgs {
    pub task_id: String,
    pub query_text: Option<String>,
    pub budget: RetrievalBudget,
    pub diagnostic_mode: DiagnosticMode,
    pub query_embedding: Option<Vec<f32>>,
}

impl Default for RetrieveArgs {
    fn default() -> Self {
        Self {
            task_id: "working-memory-retrieve".to_string(),
            query_text: None,
            budget: RetrievalBudget::default(),
            diagnostic_mode: DiagnosticMode::Compact,
            query_embedding: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetrieveOutcome {
    pub bundle: RetrievalBundle,
    pub added_identities: Vec<Identity>,
    pub excluded_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarizeArgs;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkingMemorySummary {
    pub task_statement: String,
    pub intent_label: String,
    pub active_file_count: usize,
    pub active_symbol_count: usize,
    pub selected_count: usize,
    pub excluded_count: usize,
    pub token_cap: usize,
    pub dropped_count: usize,
    pub truncated_results: usize,
    pub pinned_count: usize,
    pub top_hypotheses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinArgs {
    pub identities: Vec<Identity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinOutcome {
    pub added_count: usize,
    pub pinned_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictArgs {
    pub identities: Vec<Identity>,
    pub reason: String,
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictOutcome {
    pub evicted_count: usize,
    pub skipped_absent_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpandArgs {
    pub task_id: String,
    pub expansion_handle: String,
    pub budget: RetrievalBudget,
    pub diagnostic_mode: DiagnosticMode,
    pub query_embedding: Option<Vec<f32>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpandOutcome {
    pub bundle: RetrievalBundle,
    pub added_count: usize,
    pub excluded_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressArgs {
    pub token_budget: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompressOutcome {
    pub removed_count: usize,
    pub retained_count: usize,
    pub budget_report: BudgetReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointArgs {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointOutcome {
    pub checkpoint_id: CheckpointId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalExecution {
    pub bundle: RetrievalBundle,
    pub excluded_memories: Vec<ExcludedMemory>,
}

pub struct PhaseFourRetriever<'a> {
    pub runtime: &'a Runtime,
    pub resources: PhaseFourResources<'a>,
    pub scoring_context: &'a ScoringContext,
}

pub struct PhaseFourResources<'a> {
    pub workspace_id: &'a str,
    pub graph: Option<&'a CodeGraph>,
    pub memory_store: Option<&'a MemoryStore>,
    pub vector_index: Option<&'a dyn VectorIndex>,
    pub event_candidates: &'a [EventEnvelope],
}

impl WorkingMemoryRetriever for PhaseFourRetriever<'_> {
    fn retrieve(
        &self,
        state: &WorkingMemoryState,
        args: &RetrieveArgs,
        ctx: &OpContext<'_>,
    ) -> Result<RetrievalExecution, LatticeError> {
        let task_text = args
            .query_text
            .as_deref()
            .unwrap_or(state.task_statement.as_str());
        self.run_pipeline(
            task_text,
            &args.task_id,
            &args.budget,
            args.diagnostic_mode,
            args.query_embedding.as_deref(),
            selected_identities(state),
            state,
            ctx,
        )
    }

    fn expand(
        &self,
        state: &WorkingMemoryState,
        target: &BundleResult,
        args: &ExpandArgs,
        ctx: &OpContext<'_>,
    ) -> Result<RetrievalExecution, LatticeError> {
        let anchor = expansion_anchor_text(target)?;
        let query = format!("{}\n{}", state.task_statement, anchor);
        self.run_pipeline(
            &query,
            &args.task_id,
            &widen_budget(&args.budget),
            args.diagnostic_mode,
            args.query_embedding.as_deref(),
            expand_recent_identities(state, &target.identity),
            state,
            ctx,
        )
    }
}

impl PhaseFourRetriever<'_> {
    fn run_pipeline(
        &self,
        task_text: &str,
        task_id: &str,
        budget: &RetrievalBudget,
        diagnostic_mode: DiagnosticMode,
        query_embedding: Option<&[f32]>,
        recent_working_memory: Vec<Identity>,
        state: &WorkingMemoryState,
        ctx: &OpContext<'_>,
    ) -> Result<RetrievalExecution, LatticeError> {
        let intent = classify_intent(task_text);
        let anchors = resolve_anchors(extract_anchors(task_text, &intent), ctx.identity_resolver);
        let retrieval_ctx = RetrievalContext {
            workspace_id: self.resources.workspace_id,
            graph: self.resources.graph,
            memory_store: self.resources.memory_store,
            vector_index: self.resources.vector_index,
            query_embedding,
            event_candidates: self.resources.event_candidates,
            recent_working_memory: recent_working_memory.as_slice(),
        };
        let candidates = self.runtime.block_on(retrieve_candidates(
            &intent,
            &anchors,
            budget,
            &retrieval_ctx,
        ));
        let weights = ScoringWeights::for_profile(ctx.retrieval_profile);
        let ranked = score_candidates(
            candidates,
            &intent,
            &anchors,
            &weights,
            diagnostic_mode,
            self.scoring_context,
        );
        let diagnostics = build_diagnostics(diagnostic_mode, &intent, &anchors, &ranked, &weights);
        let shaper = shaper_with_state_pins(
            &ctx.shaper,
            &state.budget_decisions,
            budget.max_candidates_per_source,
        );
        let shaped = shape_retrieval_bundle_detailed(
            task_id,
            format!("{:?}", intent.primary_label),
            summarize_anchor_texts(&anchors),
            ranked,
            diagnostic_mode,
            diagnostics,
            &shaper,
        );
        Ok(RetrievalExecution {
            excluded_memories: to_excluded_memories(
                shaped.dropped_results,
                format!("compressed: budget={} tokens", shaper.budget.max_tokens),
            ),
            bundle: shaped.bundle,
        })
    }
}

/// `## 5. Working Memory` requires retrieval to populate active and excluded
/// context through the Phase 4 retrieval pipeline.
pub fn retrieve(
    state: &mut WorkingMemoryState,
    args: RetrieveArgs,
    ctx: &OpContext<'_>,
) -> Result<RetrieveOutcome, LatticeError> {
    let state_before = state.clone();
    maybe_replace_task(state, args.query_text.as_deref());
    let execution = ctx.retriever.retrieve(state, &args, ctx)?;
    let added = merge_selected_memories(
        &mut state.selected_memories,
        execution.bundle.results.clone(),
    );
    state
        .excluded_memories
        .extend(execution.excluded_memories.clone());
    state.interpreted_intent = classify_intent(&state.task_statement);
    apply_budget_report(&mut state.budget_decisions, &execution.bundle.budget_report);
    refresh_active_context(state);
    let summary = format!(
        "selected={} excluded={} added={}",
        state.selected_memories.len(),
        state.excluded_memories.len(),
        added.len()
    );
    record_if_changed(
        &state_before,
        state,
        WorkingMemoryOp::Retrieve,
        summary,
        ctx,
    )?;
    Ok(RetrieveOutcome {
        bundle: execution.bundle,
        added_identities: added,
        excluded_count: execution.excluded_memories.len(),
    })
}

/// `## 5. Working Memory` requires summarize to expose the active context and
/// budget state without mutating durable storage.
pub fn summarize(
    state: &mut WorkingMemoryState,
    _args: SummarizeArgs,
    _ctx: &OpContext<'_>,
) -> Result<WorkingMemorySummary, LatticeError> {
    Ok(summarize_state(state))
}

pub fn summarize_state(state: &WorkingMemoryState) -> WorkingMemorySummary {
    WorkingMemorySummary {
        task_statement: state.task_statement.clone(),
        intent_label: format!("{:?}", state.interpreted_intent.primary_label),
        active_file_count: state.active_files.len(),
        active_symbol_count: state.active_symbols.len(),
        selected_count: state.selected_memories.len(),
        excluded_count: state.excluded_memories.len(),
        token_cap: state.budget_decisions.token_cap,
        dropped_count: state.budget_decisions.dropped_count,
        truncated_results: state.budget_decisions.truncated_results,
        pinned_count: state.budget_decisions.pinned_identities.len(),
        top_hypotheses: state
            .active_hypotheses
            .iter()
            .take(3)
            .map(|hypothesis| hypothesis.text.clone())
            .collect(),
    }
}

/// `## 5. Working Memory` requires filter to move removed context into
/// `excluded_memories` with an auditable predicate reason.
pub fn filter(
    state: &mut WorkingMemoryState,
    predicate: &dyn FilterPredicate,
    ctx: &OpContext<'_>,
) -> Result<usize, LatticeError> {
    let state_before = state.clone();
    let removed = filter_selected_memories(state, predicate);
    refresh_active_context(state);
    let summary = format!("removed={} predicate={}", removed, predicate.description());
    record_if_changed(&state_before, state, WorkingMemoryOp::Filter, summary, ctx)?;
    Ok(removed)
}

/// `## 5. Working Memory` requires pin to define identities the shaper must
/// preserve across compression.
pub fn pin(
    state: &mut WorkingMemoryState,
    args: PinArgs,
    ctx: &OpContext<'_>,
) -> Result<PinOutcome, LatticeError> {
    let state_before = state.clone();
    let added_count = args
        .identities
        .into_iter()
        .filter(|identity| {
            state
                .budget_decisions
                .pinned_identities
                .insert(identity.clone())
        })
        .count();
    let summary = format!(
        "added={} pinned={}",
        added_count,
        state.budget_decisions.pinned_identities.len()
    );
    record_if_changed(&state_before, state, WorkingMemoryOp::Pin, summary, ctx)?;
    Ok(PinOutcome {
        added_count,
        pinned_count: state.budget_decisions.pinned_identities.len(),
    })
}

/// `## 5. Working Memory` requires evict to remove selected context while
/// recording the operator-supplied reason.
pub fn evict(
    state: &mut WorkingMemoryState,
    args: EvictArgs,
    ctx: &OpContext<'_>,
) -> Result<EvictOutcome, LatticeError> {
    reject_pinned_eviction(state, &args)?;
    let state_before = state.clone();
    let reason = eviction_reason(&args);
    let identities = args.identities.into_iter().collect::<BTreeSet<_>>();
    let evicted = evict_selected_memories(state, &identities, &reason);
    state
        .budget_decisions
        .pinned_identities
        .retain(|identity| !identities.contains(identity));
    refresh_active_context(state);
    let summary = format!("evicted={} requested={}", evicted, identities.len());
    record_if_changed(&state_before, state, WorkingMemoryOp::Evict, summary, ctx)?;
    Ok(EvictOutcome {
        evicted_count: evicted,
        skipped_absent_count: identities.len().saturating_sub(evicted),
    })
}

/// `## 5. Working Memory` requires expand to widen retrieval around an existing
/// selected result using its expansion handle.
pub fn expand(
    state: &mut WorkingMemoryState,
    args: ExpandArgs,
    ctx: &OpContext<'_>,
) -> Result<ExpandOutcome, LatticeError> {
    let state_before = state.clone();
    let target = selected_target(state, &args.expansion_handle)?;
    let execution = ctx.retriever.expand(state, &target, &args, ctx)?;
    let added = merge_selected_memories(
        &mut state.selected_memories,
        execution.bundle.results.clone(),
    );
    state
        .excluded_memories
        .extend(execution.excluded_memories.clone());
    apply_budget_report(&mut state.budget_decisions, &execution.bundle.budget_report);
    refresh_active_context(state);
    let summary = format!(
        "added={} selected={}",
        added.len(),
        state.selected_memories.len()
    );
    record_if_changed(&state_before, state, WorkingMemoryOp::Expand, summary, ctx)?;
    Ok(ExpandOutcome {
        bundle: execution.bundle,
        added_count: added.len(),
        excluded_count: execution.excluded_memories.len(),
    })
}

/// `## 5. Working Memory` requires compress to reuse the Retrieval V1 shaper
/// policy while honoring pinned identities.
pub fn compress(
    state: &mut WorkingMemoryState,
    args: CompressArgs,
    ctx: &OpContext<'_>,
) -> Result<CompressOutcome, LatticeError> {
    let state_before = state.clone();
    let shaper = shaper_with_state_pins(&ctx.shaper, &state.budget_decisions, args.token_budget);
    let compression = compress_bundle_results(state.selected_memories.clone(), &shaper);
    let removed_count = compression.dropped_results.len();
    state.selected_memories = compression.results;
    state.excluded_memories.extend(to_excluded_memories(
        compression.dropped_results,
        format!("compressed: budget={} tokens", args.token_budget),
    ));
    apply_budget_report(&mut state.budget_decisions, &compression.budget_report);
    refresh_active_context(state);
    let summary = format!(
        "removed={} retained={}",
        removed_count,
        state.selected_memories.len()
    );
    record_if_changed(
        &state_before,
        state,
        WorkingMemoryOp::Compress,
        summary,
        ctx,
    )?;
    Ok(CompressOutcome {
        removed_count,
        retained_count: state.selected_memories.len(),
        budget_report: compression.budget_report,
    })
}

/// `## 5. Working Memory` requires checkpoint to persist the current state
/// through the T34 checkpoint path without a parallel write path.
pub fn checkpoint(
    state: &mut WorkingMemoryState,
    args: CheckpointArgs,
    ctx: &OpContext<'_>,
) -> Result<CheckpointOutcome, LatticeError> {
    Ok(CheckpointOutcome {
        checkpoint_id: save_checkpoint(state, &args.name, ctx.conn)?,
    })
}

fn build_diagnostics(
    diagnostic_mode: DiagnosticMode,
    intent: &crate::retrieval_v1::IntentClassification,
    anchors: &[crate::retrieval_v1::ResolvedAnchor],
    ranked: &[crate::retrieval_v1::RankedCandidate],
    weights: &ScoringWeights,
) -> Option<RankingDiagnostics> {
    matches!(diagnostic_mode, DiagnosticMode::Diagnostic).then(|| {
        RankingDiagnostics::new(
            intent.clone(),
            anchors.to_vec(),
            ranked.to_vec(),
            weights.clone(),
        )
    })
}

fn maybe_replace_task(state: &mut WorkingMemoryState, query_text: Option<&str>) {
    if let Some(query_text) = query_text {
        state.task_statement = query_text.to_string();
    }
}

fn merge_selected_memories(
    selected: &mut Vec<BundleResult>,
    incoming: Vec<BundleResult>,
) -> Vec<Identity> {
    let mut index_by_identity = selected
        .iter()
        .enumerate()
        .map(|(index, result)| (result.identity.clone(), index))
        .collect::<HashMap<_, _>>();
    let mut added = Vec::new();
    for result in incoming {
        if let Some(index) = index_by_identity.get(&result.identity).copied() {
            selected[index] = result;
        } else {
            index_by_identity.insert(result.identity.clone(), selected.len());
            added.push(result.identity.clone());
            selected.push(result);
        }
    }
    added
}

fn filter_selected_memories(
    state: &mut WorkingMemoryState,
    predicate: &dyn FilterPredicate,
) -> usize {
    let mut retained = Vec::with_capacity(state.selected_memories.len());
    let mut removed = 0usize;
    for result in state.selected_memories.drain(..) {
        if predicate.matches(&result) {
            retained.push(result);
        } else {
            state.excluded_memories.push(ExcludedMemory {
                result,
                exclusion_reason: format!("filtered: {}", predicate.description()),
            });
            removed += 1;
        }
    }
    state.selected_memories = retained;
    removed
}

fn reject_pinned_eviction(
    state: &WorkingMemoryState,
    args: &EvictArgs,
) -> Result<(), LatticeError> {
    if args.force {
        return Ok(());
    }
    let pinned = args
        .identities
        .iter()
        .find(|identity| state.budget_decisions.pinned_identities.contains(*identity));
    match pinned {
        Some(identity) => Err(LatticeError::Query(format!(
            "Cannot evict pinned identity `{}` without force=true",
            identity
        ))),
        None => Ok(()),
    }
}

fn evict_selected_memories(
    state: &mut WorkingMemoryState,
    identities: &BTreeSet<Identity>,
    reason: &str,
) -> usize {
    let mut retained = Vec::with_capacity(state.selected_memories.len());
    let mut evicted = 0usize;
    for result in state.selected_memories.drain(..) {
        if identities.contains(&result.identity) {
            state.excluded_memories.push(ExcludedMemory {
                result,
                exclusion_reason: reason.to_string(),
            });
            evicted += 1;
        } else {
            retained.push(result);
        }
    }
    state.selected_memories = retained;
    evicted
}

fn eviction_reason(args: &EvictArgs) -> String {
    if args.force {
        format!("evicted: force=true; {}", args.reason)
    } else {
        format!("evicted: {}", args.reason)
    }
}

fn selected_target(
    state: &WorkingMemoryState,
    expansion_handle: &str,
) -> Result<BundleResult, LatticeError> {
    state
        .selected_memories
        .iter()
        .find(|result| result.expansion_handle == expansion_handle)
        .cloned()
        .ok_or_else(|| {
            LatticeError::Query(format!(
                "No selected memory matches expansion handle `{expansion_handle}`"
            ))
        })
}

fn apply_budget_report(decisions: &mut BudgetDecisions, report: &BudgetReport) {
    decisions.token_cap = report.token_budget;
    decisions.dropped_count = report.dropped_results;
    decisions.truncated_results = report.trimmed_snippets;
}

fn refresh_active_context(state: &mut WorkingMemoryState) {
    state.active_files = state
        .selected_memories
        .iter()
        .filter_map(|result| match &result.identity {
            Identity::File(file) => Some(file.clone()),
            Identity::Symbol(symbol) => Some(symbol.file.clone()),
            _ => None,
        })
        .collect::<BTreeSet<FileIdentity>>();
    state.active_symbols = state
        .selected_memories
        .iter()
        .filter_map(|result| match &result.identity {
            Identity::Symbol(symbol) => Some(symbol.clone()),
            _ => None,
        })
        .collect::<BTreeSet<SymbolIdentity>>();
}

fn record_if_changed(
    state_before: &WorkingMemoryState,
    state: &WorkingMemoryState,
    op: WorkingMemoryOp,
    summary: String,
    ctx: &OpContext<'_>,
) -> Result<(), LatticeError> {
    let before_hash = state_hash(state_before)?;
    let after_hash = state_hash(state)?;
    if before_hash == after_hash {
        return Ok(());
    }
    record_state_mutation(
        state_before,
        state,
        op,
        before_hash,
        after_hash,
        summary,
        ctx,
    )
}

fn record_state_mutation(
    state_before: &WorkingMemoryState,
    state_after: &WorkingMemoryState,
    op: WorkingMemoryOp,
    before_hash: String,
    after_hash: String,
    summary: String,
    ctx: &OpContext<'_>,
) -> Result<(), LatticeError> {
    match ctx.mutation_observer {
        Some(observer) => observer.record(StateMutationRecord {
            op,
            op_name: op.as_str().to_string(),
            before_hash,
            after_hash,
            summary,
            state_before: state_before.clone(),
            state_after: state_after.clone(),
        }),
        None => Ok(()),
    }
}

fn selected_identities(state: &WorkingMemoryState) -> Vec<Identity> {
    state
        .selected_memories
        .iter()
        .map(|result| result.identity.clone())
        .collect()
}

fn expand_recent_identities(state: &WorkingMemoryState, target: &Identity) -> Vec<Identity> {
    let mut identities = selected_identities(state);
    if !identities.contains(target) {
        identities.push(target.clone());
    }
    identities
}

fn summarize_anchor_texts(anchors: &[crate::retrieval_v1::ResolvedAnchor]) -> String {
    anchors
        .iter()
        .map(|anchor| anchor.anchor_text.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn shaper_with_state_pins(
    base: &ShaperContext,
    decisions: &BudgetDecisions,
    token_budget: usize,
) -> ShaperContext {
    let mut pins = base.pins.clone();
    pins.extend(decisions.pinned_identities.iter().cloned());
    ShaperContext {
        pins,
        budget: ShaperBudget {
            max_tokens: token_budget,
        },
    }
}

fn to_excluded_memories(results: Vec<BundleResult>, reason: String) -> Vec<ExcludedMemory> {
    results
        .into_iter()
        .map(|result| ExcludedMemory {
            result,
            exclusion_reason: reason.clone(),
        })
        .collect()
}

fn expansion_anchor_text(target: &BundleResult) -> Result<String, LatticeError> {
    let identity = decode_identity(&target.expansion_handle).map_err(|error| {
        LatticeError::Query(format!(
            "Invalid expansion handle `{}`: {}",
            target.expansion_handle, error
        ))
    })?;
    Ok(match identity {
        Identity::File(file) => file.repo_relative_path,
        Identity::Symbol(symbol) => symbol.qualified_name,
        Identity::Doc(doc) => doc.repo_relative_path,
        Identity::Section(section) => format!(
            "{}#{}",
            section.doc.repo_relative_path,
            section.heading_path.join(" > ")
        ),
        Identity::Event(event) => format!("event {}", event.ulid),
        Identity::Memory(memory) => format!("memory {}", memory.ulid),
        Identity::ContextHandle(handle) => format!("handle {}", handle.ulid),
    })
}

fn widen_budget(budget: &RetrievalBudget) -> RetrievalBudget {
    RetrievalBudget {
        max_candidates_per_source: budget.max_candidates_per_source.saturating_mul(2).max(1),
        max_graph_hops: budget.max_graph_hops.saturating_add(1),
        max_fts_rows: budget.max_fts_rows.saturating_mul(2).max(1),
        embedding_k: budget.embedding_k.saturating_mul(2).max(1),
        event_window: budget.event_window.saturating_mul(2).max(1),
        working_memory_window: budget.working_memory_window.saturating_mul(2).max(1),
    }
}
