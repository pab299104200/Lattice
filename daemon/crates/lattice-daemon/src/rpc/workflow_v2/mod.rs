//! Workflow Engine V2 edit-planning contract.
//!
//! Bound to `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 8: Workflow Engine V2` and `## MCP Tool Contract Principles`.
//! The shared bundle below intentionally mirrors the spec field list:
//! overview, ranked pivots, relevant context, memory highlights, event
//! episodes where relevant, suggested next expansion, stable handles, risks or
//! uncertainty, compact/full render choice, and structured payload.

use lattice_core::events::EventKind;
use lattice_core::identity::{ContextHandleId, EventId, FileId, MemoryId, SymbolId};
use lattice_core::intelligence::TaskBundle;
use lattice_core::memory::MemoryVerificationStatus;
use lattice_core::query::ContextCapsule;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[cfg(test)]
mod composition_tests;
pub mod context_capsule;
pub mod diagnose_failure;
#[cfg(test)]
mod discovery_workflows_tests;
pub mod docs_capsule;
#[cfg(test)]
mod edit_workflows_tests;
pub mod impact_from_diff;
pub mod outcome_capture;
pub mod plan_edit;
pub mod prepare_change;
pub mod relevant_tests;
pub mod trace_scenario;

/// Render choices shared by workflow v2 discovery and edit tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkflowRenderChoice {
    /// Focused response with the smallest useful working set.
    Focused,
    /// Compact response for default assistant use.
    Compact,
    /// Full response with broader candidates.
    Full,
    /// Diagnostic response with extra explanations.
    Diagnostic,
}

impl WorkflowRenderChoice {
    /// Parse a user-supplied mode string.
    pub fn from_mode_str(value: Option<&str>) -> Self {
        match value {
            Some("focused") | Some("tiny") => Self::Focused,
            Some("full") => Self::Full,
            Some("diagnostic") => Self::Diagnostic,
            _ => Self::Compact,
        }
    }
}

/// Build an expansion seed from a workflow-v2 JSON-compatible bundle.
pub fn build_expand_seed<T: Serialize>(
    bundle: &T,
) -> lattice_core::intelligence::ExpandContextSeed {
    let value = serde_json::to_value(bundle).unwrap_or_else(|_| json!({}));
    lattice_core::intelligence::ExpandContextSeed {
        query: value
            .get("query")
            .or_else(|| value.get("scenario"))
            .or_else(|| value.get("overview"))
            .and_then(Value::as_str)
            .map(ToString::to_string),
        files: collect_string_values(&value, &["file", "repo_relative_path"]),
        symbols: collect_string_values(&value, &["symbol", "qualified_name"]),
        tests: collect_test_files(&value),
        memories: value
            .get("memory_highlights")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    }
}

fn collect_string_values(value: &Value, keys: &[&str]) -> Vec<String> {
    let mut values = Vec::new();
    collect_string_values_inner(value, keys, &mut values);
    values.sort();
    values.dedup();
    values
}

fn collect_string_values_inner(value: &Value, keys: &[&str], values: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for key in keys {
                if let Some(item) = object.get(*key).and_then(Value::as_str) {
                    values.push(item.to_string());
                }
            }
            for child in object.values() {
                collect_string_values_inner(child, keys, values);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_string_values_inner(item, keys, values);
            }
        }
        _ => {}
    }
}

fn collect_test_files(value: &Value) -> Vec<String> {
    value
        .get("tests")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("file").and_then(Value::as_str))
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Assistant-facing response shared by Phase 8 workflow tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowBundle {
    /// Human-readable summary of what the workflow selected and why.
    pub overview: String,
    /// Ranked code, doc, memory, or failure pivots with stable identities.
    pub ranked_pivots: Vec<Pivot>,
    /// Bounded supporting context that should be read before broad discovery.
    pub relevant_context: Vec<ContextItem>,
    /// Memory evidence with inclusion reason and verification lifecycle labels.
    pub memory_highlights: Vec<MemoryHighlight>,
    /// Explicit reason when no memories were available or relevant.
    pub memory_empty_rationale: Option<String>,
    /// Prior event-log episodes that are relevant to this workflow result.
    pub event_episodes: Vec<EventEpisode>,
    /// Recommended next expansion target.
    pub suggested_next_expansion: Option<ExpansionHint>,
    /// Stable handles copied out for clients that prefer a flat handle list.
    pub stable_handles: Vec<String>,
    /// Risks, stale evidence labels, contradictions, and uncertainty notes.
    pub risks: Vec<RiskNote>,
    /// Render shape selected for this response.
    pub render_choice: RenderChoice,
    /// Commands inferred from affected build targets and recommended tests.
    pub verification_commands: Vec<String>,
    /// Stable record of inputs, anchors, selected candidates, and exclusions.
    pub workflow_record: WorkflowRecord,
    /// Tool-specific payload retained for backward-compatible clients.
    pub structured_payload: Value,
}

