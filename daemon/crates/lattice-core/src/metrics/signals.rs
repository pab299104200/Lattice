//! Canonical Phase 9 signal definitions and deterministic collectors.
//!
//! Required metrics:
//!
//! - tool calls per successful task
//! - irrelevant files opened per task
//! - relevant anchor recall
//! - memory inclusion precision
//! - memory later-used rate
//! - stale memory surfaced rate
//! - contradiction missed rate
//! - tests recommended versus tests needed
//! - workflow success after first plan

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use tracing::debug_span;

use crate::events::{
    EventEnvelope, EventKind, EventPayload, FileReadPayload, PatchAppliedPayload,
    WorkflowSucceededPayload,
};
use crate::identity::{EventId, MemoryId};
use crate::verification::VerificationStatus;
use crate::{DateTime, Utc};

const DEFAULT_MAX_SAMPLES: usize = 1_000;

/// Canonical Phase 9 signal names used by MCP and CLI surfaces.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MetricSignal {
    ToolCallsPerSuccessfulTask,
    IrrelevantFilesOpenedPerTask,
    RelevantAnchorRecall,
    MemoryInclusionPrecision,
    MemoryLaterUsedRate,
    StaleMemorySurfacedRate,
    ContradictionMissedRate,
    TestsRecommendedVsNeeded,
    WorkflowSuccessAfterFirstPlan,
    /// Pooled held-out PR-AUC of the `graph+git+complexity` family.
    ///
    /// Phase H5 of `docs/plans/2026-08-13-health-engine.md`. Sourced from a
    /// rerun of the H1 backtest harness and compared against the figure in
    /// `docs/reports/health-backtest/2026-08-14.md`
    /// § "Pooled, derived weights, held out".
    HealthDefectPrAuc,
    /// Pooled held-out ROC-AUC of the `graph+git+complexity` family.
    ///
    /// Same source and section as [`MetricSignal::HealthDefectPrAuc`].
    HealthDefectRocAuc,
    /// Held-out ROC-AUC of `graph+git+complexity` minus that of `graph-only`.
    ///
    /// This is success criterion 1 of `docs/plans/2026-08-13-health-engine.md`
    /// § "Success criteria" expressed as a tracked number: the extra fact
    /// families must keep beating graph facts alone, or the engine's central
    /// claim has silently stopped being true.
    HealthDefectFamilyUplift,
    /// Enrichment of the H1.2 label-quality audit.
    ///
    /// How much more often `looks_like_bug_fix` commits co-modify tests and
    /// production than commits in general. See
    /// `docs/reports/health-backtest/2026-08-14.md`
    /// § "H1.2 label-quality audit" for why enrichment — and not raw agreement
    /// or Cohen's kappa — is the figure that carries evidence here.
    HealthLabelAuditEnrichment,
}

impl MetricSignal {
    /// Every spec-required Phase 9 signal in canonical order, followed by the
    /// Phase H5 health regression signals.
    pub const ALL: [MetricSignal; 13] = [
        MetricSignal::ToolCallsPerSuccessfulTask,
        MetricSignal::IrrelevantFilesOpenedPerTask,
        MetricSignal::RelevantAnchorRecall,
        MetricSignal::MemoryInclusionPrecision,
        MetricSignal::MemoryLaterUsedRate,
        MetricSignal::StaleMemorySurfacedRate,
        MetricSignal::ContradictionMissedRate,
        MetricSignal::TestsRecommendedVsNeeded,
        MetricSignal::WorkflowSuccessAfterFirstPlan,
        MetricSignal::HealthDefectPrAuc,
        MetricSignal::HealthDefectRocAuc,
        MetricSignal::HealthDefectFamilyUplift,
        MetricSignal::HealthLabelAuditEnrichment,
    ];

    /// The nine Phase 9 signals collected from live session evidence.
    ///
    /// These are the signals a session-scoped surface can actually compute.
    /// The health signals are deliberately excluded: they are produced by
    /// replaying repository history offline, so returning them from a session
    /// tool would only ever yield permanently-null rows.
    pub const PHASE_9: [MetricSignal; 9] = [
        MetricSignal::ToolCallsPerSuccessfulTask,
        MetricSignal::IrrelevantFilesOpenedPerTask,
        MetricSignal::RelevantAnchorRecall,
        MetricSignal::MemoryInclusionPrecision,
        MetricSignal::MemoryLaterUsedRate,
        MetricSignal::StaleMemorySurfacedRate,
        MetricSignal::ContradictionMissedRate,
        MetricSignal::TestsRecommendedVsNeeded,
        MetricSignal::WorkflowSuccessAfterFirstPlan,
    ];

