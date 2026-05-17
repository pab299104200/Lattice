use std::collections::{BTreeMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

const MAX_RECENT_TOOL_TRACES: usize = 40;
const MAX_RECENT_TASKS: usize = 12;

#[derive(Debug, Clone, Default)]
pub struct ToolCallMetadata {
    pub delivery_mode: Option<String>,
    pub wire_format: Option<String>,
    pub single_anchor_used: bool,
    pub suggested_expand_focus: Option<String>,
    pub semantic_fallback_used: bool,
    pub outcome_memory_reuse_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionToolTrace {
    pub tool: String,
    pub payload_bytes: usize,
    pub approx_tokens: usize,
    pub context_handle: Option<String>,
    pub context_origin: Option<String>,
    pub delivery_mode: Option<String>,
    pub wire_format: Option<String>,
    pub single_anchor_used: bool,
    pub suggested_expand_focus: Option<String>,
    pub semantic_fallback_used: bool,
    pub outcome_memory_reuse_count: usize,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionTaskSummary {
    pub origin: String,
    pub tool_calls: usize,
    pub payload_bytes: usize,
    pub approx_tokens: usize,
    pub context_handle: Option<String>,
    pub delivery_mode: Option<String>,
    pub wire_format: Option<String>,
    pub single_anchor_used: bool,
    pub suggested_expand_focus: Option<String>,
    pub expanded: bool,
    pub semantic_fallback_used: bool,
    pub outcome_memory_reuse_count: usize,
    pub started_at: u64,
    pub last_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionMetricsReport {
    pub started_at: u64,
    pub total_tool_calls: usize,
    pub workflow_tool_calls: usize,
    pub task_count: usize,
    pub total_payload_bytes: usize,
    pub total_payload_tokens: usize,
    pub average_payload_bytes_per_tool: usize,
    pub average_payload_tokens_per_tool: usize,
    pub median_tool_calls_per_task: f64,
    pub median_payload_bytes_per_task: f64,
    pub median_payload_tokens_per_task: f64,
    pub context_handles_created: usize,
    pub context_handle_reuses: usize,
    pub context_handle_reuse_rate: f64,
    pub automatic_memory_writes: usize,
    pub outcome_pattern_writes: usize,
    pub compact_task_count: usize,
    pub tiny_task_count: usize,
    pub widened_task_count: usize,
    pub dense_wire_count: usize,
    pub single_anchor_task_count: usize,
    pub compact_to_expand_count: usize,
    pub compact_to_expand_rate: f64,
    pub follow_up_avoided_count: usize,
    pub follow_up_avoidance_rate: f64,
    pub suggested_expand_targets: usize,
    pub semantic_fallback_uses: usize,
    pub outcome_memory_reuse_count: usize,
    pub successful_workflow_calls: usize,
    pub failed_workflow_calls: usize,
    pub workflow_success_rate: f64,
    pub irrelevant_files_opened: usize,
    pub workflow_tasks_with_plan: usize,
    pub workflow_success_after_first_plan_count: usize,
    pub workflow_success_after_first_plan_rate: f64,
    pub tool_counts: BTreeMap<String, usize>,
    pub recent_tools: Vec<SessionToolTrace>,
    pub recent_tasks: Vec<SessionTaskSummary>,
}

#[derive(Debug, Clone)]
struct SessionTask {
    origin: String,
    tool_calls: usize,
    payload_bytes: usize,
    approx_tokens: usize,
    context_handle: Option<String>,
    delivery_mode: Option<String>,
    wire_format: Option<String>,
    single_anchor_used: bool,
    suggested_expand_focus: Option<String>,
    expanded: bool,
    used_follow_up_tool: bool,
    semantic_fallback_used: bool,
    outcome_memory_reuse_count: usize,
    started_at: u64,
    last_at: u64,
}

pub struct SessionMetrics {
    started_at: u64,
    total_tool_calls: usize,
    workflow_tool_calls: usize,
    total_payload_bytes: usize,
    total_payload_tokens: usize,
    context_handles_created: usize,
    context_handle_reuses: usize,
    automatic_memory_writes: usize,
    outcome_pattern_writes: usize,
    suggested_expand_targets: usize,
    semantic_fallback_uses: usize,
    outcome_memory_reuse_count: usize,
    successful_workflow_calls: usize,
    failed_workflow_calls: usize,
    irrelevant_files_opened: usize,
    workflow_tasks_with_plan: usize,
    workflow_success_after_first_plan_count: usize,
    tool_counts: BTreeMap<String, usize>,
    recent_tools: VecDeque<SessionToolTrace>,
    tasks: Vec<SessionTask>,
}

impl SessionMetrics {
    pub fn new() -> Self {
        Self {
            started_at: now_epoch_secs(),
            total_tool_calls: 0,
            workflow_tool_calls: 0,
            total_payload_bytes: 0,
            total_payload_tokens: 0,
            context_handles_created: 0,
            context_handle_reuses: 0,
            automatic_memory_writes: 0,
            outcome_pattern_writes: 0,
            suggested_expand_targets: 0,
            semantic_fallback_uses: 0,
            outcome_memory_reuse_count: 0,
            successful_workflow_calls: 0,
            failed_workflow_calls: 0,
            irrelevant_files_opened: 0,
            workflow_tasks_with_plan: 0,
            workflow_success_after_first_plan_count: 0,
            tool_counts: BTreeMap::new(),
            recent_tools: VecDeque::new(),
            tasks: Vec::new(),
        }
    }

    pub fn record_tool_call(
        &mut self,
        tool: &str,
        payload_bytes: usize,
        approx_tokens: usize,
        context_handle: Option<&str>,
        context_origin: Option<&str>,
        metadata: ToolCallMetadata,
    ) {
        let timestamp = now_epoch_secs();
        self.total_tool_calls += 1;
        self.total_payload_bytes += payload_bytes;
        self.total_payload_tokens += approx_tokens;
        *self.tool_counts.entry(tool.to_string()).or_insert(0) += 1;

        if is_workflow_tool(tool) {
            self.workflow_tool_calls += 1;
        }
        if context_handle.is_some() && tool != "expand_context" {
            self.context_handles_created += 1;
        }
        if tool == "expand_context" && context_handle.is_some() {
            self.context_handle_reuses += 1;
        }
        if metadata.suggested_expand_focus.is_some() {
            self.suggested_expand_targets += 1;
        }
        if metadata.semantic_fallback_used {
            self.semantic_fallback_uses += 1;
        }
        self.outcome_memory_reuse_count += metadata.outcome_memory_reuse_count;

        if self.recent_tools.len() >= MAX_RECENT_TOOL_TRACES {
            self.recent_tools.pop_front();
        }
        self.recent_tools.push_back(SessionToolTrace {
            tool: tool.to_string(),
            payload_bytes,
            approx_tokens,
            context_handle: context_handle.map(|item| item.to_string()),
            context_origin: context_origin.map(|item| item.to_string()),
            delivery_mode: metadata.delivery_mode.clone(),
            wire_format: metadata.wire_format.clone(),
            single_anchor_used: metadata.single_anchor_used,
            suggested_expand_focus: metadata.suggested_expand_focus.clone(),
            semantic_fallback_used: metadata.semantic_fallback_used,
            outcome_memory_reuse_count: metadata.outcome_memory_reuse_count,
            timestamp,
        });

        self.update_task_trace(
            tool,
            payload_bytes,
            approx_tokens,
            context_handle,
            context_origin,
            metadata,
            timestamp,
        );
    }

    pub fn record_auto_memory_write(&mut self, count: usize) {
        self.automatic_memory_writes += count;
    }

    pub fn record_outcome_pattern_write(&mut self, count: usize) {
        self.outcome_pattern_writes += count;
    }

    pub fn record_workflow_outcome(
        &mut self,
        success: bool,
        had_plan: bool,
        irrelevant_files_opened: usize,
    ) {
        if success {
            self.successful_workflow_calls += 1;
        } else {
            self.failed_workflow_calls += 1;
        }
        self.irrelevant_files_opened += irrelevant_files_opened;
        if had_plan {
            self.workflow_tasks_with_plan += 1;
            if success {
                self.workflow_success_after_first_plan_count += 1;
            }
        }
    }

    pub fn snapshot(&self) -> SessionMetricsReport {
        let average_payload_bytes_per_tool = if self.total_tool_calls == 0 {
            0
        } else {
            self.total_payload_bytes / self.total_tool_calls
        };
        let average_payload_tokens_per_tool = if self.total_tool_calls == 0 {
            0
        } else {
            self.total_payload_tokens / self.total_tool_calls
        };

        let tool_calls_per_task: Vec<usize> =
            self.tasks.iter().map(|task| task.tool_calls).collect();
        let payload_bytes_per_task: Vec<usize> =
            self.tasks.iter().map(|task| task.payload_bytes).collect();
        let payload_tokens_per_task: Vec<usize> =
            self.tasks.iter().map(|task| task.approx_tokens).collect();
        let compact_task_count = self
            .tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.delivery_mode.as_deref(),
                    Some("compact") | Some("tiny")
                )
            })
            .count();
        let tiny_task_count = self
            .tasks
            .iter()
            .filter(|task| task.delivery_mode.as_deref() == Some("tiny"))
            .count();
        let widened_task_count = self
            .tasks
            .iter()
            .filter(|task| task.delivery_mode.as_deref() == Some("full"))
            .count();
        let dense_wire_count = self
            .tasks
            .iter()
            .filter(|task| task.wire_format.as_deref() == Some("dense"))
            .count();
        let single_anchor_task_count = self
            .tasks
            .iter()
            .filter(|task| task.single_anchor_used)
            .count();
        let compact_to_expand_count = self
            .tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.delivery_mode.as_deref(),
                    Some("compact") | Some("tiny")
                ) && task.expanded
            })
            .count();
        let follow_up_avoided_count = self
            .tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.delivery_mode.as_deref(),
                    Some("compact") | Some("tiny")
                ) && !task.used_follow_up_tool
            })
            .count();

        SessionMetricsReport {
            started_at: self.started_at,
            total_tool_calls: self.total_tool_calls,
            workflow_tool_calls: self.workflow_tool_calls,
            task_count: self.tasks.len(),
            total_payload_bytes: self.total_payload_bytes,
            total_payload_tokens: self.total_payload_tokens,
            average_payload_bytes_per_tool,
            average_payload_tokens_per_tool,
            median_tool_calls_per_task: median_usize(&tool_calls_per_task),
            median_payload_bytes_per_task: median_usize(&payload_bytes_per_task),
            median_payload_tokens_per_task: median_usize(&payload_tokens_per_task),
            context_handles_created: self.context_handles_created,
            context_handle_reuses: self.context_handle_reuses,
            context_handle_reuse_rate: if self.context_handles_created == 0 {
                0.0
            } else {
                self.context_handle_reuses as f64 / self.context_handles_created as f64
            },
            automatic_memory_writes: self.automatic_memory_writes,
            outcome_pattern_writes: self.outcome_pattern_writes,
            compact_task_count,
            tiny_task_count,
            widened_task_count,
            dense_wire_count,
            single_anchor_task_count,
            compact_to_expand_count,
            compact_to_expand_rate: if compact_task_count == 0 {
                0.0
            } else {
                compact_to_expand_count as f64 / compact_task_count as f64
            },
            follow_up_avoided_count,
            follow_up_avoidance_rate: if compact_task_count == 0 {
                0.0
            } else {
                follow_up_avoided_count as f64 / compact_task_count as f64
            },
            suggested_expand_targets: self.suggested_expand_targets,
            semantic_fallback_uses: self.semantic_fallback_uses,
            outcome_memory_reuse_count: self.outcome_memory_reuse_count,
            successful_workflow_calls: self.successful_workflow_calls,
            failed_workflow_calls: self.failed_workflow_calls,
            workflow_success_rate: if self.successful_workflow_calls + self.failed_workflow_calls
                == 0
            {
                0.0
            } else {
                self.successful_workflow_calls as f64
                    / (self.successful_workflow_calls + self.failed_workflow_calls) as f64
            },
            irrelevant_files_opened: self.irrelevant_files_opened,
            workflow_tasks_with_plan: self.workflow_tasks_with_plan,
            workflow_success_after_first_plan_count: self.workflow_success_after_first_plan_count,
            workflow_success_after_first_plan_rate: if self.workflow_tasks_with_plan == 0 {
                0.0
            } else {
                self.workflow_success_after_first_plan_count as f64
                    / self.workflow_tasks_with_plan as f64
            },
            tool_counts: self.tool_counts.clone(),
            recent_tools: self.recent_tools.iter().cloned().collect(),
            recent_tasks: self
                .tasks
                .iter()
                .rev()
                .take(MAX_RECENT_TASKS)
                .rev()
                .map(|task| SessionTaskSummary {
                    origin: task.origin.clone(),
                    tool_calls: task.tool_calls,
                    payload_bytes: task.payload_bytes,
                    approx_tokens: task.approx_tokens,
                    context_handle: task.context_handle.clone(),
                    delivery_mode: task.delivery_mode.clone(),
                    wire_format: task.wire_format.clone(),
                    single_anchor_used: task.single_anchor_used,
                    suggested_expand_focus: task.suggested_expand_focus.clone(),
                    expanded: task.expanded,
                    semantic_fallback_used: task.semantic_fallback_used,
                    outcome_memory_reuse_count: task.outcome_memory_reuse_count,
                    started_at: task.started_at,
                    last_at: task.last_at,
                })
                .collect(),
        }
    }

    fn update_task_trace(
        &mut self,
        tool: &str,
        payload_bytes: usize,
        approx_tokens: usize,
        context_handle: Option<&str>,
        context_origin: Option<&str>,
        metadata: ToolCallMetadata,
        timestamp: u64,
    ) {
        let origin = context_origin.unwrap_or(tool);
        let is_new_task = is_task_starter(tool);

        if is_new_task {
            self.tasks.push(SessionTask {
                origin: origin.to_string(),
                tool_calls: 1,
                payload_bytes,
                approx_tokens,
                context_handle: context_handle.map(|item| item.to_string()),
                delivery_mode: metadata.delivery_mode,
                wire_format: metadata.wire_format,
                single_anchor_used: metadata.single_anchor_used,
                suggested_expand_focus: metadata.suggested_expand_focus,
                expanded: false,
                used_follow_up_tool: false,
                semantic_fallback_used: metadata.semantic_fallback_used,
                outcome_memory_reuse_count: metadata.outcome_memory_reuse_count,
                started_at: timestamp,
                last_at: timestamp,
            });
            return;
        }

        let target_index = if tool == "expand_context" {
            context_handle.and_then(|handle| {
                self.tasks
                    .iter()
                    .rposition(|task| task.context_handle.as_deref() == Some(handle))
            })
        } else if self.tasks.is_empty() {
            None
        } else {
            Some(self.tasks.len() - 1)
        };

        if let Some(index) = target_index {
            let task = &mut self.tasks[index];
            task.tool_calls += 1;
            task.payload_bytes += payload_bytes;
            task.approx_tokens += approx_tokens;
            task.last_at = timestamp;
            if tool == "expand_context" {
                task.expanded = true;
            }
            if is_follow_up_tool(tool) {
                task.used_follow_up_tool = true;
            }
            if task.context_handle.is_none() {
                task.context_handle = context_handle.map(|item| item.to_string());
            }
            if task.delivery_mode.is_none() {
                task.delivery_mode = metadata.delivery_mode;
            }
            if task.wire_format.is_none() {
                task.wire_format = metadata.wire_format;
            }
            task.single_anchor_used |= metadata.single_anchor_used;
            if task.suggested_expand_focus.is_none() {
                task.suggested_expand_focus = metadata.suggested_expand_focus;
            }
            task.semantic_fallback_used |= metadata.semantic_fallback_used;
            task.outcome_memory_reuse_count += metadata.outcome_memory_reuse_count;
        } else {
            self.tasks.push(SessionTask {
                origin: origin.to_string(),
                tool_calls: 1,
                payload_bytes,
                approx_tokens,
                context_handle: context_handle.map(|item| item.to_string()),
                delivery_mode: metadata.delivery_mode,
                wire_format: metadata.wire_format,
                single_anchor_used: metadata.single_anchor_used,
                suggested_expand_focus: metadata.suggested_expand_focus,
                expanded: tool == "expand_context",
                used_follow_up_tool: is_follow_up_tool(tool),
                semantic_fallback_used: metadata.semantic_fallback_used,
                outcome_memory_reuse_count: metadata.outcome_memory_reuse_count,
                started_at: timestamp,
                last_at: timestamp,
            });
        }
    }
}

