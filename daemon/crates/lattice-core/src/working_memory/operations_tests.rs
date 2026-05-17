use std::cell::RefCell;
use std::collections::HashSet;

use rusqlite::Connection;

use super::operations::{
    checkpoint, compress, evict, expand, filter, pin, retrieve, summarize, CheckpointArgs,
    CompressArgs, EvictArgs, ExpandArgs, FilterPredicate, OpContext, PinArgs, RetrievalExecution,
    RetrieveArgs, StateMutationObserver, StateMutationRecord, SummarizeArgs,
    WorkingMemoryRetriever,
};
use super::state::{
    initialize_schema, load_checkpoint, BudgetDecisions, Hypothesis, WorkingMemoryState,
};
use crate::graph::CodeGraph;
use crate::identity::IdentityResolver;
use crate::identity::{FileId, Identity, IdentityKind, MemoryId};
use crate::memory::MemoryVerificationStatus;
use crate::retrieval_v1::{
    schema::{BudgetReport, BundleResult, RetrievalBundle},
    DiagnosticMode, RetrievalBudget, RetrievalProfile, ShaperBudget, ShaperContext,
};

const WORKSPACE: &str = "workspace-main";

#[test]
fn retrieve_populates_selected_and_excluded_memories() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();

    let outcome = retrieve(&mut state, RetrieveArgs::default(), &harness.ctx()).expect("retrieve");

    assert_eq!(outcome.added_identities.len(), 2);
    assert_eq!(state.selected_memories.len(), 2);
    assert_eq!(state.excluded_memories.len(), 1);
    assert!(state.excluded_memories[0]
        .exclusion_reason
        .contains("compressed"));
    assert_eq!(harness.observer.records.borrow().len(), 1);
}

#[test]
fn summarize_returns_expected_fields() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();
    state.selected_memories = vec![sample_bundle_result("memory-a", 0.8)];
    state.active_hypotheses = vec![Hypothesis {
        text: "Top hypothesis".to_string(),
        evidence_refs: vec!["docs/spec.md#5".to_string()],
        confidence: 0.9,
    }];

    let summary = summarize(&mut state, SummarizeArgs, &harness.ctx()).expect("summary");

    assert_eq!(summary.task_statement, state.task_statement);
    assert_eq!(summary.selected_count, 1);
    assert_eq!(summary.top_hypotheses, vec!["Top hypothesis".to_string()]);
}

#[test]
fn filter_records_predicate_description() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();
    state.selected_memories = vec![
        sample_bundle_result("keep-me", 0.8),
        sample_bundle_result("drop-me", 0.7),
    ];

    let removed = filter(&mut state, &HeadlineFilter("keep"), &harness.ctx()).expect("filter");

    assert_eq!(removed, 1);
    assert_eq!(state.selected_memories.len(), 1);
    assert_eq!(
        state.excluded_memories[0].exclusion_reason,
        "filtered: headline contains `keep`"
    );
}

#[test]
fn pin_is_idempotent_and_compress_preserves_pinned_identity() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();
    let pinned = file_identity("src/pinned.rs");
    state.selected_memories = vec![
        long_bundle_result(pinned.clone(), "Pinned", 0.9),
        long_bundle_result(file_identity("src/other-a.rs"), "Other A", 0.8),
        long_bundle_result(file_identity("src/other-b.rs"), "Other B", 0.7),
    ];

    let first = pin(
        &mut state,
        PinArgs {
            identities: vec![pinned.clone()],
        },
        &harness.ctx(),
    )
    .expect("first pin");
    let second = pin(
        &mut state,
        PinArgs {
            identities: vec![pinned.clone()],
        },
        &harness.ctx(),
    )
    .expect("second pin");
    let compressed = compress(
        &mut state,
        CompressArgs { token_budget: 70 },
        &harness.ctx(),
    )
    .expect("compress");

    assert_eq!(first.added_count, 1);
    assert_eq!(second.added_count, 0);
    assert!(state
        .selected_memories
        .iter()
        .any(|result| result.identity == pinned));
    assert!(compressed.removed_count >= 1);
}

#[test]
fn evict_of_pinned_identity_without_force_is_refused() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();
    let pinned = sample_identity("memory-a");
    state.selected_memories = vec![sample_bundle_result("memory-a", 0.8)];
    state
        .budget_decisions
        .pinned_identities
        .insert(pinned.clone());

    let error = evict(
        &mut state,
        EvictArgs {
            identities: vec![pinned],
            reason: "manual trim".to_string(),
            force: false,
        },
        &harness.ctx(),
    )
    .expect_err("pinned eviction should fail");

    assert!(error.to_string().contains("Cannot evict pinned identity"));
}

#[test]
fn evict_forced_identity_records_reason_and_absent_count() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();
    let present = sample_identity("memory-a");
    let absent = sample_identity("missing");
    state.selected_memories = vec![sample_bundle_result("memory-a", 0.8)];

    let outcome = evict(
        &mut state,
        EvictArgs {
            identities: vec![present, absent],
            reason: "manual trim".to_string(),
            force: true,
        },
        &harness.ctx(),
    )
    .expect("forced evict");

    assert_eq!(outcome.evicted_count, 1);
    assert_eq!(outcome.skipped_absent_count, 1);
    assert_eq!(
        state.excluded_memories[0].exclusion_reason,
        "evicted: force=true; manual trim"
    );
}

