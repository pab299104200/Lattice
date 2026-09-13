//! Unified verification and explainability surface for durable memory.
//!
//! This module implements the spec contracts from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## MCP Surface`,
//! `## Verification Engine`, and
//! `## MCP Tool Contract Principles`.
//!
//! "Tool surface discipline: ten new memory tools is a meaningful cognitive
//! load for clients. Before shipping Phase 8, audit whether
//! The older verify/explain split is consolidated into this canonical handler.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use lattice_core::consolidation::{ConsolidationConfig, ConsolidationJobRuntime};
use lattice_core::graph::CodeGraph;
use lattice_core::identity::{MemoryId, ResolveOutcome};
use lattice_core::indexer::Indexer;
use lattice_core::intelligence::ExpandContextSeed;
use lattice_core::memory::{Memory, MemoryEvidence, MemoryStore, MemoryStructuredFields};
use lattice_core::storage::GraphStore;
use lattice_core::symbols::{ParsedFile, SymbolKind};
use lattice_core::verification::existence::VerifierCore;
use lattice_core::verification::{
    allows, ScopeFilter, SpanValidator, VerificationStatus, WorkspaceFileReader,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
/// Backward-compatible memory id input that accepts either a typed identity or a legacy ULID.
pub enum MemoryIdInput {
    Structured(MemoryId),
    Legacy(String),
}

impl MemoryIdInput {
    pub(crate) fn into_memory_id(self, fallback: &str) -> MemoryId {
        match self {
            Self::Structured(value) => value,
            Self::Legacy(value) => MemoryId {
                workspace_id: fallback.to_string(),
                ulid: value,
            },
        }
    }
}

/// Verification execution mode for the unified tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyExplainMode {
    /// Run fresh verification checks and return a compact status summary.
    Verify,
    /// Return the last persisted explain report without rerunning checks.
    Explain,
    /// Run fresh verification checks and return the full explain report.
    VerifyAndExplain,
}

/// Response detail mode for memory verification tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyExplainRenderMode {
    /// Status plus one-line summaries for failed checks.
    Compact,
    /// Full structured check results.
    Full,
    /// Full structured check results plus a human-readable diagnostic trace.
    Diagnostic,
}

/// Arguments for `verify_explain_memory`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyExplainArgs {
    /// Stable target memory identity.
    pub memory_id: MemoryIdInput,
    /// Whether to verify, explain from the cache, or do both.
    #[serde(default = "default_verify_explain_mode")]
    pub mode: VerifyExplainMode,
    /// Structured response detail level.
    #[serde(default = "default_render_mode")]
    pub render_mode: VerifyExplainRenderMode,
    /// Explicit repository-declared check identifier. Its absence guarantees
    /// that verification performs no external command execution.
    #[serde(default)]
    pub run_check: Option<String>,
}

/// Per-check outcome emitted by the unified tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    /// The check passed.
    Passed,
    /// The check failed and contributed to an advisory status.
    Failed,
    /// The check was skipped because no supporting evidence was available.
    Skipped,
}

/// Structured verification check result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    /// Stable check code such as `linked_symbol_missing`.
    pub kind: String,
    /// Human-readable verification target.
    pub target: String,
    /// Whether the check passed, failed, or was skipped.
    pub outcome: CheckOutcome,
    /// Stable evidence reference for the check.
    pub evidence_ref: String,
    /// Short explanation of the check result.
    pub detail: String,
}

/// Persisted explain report used by `mode=explain`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExplainReport {
    /// Verified memory identity.
    pub memory_id: MemoryId,
    /// Final verification status after classification.
    pub status: VerificationStatus,
    /// Full check trace captured during verification.
    pub checks: Vec<CheckResult>,
    /// Delta between the prior and current status-derived confidence weights.
    pub confidence_delta: f64,
    /// One-line summaries for failed checks.
    pub summary_lines: Vec<String>,
    /// Diagnostic trace describing every check that ran.
    pub diagnostic_trace: Vec<String>,
}

/// Response returned by the unified verify+explain tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifyExplainResponse {
    /// Final verification status after classification.
    pub status: VerificationStatus,
    /// Rendered check results for the requested render mode.
    pub checks: Vec<CheckResult>,
    /// Delta between the prior and current status-derived confidence weights.
    pub confidence_delta: f64,
    /// Expansion handle that lets callers pull richer context for the memory.
    pub expansion_handle: String,
    /// One-line summaries for the surfaced failures or advisory checks.
    pub summary_lines: Vec<String>,
    /// Requested structured response detail level.
    pub render_mode: VerifyExplainRenderMode,
    /// Full diagnostic trace when `render_mode=diagnostic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic_trace: Option<Vec<String>>,
}

