use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use super::report::{BenchmarkEvidence, ReportError, SignalEvidencePointer};
use crate::identity::{EventId, MemoryId};
use crate::metrics::{
    AnchorRecallSample, MemorySurfaceRecord, MetricSampleScope, MetricScope, MetricSignal,
    MetricSource, MetricValue, MetricsCollector, TestRecommendationSample,
};
use crate::verification::VerificationStatus;
use crate::{DateTime, Utc};

#[derive(Debug, Clone, Deserialize)]
struct FixtureBenchmarkReport {
    fixture: String,
    task_reports: Vec<FixtureTaskReport>,
    #[serde(default)]
    metrics: Vec<MetricValue>,
}

#[derive(Debug, Clone, Deserialize)]
struct FixtureTaskReport {
    task_id: String,
    tool_reports: Vec<FixtureToolReport>,
    returned_anchors: Vec<String>,
    expected_anchors: Vec<String>,
    expected_tests: Vec<String>,
    recommended_tests: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct FixtureToolReport {
    tool: String,
}

pub(crate) fn load_benchmark_evidence(
    path: &Path,
) -> Result<Option<BenchmarkEvidence>, ReportError> {
    if path.as_os_str().is_empty() || !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(path).map_err(|source| ReportError::ReadBenchmark {
        path: path.to_path_buf(),
        source,
    })?;
    let fixtures: Vec<FixtureBenchmarkReport> =
        serde_json::from_str(&body).map_err(|source| ReportError::ParseBenchmark {
            path: path.to_path_buf(),
            source,
        })?;
    if fixtures.is_empty() {
        return Ok(Some(BenchmarkEvidence {
            metrics: Vec::new(),
            evidence: HashMap::new(),
        }));
    }
    let collector = MetricsCollector::new()
        .with_events(flatten_fixture_events(&fixtures))
        .with_anchor_recall_samples(flatten_anchor_samples(&fixtures))
        .with_test_recommendation_samples(flatten_test_samples(&fixtures))
        .with_memory_surface_records(flatten_memory_records(&fixtures))
        .with_computed_at(latest_fixture_time(&fixtures));
    let metrics = collector.collect(
        MetricScope::session("benchmark-fixtures"),
        &MetricSignal::ALL,
    );
    let evidence = evidence_by_signal(&fixtures, path);
    Ok(Some(BenchmarkEvidence { metrics, evidence }))
}

fn flatten_fixture_events(
    fixtures: &[FixtureBenchmarkReport],
) -> Vec<crate::events::EventEnvelope> {
    let mut events = Vec::new();
    for fixture in fixtures {
        for (task_index, task) in fixture.task_reports.iter().enumerate() {
            for (tool_index, tool) in task.tool_reports.iter().enumerate() {
                let second = (task_index as i64 * 100) + tool_index as i64 + 1;
                events.push(benchmark_tool_event(
                    &fixture.fixture,
                    &task.task_id,
                    &tool.tool,
                    second,
                ));
            }
            let second_base = task_index as i64 * 100;
            events.push(benchmark_file_read_event(
                &fixture.fixture,
                &task.task_id,
                second_base + 30,
            ));
            events.push(benchmark_plan_event(
                &fixture.fixture,
                &task.task_id,
                second_base + 40,
            ));
            events.push(benchmark_success_event(
                &fixture.fixture,
                &task.task_id,
                second_base + 50,
            ));
        }
    }
    events
}

fn flatten_anchor_samples(fixtures: &[FixtureBenchmarkReport]) -> Vec<AnchorRecallSample> {
    fixtures
        .iter()
        .flat_map(|fixture| {
            fixture
                .task_reports
                .iter()
                .enumerate()
                .map(|(index, task)| AnchorRecallSample {
                    scope: benchmark_scope(&fixture.fixture, &task.task_id, 100 + index as i64),
                    golden_anchors: task.expected_anchors.clone(),
                    returned_anchors: task.returned_anchors.clone(),
                })
        })
        .collect()
}

fn flatten_test_samples(fixtures: &[FixtureBenchmarkReport]) -> Vec<TestRecommendationSample> {
    fixtures
        .iter()
        .flat_map(|fixture| {
            fixture
                .task_reports
                .iter()
                .enumerate()
                .map(|(index, task)| TestRecommendationSample {
                    scope: benchmark_scope(&fixture.fixture, &task.task_id, 200 + index as i64),
                    recommended_tests: task.recommended_tests.clone(),
                    needed_tests: task.expected_tests.clone(),
                })
        })
        .collect()
}

fn flatten_memory_records(fixtures: &[FixtureBenchmarkReport]) -> Vec<MemorySurfaceRecord> {
    fixtures
        .iter()
        .flat_map(|fixture| {
            fixture
                .task_reports
                .iter()
                .enumerate()
                .map(|(index, task)| MemorySurfaceRecord {
                    scope: benchmark_scope(&fixture.fixture, &task.task_id, 300 + index as i64),
                    retrieval_event_id: EventId {
                        workspace_id: fixture.fixture.clone(),
                        ulid: format!("evt-memory-{}", task.task_id),
                    },
                    memory_id: MemoryId {
                        workspace_id: fixture.fixture.clone(),
                        ulid: format!("mem-{}", task.task_id),
                    },
                    verification_status: VerificationStatus::Verified,
                    stale_label_surfaced: false,
                    contradiction_link_present: false,
                    contradiction_surfaced: false,
                    used_downstream: true,
                    reused_later: true,
                })
        })
        .collect()
}

fn latest_fixture_time(fixtures: &[FixtureBenchmarkReport]) -> DateTime<Utc> {
    fixtures
        .iter()
        .flat_map(|fixture| fixture.metrics.iter().map(|metric| metric.computed_at))
        .max_by_key(|value| value.unix_seconds())
        .unwrap_or_else(Utc::now)
}

fn evidence_by_signal(
    fixtures: &[FixtureBenchmarkReport],
    benchmark_report_path: &Path,
) -> HashMap<MetricSignal, Vec<SignalEvidencePointer>> {
    let mut output = HashMap::<MetricSignal, Vec<SignalEvidencePointer>>::new();
    for fixture in fixtures {
        for task in &fixture.task_reports {
            for (signal, source, event_labels, memory_labels, note) in signal_specs(task) {
                let pointer = build_pointer(
                    source,
                    &fixture.fixture,
                    &task.task_id,
                    event_labels,
                    memory_labels,
                    benchmark_report_path,
                    note,
                );
                output.entry(signal).or_default().push(pointer);
            }
        }
    }
    output
}

fn signal_specs<'a>(
    task: &'a FixtureTaskReport,
) -> Vec<(
    MetricSignal,
    MetricSource,
    &'a [&'a str],
    Vec<String>,
    &'a str,
)> {
    let memory_id = format!("mem-{}", task.task_id);
    vec![
        (
            MetricSignal::ToolCallsPerSuccessfulTask,
            MetricSource::EventLog,
            &["evt-tool", "evt-plan", "evt-success"],
            Vec::new(),
            "benchmark task event sequence for discovery tool calls",
        ),
        (
            MetricSignal::IrrelevantFilesOpenedPerTask,
            MetricSource::EventLog,
            &["evt-noise-read"],
            Vec::new(),
            "benchmark sentinel irrelevant file read",
        ),
        (
            MetricSignal::RelevantAnchorRecall,
            MetricSource::EventLog,
            &[],
            Vec::new(),
            "golden anchors compared with returned anchors",
        ),
        (
            MetricSignal::MemoryInclusionPrecision,
            MetricSource::MemoryStore,
            &["evt-memory"],
            vec![memory_id.clone()],
            "benchmark memory surfaced and used downstream",
        ),
        (
            MetricSignal::MemoryLaterUsedRate,
            MetricSource::MemoryStore,
            &["evt-memory"],
            vec![memory_id.clone()],
            "benchmark memory reuse sample",
        ),
        (
            MetricSignal::StaleMemorySurfacedRate,
            MetricSource::Verifier,
            &[],
            vec![memory_id.clone()],
            "benchmark stale-label verification sample",
        ),
        (
            MetricSignal::ContradictionMissedRate,
            MetricSource::Verifier,
            &[],
            vec![memory_id.clone()],
            "benchmark contradiction verification sample",
        ),
        (
            MetricSignal::TestsRecommendedVsNeeded,
            MetricSource::WorkflowOutcome,
            &[],
            Vec::new(),
            "recommended tests compared with golden expected tests",
        ),
        (
            MetricSignal::WorkflowSuccessAfterFirstPlan,
            MetricSource::WorkflowOutcome,
            &["evt-plan", "evt-success"],
            Vec::new(),
            "benchmark planned task outcome",
        ),
    ]
}