    /// The Phase H5 health regression signals, in canonical order.
    ///
    /// These are the only signals sourced from the backtest harness rather
    /// than from the event log, memory store, verifier, or workflow outcomes.
    pub const HEALTH: [MetricSignal; 4] = [
        MetricSignal::HealthDefectPrAuc,
        MetricSignal::HealthDefectRocAuc,
        MetricSignal::HealthDefectFamilyUplift,
        MetricSignal::HealthLabelAuditEnrichment,
    ];

    /// Whether this signal comes from the health backtest harness.
    pub fn is_health(self) -> bool {
        matches!(
            self,
            Self::HealthDefectPrAuc
                | Self::HealthDefectRocAuc
                | Self::HealthDefectFamilyUplift
                | Self::HealthLabelAuditEnrichment
        )
    }

    /// Return the canonical snake_case wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ToolCallsPerSuccessfulTask => "tool_calls_per_successful_task",
            Self::IrrelevantFilesOpenedPerTask => "irrelevant_files_opened_per_task",
            Self::RelevantAnchorRecall => "relevant_anchor_recall",
            Self::MemoryInclusionPrecision => "memory_inclusion_precision",
            Self::MemoryLaterUsedRate => "memory_later_used_rate",
            Self::StaleMemorySurfacedRate => "stale_memory_surfaced_rate",
            Self::ContradictionMissedRate => "contradiction_missed_rate",
            Self::TestsRecommendedVsNeeded => "tests_recommended_vs_needed",
            Self::WorkflowSuccessAfterFirstPlan => "workflow_success_after_first_plan",
            Self::HealthDefectPrAuc => "health_defect_pr_auc",
            Self::HealthDefectRocAuc => "health_defect_roc_auc",
            Self::HealthDefectFamilyUplift => "health_defect_family_uplift",
            Self::HealthLabelAuditEnrichment => "health_label_audit_enrichment",
        }
    }
}

/// Provenance for one collected metric.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MetricSource {
    EventLog,
    MemoryStore,
    WorkflowOutcome,
    Verifier,
    SessionMetrics,
    /// A replay of repository history by the Phase H1 backtest harness
    /// (`crate::health::backtest`).
    HealthBacktest,
}

/// Optional UTC time-range filter for collection.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MetricTimeRange {
    /// Inclusive lower bound for considered evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
    /// Inclusive upper bound for considered evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<DateTime<Utc>>,
}

/// Top-level metric scope taxonomy for bounded collection.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MetricScopeKind {
    Session,
    Branch,
    Repo,
    User,
    Organization,
}

/// Collection scope for Phase 9 metrics.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MetricScope {
    /// Scope taxonomy requested by the caller.
    pub kind: MetricScopeKind,
    /// Workspace identifier required for repo and branch scopes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Branch name for branch-scoped metrics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Session identifier for session-scoped metrics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// User identifier for user-scoped metrics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Organization identifier for organization-scoped metrics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    /// Optional time-range filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_range: Option<MetricTimeRange>,
}

impl MetricScope {
    /// Construct a session scope.
    pub fn session(session_id: impl Into<String>) -> Self {
        Self {
            kind: MetricScopeKind::Session,
            workspace_id: None,
            branch: None,
            session_id: Some(session_id.into()),
            user_id: None,
            organization_id: None,
            time_range: None,
        }
    }

    /// Construct a branch scope.
    pub fn branch(workspace_id: impl Into<String>, branch: impl Into<String>) -> Self {
        Self {
            kind: MetricScopeKind::Branch,
            workspace_id: Some(workspace_id.into()),
            branch: Some(branch.into()),
            session_id: None,
            user_id: None,
            organization_id: None,
            time_range: None,
        }
    }

    /// Construct a repo scope.
    pub fn repo(workspace_id: impl Into<String>) -> Self {
        Self {
            kind: MetricScopeKind::Repo,
            workspace_id: Some(workspace_id.into()),
            branch: None,
            session_id: None,
            user_id: None,
            organization_id: None,
            time_range: None,
        }
    }

