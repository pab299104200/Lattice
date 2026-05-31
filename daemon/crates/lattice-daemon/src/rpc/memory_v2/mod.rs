//! Memory V2 MCP surface for task-scoped retrieval, durable writes, and
//! auditable memory evolution.
//!
//! This module implements the task's `### 9. MCP Surface` and
//! `### 4. Memory Graph` contracts from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`.
//!
//! "Tool surface discipline: ten new memory tools is a meaningful cognitive
//! load for clients. Before shipping Phase 8, audit whether
//! `propose_memory_evolution` + `apply_memory_evolution` can collapse into a
//! single tool with an `action` parameter, and whether `verify_memory` +
//! `explain_memory` can be unified. The goal is the smallest surface that
//! covers all assistant workflows. Consolidate before stabilizing the MCP
//! contract."

use lattice_core::identity::{encode_identity, Identity, MemoryId};
use lattice_core::memory::model::{MemoryAssertionType, MemoryFreshnessPolicy, MemoryProvenance};
use lattice_core::memory::{
    Memory, MemoryAccessRecord, MemoryClass, MemoryEvidence, MemoryLinkRecord, MemoryScoreRecord,
    MemoryStructuredFields, MemoryType, MemoryVerificationStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub mod consolidate_session;
pub mod get_event_trace;
pub mod get_memory_metrics;
pub mod get_task_memory;
pub mod list_memory_conflicts;
pub mod propose_memory_evolution;
pub mod save_memory;
pub mod save_quick_memory;
pub mod verify_explain_memory;

#[cfg(test)]
mod admin_tools_tests;
#[cfg(test)]
mod memory_tools_tests;
#[cfg(test)]
mod verify_explain_tests;

/// The compact memory bundle returned for a task-scoped retrieval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskMemoryBundle {
    /// The task whose active and durable memory context was retrieved.
    pub task_id: String,
    /// The working-memory checkpoint used as the active context source, when present.
    pub checkpoint_id: Option<i64>,
    /// The current verification status of the task's working-memory state.
    pub working_memory_verification_status: String,
    /// The compact memory records returned for the task.
    pub memories: Vec<MemoryRecord>,
}

/// The fully annotated memory record returned by Memory V2 tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// Stable memory id.
    pub id: String,
    /// Encoded expansion handle for the memory identity.
    pub expansion_handle: String,
    /// Human-readable memory content.
    pub content: String,
    /// Spec-level memory class.
    pub memory_class: MemoryClass,
    /// Assertion type recorded for the claim.
    pub assertion_type: MemoryAssertionType,
    /// Durability scope for visibility.
    pub scope: String,
    /// Stored confidence score.
    pub confidence: f64,
    /// Optional explanation for the confidence score.
    pub confidence_reason: Option<String>,
    /// Current verification status.
    pub verification_status: String,
    /// Trust tier callers should use before treating the memory as guidance.
    pub trust_status: String,
    /// Machine-readable reason for the trust tier.
    pub trust_reason: String,
    /// High-risk domains inferred from the memory text and links.
    pub risk_domains: Vec<String>,
    /// Whether callers should re-check the claim before relying on it.
    pub requires_reverification: bool,
    /// Machine-readable reason for the re-verification requirement.
    pub reverification_reason: String,
    /// Current freshness status for the memory surface.
    pub freshness_status: String,
    /// Contradiction state summary.
    pub contradiction_state: String,
    /// Supersession state summary.
    pub supersession_state: String,
    /// Why the memory was included in the response.
    pub inclusion_reason: String,
    /// Approximate evidence strength for the claim.
    pub evidence_strength: f64,
    /// Files linked to the memory.
    pub linked_files: Vec<String>,
    /// Symbols linked to the memory.
    pub linked_symbols: Vec<String>,
    /// Docs linked to the memory.
    pub linked_docs: Vec<String>,
    /// Tests linked to the memory.
    pub linked_tests: Vec<String>,
    /// Memory ids linked from the memory.
    pub linked_memories: Vec<String>,
    /// Validity conditions for the claim.
    pub validity_conditions: Vec<String>,
    /// Invalidation triggers for the claim.
    pub invalidation_triggers: Vec<String>,
    /// Structured provenance entries.
    pub provenance: Vec<MemoryProvenance>,
    /// Structured evidence entries.
    pub evidence: Vec<MemoryEvidence>,
    /// Memory-to-memory links.
    pub links: Vec<MemoryLinkRecord>,
    /// Access history rows for the memory.
    pub access_history: Vec<MemoryAccessRecord>,
    /// Usefulness scores for the memory.
    pub usefulness_scores: Vec<MemoryScoreRecord>,
    /// Optional source query captured with the memory.
    pub source_query: Option<String>,
    /// Optional branch for branch-scoped memory.
    pub branch: Option<String>,
    /// Optional refresh key.
    pub refresh_key: Option<String>,
    /// Optional last verification timestamp.
    pub last_verified_at: Option<u64>,
    /// Optional verification graph snapshot id.
    pub last_verified_graph_snapshot_id: Option<u64>,
    /// Recorded and current git checkout state for the memory workspace.
    pub checkout_state: MemoryCheckoutState,
    /// Workspace/path mismatch diagnostic, when linked absolute paths do not belong to the memory workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_conflict: Option<Value>,
    /// Path ownership diagnostic for relative links that cannot prove workspace ownership.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path_diagnostic: Option<Value>,
    /// Bounded commands or command-like probes callers can run to re-check the claim now.
    pub recheck_commands: Vec<String>,
}