/// A ranked workflow pivot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pivot {
    /// Stable identity for the pivot.
    pub identity: StableIdentity,
    /// File, symbol, doc, memory, or event.
    pub kind: String,
    /// Display label for compact rendering.
    pub label: String,
    /// Repo-relative path when applicable.
    pub file: Option<String>,
    /// Symbol or heading name when applicable.
    pub symbol: Option<String>,
    /// One-based source line when applicable.
    pub line: Option<usize>,
    /// Ranked confidence score.
    pub score: f64,
    /// Reason this pivot was selected.
    pub inclusion_reason: String,
    /// Bounded one-line relevance digest for compact render mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relevance_summary: Option<String>,
    /// Full ranking-signal breakdown for full and diagnostic render modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relevance_breakdown: Option<RelevanceBreakdown>,
    /// Stable handle used to fetch the full relevance breakdown via
    /// `expand_context` without rerunning the workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relevance_detail_handle: Option<String>,
    /// Stable focus string paired with `relevance_detail_handle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relevance_detail_focus: Option<String>,
}

/// Supporting context item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextItem {
    /// Stable identity for the context item.
    pub identity: StableIdentity,
    /// Context kind such as file, symbol, doc, test, or memory.
    pub kind: String,
    /// Compact display label.
    pub label: String,
    /// Repo-relative file when applicable.
    pub file: Option<String>,
    /// Summary of the context item.
    pub summary: String,
    /// Reason the item was included.
    pub inclusion_reason: String,
}

/// Memory evidence surfaced by a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryHighlight {
    /// Stable memory identity.
    pub memory_id: MemoryId,
    /// Compact memory content.
    pub content: String,
    /// Memory type.
    pub memory_type: String,
    /// Session, branch, repo, or organization scope.
    pub scope: String,
    /// Reason retrieval included this memory.
    pub inclusion_reason: String,
    /// Evidence strength label.
    pub evidence_strength: String,
    /// Verification lifecycle state.
    pub verification_status: String,
    /// Trust tier callers should use before treating this memory as guidance.
    pub trust_status: String,
    /// Machine-readable reason for the trust tier.
    pub trust_reason: String,
    /// High-risk domains inferred from memory text and links.
    pub risk_domains: Vec<String>,
    /// Whether callers should re-check the claim before relying on it.
    pub requires_reverification: bool,
    /// Machine-readable reason for the re-verification requirement.
    pub reverification_reason: String,
    /// Freshness state derived from stale and verification fields.
    pub freshness_status: String,
    /// Contradiction or supersession state.
    pub contradiction_state: String,
    /// Expansion target for this memory.
    pub expansion_target: String,
    /// Required warning label for stale or otherwise unsafe statuses.
    pub stale_label: Option<String>,
    /// Bounded commands or command-like probes callers can run to re-check the claim now.
    pub recheck_commands: Vec<String>,
    /// Bounded one-line relevance digest for compact render mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relevance_summary: Option<String>,
    /// Full ranking-signal breakdown for full and diagnostic render modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relevance_breakdown: Option<RelevanceBreakdown>,
    /// Stable handle used to fetch the full relevance breakdown via
    /// `expand_context` without rerunning the workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relevance_detail_handle: Option<String>,
    /// Stable focus string paired with `relevance_detail_handle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relevance_detail_focus: Option<String>,
}

