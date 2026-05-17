use std::cell::RefCell;
use std::collections::HashSet;

use rusqlite::Connection;

use super::operations::{
    ExpandArgs, OpContext, RetrievalExecution, RetrieveArgs, StateMutationObserver,
    StateMutationRecord, WorkingMemoryRetriever,
};
use super::state::{initialize_schema, BudgetDecisions, ExcludedMemory, WorkingMemoryState};
use crate::graph::CodeGraph;
use crate::identity::IdentityResolver;
use crate::identity::{FileId, Identity, IdentityKind, MemoryId};
use crate::memory::MemoryVerificationStatus;
use crate::retrieval_v1::{
    schema::{BudgetReport, BundleResult, RetrievalBundle},
    DiagnosticMode, RetrievalBudget, RetrievalProfile, ShaperBudget, ShaperContext,
};

pub(crate) const WORKSPACE: &str = "workspace-main";

#[derive(Default)]
pub(crate) struct RecordingObserver {
    pub(crate) records: RefCell<Vec<StateMutationRecord>>,
}

impl StateMutationObserver for RecordingObserver {
    fn record(&self, mutation: StateMutationRecord) -> Result<(), crate::error::LatticeError> {
        self.records.borrow_mut().push(mutation);
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct FakeRetriever {
    pub(crate) retrieve_execution: RetrievalExecution,
    pub(crate) expand_execution: RetrievalExecution,
}

impl Default for FakeRetriever {
    fn default() -> Self {
        Self {
            retrieve_execution: RetrievalExecution {
                bundle: sample_bundle(
                    vec![
                        sample_bundle_result("memory-a", 0.8),
                        sample_bundle_result("memory-b", 0.7),
                    ],
                    240,
                    false,
                    0,
                    0,
                ),
                excluded_memories: vec![ExcludedMemory {
                    result: sample_bundle_result("memory-c", 0.6),
                    exclusion_reason: "compressed: budget=240 tokens".to_string(),
                }],
            },
            expand_execution: RetrievalExecution {
                bundle: sample_bundle(
                    vec![sample_bundle_result("memory-a", 0.8)],
                    240,
                    false,
                    0,
                    0,
                ),
                excluded_memories: Vec::new(),
            },
        }
    }
}

impl FakeRetriever {
    pub(crate) fn with_retrieve_execution(retrieve_execution: RetrievalExecution) -> Self {
        Self {
            retrieve_execution,
            ..Self::default()
        }
    }
}

impl WorkingMemoryRetriever for FakeRetriever {
    fn retrieve(
        &self,
        _state: &WorkingMemoryState,
        _args: &RetrieveArgs,
        _ctx: &OpContext<'_>,
    ) -> Result<RetrievalExecution, crate::error::LatticeError> {
        Ok(self.retrieve_execution.clone())
    }

    fn expand(
        &self,
        _state: &WorkingMemoryState,
        _target: &BundleResult,
        _args: &ExpandArgs,
        _ctx: &OpContext<'_>,
    ) -> Result<RetrievalExecution, crate::error::LatticeError> {
        Ok(self.expand_execution.clone())
    }
}

pub(crate) struct Harness {
    pub(crate) conn: Connection,
    resolver: &'static IdentityResolver<'static>,
    retriever: FakeRetriever,
    pub(crate) observer: RecordingObserver,
    shaper_budget: usize,
}

impl Harness {
    pub(crate) fn new(retriever: FakeRetriever) -> Self {
        Self::with_shaper_budget(retriever, 320)
    }

    pub(crate) fn with_shaper_budget(retriever: FakeRetriever, shaper_budget: usize) -> Self {
        let conn = Connection::open_in_memory().expect("in-memory connection");
        initialize_schema(&conn).expect("working memory schema");
        let graph = Box::leak(Box::new(CodeGraph::default()));
        let file_index = Box::leak(Box::new(Default::default()));
        let parsed_files = Box::leak(Box::new(Default::default()));
        let resolver = Box::leak(Box::new(IdentityResolver::new(
            graph,
            file_index,
            parsed_files,
            WORKSPACE.to_string(),
            Vec::new(),
        )));
        Self {
            conn,
            resolver,
            retriever,
            observer: RecordingObserver::default(),
            shaper_budget,
        }
    }

    pub(crate) fn ctx(&self) -> OpContext<'_> {
        OpContext {
            conn: &self.conn,
            retrieval_profile: RetrievalProfile::Balanced,
            identity_resolver: self.resolver,
            shaper: ShaperContext {
                pins: HashSet::new(),
                budget: ShaperBudget {
                    max_tokens: self.shaper_budget,
                },
            },
            retriever: &self.retriever,
            mutation_observer: Some(&self.observer),
        }
    }
}

pub(crate) fn sample_state() -> WorkingMemoryState {
    WorkingMemoryState {
        task_statement: "Investigate working memory operations".to_string(),
        interpreted_intent: crate::retrieval_v1::classify_intent(
            "Investigate working memory operations",
        ),
        active_files: Default::default(),
        active_symbols: Default::default(),
        active_hypotheses: Vec::new(),
        active_failures: Vec::new(),
        current_plan: None,
        selected_memories: Vec::new(),
        excluded_memories: Vec::new(),
        budget_decisions: BudgetDecisions {
            token_cap: 0,
            dropped_count: 0,
            truncated_results: 0,
            pinned_identities: Default::default(),
        },
        unresolved_questions: Vec::new(),
        verification_status: super::state::WorkingMemoryVerification {
            last_verified_at: None,
            status: MemoryVerificationStatus::Unverified,
            notes: Vec::new(),
        },
    }
}

pub(crate) fn sample_bundle(
    results: Vec<BundleResult>,
    token_budget: usize,
    truncated: bool,
    trimmed_snippets: usize,
    dropped_results: usize,
) -> RetrievalBundle {
    RetrievalBundle {
        task_id: "task".to_string(),
        intent_summary: "Debug".to_string(),
        anchors_summary: "memory".to_string(),
        results,
        diagnostics: None,
        budget_report: BudgetReport {
            token_budget,
            estimated_tokens: token_budget / 2,
            trimmed_snippets,
            dropped_results,
            truncated,
        },
    }
}

pub(crate) fn sample_bundle_result(ulid: &str, score: f32) -> BundleResult {
    BundleResult {
        identity: sample_identity(ulid),
        kind: IdentityKind::Memory,
        headline: format!("Memory {ulid}"),
        snippet: format!("Snippet for {ulid}"),
        inclusion_reason: "selected memory".to_string(),
        expansion_handle: crate::identity::encode_identity(&sample_identity(ulid)),
        source: Vec::new(),
        score,
    }
}

pub(crate) fn long_bundle_result(identity: Identity, headline: &str, score: f32) -> BundleResult {
    BundleResult {
        identity: identity.clone(),
        kind: identity.kind(),
        headline: headline.to_string(),
        snippet: "This snippet is intentionally long so compression has to trim and evict results from working memory."
            .repeat(4),
        inclusion_reason: "selected memory".to_string(),
        expansion_handle: crate::identity::encode_identity(&identity),
        source: Vec::new(),
        score,
    }
}

pub(crate) fn huge_bundle_result(ulid: &str, score: f32) -> BundleResult {
    BundleResult {
        snippet: "budget ".repeat(220),
        ..sample_bundle_result(ulid, score)
    }
}

pub(crate) fn sample_identity(ulid: &str) -> Identity {
    Identity::Memory(MemoryId {
        workspace_id: WORKSPACE.to_string(),
        ulid: ulid.to_string(),
    })
}

pub(crate) fn file_identity(path: &str) -> Identity {
    Identity::File(FileId {
        workspace_id: WORKSPACE.to_string(),
        repo_relative_path: path.to_string(),
        content_hash: "hash".to_string(),
    })
}

pub(crate) fn retrieval_args() -> RetrieveArgs {
    RetrieveArgs {
        task_id: "working-memory-retrieve".to_string(),
        query_text: None,
        budget: RetrievalBudget::default(),
        diagnostic_mode: DiagnosticMode::Compact,
        query_embedding: None,
    }
}
