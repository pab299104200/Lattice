use crate::events::hashing::PayloadHash;
use crate::events::{
    Actor, BranchRef, CompactSummary, EventEnvelope, EventKind, EventPayload, PayloadLocation,
    SessionId, TaskId, WorkflowSucceededPayload,
};
use crate::identity::{EventId, FileId, MemoryId, WorkspaceId};
use crate::metrics::{
    AnchorRecallSample, MemorySurfaceRecord, MetricSampleScope, MetricScope, MetricSignal,
    MetricsCollector, TestRecommendationSample,
};
use crate::verification::VerificationStatus;
use crate::{DateTime, Utc};

#[test]
fn every_metric_signal_variant_collects_deterministically() {
    let collector = fixture_collector();
    let metrics = collector.collect(scope_session_a(), &MetricSignal::ALL);
    assert_eq!(metrics.len(), MetricSignal::ALL.len());
    for (metric, signal) in metrics.iter().zip(MetricSignal::ALL.iter()) {
        assert_eq!(&metric.signal, signal);
        assert_eq!(metric.computed_at, fixed_time(100));
    }
}

#[test]
fn collectors_report_honest_nulls_when_scope_has_no_data() {
    let collector = MetricsCollector::new().with_computed_at(fixed_time(100));
    for metric in collector.collect(scope_session_a(), &MetricSignal::ALL) {
        assert_eq!(metric.value, None);
        assert!(metric.reason_if_null.is_some());
    }
}

#[test]
fn collectors_return_expected_values_for_handcrafted_fixture() {
    let collector = fixture_collector();
    let metrics = collector.collect(scope_session_a(), &MetricSignal::ALL);
    assert_metric(&metrics, MetricSignal::ToolCallsPerSuccessfulTask, 2.0, 1);
    assert_metric(&metrics, MetricSignal::IrrelevantFilesOpenedPerTask, 1.0, 1);
    assert_metric(&metrics, MetricSignal::RelevantAnchorRecall, 2.0 / 3.0, 3);
    assert_metric(&metrics, MetricSignal::MemoryInclusionPrecision, 0.5, 2);
    assert_metric(&metrics, MetricSignal::MemoryLaterUsedRate, 0.5, 2);
    assert_metric(&metrics, MetricSignal::StaleMemorySurfacedRate, 1.0, 1);
    assert_metric(&metrics, MetricSignal::ContradictionMissedRate, 1.0, 1);
    assert_metric(&metrics, MetricSignal::TestsRecommendedVsNeeded, 0.5, 2);
    assert_metric(
        &metrics,
        MetricSignal::WorkflowSuccessAfterFirstPlan,
        1.0,
        1,
    );
}

#[test]
fn scope_filters_prevent_cross_scope_leakage() {
    let collector = fixture_collector();
    let session_metrics = collector.collect(
        scope_session_b(),
        &[MetricSignal::ToolCallsPerSuccessfulTask],
    );
    let repo_metrics = collector.collect(
        MetricScope::repo("workspace-a"),
        &[MetricSignal::ToolCallsPerSuccessfulTask],
    );
    assert_eq!(session_metrics[0].value, Some(1.0));
    assert_eq!(repo_metrics[0].value, Some(1.5));
}

#[test]
fn sample_size_bounds_mark_metrics_incomplete() {
    let collector = fixture_collector().with_max_samples(7);
    let metric = collector.collect(
        MetricScope::repo("workspace-a"),
        &[MetricSignal::ToolCallsPerSuccessfulTask],
    );
    assert_eq!(metric[0].value, Some(2.0));
    assert!(metric[0].incomplete);
    assert_eq!(metric[0].sample_count, 1);
}

fn assert_metric(
    metrics: &[crate::metrics::MetricValue],
    signal: MetricSignal,
    expected_value: f64,
    expected_denominator: u64,
) {
    let metric = metrics
        .iter()
        .find(|metric| metric.signal == signal)
        .expect("missing metric");
    assert_eq!(metric.value, Some(expected_value));
    assert_eq!(metric.denominator, Some(expected_denominator));
}