/// Full ranking-signal breakdown for one surfaced retrieval result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelevanceBreakdown {
    /// Final normalized score used for ordering.
    pub total_score: f64,
    /// Per-signal contribution scores from the retrieval ranker.
    pub ranking_signals: RelevanceSignalScores,
    /// Human-readable explanation for why the result was selected or excluded.
    pub explanation: String,
}

/// All retrieval-engine ranking-signal columns required by the plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelevanceSignalScores {
    /// Compatibility between the candidate and the inferred task type.
    pub task_type_compatibility: f64,
    /// Proximity between the candidate and resolved anchors.
    pub graph_proximity_to_anchors: f64,
    /// Whether the candidate exactly matched an identifier from the input.
    pub exact_identifier_match: f64,
    /// Semantic similarity to the task text.
    pub semantic_similarity: f64,
    /// Score contribution from verification state.
    pub verification_status: f64,
    /// Score contribution from freshness or staleness.
    pub freshness: f64,
    /// Score contribution from scope alignment.
    pub scope: f64,
    /// Score contribution from evidence strength.
    pub evidence_strength: f64,
    /// Score contribution from contradiction or supersession state.
    pub contradiction_supersession_state: f64,
    /// Score contribution from historically observed usefulness.
    pub past_usefulness: f64,
    /// Score contribution from recent successful reuse.
    pub recent_successful_reuse: f64,
    /// Score contribution from user or workflow preference compatibility.
    pub user_preference_compatibility: f64,
    /// Score contribution after token-cost normalization.
    pub token_cost: f64,
}

/// Prior event-log episode relevant to the workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEpisode {
    /// Stable event identity.
    pub event_id: EventId,
    /// Event kind such as workflow_failed or tool_called.
    pub event_kind: String,
    /// Compact event summary.
    pub summary: String,
    /// Reason this episode was included.
    pub inclusion_reason: String,
}

/// Risk or uncertainty surfaced by the workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskNote {
    /// Risk severity: info, warning, or error.
    pub severity: String,
    /// Stable identity this risk is about when applicable.
    pub identity: Option<StableIdentity>,
    /// Risk summary.
    pub message: String,
    /// Recommended mitigation.
    pub mitigation: String,
}

/// Suggested expansion target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpansionHint {
    /// Stable focus string accepted by expand_context.
    pub focus: String,
    /// Reason this expansion is the next best step.
    pub reason: String,
}

/// Stable identity wrapper for workflow payloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value")]
pub enum StableIdentity {
    /// Stable file identity.
    File(FileId),
    /// Stable symbol identity.
    Symbol(SymbolId),
    /// Stable memory identity.
    Memory(MemoryId),
    /// Stable event identity.
    Event(EventId),
    /// Stable context handle identity.
    ContextHandle(ContextHandleId),
    /// Legacy expansion handle retained for backward compatibility.
    LegacyHandle(String),
}

/// Render mode selected by the workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderChoice {
    /// compact or full.
    pub mode: String,
    /// Why this render mode was selected.
    pub reason: String,
}

/// Recorded workflow composition metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRecord {
    /// Tool name.
    pub tool: String,
    /// Raw query or failure input.
    pub input: String,
    /// Resolved anchors used by retrieval and ranking.
    pub resolved_anchors: Vec<StableIdentity>,
    /// Selected candidate handles.
    pub selected_candidates: Vec<String>,
    /// Excluded high-scoring candidates and reasons.
    pub excluded_high_scoring_candidates: Vec<String>,
    /// Working-memory operation summary.
    pub working_memory_summary: String,
}

/// Inputs common to edit-planning workflows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRequest {
    /// Query, scenario, or failure text.
    pub input: String,
    /// Optional starting files.
    pub entry_files: Vec<String>,
    /// Optional starting symbols.
    pub entry_symbols: Vec<String>,
    /// compact or full.
    pub render_mode: String,
}

/// Event capture abstraction used by tests and MCP integration.
pub trait WorkflowEventSink {
    /// Record one workflow event.
    fn record_event(&mut self, kind: EventKind, summary: &str);
}

/// In-memory event sink for unit tests.
#[derive(Debug, Default)]
pub struct VecEventSink {
    /// Recorded event kinds in order.
    pub events: Vec<EventKind>,
}