/// Internal execution bundle returned before the caller attaches a context handle.
#[derive(Debug, Clone)]
pub struct VerifyExplainExecution {
    /// Full explain report persisted for future `mode=explain` calls.
    pub report: ExplainReport,
    /// Target memory identity.
    pub memory_id: MemoryId,
    /// Prior structured verification status before the current verification run.
    pub prior_status: VerificationStatus,
    /// Expand-context seed derived from the memory links.
    pub expansion_seed: ExpandContextSeed,
}

pub fn tool_definition() -> Value {
    json!({
        "name": "verify_explain_memory",
        "description": "Run fresh verification checks for a durable memory or explain the last persisted verification report.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "memory_id": {
                    "oneOf": [
                        {
                            "type": "object",
                            "properties": {
                                "workspace_id": {"type": "string"},
                                "ulid": {"type": "string"}
                            },
                            "required": ["workspace_id", "ulid"]
                        },
                        {
                            "type": "string",
                            "description": "Legacy compatibility form that accepts the memory ULID directly."
                        }
                    ]
                },
                "mode": {
                    "type": "string",
                    "enum": ["verify", "explain", "verify_and_explain"],
                    "default": "verify_and_explain"
                },
                "render_mode": {
                    "type": "string",
                    "enum": ["compact", "full", "diagnostic"],
                    "default": "full"
                },
                "run_check": {
                    "type": "string",
                    "description": "Explicitly run this repository-declared check before verification. Omit to execute no commands."
                }
            },
            "required": ["memory_id"]
        }
    })
}

pub fn parse_args(args: &Value) -> Result<VerifyExplainArgs, String> {
    serde_json::from_value(args.clone())
        .map_err(|error| format!("Invalid verify_explain_memory arguments: {error}"))
}

#[allow(clippy::too_many_arguments)]
pub fn execute_with_behavioral_validations(
    store: &MemoryStore,
    indexer: &Indexer,
    graph_store: &GraphStore,
    workspace_root: &Path,
    scope_filter: &ScopeFilter,
    reports: &mut HashMap<String, ExplainReport>,
    args: VerifyExplainArgs,
    validations: &[lattice_core::memory::BehavioralValidationRecord],
    repository_id: Option<&str>,
    checkout_id: Option<&str>,
    revision: Option<&str>,
    commit_binding: Option<&lattice_core::memory::store::VerificationCommitBinding>,
) -> Result<VerifyExplainExecution, String> {
    let memory_id = args
        .memory_id
        .clone()
        .into_memory_id(&scope_filter.workspace_id);
    match args.mode {
        VerifyExplainMode::Explain => {
            explain_only(store, scope_filter, reports, memory_id, checkout_id)
        }
        VerifyExplainMode::Verify | VerifyExplainMode::VerifyAndExplain => verify_and_cache(
            store,
            indexer,
            graph_store,
            workspace_root,
            scope_filter,
            reports,
            memory_id,
            validations,
            repository_id,
            checkout_id,
            revision,
            commit_binding,
        ),
    }
}

pub fn render_response(
    report: &ExplainReport,
    render_mode: VerifyExplainRenderMode,
    expansion_handle: String,
) -> VerifyExplainResponse {
    let checks = match render_mode {
        VerifyExplainRenderMode::Compact => compact_checks(&report.checks),
        VerifyExplainRenderMode::Full | VerifyExplainRenderMode::Diagnostic => {
            report.checks.clone()
        }
    };
    VerifyExplainResponse {
        status: report.status,
        checks,
        confidence_delta: report.confidence_delta,
        expansion_handle,
        summary_lines: report.summary_lines.clone(),
        render_mode,
        diagnostic_trace: matches!(render_mode, VerifyExplainRenderMode::Diagnostic)
            .then_some(report.diagnostic_trace.clone()),
    }
}

fn explain_only(
    store: &MemoryStore,
    scope_filter: &ScopeFilter,
    reports: &HashMap<String, ExplainReport>,
    memory_id: MemoryId,
    checkout_id: Option<&str>,
) -> Result<VerifyExplainExecution, String> {
    let memory = load_memory(store, scope_filter, &memory_id, checkout_id)?;
    let report = reports.get(&memory.id).cloned().ok_or_else(|| {
        format!(
            "No persisted explain report exists for memory `{}`",
            memory.id
        )
    })?;
    let prior_status = report.status;
    Ok(VerifyExplainExecution {
        report,
        memory_id,
        prior_status,
        expansion_seed: expand_seed(&memory),
    })
}