fn fixture_collector() -> MetricsCollector {
    MetricsCollector::new()
        .with_events(vec![
            tool_called(
                "workspace-a",
                "main",
                "session-a",
                "task-a",
                "prepare_change",
                1,
            ),
            tool_called(
                "workspace-a",
                "main",
                "session-a",
                "task-a",
                "find_relevant_tests",
                2,
            ),
            file_read(
                "workspace-a",
                "main",
                "session-a",
                "task-a",
                "src/relevant.rs",
                3,
            ),
            file_read(
                "workspace-a",
                "main",
                "session-a",
                "task-a",
                "src/noise.rs",
                4,
            ),
            patch_applied(
                "workspace-a",
                "main",
                "session-a",
                "task-a",
                &["src/relevant.rs"],
                5,
            ),
            plan_created("workspace-a", "main", "session-a", "task-a", 6),
            workflow_succeeded("workspace-a", "main", "session-a", "task-a", &["mem-a"], 7),
            tool_called(
                "workspace-a",
                "main",
                "session-b",
                "task-b",
                "prepare_change",
                8,
            ),
            workflow_failed("workspace-a", "main", "session-b", "task-b", 9),
            tool_called(
                "workspace-a",
                "main",
                "session-b",
                "task-c",
                "prepare_change",
                10,
            ),
            plan_created("workspace-a", "main", "session-b", "task-c", 11),
            workflow_succeeded("workspace-a", "main", "session-b", "task-c", &["mem-b"], 12),
        ])
        .with_memory_surface_records(vec![
            MemorySurfaceRecord {
                scope: sample_scope("workspace-a", "main", "session-a", "task-a", 13),
                retrieval_event_id: event_id("workspace-a", "evt-13"),
                memory_id: memory_id("workspace-a", "mem-a"),
                verification_status: VerificationStatus::Verified,
                stale_label_surfaced: false,
                contradiction_link_present: false,
                contradiction_surfaced: false,
                used_downstream: true,
                reused_later: false,
            },
            MemorySurfaceRecord {
                scope: sample_scope("workspace-a", "main", "session-a", "task-a", 14),
                retrieval_event_id: event_id("workspace-a", "evt-14"),
                memory_id: memory_id("workspace-a", "mem-stale"),
                verification_status: VerificationStatus::Stale,
                stale_label_surfaced: false,
                contradiction_link_present: true,
                contradiction_surfaced: false,
                used_downstream: false,
                reused_later: true,
            },
        ])
        .with_anchor_recall_samples(vec![AnchorRecallSample {
            scope: sample_scope("workspace-a", "main", "session-a", "task-a", 15),
            golden_anchors: vec![
                "src/relevant.rs".to_string(),
                "login_user".to_string(),
                "prepare_change".to_string(),
            ],
            returned_anchors: vec!["src/relevant.rs".to_string(), "prepare_change".to_string()],
        }])
        .with_test_recommendation_samples(vec![TestRecommendationSample {
            scope: sample_scope("workspace-a", "main", "session-a", "task-a", 16),
            recommended_tests: vec!["tests/auth.rs".to_string()],
            needed_tests: vec!["tests/auth.rs".to_string(), "tests/session.rs".to_string()],
        }])
        .with_computed_at(fixed_time(100))
}

fn scope_session_a() -> MetricScope {
    MetricScope::session("session-a")
}

fn scope_session_b() -> MetricScope {
    MetricScope::session("session-b")
}

fn tool_called(
    workspace: &str,
    branch: &str,
    session: &str,
    task: &str,
    tool_name: &str,
    second: i64,
) -> EventEnvelope {
    event(
        workspace,
        branch,
        session,
        task,
        second,
        EventKind::ToolCalled,
        EventPayload::ToolCalled(crate::events::ToolCalledPayload {
            call_id: format!("call-{second}"),
            tool_name: tool_name.to_string(),
            context_handle_id: None,
            source_event_id: None,
            input_summary: tool_name.to_string(),
        }),
    )
}

fn file_read(
    workspace: &str,
    branch: &str,
    session: &str,
    task: &str,
    path: &str,
    second: i64,
) -> EventEnvelope {
    event(
        workspace,
        branch,
        session,
        task,
        second,
        EventKind::FileRead,
        EventPayload::FileRead(crate::events::FileReadPayload {
            file_id: file_id(workspace, path),
            source_event_id: None,
            byte_start: None,
            byte_end: None,
            reason: "inspect".to_string(),
        }),
    )
}