impl WorkflowEventSink for VecEventSink {
    fn record_event(&mut self, kind: EventKind, _summary: &str) {
        self.events.push(kind);
    }
}

pub(crate) fn emit_standard_events(sink: &mut dyn WorkflowEventSink, tool: &str) {
    sink.record_event(EventKind::AssistantTaskStarted, "workflow v2 task started");
    sink.record_event(EventKind::ToolCalled, tool);
    sink.record_event(
        EventKind::ContextBundleReturned,
        "workflow v2 bundle returned",
    );
    sink.record_event(EventKind::MemoryRetrieved, "workflow v2 memories retrieved");
    sink.record_event(EventKind::PlanCreated, "workflow v2 plan created");
}

pub(crate) fn file_identity(workspace_id: &str, file: &str) -> FileId {
    FileId {
        workspace_id: workspace_id.to_string(),
        repo_relative_path: file.to_string(),
        content_hash: "unknown".to_string(),
    }
}

pub(crate) fn memory_identity(workspace_id: &str, id: &str) -> MemoryId {
    MemoryId {
        workspace_id: workspace_id.to_string(),
        ulid: id.to_string(),
    }
}

pub(crate) fn event_identity(workspace_id: &str, id: &str) -> EventId {
    EventId {
        workspace_id: workspace_id.to_string(),
        ulid: id.to_string(),
    }
}

pub(crate) fn memory_highlights(
    workspace_id: &str,
    memories: &[Value],
    fallback_reason: &str,
) -> Vec<MemoryHighlight> {
    memories
        .iter()
        .enumerate()
        .filter_map(|(index, value)| memory_highlight(workspace_id, value, index, fallback_reason))
        .collect()
}

fn memory_highlight(
    workspace_id: &str,
    value: &Value,
    index: usize,
    fallback_reason: &str,
) -> Option<MemoryHighlight> {
    let object = value.as_object()?;
    let content = string_field(object, &["content", "summary"])?;
    let status =
        string_field(object, &["verification_status"]).unwrap_or_else(|| "unverified".to_string());
    let is_stale = object
        .get("is_stale")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let memory_id =
        string_field(object, &["id", "memory_id"]).unwrap_or_else(|| format!("memory-{index}"));
    let status_kind = MemoryVerificationStatus::from_str(&status);
    let stale_label = stale_label(status_kind, is_stale);
    let evidence_is_empty = object
        .get("evidence")
        .and_then(Value::as_array)
        .map_or(true, Vec::is_empty);
    let recheck_commands = memory_recheck_commands(object);
    let risk_domains = memory_risk_domains(object, &content);
    let (requires_reverification, reverification_reason) =
        requires_reverification(status_kind, is_stale, evidence_is_empty, &risk_domains);
    Some(MemoryHighlight {
        memory_id: memory_identity(workspace_id, &memory_id),
        content,
        memory_type: string_field(object, &["memory_type", "type"])
            .unwrap_or_else(|| "observation".to_string()),
        scope: string_field(object, &["scope"]).unwrap_or_else(|| "session".to_string()),
        inclusion_reason: string_field(object, &["inclusion_reason"])
            .unwrap_or_else(|| fallback_reason.to_string()),
        evidence_strength: evidence_strength(object),
        verification_status: status,
        trust_status: trust_status(status_kind, is_stale, evidence_is_empty).to_string(),
        trust_reason: trust_reason(status_kind, is_stale, evidence_is_empty).to_string(),
        risk_domains,
        requires_reverification,
        reverification_reason,
        freshness_status: freshness_status(status_kind, is_stale).to_string(),
        contradiction_state: contradiction_state(object, status_kind),
        expansion_target: format!("memory:{memory_id}"),
        stale_label,
        recheck_commands,
        relevance_summary: None,
        relevance_breakdown: None,
        relevance_detail_handle: None,
        relevance_detail_focus: None,
    })
}