    /// Attach a UTC time range.
    pub fn with_time_range(
        mut self,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Self {
        self.time_range = Some(MetricTimeRange { since, until });
        self
    }

    fn time_matches(&self, timestamp: DateTime<Utc>) -> bool {
        let Some(range) = &self.time_range else {
            return true;
        };
        if let Some(since) = range.since {
            if timestamp.unix_seconds() < since.unix_seconds() {
                return false;
            }
        }
        if let Some(until) = range.until {
            if timestamp.unix_seconds() > until.unix_seconds() {
                return false;
            }
        }
        true
    }

    fn missing_dimension_reason(&self) -> Option<String> {
        match self.kind {
            MetricScopeKind::Session if self.session_id.is_none() => {
                Some("session scope requires session_id".to_string())
            }
            MetricScopeKind::Branch if self.workspace_id.is_none() || self.branch.is_none() => {
                Some("branch scope requires workspace_id and branch".to_string())
            }
            MetricScopeKind::Repo if self.workspace_id.is_none() => {
                Some("repo scope requires workspace_id".to_string())
            }
            MetricScopeKind::User if self.user_id.is_none() => {
                Some("user scope requires user_id".to_string())
            }
            MetricScopeKind::Organization if self.organization_id.is_none() => {
                Some("organization scope requires organization_id".to_string())
            }
            _ => None,
        }
    }

    fn matches_event(&self, event: &EventEnvelope) -> bool {
        if !self.time_matches(event.timestamp) {
            return false;
        }
        match self.kind {
            MetricScopeKind::Session => self
                .session_id
                .as_ref()
                .is_some_and(|session_id| session_id == &event.session_id.value),
            MetricScopeKind::Branch => {
                self.workspace_id.as_ref() == Some(&event.workspace_id)
                    && self.branch.as_ref() == Some(&event.branch.name)
            }
            MetricScopeKind::Repo => self.workspace_id.as_ref() == Some(&event.workspace_id),
            MetricScopeKind::User => false,
            MetricScopeKind::Organization => false,
        }
    }

    fn matches_sample(&self, sample: &MetricSampleScope) -> bool {
        if !self.time_matches(sample.observed_at) {
            return false;
        }
        match self.kind {
            MetricScopeKind::Session => self.session_id.as_ref() == sample.session_id.as_ref(),
            MetricScopeKind::Branch => {
                self.workspace_id.as_ref() == Some(&sample.workspace_id)
                    && self.branch.as_ref() == sample.branch.as_ref()
            }
            MetricScopeKind::Repo => self.workspace_id.as_ref() == Some(&sample.workspace_id),
            MetricScopeKind::User => self.user_id.as_ref() == sample.user_id.as_ref(),
            MetricScopeKind::Organization => {
                self.organization_id.as_ref() == sample.organization_id.as_ref()
            }
        }
    }

    fn label(&self) -> &'static str {
        match self.kind {
            MetricScopeKind::Session => "session",
            MetricScopeKind::Branch => "branch",
            MetricScopeKind::Repo => "repo",
            MetricScopeKind::User => "user",
            MetricScopeKind::Organization => "organization",
        }
    }
}

/// One collected metric snapshot.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MetricValue {
    /// Signal name.
    pub signal: MetricSignal,
    /// Metric value when enough evidence exists to compute it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// Optional denominator used for ratio or mean reporting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denominator: Option<u64>,
    /// Number of contributing samples actually processed.
    pub sample_count: u64,
    /// Provenance for this metric.
    pub source: MetricSource,
    /// UTC timestamp when the value was computed.
    pub computed_at: DateTime<Utc>,
    /// Whether the collector truncated evidence due to bounds.
    pub incomplete: bool,
    /// Truthful reason for a null value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_if_null: Option<String>,
}

/// Shared scope metadata for non-event benchmark samples.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MetricSampleScope {
    /// Workspace that owns the sample.
    pub workspace_id: String,
    /// Optional branch for branch-scoped samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Optional session id for session-scoped samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Optional user id for user-scoped samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Optional organization id for organization-scoped samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    /// Optional task id for task-local samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Timestamp used for time-range filtering.
    pub observed_at: DateTime<Utc>,
}

/// One retrieved memory entry enriched with verifier and usefulness facts.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MemorySurfaceRecord {
    /// Scope coordinates for this retrieval entry.
    pub scope: MetricSampleScope,
    /// Event that surfaced the memory.
    pub retrieval_event_id: EventId,
    /// Retrieved memory identifier.
    pub memory_id: MemoryId,
    /// Verification status at retrieval time.
    pub verification_status: VerificationStatus,
    /// Whether the surfaced payload labeled the memory as stale.
    pub stale_label_surfaced: bool,
    /// Whether a contradiction link existed when surfaced.
    pub contradiction_link_present: bool,
    /// Whether the surfaced payload exposed the contradiction.
    pub contradiction_surfaced: bool,
    /// Whether the memory was used downstream after retrieval.
    pub used_downstream: bool,
    /// Whether the memory was reused later in the same or later session window.
    pub reused_later: bool,
}