fn patch_applied(
    workspace: &str,
    branch: &str,
    session: &str,
    task: &str,
    paths: &[&str],
    second: i64,
) -> EventEnvelope {
    event(
        workspace,
        branch,
        session,
        task,
        second,
        EventKind::PatchApplied,
        EventPayload::PatchApplied(crate::events::PatchAppliedPayload {
            patch_id: format!("patch-{second}"),
            source_event_id: None,
            file_ids: paths.iter().map(|path| file_id(workspace, path)).collect(),
            symbol_ids: Vec::new(),
            lines_added: 1,
            lines_removed: 0,
        }),
    )
}

fn plan_created(
    workspace: &str,
    branch: &str,
    session: &str,
    task: &str,
    second: i64,
) -> EventEnvelope {
    event(
        workspace,
        branch,
        session,
        task,
        second,
        EventKind::PlanCreated,
        EventPayload::PlanCreated(crate::events::PlanCreatedPayload {
            context_handle_id: None,
            source_event_id: None,
            memory_ids: Vec::new(),
            step_count: 2,
            plan_summary: "plan".to_string(),
        }),
    )
}

fn workflow_succeeded(
    workspace: &str,
    branch: &str,
    session: &str,
    task: &str,
    memories: &[&str],
    second: i64,
) -> EventEnvelope {
    event(
        workspace,
        branch,
        session,
        task,
        second,
        EventKind::WorkflowSucceeded,
        EventPayload::WorkflowSucceeded(WorkflowSucceededPayload {
            workflow_name: "workflow".to_string(),
            terminal_event_id: None,
            output_context_handle_id: None,
            memory_ids: memories.iter().map(|id| memory_id(workspace, id)).collect(),
            result_summary: "done".to_string(),
        }),
    )
}

fn workflow_failed(
    workspace: &str,
    branch: &str,
    session: &str,
    task: &str,
    second: i64,
) -> EventEnvelope {
    event(
        workspace,
        branch,
        session,
        task,
        second,
        EventKind::WorkflowFailed,
        EventPayload::WorkflowFailed(crate::events::WorkflowFailedPayload {
            workflow_name: "workflow".to_string(),
            terminal_event_id: None,
            diagnostic_event_ids: Vec::new(),
            retryable: false,
            failure_summary: "failed".to_string(),
        }),
    )
}

fn event(
    workspace: &str,
    branch: &str,
    session: &str,
    task: &str,
    second: i64,
    kind: EventKind,
    payload: EventPayload,
) -> EventEnvelope {
    EventEnvelope::new(
        event_id(workspace, &format!("evt-{second}")),
        WorkspaceId::from(workspace.to_string()),
        BranchRef {
            name: branch.to_string(),
        },
        SessionId {
            value: session.to_string(),
        },
        Some(TaskId {
            value: task.to_string(),
        }),
        Actor::Assistant {
            model: "gpt".to_string(),
        },
        fixed_time(second),
        kind,
        Vec::new(),
        PayloadHash::new([second as u8; 32]),
        CompactSummary::new(format!("event-{second}")).expect("summary"),
        PayloadLocation::Inline { bytes_len: 16 },
        payload,
    )
    .expect("event envelope")
}

fn sample_scope(
    workspace: &str,
    branch: &str,
    session: &str,
    task: &str,
    second: i64,
) -> MetricSampleScope {
    MetricSampleScope {
        workspace_id: workspace.to_string(),
        branch: Some(branch.to_string()),
        session_id: Some(session.to_string()),
        user_id: None,
        organization_id: None,
        task_id: Some(task.to_string()),
        observed_at: fixed_time(second),
    }
}

fn fixed_time(second: i64) -> DateTime<Utc> {
    DateTime::from_unix_seconds(second)
}

fn event_id(workspace: &str, ulid: &str) -> EventId {
    EventId {
        workspace_id: workspace.to_string(),
        ulid: ulid.to_string(),
    }
}

fn file_id(workspace: &str, path: &str) -> FileId {
    FileId {
        workspace_id: workspace.to_string(),
        repo_relative_path: path.to_string(),
        content_hash: format!("hash-{path}"),
    }
}

fn memory_id(workspace: &str, ulid: &str) -> MemoryId {
    MemoryId {
        workspace_id: workspace.to_string(),
        ulid: ulid.to_string(),
    }
}