fn memory_recheck_commands(object: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut commands = Vec::new();
    for test in string_array_field(object, "linked_tests") {
        push_unique(&mut commands, command_for_test_ref(&test));
    }
    for file in string_array_field(object, "linked_files") {
        push_file_recheck(&mut commands, &file);
    }
    for doc in string_array_field(object, "linked_docs") {
        let path = doc.split_once('#').map_or(doc.as_str(), |item| item.0);
        if !path.trim().is_empty() {
            push_unique(
                &mut commands,
                format!("rg -n \"TODO|blocked|resolved|verified|stale\" {path}"),
            );
        }
    }
    if let Some(items) = object.get("evidence").and_then(Value::as_array) {
        for evidence in items.iter().filter_map(Value::as_object) {
            let kind = string_field(evidence, &["kind"]).unwrap_or_default();
            let reference = string_field(evidence, &["reference"]).unwrap_or_default();
            if reference.is_empty() {
                continue;
            }
            match kind.as_str() {
                "test" => push_unique(&mut commands, command_for_test_ref(&reference)),
                "file" => push_file_recheck(&mut commands, &reference),
                _ => {}
            }
        }
    }
    commands.truncate(8);
    commands
}

fn memory_risk_domains(object: &serde_json::Map<String, Value>, content: &str) -> Vec<String> {
    let haystack = format!(
        "{} {} {} {} {}",
        content,
        string_array_field(object, "linked_files").join(" "),
        string_array_field(object, "linked_symbols").join(" "),
        string_array_field(object, "linked_docs").join(" "),
        string_array_field(object, "linked_tests").join(" ")
    )
    .to_ascii_lowercase();
    classify_risk_domains(&haystack)
}

fn classify_risk_domains(haystack: &str) -> Vec<String> {
    let mut domains = Vec::new();
    push_domain_if(
        &mut domains,
        "security",
        haystack,
        &[
            "security",
            "auth",
            "jwt",
            "token",
            "password",
            "secret",
            "permission",
            "authorization",
            "vulnerability",
            "cve",
        ],
    );
    push_domain_if(
        &mut domains,
        "tenancy",
        haystack,
        &[
            "tenant",
            "tenancy",
            "rls",
            "org isolation",
            "workspace boundary",
        ],
    );
    push_domain_if(
        &mut domains,
        "migration",
        haystack,
        &["migration", "schema", "alembic", "diesel", "backfill"],
    );
    push_domain_if(
        &mut domains,
        "deploy",
        haystack,
        &["deploy", "release", "rollout", "rollback", "production"],
    );
    push_domain_if(
        &mut domains,
        "dependency",
        haystack,
        &[
            "dependency",
            "dependencies",
            "pip-audit",
            "npm audit",
            "cargo audit",
            "lockfile",
        ],
    );
    push_domain_if(
        &mut domains,
        "test_suite",
        haystack,
        &[
            "full suite",
            "test suite",
            "cargo test --workspace",
            "pytest",
            "npm test",
        ],
    );
    domains
}

fn requires_reverification(
    status: MemoryVerificationStatus,
    is_stale: bool,
    evidence_is_empty: bool,
    risk_domains: &[String],
) -> (bool, String) {
    if risk_domains.is_empty() {
        return (false, "not_high_risk".to_string());
    }
    if is_stale {
        return (true, "stale_high_risk_memory".to_string());
    }
    if status != MemoryVerificationStatus::Verified {
        return (true, format!("high_risk_{}", status.as_str()));
    }
    if evidence_is_empty {
        return (true, "high_risk_missing_evidence".to_string());
    }
    (false, "high_risk_verified_with_evidence".to_string())
}

fn push_domain_if(domains: &mut Vec<String>, domain: &str, haystack: &str, needles: &[&str]) {
    if needles.iter().any(|needle| haystack.contains(needle))
        && !domains.iter().any(|existing| existing == domain)
    {
        domains.push(domain.to_string());
    }
}

fn string_field(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| object.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .next()
}