fn verify_and_cache(
    store: &MemoryStore,
    indexer: &Indexer,
    graph_store: &GraphStore,
    workspace_root: &Path,
    scope_filter: &ScopeFilter,
    reports: &mut HashMap<String, ExplainReport>,
    memory_id: MemoryId,
    validations: &[lattice_core::memory::BehavioralValidationRecord],
    repository_id: Option<&str>,
    checkout_id: Option<&str>,
    revision: Option<&str>,
    commit_binding: Option<&lattice_core::memory::store::VerificationCommitBinding>,
) -> Result<VerifyExplainExecution, String> {
    let repository_id = repository_id
        .filter(|id| !id.trim().is_empty())
        .ok_or("Verification requires explicit repository authority")?;
    let checkout_id = checkout_id
        .filter(|id| !id.trim().is_empty())
        .ok_or("Verification requires explicit checkout authority")?;
    if repository_id != scope_filter.workspace_id {
        return Err(
            "Verification repository authority does not match the active scope".to_string(),
        );
    }
    let applicable = store.with_connection(|conn| {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM memories WHERE id=?1 AND workspace_id=?2 AND is_invalidated=0 AND (applicable_checkout_id IS NULL OR applicable_checkout_id=?3))",
            rusqlite::params![memory_id.ulid, repository_id, checkout_id], |row| row.get::<_,bool>(0),
        ).map_err(|e| lattice_core::LatticeError::Storage(e.to_string()))
    }).map_err(|e| format!("Failed to validate verification checkout: {e}"))?;
    if !applicable {
        return Err("Memory is missing or outside the active verification checkout".to_string());
    }
    let memory = load_memory(store, scope_filter, &memory_id, Some(checkout_id))?;
    let mut fields = store
        .get_structured_fields(&memory.id)
        .map_err(|error| format!("Failed to load structured fields: {error}"))?
        .unwrap_or_default();
    let prior_status = memory_status_to_phase7(fields.verification_status);
    let owned_binding;
    let commit_binding = match commit_binding {
        Some(binding) => binding,
        None => {
            let authority = repository_id;
            owned_binding = lattice_core::memory::store::VerificationCommitBinding {
                repository_id: authority.to_owned(),
                checkout_id: checkout_id.to_owned(),
                branch: scope_filter
                    .branch
                    .as_ref()
                    .map(|branch| branch.name.clone())
                    .unwrap_or_else(|| "unknown".to_string()),
                target_digest: lattice_core::memory::MemoryStore::verification_target_digest_for(
                    &memory, &fields, authority,
                )
                .map_err(|error| format!("Failed to bind verification target: {error}"))?,
                observations: store
                    .trusted_check_observations(&memory.id)
                    .map_err(|error| format!("Failed to bind trusted observations: {error}"))?,
            };
            &owned_binding
        }
    };
    let phase7_status = run_phase7_verifier(
        store,
        indexer,
        graph_store,
        workspace_root,
        &memory,
        validations,
        repository_id,
        checkout_id,
        scope_filter
            .branch
            .as_ref()
            .map(|branch| branch.name.as_str())
            .unwrap_or("unknown"),
        revision,
    )?;
    let report = build_report(
        store,
        indexer,
        graph_store,
        workspace_root,
        scope_filter,
        &memory,
        &mut fields,
        prior_status,
        phase7_status,
    )?;
    persist_status(store, indexer, &memory, &fields, &report, commit_binding)?;
    reports.insert(memory.id.clone(), report.clone());
    Ok(VerifyExplainExecution {
        report,
        memory_id,
        prior_status,
        expansion_seed: expand_seed(&memory),
    })
}