/// Golden anchor-recall benchmark sample.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AnchorRecallSample {
    /// Scope coordinates for the benchmark sample.
    pub scope: MetricSampleScope,
    /// Golden anchors expected for the task.
    pub golden_anchors: Vec<String>,
    /// Anchors returned by the workflow bundle.
    pub returned_anchors: Vec<String>,
}

/// Golden test-recommendation benchmark sample.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TestRecommendationSample {
    /// Scope coordinates for the benchmark sample.
    pub scope: MetricSampleScope,
    /// Tests recommended by `find_relevant_tests`.
    pub recommended_tests: Vec<String>,
    /// Tests required by the golden answer.
    pub needed_tests: Vec<String>,
}

/// Deterministic bounded collector for Phase 9 metrics.
#[derive(Clone, Debug, Default)]
pub struct MetricsCollector {
    events: Vec<EventEnvelope>,
    memory_surface_records: Vec<MemorySurfaceRecord>,
    anchor_recall_samples: Vec<AnchorRecallSample>,
    test_recommendation_samples: Vec<TestRecommendationSample>,
    max_samples: usize,
    computed_at: Option<DateTime<Utc>>,
}

impl MetricsCollector {
    /// Create an empty collector.
    pub fn new() -> Self {
        Self {
            max_samples: DEFAULT_MAX_SAMPLES,
            ..Self::default()
        }
    }

    /// Replace the event-log evidence used by the collector.
    pub fn with_events(mut self, events: Vec<EventEnvelope>) -> Self {
        self.events = events;
        self
    }

    /// Replace the memory surface evidence used by the collector.
    pub fn with_memory_surface_records(mut self, records: Vec<MemorySurfaceRecord>) -> Self {
        self.memory_surface_records = records;
        self
    }

    /// Replace the golden anchor-recall samples used by the collector.
    pub fn with_anchor_recall_samples(mut self, samples: Vec<AnchorRecallSample>) -> Self {
        self.anchor_recall_samples = samples;
        self
    }

    /// Replace the test-recommendation samples used by the collector.
    pub fn with_test_recommendation_samples(
        mut self,
        samples: Vec<TestRecommendationSample>,
    ) -> Self {
        self.test_recommendation_samples = samples;
        self
    }

    /// Override the maximum number of samples processed per signal.
    pub fn with_max_samples(mut self, max_samples: usize) -> Self {
        self.max_samples = max_samples.max(1);
        self
    }

    /// Override the computed-at timestamp for deterministic tests.
    pub fn with_computed_at(mut self, computed_at: DateTime<Utc>) -> Self {
        self.computed_at = Some(computed_at);
        self
    }

    /// Collect the requested signals for the given scope.
    pub fn collect(&self, scope: MetricScope, signals: &[MetricSignal]) -> Vec<MetricValue> {
        let selected = select_signals(signals);
        selected
            .into_iter()
            .map(|signal| self.collect_signal(&scope, signal))
            .collect()
    }

    fn collect_signal(&self, scope: &MetricScope, signal: MetricSignal) -> MetricValue {
        let source = signal_source(signal);
        let Some(reason) = scope
            .missing_dimension_reason()
            .filter(|_| scope_needs_dimensions(signal))
        else {
            let metric = match signal {
                MetricSignal::ToolCallsPerSuccessfulTask => {
                    self.collect_tool_calls_per_successful_task(scope)
                }
                MetricSignal::IrrelevantFilesOpenedPerTask => {
                    self.collect_irrelevant_files_opened_per_task(scope)
                }
                MetricSignal::RelevantAnchorRecall => self.collect_relevant_anchor_recall(scope),
                MetricSignal::MemoryInclusionPrecision => {
                    self.collect_memory_inclusion_precision(scope)
                }
                MetricSignal::MemoryLaterUsedRate => self.collect_memory_later_used_rate(scope),
                MetricSignal::StaleMemorySurfacedRate => {
                    self.collect_stale_memory_surfaced_rate(scope)
                }
                MetricSignal::ContradictionMissedRate => {
                    self.collect_contradiction_missed_rate(scope)
                }
                MetricSignal::TestsRecommendedVsNeeded => {
                    self.collect_tests_recommended_vs_needed(scope)
                }
                MetricSignal::WorkflowSuccessAfterFirstPlan => {
                    self.collect_workflow_success_after_first_plan(scope)
                }
                // Health signals are produced by replaying git history, not by
                // reading this session's evidence, so the collector reports
                // them as absent-with-a-reason rather than inventing a value.
                // `crate::metrics::health_backtest` is their only producer.
                MetricSignal::HealthDefectPrAuc
                | MetricSignal::HealthDefectRocAuc
                | MetricSignal::HealthDefectFamilyUplift
                | MetricSignal::HealthLabelAuditEnrichment => SignalMetric::null(
                    "health signals come from the backtest harness, not the metrics collector",
                    false,
                ),
            };
            return finalize_metric(metric, signal, source, scope.label(), self.now());
        };
        finalize_metric(
            SignalMetric::null(reason, false),
            signal,
            source,
            scope.label(),
            self.now(),
        )
    }