fn string_array_field(object: &serde_json::Map<String, Value>, key: &str) -> Vec<String> {
    object
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn command_for_test_ref(reference: &str) -> String {
    if reference.ends_with(".py") || reference.contains(".py::") {
        format!("pytest {reference}")
    } else if reference.ends_with(".rs") || reference.contains(".rs::") {
        if let Some((_, test_name)) = reference.rsplit_once("::") {
            format!("cd daemon && cargo test {test_name}")
        } else {
            "cd daemon && cargo test --workspace".to_string()
        }
    } else if reference.ends_with(".ts")
        || reference.ends_with(".tsx")
        || reference.ends_with(".js")
        || reference.ends_with(".jsx")
    {
        format!("npm test -- {reference}")
    } else {
        format!("Run targeted test reference {reference}")
    }
}

fn push_file_recheck(commands: &mut Vec<String>, file: &str) {
    let path = file.trim();
    if path.is_empty() {
        return;
    }
    if path.ends_with(".rs") {
        if path.starts_with("daemon/crates/lattice-core") {
            push_unique(
                commands,
                "cd daemon && cargo test -p lattice-core".to_string(),
            );
        } else if path.starts_with("daemon/crates/lattice-daemon") {
            push_unique(
                commands,
                "cd daemon && cargo test -p lattice-daemon --lib".to_string(),
            );
        } else {
            push_unique(commands, "cd daemon && cargo test --workspace".to_string());
        }
    } else if path.ends_with(".py") {
        push_unique(commands, format!("pytest {path}"));
    } else if path.ends_with(".ts") || path.ends_with(".tsx") {
        push_unique(commands, format!("npm test -- {path}"));
    } else if path.ends_with(".md") {
        push_unique(
            commands,
            format!("rg -n \"TODO|blocked|resolved|verified|stale\" {path}"),
        );
    }
    push_unique(commands, format!("git diff -- {path}"));
}

fn push_unique(commands: &mut Vec<String>, command: String) {
    if command.trim().is_empty() || commands.iter().any(|existing| existing == &command) {
        return;
    }
    commands.push(command);
}

fn evidence_strength(object: &serde_json::Map<String, Value>) -> String {
    object
        .get("confidence")
        .and_then(Value::as_f64)
        .map(|score| {
            if score >= 0.8 {
                "strong"
            } else if score >= 0.45 {
                "moderate"
            } else {
                "weak"
            }
        })
        .unwrap_or("unverified")
        .to_string()
}

fn stale_label(status: MemoryVerificationStatus, is_stale: bool) -> Option<String> {
    let unsafe_status = matches!(
        status,
        MemoryVerificationStatus::Stale
            | MemoryVerificationStatus::Contradicted
            | MemoryVerificationStatus::Superseded
            | MemoryVerificationStatus::Expired
            | MemoryVerificationStatus::Invalidated
    );
    if unsafe_status || is_stale {
        Some(format!("STALE_OR_UNTRUSTED: {}", status.as_str()))
    } else {
        None
    }
}

fn trust_status(
    status: MemoryVerificationStatus,
    is_stale: bool,
    evidence_is_empty: bool,
) -> &'static str {
    if is_stale
        || matches!(
            status,
            MemoryVerificationStatus::Stale
                | MemoryVerificationStatus::Contradicted
                | MemoryVerificationStatus::Superseded
                | MemoryVerificationStatus::Expired
                | MemoryVerificationStatus::Invalidated
        )
    {
        "stale"
    } else if evidence_is_empty
        || matches!(
            status,
            MemoryVerificationStatus::Unverified | MemoryVerificationStatus::InReview
        )
    {
        "advisory"
    } else {
        "trusted"
    }
}

fn trust_reason(
    status: MemoryVerificationStatus,
    is_stale: bool,
    evidence_is_empty: bool,
) -> &'static str {
    if is_stale {
        return "marked_stale";
    }
    match status {
        MemoryVerificationStatus::Verified if evidence_is_empty => "missing_evidence",
        MemoryVerificationStatus::Verified => "verified",
        MemoryVerificationStatus::InReview => "verification_in_review",
        MemoryVerificationStatus::Unverified => "unverified",
        MemoryVerificationStatus::Stale => "verification_stale",
        MemoryVerificationStatus::Contradicted => "contradicted",
        MemoryVerificationStatus::Superseded => "superseded",
        MemoryVerificationStatus::Expired => "expired",
        MemoryVerificationStatus::Invalidated => "invalidated",
    }
}