fn is_workflow_tool(tool: &str) -> bool {
    matches!(
        tool,
        "get_context_capsule"
            | "prepare_change"
            | "find_relevant_tests"
            | "impact_from_diff"
            | "get_working_set_context"
            | "diagnose_failure"
            | "expand_context"
            | "summarize_subsystem"
            | "get_repo_playbook"
            | "record_workflow_outcome"
    )
}

fn is_task_starter(tool: &str) -> bool {
    matches!(
        tool,
        "get_context_capsule"
            | "prepare_change"
            | "find_relevant_tests"
            | "impact_from_diff"
            | "get_working_set_context"
            | "diagnose_failure"
            | "summarize_subsystem"
            | "get_repo_playbook"
    )
}

fn is_follow_up_tool(tool: &str) -> bool {
    matches!(
        tool,
        "expand_context"
            | "get_symbol"
            | "get_dependencies"
            | "get_dependents"
            | "get_impact_graph"
            | "search_symbols"
            | "search_logic_flow"
            | "get_docs_capsule"
            | "get_backlinks"
            | "get_outgoing_links"
            | "find_stale_docs"
            | "list_observations"
            | "list_stale_memories"
            | "search_memory"
            | "get_session_context"
            | "workspace_setup"
            | "index_status"
            | "get_project_rules"
    )
}