/// Git checkout state associated with a memory claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryCheckoutState {
    /// Git ref recorded when the memory was created, when available.
    pub recorded_head_ref: Option<String>,
    /// Git object id recorded when the memory was created, when available.
    pub recorded_head_oid: Option<String>,
    /// Current git ref for the memory workspace, when available.
    pub current_head_ref: Option<String>,
    /// Current git object id for the memory workspace, when available.
    pub current_head_oid: Option<String>,
    /// Comparison between recorded and current state.
    pub status: String,
}

/// The action accepted by the unified memory evolution tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionAction {
    /// Persist a proposal without mutating durable memory.
    Propose,
    /// Apply a previously staged proposal.
    Apply,
    /// Reject a previously staged proposal.
    Reject,
}

/// The auditable proposal returned by the unified memory evolution tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvolutionProposal {
    /// Proposal id persisted in the consolidation proposal store.
    pub proposal_id: String,
    /// Action executed by the tool.
    pub action: EvolutionAction,
    /// Source memory targeted by the proposal, when any.
    pub source_memory_id: Option<String>,
    /// Consolidation proposal kind inferred for the delta.
    pub proposal_kind: String,
    /// Current proposal decision state.
    pub decision: String,
    /// Prior state snapshot for auditable evolution.
    pub prior_state: serde_json::Value,
    /// Proposed state snapshot or applied state snapshot.
    pub proposed_state: serde_json::Value,
    /// Optional deprecation warning attached by shimmed calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecation_warning: Option<String>,
}

pub(crate) fn coarse_memory_type(memory_class: &MemoryClass) -> MemoryType {
    match memory_class {
        MemoryClass::Decision => MemoryType::Decision,
        MemoryClass::Pattern
        | MemoryClass::WorkflowOutcome
        | MemoryClass::FailurePattern
        | MemoryClass::Procedure
        | MemoryClass::Preference
        | MemoryClass::ArchitectureInvariant
        | MemoryClass::DocsContract => MemoryType::Pattern,
        MemoryClass::AntiPattern | MemoryClass::CounterMemory => MemoryType::AntiPattern,
        MemoryClass::Observation | MemoryClass::Constraint => MemoryType::Observation,
        MemoryClass::OpenQuestion => MemoryType::Exploration,
    }
}

pub(crate) fn default_assertion_type(memory_class: &MemoryClass) -> MemoryAssertionType {
    match memory_class {
        MemoryClass::Observation => MemoryAssertionType::Observation,
        MemoryClass::Decision => MemoryAssertionType::Decision,
        MemoryClass::Constraint => MemoryAssertionType::Constraint,
        MemoryClass::Pattern | MemoryClass::ArchitectureInvariant | MemoryClass::DocsContract => {
            MemoryAssertionType::Pattern
        }
        MemoryClass::AntiPattern => MemoryAssertionType::AntiPattern,
        MemoryClass::WorkflowOutcome | MemoryClass::FailurePattern => {
            MemoryAssertionType::WorkflowOutcome
        }
        MemoryClass::Procedure => MemoryAssertionType::Procedure,
        MemoryClass::Preference => MemoryAssertionType::Preference,
        MemoryClass::OpenQuestion => MemoryAssertionType::Question,
        MemoryClass::CounterMemory => MemoryAssertionType::Counter,
    }
}

pub(crate) fn expansion_handle(memory: &Memory, workspace_id: &str) -> String {
    encode_identity(&Identity::Memory(MemoryId {
        workspace_id: memory
            .workspace_id
            .clone()
            .unwrap_or_else(|| workspace_id.to_string()),
        ulid: memory.id.clone(),
    }))
}