fn freshness_status(status: MemoryVerificationStatus, is_stale: bool) -> &'static str {
    if is_stale || matches!(status, MemoryVerificationStatus::Stale) {
        "stale"
    } else if matches!(status, MemoryVerificationStatus::Expired) {
        "expired"
    } else {
        "fresh"
    }
}

fn contradiction_state(
    object: &serde_json::Map<String, Value>,
    status: MemoryVerificationStatus,
) -> String {
    if matches!(status, MemoryVerificationStatus::Contradicted) {
        return "contradicted".to_string();
    }
    if matches!(status, MemoryVerificationStatus::Superseded) {
        return "superseded".to_string();
    }
    if object
        .get("contradicted_by_memory_ids")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty())
    {
        return "has_contradictions".to_string();
    }
    "none".to_string()
}

pub(crate) fn risks_from_memories(memories: &[MemoryHighlight]) -> Vec<RiskNote> {
    memories
        .iter()
        .filter_map(|memory| match memory.trust_status.as_str() {
            "trusted" if !memory.requires_reverification => None,
            "trusted" => Some(RiskNote {
                severity: "warning".to_string(),
                identity: Some(StableIdentity::Memory(memory.memory_id.clone())),
                message: format!(
                    "High-risk memory needs re-check before use: {}",
                    memory.reverification_reason
                ),
                mitigation:
                    "Run the memory recheck commands and inspect current code, docs, and tests."
                        .to_string(),
            }),
            "stale" => memory.stale_label.as_ref().map(|label| RiskNote {
                severity: "warning".to_string(),
                identity: Some(StableIdentity::Memory(memory.memory_id.clone())),
                message: format!("Memory is not trusted guidance: {label}"),
                mitigation: "Use it only as historical evidence and verify against current code."
                    .to_string(),
            }),
            _ => Some(RiskNote {
                severity: "warning".to_string(),
                identity: Some(StableIdentity::Memory(memory.memory_id.clone())),
                message: format!("Memory is advisory, not proof: {}", memory.trust_reason),
                mitigation:
                    "Verify the claim against current code, docs, and tests before relying on it."
                        .to_string(),
            }),
        })
        .collect()
}

pub(crate) fn verification_commands(files: &[String], tests: &[String]) -> Vec<String> {
    let mut commands = Vec::new();
    if files.iter().any(|file| file.ends_with(".rs"))
        || tests.iter().any(|file| file.ends_with(".rs"))
    {
        commands.push("cd daemon && cargo test --workspace".to_string());
    }
    if files
        .iter()
        .any(|file| file.starts_with("daemon/crates/lattice-daemon"))
        || tests.iter().any(|file| file.contains("lattice-daemon"))
    {
        commands.push("cd daemon && cargo test -p lattice-daemon --lib".to_string());
    }
    if files
        .iter()
        .any(|file| file.ends_with(".ts") || file.ends_with(".tsx"))
    {
        commands.push("cd extension && npm run compile".to_string());
    }
    commands.extend(tests.iter().map(|file| {
        if file.ends_with(".rs") {
            format!("cd daemon && cargo test {}", file.replace('/', "::"))
        } else {
            format!("Run targeted test file {file}")
        }
    }));
    if commands.is_empty() {
        commands.push("cd daemon && cargo test --workspace".to_string());
    }
    commands.sort();
    commands.dedup();
    commands
}