fn build_report(
    store: &MemoryStore,
    indexer: &Indexer,
    graph_store: &GraphStore,
    workspace_root: &Path,
    scope_filter: &ScopeFilter,
    memory: &Memory,
    fields: &mut MemoryStructuredFields,
    prior_status: VerificationStatus,
    phase7_status: VerificationStatus,
) -> Result<ExplainReport, String> {
    let file_index = graph_store
        .load_file_index()
        .map_err(|error| format!("Failed to load file index: {error}"))?;
    let checks = build_checks(
        indexer.graph(),
        indexer.parsed_files(),
        workspace_root,
        scope_filter,
        memory,
        fields,
        &file_index,
    )?;
    let status = classify_status(prior_status, phase7_status, &checks);
    let confidence_delta = status_weight(status) - status_weight(prior_status);
    let summary_lines = summarize_failed_checks(&checks);
    let diagnostic_trace = checks
        .iter()
        .map(|check| {
            format!(
                "{} [{}] {} — {}",
                check.target,
                outcome_label(check),
                check.kind,
                check.detail
            )
        })
        .collect();
    fields.verification_status = phase7_to_memory_status(status);
    let _ = store;
    Ok(ExplainReport {
        memory_id: MemoryId {
            workspace_id: memory
                .workspace_id
                .clone()
                .unwrap_or_else(|| scope_filter.workspace_id.clone()),
            ulid: memory.id.clone(),
        },
        status,
        checks,
        confidence_delta,
        summary_lines,
        diagnostic_trace,
    })
}

fn build_checks(
    graph: &CodeGraph,
    parsed_files: &HashMap<String, ParsedFile>,
    workspace_root: &Path,
    scope_filter: &ScopeFilter,
    memory: &Memory,
    fields: &MemoryStructuredFields,
    file_index: &HashMap<String, lattice_core::storage::graph_store::FileIndexEntry>,
) -> Result<Vec<CheckResult>, String> {
    let mut checks = Vec::new();
    let resolver = lattice_core::identity::IdentityResolver::new(
        graph,
        file_index,
        parsed_files,
        scope_filter.workspace_id.clone(),
        Vec::new(),
    );
    push_file_checks(&mut checks, workspace_root, memory, fields);
    push_symbol_checks(&mut checks, graph, &resolver, memory, fields);
    push_doc_checks(&mut checks, workspace_root, parsed_files, fields);
    push_test_checks(&mut checks, graph, &resolver, fields);
    push_span_checks(&mut checks, workspace_root, fields)?;
    push_contradiction_checks(&mut checks, fields);
    push_supersession_checks(&mut checks, fields);
    push_scope_check(&mut checks, scope_filter, memory);
    push_expiry_check(&mut checks, memory, fields);
    Ok(checks)
}

fn push_file_checks(
    checks: &mut Vec<CheckResult>,
    workspace_root: &Path,
    memory: &Memory,
    fields: &MemoryStructuredFields,
) {
    let mut targets = BTreeMap::new();
    for file in &memory.linked_files {
        targets.insert(file.clone(), format!("linked_file:{file}"));
    }
    for evidence in file_evidence(fields) {
        if let Some(reference) = evidence.reference.as_ref() {
            targets
                .entry(reference.clone())
                .or_insert_with(|| evidence_ref(evidence));
        }
    }
    for (path, reference) in targets {
        let exists = workspace_root.join(&path).exists();
        checks.push(CheckResult {
            kind: if exists {
                "linked_file_exists".to_string()
            } else {
                "linked_file_missing".to_string()
            },
            target: path.clone(),
            outcome: if exists {
                CheckOutcome::Passed
            } else {
                CheckOutcome::Failed
            },
            evidence_ref: reference,
            detail: if exists {
                format!("linked file `{path}` still exists")
            } else {
                format!("linked file `{path}` no longer exists")
            },
        });
    }
}

fn push_symbol_checks(
    checks: &mut Vec<CheckResult>,
    graph: &CodeGraph,
    resolver: &lattice_core::identity::IdentityResolver<'_>,
    memory: &Memory,
    fields: &MemoryStructuredFields,
) {
    let mut targets = BTreeMap::new();
    for symbol in &memory.linked_symbols {
        targets.insert(symbol.clone(), format!("linked_symbol:{symbol}"));
    }
    for evidence in symbol_evidence(fields) {
        if let Some(reference) = evidence.reference.as_ref() {
            targets
                .entry(reference.clone())
                .or_insert_with(|| evidence_ref(evidence));
        }
    }
    for (symbol, reference) in targets {
        let exists = match resolver.resolve_symbol(resolver.default_workspace_id(), &symbol) {
            ResolveOutcome::Unique(symbol_id) => {
                graph.get_node(&legacy_symbol_id(&symbol_id)).is_some()
            }
            ResolveOutcome::Ambiguous(_) | ResolveOutcome::NotFound(_) => false,
        };
        checks.push(CheckResult {
            kind: if exists {
                "linked_symbol_exists".to_string()
            } else {
                "linked_symbol_missing".to_string()
            },
            target: symbol.clone(),
            outcome: if exists {
                CheckOutcome::Passed
            } else {
                CheckOutcome::Failed
            },
            evidence_ref: reference,
            detail: if exists {
                format!("linked symbol `{symbol}` still resolves")
            } else {
                format!("linked symbol `{symbol}` no longer resolves")
            },
        });
    }
}