pub(crate) fn freshness_status(
    memory: &Memory,
    fields: &MemoryStructuredFields,
    expires_at: Option<lattice_core::DateTime<lattice_core::Utc>>,
) -> String {
    if matches!(
        fields.verification_status,
        MemoryVerificationStatus::Expired
    ) {
        return "expired".to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    if expires_at.is_some_and(|value| value.unix_seconds() <= now) {
        return "expired".to_string();
    }
    if memory.is_stale || matches!(fields.verification_status, MemoryVerificationStatus::Stale) {
        return "stale".to_string();
    }
    match fields.freshness_policy {
        MemoryFreshnessPolicy::ManualReview => "manual_review".to_string(),
        MemoryFreshnessPolicy::TimeBound => "time_bound".to_string(),
        MemoryFreshnessPolicy::BranchScoped => "branch_scoped".to_string(),
        MemoryFreshnessPolicy::RepoScoped => "repo_scoped".to_string(),
        MemoryFreshnessPolicy::SessionScoped => "session_scoped".to_string(),
    }
}

pub(crate) fn contradiction_state(fields: &MemoryStructuredFields) -> String {
    if !fields.contradicted_by_memory_ids.is_empty() {
        "contradicted_by_other_memory".to_string()
    } else if !fields.contradicts_memory_ids.is_empty() {
        "contradicts_other_memory".to_string()
    } else {
        "none".to_string()
    }
}

pub(crate) fn supersession_state(fields: &MemoryStructuredFields) -> String {
    if fields.superseded_by_memory_id.is_some() {
        "superseded".to_string()
    } else if fields.supersedes_memory_id.is_some() {
        "supersedes_other_memory".to_string()
    } else {
        "none".to_string()
    }
}

pub(crate) fn evidence_strength(
    fields: &MemoryStructuredFields,
    scores: &[MemoryScoreRecord],
    access_count: u32,
) -> f64 {
    let evidence_weight = (fields.evidence.len() as f64 * 0.12).min(0.6);
    let provenance_weight = (fields.provenance.len() as f64 * 0.08).min(0.24);
    let score_weight = scores
        .iter()
        .map(|score| score.value.max(0.0) as f64)
        .fold(0.0_f64, f64::max)
        .min(1.0)
        * 0.12;
    let access_weight = (f64::from(access_count.min(8)) / 8.0) * 0.04;
    (0.1 + evidence_weight + provenance_weight + score_weight + access_weight).min(1.0)
}

pub(crate) fn memory_recheck_commands(
    memory: &Memory,
    fields: &MemoryStructuredFields,
) -> Vec<String> {
    let mut commands = Vec::new();
    for test in &fields.linked_tests {
        push_unique(&mut commands, command_for_test_ref(test));
    }
    for evidence in fields.evidence.iter().filter(|item| item.kind == "test") {
        if let Some(reference) = evidence.reference.as_deref() {
            push_unique(&mut commands, command_for_test_ref(reference));
        }
    }
    for file in &memory.linked_files {
        push_file_recheck(&mut commands, file);
    }
    for evidence in fields.evidence.iter().filter(|item| item.kind == "file") {
        if let Some(reference) = evidence.reference.as_deref() {
            push_file_recheck(&mut commands, reference);
        }
    }
    for doc in &fields.linked_docs {
        let path = doc.split_once('#').map_or(doc.as_str(), |item| item.0);
        if !path.trim().is_empty() {
            push_unique(
                &mut commands,
                format!("rg -n \"TODO|blocked|resolved|verified|stale\" {path}"),
            );
        }
    }
    for symbol in &memory.linked_symbols {
        push_unique(
            &mut commands,
            format!("rg -n \"{}\"", shell_safe_pattern(symbol)),
        );
    }
    if let Some(refresh_key) = memory.refresh_key.as_deref() {
        push_unique(
            &mut commands,
            format!("rg -n \"{}\" .", shell_safe_pattern(refresh_key)),
        );
    }
    commands.truncate(8);
    commands
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

fn shell_safe_pattern(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

pub(crate) fn memory_trust_status(
    memory: &Memory,
    fields: &MemoryStructuredFields,
    checkout_state: &MemoryCheckoutState,
) -> &'static str {
    if memory.is_stale
        || matches!(
            fields.verification_status,
            MemoryVerificationStatus::Stale
                | MemoryVerificationStatus::Superseded
                | MemoryVerificationStatus::Contradicted
                | MemoryVerificationStatus::Expired
                | MemoryVerificationStatus::Invalidated
        )
    {
        "stale"
    } else if matches!(
        fields.verification_status,
        MemoryVerificationStatus::Unverified | MemoryVerificationStatus::InReview
    ) || fields.evidence.is_empty()
        || matches!(
            checkout_state.status.as_str(),
            "head_changed" | "recorded_unknown"
        )
    {
        "advisory"
    } else {
        "trusted"
    }
}

pub(crate) fn memory_trust_reason(
    memory: &Memory,
    fields: &MemoryStructuredFields,
    checkout_state: &MemoryCheckoutState,
) -> &'static str {
    if memory.is_stale {
        return "marked_stale";
    }
    match fields.verification_status {
        MemoryVerificationStatus::Stale => "verification_stale",
        MemoryVerificationStatus::Superseded => "superseded",
        MemoryVerificationStatus::Contradicted => "contradicted",
        MemoryVerificationStatus::Expired => "expired",
        MemoryVerificationStatus::Invalidated => "invalidated",
        MemoryVerificationStatus::Unverified => "unverified",
        MemoryVerificationStatus::InReview => "verification_in_review",
        MemoryVerificationStatus::Verified => {
            if fields.evidence.is_empty() {
                "missing_evidence"
            } else if matches!(checkout_state.status.as_str(), "head_changed") {
                "git_head_changed"
            } else if matches!(checkout_state.status.as_str(), "recorded_unknown") {
                "git_recorded_state_missing"
            } else {
                "verified"
            }
        }
    }
}

pub(crate) fn memory_risk_domains(memory: &Memory, fields: &MemoryStructuredFields) -> Vec<String> {
    let haystack = format!(
        "{} {} {} {} {} {} {} {}",
        memory.content,
        memory.linked_files.join(" "),
        memory.linked_symbols.join(" "),
        fields.linked_docs.join(" "),
        fields.linked_tests.join(" "),
        fields.validity_conditions.join(" "),
        fields.invalidation_triggers.join(" "),
        fields
            .evidence
            .iter()
            .filter_map(|item| serde_json::to_string(item).ok())
            .collect::<Vec<_>>()
            .join(" ")
    )
    .to_ascii_lowercase();
    classify_risk_domains(&haystack)
}

pub(crate) fn classify_risk_domains(haystack: &str) -> Vec<String> {
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

pub(crate) fn memory_requires_reverification(
    memory: &Memory,
    fields: &MemoryStructuredFields,
    checkout_state: &MemoryCheckoutState,
    risk_domains: &[String],
    last_verified_at: Option<u64>,
) -> (bool, String) {
    if risk_domains.is_empty() {
        return (false, "not_high_risk".to_string());
    }
    if memory.is_stale {
        return (true, "stale_high_risk_memory".to_string());
    }
    if fields.verification_status != MemoryVerificationStatus::Verified {
        return (
            true,
            format!("high_risk_{}", fields.verification_status.as_str()),
        );
    }
    if fields.evidence.is_empty() {
        return (true, "high_risk_missing_evidence".to_string());
    }
    if checkout_state.status != "same_head" {
        return (true, format!("high_risk_{}", checkout_state.status));
    }
    if last_verified_at.is_none() {
        return (true, "high_risk_never_verified".to_string());
    }
    (false, "high_risk_verified_current".to_string())
}

fn push_domain_if(domains: &mut Vec<String>, domain: &str, haystack: &str, needles: &[&str]) {
    if needles.iter().any(|needle| haystack.contains(needle))
        && !domains.iter().any(|existing| existing == domain)
    {
        domains.push(domain.to_string());
    }
}

pub(crate) fn checkout_state_for_memory(
    memory: &Memory,
    fields: &MemoryStructuredFields,
    fallback_workspace_id: &str,
) -> MemoryCheckoutState {
    let recorded_head_ref = provenance_reference(fields, "git_head_ref");
    let recorded_head_oid = provenance_reference(fields, "git_head_oid");
    let workspace = memory
        .workspace_id
        .as_deref()
        .unwrap_or(fallback_workspace_id);
    let current = read_git_state(Path::new(workspace));
    let current_head_ref = current.as_ref().and_then(|state| state.head_ref.clone());
    let current_head_oid = current.as_ref().and_then(|state| state.head_oid.clone());
    let status = if recorded_head_oid.is_none() && recorded_head_ref.is_none() {
        "recorded_unknown"
    } else if current.is_none() {
        "current_unknown"
    } else if recorded_head_oid.is_some()
        && current_head_oid.is_some()
        && recorded_head_oid != current_head_oid
    {
        "head_changed"
    } else if recorded_head_ref.is_some()
        && current_head_ref.is_some()
        && recorded_head_ref != current_head_ref
    {
        "head_changed"
    } else {
        "same_head"
    };
    MemoryCheckoutState {
        recorded_head_ref,
        recorded_head_oid,
        current_head_ref,
        current_head_oid,
        status: status.to_string(),
    }
}

pub(crate) fn git_provenance(workspace_id: &str) -> Vec<MemoryProvenance> {
    let Some(state) = read_git_state(Path::new(workspace_id)) else {
        return Vec::new();
    };
    let mut provenance = Vec::new();
    if let Some(head_ref) = state.head_ref {
        provenance.push(MemoryProvenance {
            source: "git_head_ref".to_string(),
            reference: Some(head_ref),
            captured_at: None,
            note: Some("recorded_checkout_state".to_string()),
        });
    }
    if let Some(head_oid) = state.head_oid {
        provenance.push(MemoryProvenance {
            source: "git_head_oid".to_string(),
            reference: Some(head_oid),
            captured_at: None,
            note: Some("recorded_checkout_state".to_string()),
        });
    }
    provenance
}

pub(crate) fn memory_workspace_conflict(memory: &Memory) -> Option<Value> {
    let workspace = memory.workspace_id.as_deref()?;
    let mut conflicting_paths = Vec::new();
    for path in memory
        .linked_files
        .iter()
        .filter(|file| file.starts_with('/'))
    {
        if !Path::new(path).starts_with(workspace) {
            conflicting_paths.push(path.clone());
        }
    }
    if conflicting_paths.is_empty() {
        return None;
    }
    Some(json!({
        "kind": "linked_absolute_path_outside_memory_workspace",
        "workspace_id": workspace,
        "conflicting_paths": conflicting_paths.into_iter().take(8).collect::<Vec<_>>(),
        "reason": "Memory workspace provenance does not contain one or more linked absolute file paths; verify against the current checkout before using the claim."
    }))
}

pub(crate) fn memory_workspace_path_diagnostic(memory: &Memory) -> Option<Value> {
    memory.workspace_id.as_deref()?;
    if memory.linked_files.is_empty()
        || memory.linked_files.iter().any(|file| file.starts_with('/'))
    {
        return None;
    }
    Some(json!({
        "kind": "relative_paths_require_workspace_context",
        "reason": "Linked files are relative paths; workspace ownership is inferred from the memory workspace_id and should be rechecked if the claim came from another checkout."
    }))
}

fn provenance_reference(fields: &MemoryStructuredFields, source: &str) -> Option<String> {
    fields
        .provenance
        .iter()
        .find(|item| item.source == source)
        .and_then(|item| item.reference.clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitState {
    head_ref: Option<String>,
    head_oid: Option<String>,
}

fn read_git_state(workspace_root: &Path) -> Option<GitState> {
    let git_dir = resolve_git_dir(workspace_root)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let trimmed = head.trim();
    if let Some(head_ref) = trimmed.strip_prefix("ref:").map(str::trim) {
        Some(GitState {
            head_ref: Some(head_ref.to_string()),
            head_oid: resolve_ref_oid(&git_dir, head_ref),
        })
    } else if !trimmed.is_empty() {
        Some(GitState {
            head_ref: None,
            head_oid: Some(trimmed.to_string()),
        })
    } else {
        None
    }
}

fn resolve_git_dir(workspace_root: &Path) -> Option<PathBuf> {
    let git_path = workspace_root.join(".git");
    if git_path.is_dir() {
        return Some(git_path);
    }
    if git_path.is_file() {
        let gitdir = std::fs::read_to_string(&git_path).ok()?;
        let relative = gitdir.trim().strip_prefix("gitdir:")?.trim();
        return Some(workspace_root.join(relative));
    }
    None
}

fn resolve_ref_oid(git_dir: &Path, head_ref: &str) -> Option<String> {
    let ref_path = git_dir.join(head_ref);
    if let Ok(contents) = std::fs::read_to_string(ref_path) {
        let oid = contents.trim();
        if !oid.is_empty() {
            return Some(oid.to_string());
        }
    }
    let packed_refs = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
    for line in packed_refs.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('^') {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let oid = parts.next()?;
        let packed_ref = parts.next()?;
        if packed_ref == head_ref {
            return Some(oid.to_string());
        }
    }
    None
}