pub(crate) fn bundle_from_task(
    workspace_id: &str,
    request: &WorkflowRequest,
    task: &TaskBundle,
    capsule: &ContextCapsule,
) -> WorkflowBundle {
    let memories = memory_highlights(workspace_id, &capsule.memories, "matched workflow query");
    let mut risks = risks_from_memories(&memories);
    risks.extend(task.risks.iter().map(|risk| RiskNote {
        severity: risk.level.clone(),
        identity: Some(StableIdentity::File(file_identity(
            workspace_id,
            &risk.file,
        ))),
        message: risk.reason.clone(),
        mitigation: "Inspect dependents and run the recommended tests before editing.".to_string(),
    }));
    let files = task
        .primary_files
        .iter()
        .map(|item| item.file.clone())
        .collect::<Vec<_>>();
    let tests = task
        .tests
        .iter()
        .map(|item| item.file.clone())
        .collect::<Vec<_>>();
    let mut commands = verification_commands(&files, &tests);
    for memory in &memories {
        for command in &memory.recheck_commands {
            if !commands.iter().any(|existing| existing == command) {
                commands.push(command.clone());
            }
        }
    }
    commands.sort();
    commands.dedup();
    WorkflowBundle {
        overview: task.overview.clone(),
        ranked_pivots: task
            .symbols
            .iter()
            .map(|symbol| Pivot {
                identity: StableIdentity::LegacyHandle(
                    symbol
                        .symbol_handle
                        .clone()
                        .unwrap_or_else(|| format!("symbol:{}", symbol.symbol)),
                ),
                kind: "symbol".to_string(),
                label: symbol.symbol.clone(),
                file: Some(symbol.file.clone()),
                symbol: Some(symbol.symbol.clone()),
                line: Some(symbol.line),
                score: symbol.score,
                inclusion_reason: symbol.evidence.join("; "),
                relevance_summary: None,
                relevance_breakdown: None,
                relevance_detail_handle: None,
                relevance_detail_focus: None,
            })
            .collect(),
        relevant_context: task
            .primary_files
            .iter()
            .chain(task.secondary_files.iter())
            .map(|file| ContextItem {
                identity: StableIdentity::File(file_identity(workspace_id, &file.file)),
                kind: "file".to_string(),
                label: file.file.clone(),
                file: Some(file.file.clone()),
                summary: file.reasons.join("; "),
                inclusion_reason: file.reasons.join("; "),
            })
            .collect(),
        memory_empty_rationale: empty_memory_rationale(&memories),
        memory_highlights: memories,
        event_episodes: event_episodes(workspace_id, "prepare_change"),
        suggested_next_expansion: expansion_from_value(
            &serde_json::to_value(&task.suggested_expand).ok(),
        ),
        stable_handles: stable_handles(&files),
        risks,
        render_choice: render_choice(request),
        verification_commands: commands,
        workflow_record: workflow_record("prepare_change", request, &files, &task.rationale),
        structured_payload: serde_json::to_value(task).unwrap_or_else(|_| json!({})),
    }
}

fn expansion_from_value(value: &Option<Value>) -> Option<ExpansionHint> {
    let object = value.as_ref()?.as_object()?;
    Some(ExpansionHint {
        focus: object.get("focus")?.as_str()?.to_string(),
        reason: object.get("reason")?.as_str()?.to_string(),
    })
}

pub(crate) fn render_choice(request: &WorkflowRequest) -> RenderChoice {
    RenderChoice {
        mode: request.render_mode.clone(),
        reason: "MCP request selected the workflow render mode".to_string(),
    }
}

pub(crate) fn empty_memory_rationale(memories: &[MemoryHighlight]) -> Option<String> {
    if memories.is_empty() {
        Some("No relevant memory matched the resolved anchors.".to_string())
    } else {
        None
    }
}

pub(crate) fn event_episodes(workspace_id: &str, tool: &str) -> Vec<EventEpisode> {
    vec![EventEpisode {
        event_id: event_identity(workspace_id, &format!("{tool}:current")),
        event_kind: "tool_called".to_string(),
        summary: format!("{tool} composed graph, memory, event, and working-memory context"),
        inclusion_reason: "current workflow execution".to_string(),
    }]
}

pub(crate) fn stable_handles(files: &[String]) -> Vec<String> {
    files.iter().map(|file| format!("file_id:{file}")).collect()
}

pub(crate) fn workflow_record(
    tool: &str,
    request: &WorkflowRequest,
    selected: &[String],
    rationale: &[String],
) -> WorkflowRecord {
    WorkflowRecord {
        tool: tool.to_string(),
        input: request.input.clone(),
        resolved_anchors: request
            .entry_files
            .iter()
            .map(|file| StableIdentity::File(file_identity("workspace", file)))
            .collect(),
        selected_candidates: selected.to_vec(),
        excluded_high_scoring_candidates: Vec::new(),
        working_memory_summary: if rationale.is_empty() {
            "working memory considered; no additional rationale returned".to_string()
        } else {
            rationale.join("; ")
        },
    }
}