fn push_doc_checks(
    checks: &mut Vec<CheckResult>,
    workspace_root: &Path,
    parsed_files: &HashMap<String, ParsedFile>,
    fields: &MemoryStructuredFields,
) {
    for doc in &fields.linked_docs {
        let (path, heading) = doc
            .split_once('#')
            .map_or((doc.as_str(), None), |value| (value.0, Some(value.1)));
        let file_exists = workspace_root.join(path).exists();
        let heading_exists = heading.map_or(true, |value| {
            parsed_files.get(path).is_some_and(|parsed| {
                parsed
                    .symbols
                    .iter()
                    .any(|symbol| symbol.kind == SymbolKind::Section && symbol.name == value)
            })
        });
        let exists = file_exists && heading_exists;
        checks.push(CheckResult {
            kind: if exists {
                "linked_doc_exists".to_string()
            } else {
                "linked_doc_missing".to_string()
            },
            target: doc.clone(),
            outcome: if exists {
                CheckOutcome::Passed
            } else {
                CheckOutcome::Failed
            },
            evidence_ref: format!("linked_doc:{doc}"),
            detail: if exists {
                format!("linked doc `{doc}` still resolves")
            } else {
                format!("linked doc `{doc}` no longer resolves")
            },
        });
    }
}

fn push_test_checks(
    checks: &mut Vec<CheckResult>,
    graph: &CodeGraph,
    resolver: &lattice_core::identity::IdentityResolver<'_>,
    fields: &MemoryStructuredFields,
) {
    for test in &fields.linked_tests {
        let exists = match resolver.resolve_test(resolver.default_workspace_id(), test) {
            ResolveOutcome::Unique(symbol_id) => {
                graph.get_node(&legacy_symbol_id(&symbol_id)).is_some()
            }
            ResolveOutcome::Ambiguous(_) | ResolveOutcome::NotFound(_) => false,
        };
        checks.push(CheckResult {
            kind: if exists {
                "linked_test_exists".to_string()
            } else {
                "linked_test_missing".to_string()
            },
            target: test.clone(),
            outcome: if exists {
                CheckOutcome::Passed
            } else {
                CheckOutcome::Failed
            },
            evidence_ref: format!("linked_test:{test}"),
            detail: if exists {
                format!("linked test `{test}` still resolves")
            } else {
                format!("linked test `{test}` no longer resolves")
            },
        });
    }
}

fn push_span_checks(
    checks: &mut Vec<CheckResult>,
    workspace_root: &Path,
    fields: &MemoryStructuredFields,
) -> Result<(), String> {
    let reader = WorkspaceFileReader::new(workspace_root.to_path_buf());
    for (index, evidence) in fields.evidence.iter().enumerate() {
        if evidence.span.is_none() {
            continue;
        }
        let evidence_id = format!("evidence:{index}");
        let verdict = SpanValidator::validate_with_reader(&evidence_id, evidence, &reader)
            .map_err(|error| format!("Failed to validate span evidence: {error}"))?;
        let passed = verdict.status == VerificationStatus::Verified;
        let target = evidence
            .reference
            .clone()
            .unwrap_or_else(|| "exact_span".to_string());
        checks.push(CheckResult {
            kind: if passed {
                "evidence_span_matches".to_string()
            } else {
                "evidence_span_mismatch".to_string()
            },
            target,
            outcome: if passed {
                CheckOutcome::Passed
            } else {
                CheckOutcome::Failed
            },
            evidence_ref: evidence_ref(evidence),
            detail: verdict.reason,
        });
    }
    Ok(())
}

fn push_contradiction_checks(checks: &mut Vec<CheckResult>, fields: &MemoryStructuredFields) {
    if fields.contradicted_by_memory_ids.is_empty() && fields.contradicts_memory_ids.is_empty() {
        checks.push(CheckResult {
            kind: "contradiction_state_coherent".to_string(),
            target: "memory".to_string(),
            outcome: CheckOutcome::Passed,
            evidence_ref: "memory:contradiction_state".to_string(),
            detail: "no contradiction edges are attached to the memory".to_string(),
        });
        return;
    }
    let mut targets = fields.contradicted_by_memory_ids.clone();
    targets.extend(fields.contradicts_memory_ids.clone());
    checks.push(CheckResult {
        kind: "memory_contradicted".to_string(),
        target: targets.join(", "),
        outcome: CheckOutcome::Failed,
        evidence_ref: "memory:contradiction_state".to_string(),
        detail: "contradiction edges are present on the memory".to_string(),
    });
}