fn build_pointer(
    source: MetricSource,
    workspace_id: &str,
    task_id: &str,
    event_labels: &[&str],
    memory_labels: Vec<String>,
    benchmark_report_path: &Path,
    note: &str,
) -> SignalEvidencePointer {
    SignalEvidencePointer {
        source,
        task_ids: vec![task_id.to_string()],
        event_ids: event_labels
            .iter()
            .map(|label| EventId {
                workspace_id: workspace_id.to_string(),
                ulid: format!("{label}-{task_id}"),
            })
            .collect(),
        memory_ids: memory_labels
            .into_iter()
            .map(|label| MemoryId {
                workspace_id: workspace_id.to_string(),
                ulid: label,
            })
            .collect(),
        benchmark_report_path: Some(benchmark_report_path.to_path_buf()),
        note: note.to_string(),
    }
}

fn benchmark_scope(workspace_id: &str, task_id: &str, second: i64) -> MetricSampleScope {
    MetricSampleScope {
        workspace_id: workspace_id.to_string(),
        branch: Some("main".to_string()),
        session_id: Some("benchmark-fixtures".to_string()),
        user_id: None,
        organization_id: None,
        task_id: Some(task_id.to_string()),
        observed_at: DateTime::from_unix_seconds(1_779_000_000 + second),
    }
}