#[test]
fn expand_appends_new_high_ranked_candidates() {
    let harness = Harness::new(FakeRetriever::with_expand_result());
    let mut state = sample_state();
    let target = sample_bundle_result("memory-a", 0.8);
    state.selected_memories = vec![target.clone()];

    let outcome = expand(
        &mut state,
        ExpandArgs {
            task_id: "expand-case".to_string(),
            expansion_handle: target.expansion_handle.clone(),
            budget: RetrievalBudget::default(),
            diagnostic_mode: DiagnosticMode::Compact,
            query_embedding: None,
        },
        &harness.ctx(),
    )
    .expect("expand");

    assert_eq!(outcome.added_count, 1);
    assert_eq!(state.selected_memories.len(), 2);
}

#[test]
fn checkpoint_round_trips_state_bytes() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();
    state.selected_memories = vec![sample_bundle_result("memory-a", 0.8)];

    let outcome = checkpoint(
        &mut state,
        CheckpointArgs {
            name: "before-compress".to_string(),
        },
        &harness.ctx(),
    )
    .expect("checkpoint");
    let loaded = load_checkpoint(outcome.checkpoint_id, &harness.conn).expect("load checkpoint");

    assert_eq!(loaded, state);
}

#[test]
fn checkpoint_name_versions_instead_of_overwriting() {
    let harness = Harness::new(FakeRetriever::default());
    let mut state = sample_state();

    let first = checkpoint(
        &mut state,
        CheckpointArgs {
            name: "same-name".to_string(),
        },
        &harness.ctx(),
    )
    .expect("first checkpoint");
    let second = checkpoint(
        &mut state,
        CheckpointArgs {
            name: "same-name".to_string(),
        },
        &harness.ctx(),
    )
    .expect("second checkpoint");

    assert!(second.checkpoint_id > first.checkpoint_id);
}

struct HeadlineFilter(&'static str);

impl FilterPredicate for HeadlineFilter {
    fn description(&self) -> &str {
        "headline contains `keep`"
    }

    fn matches(&self, result: &BundleResult) -> bool {
        result.headline.contains(self.0)
    }
}

#[derive(Clone)]
struct FakeRetriever {
    retrieve_execution: RetrievalExecution,
    expand_execution: RetrievalExecution,
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
                ),
                excluded_memories: vec![super::state::ExcludedMemory {
                    result: sample_bundle_result("memory-c", 0.6),
                    exclusion_reason: "compressed: budget=240 tokens".to_string(),
                }],
            },
            expand_execution: RetrievalExecution {
                bundle: sample_bundle(vec![sample_bundle_result("memory-a", 0.8)], 240),
                excluded_memories: Vec::new(),
            },
        }
    }
}

impl FakeRetriever {
    fn with_expand_result() -> Self {
        Self {
            expand_execution: RetrievalExecution {
                bundle: sample_bundle(
                    vec![
                        sample_bundle_result("memory-a", 0.8),
                        sample_bundle_result("memory-expanded", 0.75),
                    ],
                    320,
                ),
                excluded_memories: Vec::new(),
            },
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

struct Observer {
    records: RefCell<Vec<StateMutationRecord>>,
}

impl StateMutationObserver for Observer {
    fn record(&self, mutation: StateMutationRecord) -> Result<(), crate::error::LatticeError> {
        self.records.borrow_mut().push(mutation);
        Ok(())
    }
}

struct Harness {
    conn: Connection,
    resolver: &'static IdentityResolver<'static>,
    retriever: FakeRetriever,
    observer: Observer,
}

impl Harness {
    fn new(retriever: FakeRetriever) -> Self {
        let conn = Connection::open_in_memory().expect("in-memory connection");
        initialize_schema(&conn).expect("schema");
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
            observer: Observer {
                records: RefCell::new(Vec::new()),
            },
        }
    }

    fn ctx(&self) -> OpContext<'_> {
        OpContext {
            conn: &self.conn,
            retrieval_profile: RetrievalProfile::Balanced,
            identity_resolver: self.resolver,
            shaper: ShaperContext {
                pins: HashSet::new(),
                budget: ShaperBudget { max_tokens: 320 },
            },
            retriever: &self.retriever,
            mutation_observer: Some(&self.observer),
        }
    }
}

fn sample_state() -> WorkingMemoryState {
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

fn sample_bundle(results: Vec<BundleResult>, token_budget: usize) -> RetrievalBundle {
    RetrievalBundle {
        task_id: "task".to_string(),
        intent_summary: "Debug".to_string(),
        anchors_summary: "memory".to_string(),
        results,
        diagnostics: None,
        budget_report: BudgetReport {
            token_budget,
            estimated_tokens: token_budget / 2,
            trimmed_snippets: 0,
            dropped_results: 0,
            truncated: false,
        },
    }
}

fn sample_bundle_result(ulid: &str, score: f32) -> BundleResult {
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

fn long_bundle_result(identity: Identity, headline: &str, score: f32) -> BundleResult {
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

fn sample_identity(ulid: &str) -> Identity {
    Identity::Memory(MemoryId {
        workspace_id: WORKSPACE.to_string(),
        ulid: ulid.to_string(),
    })
}

fn file_identity(path: &str) -> Identity {
    Identity::File(FileId {
        workspace_id: WORKSPACE.to_string(),
        repo_relative_path: path.to_string(),
        content_hash: "hash".to_string(),
    })
}