fn median_usize(values: &[usize]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }

    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[mid - 1] as f64 + sorted[mid] as f64) / 2.0
    } else {
        sorted[mid] as f64
    }
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::{SessionMetrics, ToolCallMetadata};

    #[test]
    fn test_session_metrics_groups_expand_context_under_prior_task() {
        let mut metrics = SessionMetrics::new();
        metrics.record_tool_call(
            "prepare_change",
            400,
            100,
            Some("ctx-1"),
            Some("prepare_change"),
            ToolCallMetadata {
                delivery_mode: Some("tiny".to_string()),
                wire_format: Some("dense".to_string()),
                single_anchor_used: true,
                suggested_expand_focus: Some("symbol:loginUser".to_string()),
                ..ToolCallMetadata::default()
            },
        );
        metrics.record_tool_call(
            "expand_context",
            120,
            30,
            Some("ctx-1"),
            Some("prepare_change"),
            ToolCallMetadata::default(),
        );

        let report = metrics.snapshot();
        assert_eq!(report.task_count, 1);
        assert_eq!(report.context_handle_reuses, 1);
        assert_eq!(report.recent_tasks[0].tool_calls, 2);
        assert_eq!(report.compact_to_expand_count, 1);
        assert_eq!(report.follow_up_avoided_count, 0);
        assert_eq!(report.tiny_task_count, 1);
        assert_eq!(report.dense_wire_count, 1);
        assert_eq!(report.single_anchor_task_count, 1);
    }

    #[test]
    fn test_session_metrics_tracks_auto_memory_writes() {
        let mut metrics = SessionMetrics::new();
        metrics.record_tool_call(
            "get_repo_playbook",
            300,
            75,
            Some("ctx-2"),
            Some("get_repo_playbook"),
            ToolCallMetadata {
                delivery_mode: Some("compact".to_string()),
                semantic_fallback_used: true,
                outcome_memory_reuse_count: 2,
                ..ToolCallMetadata::default()
            },
        );
        metrics.record_auto_memory_write(1);
        metrics.record_outcome_pattern_write(1);

        let report = metrics.snapshot();
        assert_eq!(report.automatic_memory_writes, 1);
        assert_eq!(report.outcome_pattern_writes, 1);
        assert_eq!(report.workflow_tool_calls, 1);
        assert_eq!(report.semantic_fallback_uses, 1);
        assert_eq!(report.outcome_memory_reuse_count, 2);
    }

    #[test]
    fn test_session_metrics_only_counts_true_follow_up_avoidance() {
        let mut metrics = SessionMetrics::new();
        metrics.record_tool_call(
            "prepare_change",
            420,
            105,
            Some("ctx-3"),
            Some("prepare_change"),
            ToolCallMetadata {
                delivery_mode: Some("compact".to_string()),
                ..ToolCallMetadata::default()
            },
        );
        metrics.record_tool_call(
            "get_symbol",
            180,
            45,
            None,
            Some("prepare_change"),
            ToolCallMetadata::default(),
        );

        let report = metrics.snapshot();
        assert_eq!(report.task_count, 1);
        assert_eq!(report.compact_task_count, 1);
        assert_eq!(report.compact_to_expand_count, 0);
        assert_eq!(report.follow_up_avoided_count, 0);
        assert_eq!(report.follow_up_avoidance_rate, 0.0);
    }
}