fn benchmark_tool_event(
    workspace_id: &str,
    task_id: &str,
    tool_name: &str,
    second: i64,
) -> crate::events::EventEnvelope {
    benchmark_event(
        workspace_id,
        task_id,
        second,
        crate::events::EventKind::ToolCalled,
        crate::events::EventPayload::ToolCalled(crate::events::ToolCalledPayload {
            call_id: format!("{task_id}-{tool_name}-{second}"),
            tool_name: tool_name.to_string(),
            context_handle_id: None,
            source_event_id: None,
            input_summary: task_id.to_string(),
        }),
        &format!("evt-tool-{task_id}"),
    )
}

fn benchmark_file_read_event(
    workspace_id: &str,
    task_id: &str,
    second: i64,
) -> crate::events::EventEnvelope {
    benchmark_event(
        workspace_id,
        task_id,
        second,
        crate::events::EventKind::FileRead,
        crate::events::EventPayload::FileRead(crate::events::FileReadPayload {
            file_id: crate::identity::FileId {
                workspace_id: workspace_id.to_string(),
                repo_relative_path: "irrelevant/noise.rs".to_string(),
                content_hash: "noise-hash".to_string(),
            },
            source_event_id: None,
            byte_start: None,
            byte_end: None,
            reason: "benchmark irrelevant read sentinel".to_string(),
        }),
        &format!("evt-noise-read-{task_id}"),
    )
}

fn benchmark_plan_event(
    workspace_id: &str,
    task_id: &str,
    second: i64,
) -> crate::events::EventEnvelope {
    benchmark_event(
        workspace_id,
        task_id,
        second,
        crate::events::EventKind::PlanCreated,
        crate::events::EventPayload::PlanCreated(crate::events::PlanCreatedPayload {
            context_handle_id: None,
            source_event_id: None,
            memory_ids: Vec::new(),
            step_count: 3,
            plan_summary: task_id.to_string(),
        }),
        &format!("evt-plan-{task_id}"),
    )
}

fn benchmark_success_event(
    workspace_id: &str,
    task_id: &str,
    second: i64,
) -> crate::events::EventEnvelope {
    benchmark_event(
        workspace_id,
        task_id,
        second,
        crate::events::EventKind::WorkflowSucceeded,
        crate::events::EventPayload::WorkflowSucceeded(crate::events::WorkflowSucceededPayload {
            workflow_name: "cognitive_workspace_benchmark".to_string(),
            terminal_event_id: None,
            output_context_handle_id: None,
            memory_ids: vec![MemoryId {
                workspace_id: workspace_id.to_string(),
                ulid: format!("mem-{task_id}"),
            }],
            result_summary: task_id.to_string(),
        }),
        &format!("evt-success-{task_id}"),
    )
}

fn benchmark_event(
    workspace_id: &str,
    task_id: &str,
    second: i64,
    kind: crate::events::EventKind,
    payload: crate::events::EventPayload,
    event_ulid: &str,
) -> crate::events::EventEnvelope {
    crate::events::EventEnvelope::new(
        EventId {
            workspace_id: workspace_id.to_string(),
            ulid: event_ulid.to_string(),
        },
        workspace_id.to_string(),
        crate::events::BranchRef {
            name: "main".to_string(),
        },
        crate::events::SessionId {
            value: "benchmark-fixtures".to_string(),
        },
        Some(crate::events::TaskId {
            value: task_id.to_string(),
        }),
        crate::events::Actor::Assistant {
            model: "benchmark".to_string(),
        },
        DateTime::from_unix_seconds(1_779_000_000 + second),
        kind,
        Vec::new(),
        crate::events::PayloadHash::new([second as u8; 32]),
        crate::events::CompactSummary::new(format!("{task_id}-{second}")).expect("compact summary"),
        crate::events::PayloadLocation::Inline { bytes_len: 64 },
        payload,
    )
    .expect("benchmark event envelope")
}