fn push_supersession_checks(checks: &mut Vec<CheckResult>, fields: &MemoryStructuredFields) {
    if fields.superseded_by_memory_id.is_none() && fields.supersedes_memory_id.is_none() {
        checks.push(CheckResult {
            kind: "supersession_state_coherent".to_string(),
            target: "memory".to_string(),
            outcome: CheckOutcome::Passed,
            evidence_ref: "memory:supersession_state".to_string(),
            detail: "no supersession edges are attached to the memory".to_string(),
        });
        return;
    }
    let target = fields
        .superseded_by_memory_id
        .clone()
        .or_else(|| fields.supersedes_memory_id.clone())
        .unwrap_or_else(|| "unknown".to_string());
    checks.push(CheckResult {
        kind: "memory_superseded".to_string(),
        target,
        outcome: CheckOutcome::Failed,
        evidence_ref: "memory:supersession_state".to_string(),
        detail: "supersession edges are present on the memory".to_string(),
    });
}

fn push_scope_check(checks: &mut Vec<CheckResult>, scope_filter: &ScopeFilter, memory: &Memory) {
    let allowed = allows(memory, scope_filter);
    checks.push(CheckResult {
        kind: if allowed {
            "scope_holds".to_string()
        } else {
            "scope_leak_detected".to_string()
        },
        target: memory.id.clone(),
        outcome: if allowed {
            CheckOutcome::Passed
        } else {
            CheckOutcome::Failed
        },
        evidence_ref: format!("memory:scope:{}", memory.scope.as_str()),
        detail: if allowed {
            "memory scope matches the caller scope filter".to_string()
        } else {
            "memory scope would leak outside the caller scope filter".to_string()
        },
    });
}

fn push_expiry_check(
    checks: &mut Vec<CheckResult>,
    memory: &Memory,
    fields: &MemoryStructuredFields,
) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let expired = fields
        .freshness_policy_detail
        .as_deref()
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|value| value <= now);
    checks.push(CheckResult {
        kind: if expired {
            "memory_expired".to_string()
        } else {
            "expiry_holds".to_string()
        },
        target: memory.id.clone(),
        outcome: if expired {
            CheckOutcome::Failed
        } else {
            CheckOutcome::Passed
        },
        evidence_ref: "memory:expiry".to_string(),
        detail: if expired {
            "time-bound freshness policy has expired".to_string()
        } else {
            "memory is still within its freshness window".to_string()
        },
    });
}

fn persist_status(
    store: &MemoryStore,
    indexer: &Indexer,
    memory: &Memory,
    fields: &MemoryStructuredFields,
    report: &ExplainReport,
    binding: &lattice_core::memory::store::VerificationCommitBinding,
) -> Result<(), String> {
    let stale_reason = report.summary_lines.first().cloned();
    let is_stale = matches!(report.status, VerificationStatus::Stale);
    store
        .persist_verification_result(
            &memory.id,
            fields,
            phase7_to_memory_status(report.status),
            is_stale,
            stale_reason.as_deref(),
            now_unix_secs(),
            Some(indexer.graph_snapshot_id()),
            binding,
        )
        .map_err(|error| format!("Failed to persist memory verification status: {error}"))
}

fn load_memory(
    store: &MemoryStore,
    scope_filter: &ScopeFilter,
    memory_id: &MemoryId,
    checkout_id: Option<&str>,
) -> Result<Memory, String> {
    let memory = match checkout_id {
        Some(checkout) => {
            store.get_by_id_scoped_for_checkout(&memory_id.ulid, scope_filter, checkout)
        }
        None => store.get_by_id_scoped(&memory_id.ulid, scope_filter),
    }
    .map_err(|error| format!("Failed to load scoped memory: {error}"))?;
    if let Some(memory) = memory {
        return Ok(memory);
    }
    let exists_out_of_scope = store
        .get_by_id(&memory_id.ulid)
        .map_err(|error| format!("Failed to inspect memory scope: {error}"))?
        .is_some();
    if exists_out_of_scope {
        return Err(format!(
            "Memory `{}` is outside the active scope filter",
            memory_id.ulid
        ));
    }
    Err(format!("Memory `{}` was not found", memory_id.ulid))
}