    fn collect_tool_calls_per_successful_task(&self, scope: &MetricScope) -> SignalMetric {
        let events = bounded_events(&self.events, scope, self.max_samples);
        let tasks = successful_task_metrics(&events.items);
        mean_metric(
            tasks
                .iter()
                .map(|metric| metric.tool_calls as f64)
                .collect(),
            tasks.len() as u64,
            events.incomplete,
            "no successful tasks matched the requested scope",
        )
    }

    fn collect_irrelevant_files_opened_per_task(&self, scope: &MetricScope) -> SignalMetric {
        let events = bounded_events(&self.events, scope, self.max_samples);
        let tasks = successful_task_metrics(&events.items);
        let values = tasks
            .iter()
            .map(|metric| count_irrelevant_file_reads(metric) as f64)
            .collect::<Vec<_>>();
        mean_metric(
            values,
            tasks.len() as u64,
            events.incomplete,
            "no successful tasks matched the requested scope",
        )
    }

    fn collect_relevant_anchor_recall(&self, scope: &MetricScope) -> SignalMetric {
        let samples = bounded_samples(
            &self.anchor_recall_samples,
            scope,
            self.max_samples,
            |sample| &sample.scope,
        );
        ratio_metric(
            samples
                .items
                .iter()
                .map(anchor_recall_counts)
                .fold((0u64, 0u64), accumulate_pair),
            samples.items.len() as u64,
            samples.incomplete,
            "no anchor recall samples matched the requested scope",
        )
    }

    fn collect_memory_inclusion_precision(&self, scope: &MetricScope) -> SignalMetric {
        let records = bounded_samples(
            &self.memory_surface_records,
            scope,
            self.max_samples,
            |record| &record.scope,
        );
        ratio_metric(
            usage_counts(&records.items, |record| record.used_downstream),
            records.items.len() as u64,
            records.incomplete,
            "no retrieved memories matched the requested scope",
        )
    }

    fn collect_memory_later_used_rate(&self, scope: &MetricScope) -> SignalMetric {
        let records = bounded_samples(
            &self.memory_surface_records,
            scope,
            self.max_samples,
            |record| &record.scope,
        );
        ratio_metric(
            usage_counts(&records.items, |record| record.reused_later),
            records.items.len() as u64,
            records.incomplete,
            "no retrieved memories matched the requested scope",
        )
    }

    fn collect_stale_memory_surfaced_rate(&self, scope: &MetricScope) -> SignalMetric {
        let records = bounded_samples(
            &self.memory_surface_records,
            scope,
            self.max_samples,
            |record| &record.scope,
        );
        let counts = filtered_ratio_counts(&records.items, is_stale_record, |record| {
            !record.stale_label_surfaced
        });
        ratio_metric(
            counts,
            counts.1,
            records.incomplete,
            "no stale retrieved memories matched the requested scope",
        )
    }

    fn collect_contradiction_missed_rate(&self, scope: &MetricScope) -> SignalMetric {
        let records = bounded_samples(
            &self.memory_surface_records,
            scope,
            self.max_samples,
            |record| &record.scope,
        );
        let counts = filtered_ratio_counts(&records.items, has_contradiction_link, |record| {
            !record.contradiction_surfaced
        });
        ratio_metric(
            counts,
            counts.1,
            records.incomplete,
            "no contradicted retrieved memories matched the requested scope",
        )
    }

    fn collect_tests_recommended_vs_needed(&self, scope: &MetricScope) -> SignalMetric {
        let samples = bounded_samples(
            &self.test_recommendation_samples,
            scope,
            self.max_samples,
            |sample| &sample.scope,
        );
        ratio_metric(
            samples
                .items
                .iter()
                .map(test_recommendation_counts)
                .fold((0u64, 0u64), accumulate_pair),
            samples.items.len() as u64,
            samples.incomplete,
            "no test recommendation samples matched the requested scope",
        )
    }