fn run_phase7_verifier(
    store: &MemoryStore,
    indexer: &Indexer,
    graph_store: &GraphStore,
    workspace_root: &Path,
    memory: &Memory,
    validations: &[lattice_core::memory::BehavioralValidationRecord],
    repository_id: &str,
    checkout_id: &str,
    branch: &str,
    revision: Option<&str>,
) -> Result<VerificationStatus, String> {
    let conn = rusqlite::Connection::open_in_memory()
        .map_err(|error| format!("Failed to open verifier runtime database: {error}"))?;
    let mut runtime = ConsolidationJobRuntime::new(conn, ConsolidationConfig::default())
        .map_err(|error| format!("Failed to initialize verifier runtime: {error}"))?;
    let file_index = graph_store
        .load_file_index()
        .map_err(|error| format!("Failed to load file index: {error}"))?;
    let reader = WorkspaceFileReader::new(workspace_root.to_path_buf());
    if repository_id.trim().is_empty() || memory.workspace_id.as_deref() != Some(repository_id) {
        return Err("Verification requires matching active repository authority".to_string());
    }
    let workspace_id = repository_id;
    let authority = lattice_core::consolidation::EvolutionAuthority {
        repository_id,
        checkout_id,
        branch,
    };
    let mut verifier = VerifierCore::new(
        store,
        &mut runtime,
        indexer.graph(),
        &file_index,
        indexer.parsed_files(),
        &reader,
        workspace_id,
        &authority,
    );
    verifier = verifier.with_behavioral_validations(
        validations,
        repository_id,
        checkout_id,
        revision,
        Some(indexer.graph_snapshot_id()),
        now_unix_secs(),
        24 * 60 * 60,
    );
    verifier
        .evaluate_memory(&memory.id)
        .map(|outcome| outcome.verdict.status)
        .map_err(|error| {
            format!(
                "Phase 7 verifier failed for memory `{}`: {error}",
                memory.id
            )
        })
}

fn classify_status(
    prior_status: VerificationStatus,
    phase7_status: VerificationStatus,
    checks: &[CheckResult],
) -> VerificationStatus {
    if has_failed_check(checks, "scope_leak_detected") {
        return VerificationStatus::Invalidated;
    }
    if has_failed_check(checks, "memory_expired") {
        return VerificationStatus::Expired;
    }
    if has_failed_check(checks, "memory_contradicted") {
        return VerificationStatus::Contradicted;
    }
    if has_failed_check(checks, "memory_superseded") {
        return VerificationStatus::Superseded;
    }
    if checks.iter().any(is_staleness_failure) {
        return VerificationStatus::Stale;
    }
    if matches!(
        phase7_status,
        VerificationStatus::InReview | VerificationStatus::Unverified
    ) {
        return phase7_status;
    }
    if matches!(prior_status, VerificationStatus::InReview) {
        return VerificationStatus::InReview;
    }
    VerificationStatus::Verified
}

fn is_staleness_failure(check: &CheckResult) -> bool {
    matches!(check.outcome, CheckOutcome::Failed)
        && matches!(
            check.kind.as_str(),
            "linked_file_missing"
                | "linked_symbol_missing"
                | "linked_doc_missing"
                | "linked_test_missing"
                | "evidence_span_mismatch"
        )
}

fn summarize_failed_checks(checks: &[CheckResult]) -> Vec<String> {
    let summaries: Vec<String> = checks
        .iter()
        .filter(|check| matches!(check.outcome, CheckOutcome::Failed))
        .map(|check| format!("{}: {}", check.target, check.detail))
        .collect();
    if summaries.is_empty() {
        vec!["all verification checks passed".to_string()]
    } else {
        summaries
    }
}

fn compact_checks(checks: &[CheckResult]) -> Vec<CheckResult> {
    let failed: Vec<CheckResult> = checks
        .iter()
        .filter(|check| matches!(check.outcome, CheckOutcome::Failed))
        .cloned()
        .collect();
    if failed.is_empty() {
        checks
            .iter()
            .filter(|check| check.kind == "scope_holds")
            .cloned()
            .collect()
    } else {
        failed
    }
}

fn evidence_ref(evidence: &MemoryEvidence) -> String {
    evidence
        .reference
        .clone()
        .or_else(|| evidence.span.as_ref().map(|span| span.file_id.to_string()))
        .unwrap_or_else(|| format!("evidence:{}", evidence.kind))
}

fn file_evidence(fields: &MemoryStructuredFields) -> impl Iterator<Item = &MemoryEvidence> {
    fields
        .evidence
        .iter()
        .filter(|evidence| evidence.kind == "file")
}

fn symbol_evidence(fields: &MemoryStructuredFields) -> impl Iterator<Item = &MemoryEvidence> {
    fields
        .evidence
        .iter()
        .filter(|evidence| evidence.kind == "symbol")
}

fn legacy_symbol_id(
    symbol_id: &lattice_core::identity::SymbolId,
) -> lattice_core::symbols::SymbolId {
    lattice_core::symbols::SymbolId {
        file: symbol_id.file.repo_relative_path.clone(),
        name: symbol_id.qualified_name.clone(),
        byte_offset: symbol_id.byte_offset,
    }
}

fn expand_seed(memory: &Memory) -> ExpandContextSeed {
    ExpandContextSeed {
        query: Some(format!("verify durable memory {}", memory.id)),
        files: memory.linked_files.clone(),
        symbols: memory.linked_symbols.clone(),
        tests: Vec::new(),
        memories: vec![json!(memory.id)],
    }
}

fn phase7_to_memory_status(
    status: VerificationStatus,
) -> lattice_core::memory::MemoryVerificationStatus {
    match status {
        VerificationStatus::Verified => lattice_core::memory::MemoryVerificationStatus::Verified,
        VerificationStatus::Unverified => {
            lattice_core::memory::MemoryVerificationStatus::Unverified
        }
        VerificationStatus::InReview => lattice_core::memory::MemoryVerificationStatus::InReview,
        VerificationStatus::Stale => lattice_core::memory::MemoryVerificationStatus::Stale,
        VerificationStatus::Contradicted => {
            lattice_core::memory::MemoryVerificationStatus::Contradicted
        }
        VerificationStatus::Superseded => {
            lattice_core::memory::MemoryVerificationStatus::Superseded
        }
        VerificationStatus::Expired => lattice_core::memory::MemoryVerificationStatus::Expired,
        VerificationStatus::Invalidated => {
            lattice_core::memory::MemoryVerificationStatus::Invalidated
        }
    }
}

fn memory_status_to_phase7(
    status: lattice_core::memory::MemoryVerificationStatus,
) -> VerificationStatus {
    match status {
        lattice_core::memory::MemoryVerificationStatus::Verified => VerificationStatus::Verified,
        lattice_core::memory::MemoryVerificationStatus::Unverified => {
            VerificationStatus::Unverified
        }
        lattice_core::memory::MemoryVerificationStatus::InReview => VerificationStatus::InReview,
        lattice_core::memory::MemoryVerificationStatus::Stale => VerificationStatus::Stale,
        lattice_core::memory::MemoryVerificationStatus::Contradicted => {
            VerificationStatus::Contradicted
        }
        lattice_core::memory::MemoryVerificationStatus::Superseded => {
            VerificationStatus::Superseded
        }
        lattice_core::memory::MemoryVerificationStatus::Expired => VerificationStatus::Expired,
        lattice_core::memory::MemoryVerificationStatus::Invalidated => {
            VerificationStatus::Invalidated
        }
    }
}

fn has_failed_check(checks: &[CheckResult], kind: &str) -> bool {
    checks
        .iter()
        .any(|check| check.kind == kind && matches!(check.outcome, CheckOutcome::Failed))
}

fn status_weight(status: VerificationStatus) -> f64 {
    match status {
        VerificationStatus::Verified => 0.25,
        VerificationStatus::Unverified => 0.0,
        VerificationStatus::InReview => -0.05,
        VerificationStatus::Stale => -0.25,
        VerificationStatus::Contradicted => -0.45,
        VerificationStatus::Superseded => -0.35,
        VerificationStatus::Expired => -0.55,
        VerificationStatus::Invalidated => -0.70,
    }
}

fn outcome_label(check: &CheckResult) -> &'static str {
    match check.outcome {
        CheckOutcome::Passed => "passed",
        CheckOutcome::Failed => "failed",
        CheckOutcome::Skipped => "skipped",
    }
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn default_verify_explain_mode() -> VerifyExplainMode {
    VerifyExplainMode::VerifyAndExplain
}

fn default_render_mode() -> VerifyExplainRenderMode {
    VerifyExplainRenderMode::Full
}