    fn collect_workflow_success_after_first_plan(&self, scope: &MetricScope) -> SignalMetric {
        let events = bounded_events(&self.events, scope, self.max_samples);
        let tasks = planned_task_metrics(&events.items);
        let succeeded = tasks
            .iter()
            .filter(|metric| metric.succeeded_after_first_plan)
            .count() as u64;
        ratio_metric(
            (succeeded, tasks.len() as u64),
            tasks.len() as u64,
            events.incomplete,
            "no planned tasks matched the requested scope",
        )
    }

    fn now(&self) -> DateTime<Utc> {
        self.computed_at.unwrap_or_else(Utc::now)
    }
}

#[derive(Clone, Debug)]
struct SignalMetric {
    value: Option<f64>,
    denominator: Option<u64>,
    sample_count: u64,
    incomplete: bool,
    reason_if_null: Option<String>,
}

impl SignalMetric {
    fn null(reason: impl Into<String>, incomplete: bool) -> Self {
        Self {
            value: None,
            denominator: None,
            sample_count: 0,
            incomplete,
            reason_if_null: Some(reason.into()),
        }
    }
}

#[derive(Clone, Debug)]
struct BoundedData<T> {
    items: Vec<T>,
    incomplete: bool,
}

#[derive(Clone, Debug)]
struct SuccessfulTaskMetric {
    tool_calls: usize,
    read_files: BTreeSet<String>,
    changed_files: BTreeSet<String>,
    artifact_files: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct PlannedTaskMetric {
    succeeded_after_first_plan: bool,
}

fn finalize_metric(
    metric: SignalMetric,
    signal: MetricSignal,
    source: MetricSource,
    scope_label: &str,
    computed_at: DateTime<Utc>,
) -> MetricValue {
    let _span = debug_span!(
        "metrics.collect_signal",
        signal = signal.as_str(),
        scope = scope_label,
        sample_count = metric.sample_count,
        incomplete = metric.incomplete
    )
    .entered();
    MetricValue {
        signal,
        value: metric.value,
        denominator: metric.denominator,
        sample_count: metric.sample_count,
        source,
        computed_at,
        incomplete: metric.incomplete,
        reason_if_null: metric.reason_if_null,
    }
}

fn select_signals(signals: &[MetricSignal]) -> Vec<MetricSignal> {
    if signals.is_empty() {
        return MetricSignal::ALL.to_vec();
    }
    signals.to_vec()
}

fn signal_source(signal: MetricSignal) -> MetricSource {
    match signal {
        MetricSignal::ToolCallsPerSuccessfulTask => MetricSource::EventLog,
        MetricSignal::IrrelevantFilesOpenedPerTask => MetricSource::EventLog,
        MetricSignal::RelevantAnchorRecall => MetricSource::EventLog,
        MetricSignal::MemoryInclusionPrecision => MetricSource::MemoryStore,
        MetricSignal::MemoryLaterUsedRate => MetricSource::MemoryStore,
        MetricSignal::StaleMemorySurfacedRate => MetricSource::Verifier,
        MetricSignal::ContradictionMissedRate => MetricSource::Verifier,
        MetricSignal::TestsRecommendedVsNeeded => MetricSource::WorkflowOutcome,
        MetricSignal::WorkflowSuccessAfterFirstPlan => MetricSource::WorkflowOutcome,
        MetricSignal::HealthDefectPrAuc
        | MetricSignal::HealthDefectRocAuc
        | MetricSignal::HealthDefectFamilyUplift
        | MetricSignal::HealthLabelAuditEnrichment => MetricSource::HealthBacktest,
    }
}

/// Whether a signal is meaningless without session/branch scope dimensions.
///
/// The health signals are properties of a repository's whole replayed history,
/// so a missing session dimension does not make them unmeasurable the way it
/// does for the event-log signals.
fn scope_needs_dimensions(signal: MetricSignal) -> bool {
    !signal.is_health()
}

fn bounded_events(
    events: &[EventEnvelope],
    scope: &MetricScope,
    max_samples: usize,
) -> BoundedData<EventEnvelope> {
    let filtered = events
        .iter()
        .filter(|event| scope.matches_event(event))
        .cloned()
        .collect::<Vec<_>>();
    truncate_items(filtered, max_samples)
}

fn bounded_samples<T: Clone, F>(
    samples: &[T],
    scope: &MetricScope,
    max_samples: usize,
    sample_scope: F,
) -> BoundedData<T>
where
    F: Fn(&T) -> &MetricSampleScope,
{
    let filtered = samples
        .iter()
        .filter(|sample| scope.matches_sample(sample_scope(sample)))
        .cloned()
        .collect::<Vec<_>>();
    truncate_items(filtered, max_samples)
}

fn truncate_items<T>(items: Vec<T>, max_samples: usize) -> BoundedData<T> {
    let incomplete = items.len() > max_samples;
    let items = items.into_iter().take(max_samples).collect();
    BoundedData { items, incomplete }
}

fn successful_task_metrics(events: &[EventEnvelope]) -> Vec<SuccessfulTaskMetric> {
    let groups = group_events_by_task(events);
    groups
        .into_values()
        .filter_map(|task_events| successful_task_metric(&task_events))
        .collect()
}

fn successful_task_metric(events: &[EventEnvelope]) -> Option<SuccessfulTaskMetric> {
    if !is_successful_task(events) {
        return None;
    }
    let tool_calls = events
        .iter()
        .filter(|event| event.kind == EventKind::ToolCalled)
        .count();
    let read_files = collect_file_reads(events);
    let changed_files = collect_patch_files(events);
    let artifact_files = collect_workflow_artifact_files(events);
    Some(SuccessfulTaskMetric {
        tool_calls,
        read_files,
        changed_files,
        artifact_files,
    })
}

fn planned_task_metrics(events: &[EventEnvelope]) -> Vec<PlannedTaskMetric> {
    group_events_by_task(events)
        .into_values()
        .filter_map(|task_events| planned_task_metric(&task_events))
        .collect()
}

fn planned_task_metric(events: &[EventEnvelope]) -> Option<PlannedTaskMetric> {
    let first_plan_index = events
        .iter()
        .position(|event| event.kind == EventKind::PlanCreated)?;
    let has_failure_after_plan = events[first_plan_index + 1..]
        .iter()
        .any(|event| event.kind == EventKind::WorkflowFailed);
    let has_success_after_plan = events[first_plan_index + 1..]
        .iter()
        .any(|event| event.kind == EventKind::WorkflowSucceeded);
    Some(PlannedTaskMetric {
        succeeded_after_first_plan: has_success_after_plan && !has_failure_after_plan,
    })
}

fn group_events_by_task(events: &[EventEnvelope]) -> BTreeMap<String, Vec<EventEnvelope>> {
    let mut groups = BTreeMap::new();
    for event in events.iter().filter(|event| event.task_id.is_some()) {
        let task_id = event
            .task_id
            .as_ref()
            .map(|task_id| task_id.value.clone())
            .unwrap_or_default();
        groups
            .entry(task_id)
            .or_insert_with(Vec::new)
            .push(event.clone());
    }
    groups
}

fn is_successful_task(events: &[EventEnvelope]) -> bool {
    let mut terminal: Option<EventKind> = None;
    for event in events {
        if matches!(
            event.kind,
            EventKind::WorkflowSucceeded | EventKind::WorkflowFailed
        ) {
            terminal = Some(event.kind);
        }
    }
    terminal == Some(EventKind::WorkflowSucceeded)
}

fn collect_file_reads(events: &[EventEnvelope]) -> BTreeSet<String> {
    events
        .iter()
        .filter_map(read_file_path)
        .collect::<BTreeSet<_>>()
}

fn collect_patch_files(events: &[EventEnvelope]) -> BTreeSet<String> {
    events
        .iter()
        .flat_map(patch_file_paths)
        .collect::<BTreeSet<_>>()
}

fn collect_workflow_artifact_files(events: &[EventEnvelope]) -> BTreeSet<String> {
    let successful_memory_ids = events
        .iter()
        .filter_map(success_payload)
        .flat_map(|payload| payload.memory_ids.iter().cloned())
        .collect::<HashSet<_>>();
    let mut files = BTreeSet::new();
    for event in events {
        if let EventPayload::MemoryRetrieved(payload) = &event.payload {
            if payload
                .memory_ids
                .iter()
                .any(|memory_id| successful_memory_ids.contains(memory_id))
            {
                files.extend(identity_file_paths(&payload.included_context));
            }
        }
        if let EventPayload::MemoryExpanded(payload) = &event.payload {
            if payload
                .memory_id
                .as_ref()
                .is_some_and(|memory_id| successful_memory_ids.contains(memory_id))
            {
                files.extend(identity_file_paths(&payload.included_context));
            }
        }
    }
    files
}

fn read_file_path(event: &EventEnvelope) -> Option<String> {
    let EventPayload::FileRead(FileReadPayload { file_id, .. }) = &event.payload else {
        return None;
    };
    Some(file_id.repo_relative_path.clone())
}

fn patch_file_paths(event: &EventEnvelope) -> Vec<String> {
    let EventPayload::PatchApplied(PatchAppliedPayload { file_ids, .. }) = &event.payload else {
        return Vec::new();
    };
    file_ids
        .iter()
        .map(|file_id| file_id.repo_relative_path.clone())
        .collect()
}

fn success_payload(event: &EventEnvelope) -> Option<&WorkflowSucceededPayload> {
    let EventPayload::WorkflowSucceeded(payload) = &event.payload else {
        return None;
    };
    Some(payload)
}

fn identity_file_paths(
    included_context: &[crate::events::IncludedContextDelta],
) -> BTreeSet<String> {
    included_context
        .iter()
        .filter_map(|delta| match &delta.identity {
            crate::identity::Identity::File(file_id) => Some(file_id.repo_relative_path.clone()),
            crate::identity::Identity::Symbol(symbol_id) => {
                Some(symbol_id.file.repo_relative_path.clone())
            }
            _ => None,
        })
        .collect()
}

fn count_irrelevant_file_reads(task: &SuccessfulTaskMetric) -> usize {
    task.read_files
        .iter()
        .filter(|path| !task.changed_files.contains(*path) && !task.artifact_files.contains(*path))
        .count()
}

fn anchor_recall_counts(sample: &AnchorRecallSample) -> (u64, u64) {
    let returned = sample
        .returned_anchors
        .iter()
        .map(|anchor| anchor.as_str())
        .collect::<HashSet<_>>();
    let hits = sample
        .golden_anchors
        .iter()
        .filter(|anchor| returned.contains(anchor.as_str()))
        .count() as u64;
    (hits, sample.golden_anchors.len() as u64)
}

fn usage_counts<F>(records: &[MemorySurfaceRecord], predicate: F) -> (u64, u64)
where
    F: Fn(&MemorySurfaceRecord) -> bool,
{
    let hits = records.iter().filter(|record| predicate(record)).count() as u64;
    (hits, records.len() as u64)
}

fn filtered_ratio_counts<F, G>(records: &[MemorySurfaceRecord], include: F, hit: G) -> (u64, u64)
where
    F: Fn(&MemorySurfaceRecord) -> bool,
    G: Fn(&MemorySurfaceRecord) -> bool,
{
    let filtered = records
        .iter()
        .filter(|record| include(record))
        .collect::<Vec<_>>();
    let hits = filtered.iter().filter(|record| hit(record)).count() as u64;
    (hits, filtered.len() as u64)
}

fn is_stale_record(record: &MemorySurfaceRecord) -> bool {
    record.verification_status == VerificationStatus::Stale
}

fn has_contradiction_link(record: &MemorySurfaceRecord) -> bool {
    record.contradiction_link_present
}

fn test_recommendation_counts(sample: &TestRecommendationSample) -> (u64, u64) {
    let recommended = sample
        .recommended_tests
        .iter()
        .map(|test| test.as_str())
        .collect::<HashSet<_>>();
    let hits = sample
        .needed_tests
        .iter()
        .filter(|test| recommended.contains(test.as_str()))
        .count() as u64;
    (hits, sample.needed_tests.len() as u64)
}

fn accumulate_pair(acc: (u64, u64), item: (u64, u64)) -> (u64, u64) {
    (acc.0 + item.0, acc.1 + item.1)
}

fn ratio_metric(
    counts: (u64, u64),
    sample_count: u64,
    incomplete: bool,
    no_data_reason: &str,
) -> SignalMetric {
    if counts.1 == 0 {
        return SignalMetric::null(no_data_reason, incomplete);
    }
    SignalMetric {
        value: Some(counts.0 as f64 / counts.1 as f64),
        denominator: Some(counts.1),
        sample_count,
        incomplete,
        reason_if_null: None,
    }
}

fn mean_metric(
    values: Vec<f64>,
    denominator: u64,
    incomplete: bool,
    no_data_reason: &str,
) -> SignalMetric {
    if values.is_empty() {
        return SignalMetric::null(no_data_reason, incomplete);
    }
    let total = values.iter().sum::<f64>();
    SignalMetric {
        value: Some(total / values.len() as f64),
        denominator: Some(denominator),
        sample_count: values.len() as u64,
        incomplete,
        reason_if_null: None,
    }
}
