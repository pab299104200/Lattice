//! Conflict surfacing for contradiction and supersession review.
//!
//! This module implements the spec contracts from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## MCP Surface`,
//! `## Verification Engine`, and
//! `## MCP Tool Contract Principles`.

use std::collections::BTreeMap;

use lattice_core::identity::{FileId, MemoryId, SectionId, SymbolId};
use lattice_core::memory::{query_conflicts, ConflictAnchorQuery, MemoryStore};
use lattice_core::verification::{ScopeFilter, VerificationStatus};
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
    fn ulid(&self) -> &str {
        match self {
            Self::Structured(value) => &value.ulid,
            Self::Legacy(value) => value.as_str(),
        }
    }

    fn workspace_id(&self, fallback: &str) -> String {
        match self {
            Self::Structured(value) => value.workspace_id.clone(),
            Self::Legacy(_) => fallback.to_string(),
        }
    }
}

/// Supported anchors for the conflict listing tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConflictAnchor {
    /// Anchor on a stable memory identity.
    Memory(MemoryIdInput),
    /// Anchor on a stable file identity.
    File(FileId),
    /// Anchor on a stable symbol identity.
    Symbol(SymbolId),
    /// Anchor on a stable doc-section identity.
    DocSection(SectionId),
}

/// Arguments for `list_memory_conflicts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListMemoryConflictsArgs {
    /// Memory or graph-node anchor to inspect.
    pub anchor: ConflictAnchor,
    /// Structured response detail level.
    #[serde(default = "default_render_mode")]
    pub render_mode: super::verify_explain_memory::VerifyExplainRenderMode,
    /// Maximum number of conflicts to return.
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Optional pagination cursor encoded as a numeric offset.
    #[serde(default)]
    pub cursor: Option<usize>,
}

/// Conflict edge surfaced for operator review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConflictRecord {
    /// Encoded source memory identity.
    pub source: String,
    /// Encoded target memory identity.
    pub target: String,
    /// Conflict edge type.
    pub link_type: String,
    /// Link strength on a normalized 0..1 scale.
    pub link_strength: f32,
    /// Actor or subsystem that created the edge.
    pub created_by: String,
    /// Link creation timestamp.
    pub created_at: u64,
    /// Verification status attached to the edge.
    pub link_verification_status: VerificationStatus,
    /// Human-readable reason for the conflict.
    pub reason: String,
}

/// Response returned by `list_memory_conflicts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListMemoryConflictsResponse {
    /// Human-readable anchor summary.
    pub anchor: String,
    /// Returned conflict edges for the current page.
    pub conflicts: Vec<ConflictRecord>,
    /// Total number of in-scope conflicts before pagination.
    pub total: usize,
    /// Next pagination cursor, when more conflicts remain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<usize>,
    /// Requested structured response detail level.
    pub render_mode: super::verify_explain_memory::VerifyExplainRenderMode,
    /// Compact per-conflict summaries.
    pub summary_lines: Vec<String>,
}

/// Internal execution bundle returned before event emission.
#[derive(Debug, Clone)]
pub struct ListMemoryConflictsExecution {
    /// Final rendered response payload.
    pub response: ListMemoryConflictsResponse,
    /// Memory identities surfaced in the response.
    pub surfaced_memory_ids: Vec<MemoryId>,
}

pub fn tool_definition() -> Value {
    json!({
        "name": "list_memory_conflicts",
        "description": "List contradiction and supersession edges anchored on a memory or graph node, with scope-aware pagination.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "anchor": {
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
                            "type": "object",
                            "properties": {
                                "workspace_id": {"type": "string"},
                                "repo_relative_path": {"type": "string"},
                                "content_hash": {"type": "string"}
                            },
                            "required": ["workspace_id", "repo_relative_path", "content_hash"]
                        },
                        {
                            "type": "object",
                            "properties": {
                                "file": {"type": "object"},
                                "qualified_name": {"type": "string"},
                                "byte_offset": {"type": "integer"},
                                "kind": {"type": "string"}
                            },
                            "required": ["file", "qualified_name", "byte_offset", "kind"]
                        },
                        {
                            "type": "object",
                            "properties": {
                                "doc": {"type": "object"},
                                "heading_path": {"type": "array", "items": {"type": "string"}},
                                "byte_offset": {"type": "integer"}
                            },
                            "required": ["doc", "heading_path", "byte_offset"]
                        },
                        {
                            "type": "string",
                            "description": "Legacy compatibility form that accepts the memory ULID directly."
                        }
                    ]
                },
                "render_mode": {
                    "type": "string",
                    "enum": ["compact", "full", "diagnostic"],
                    "default": "full"
                },
                "limit": {
                    "type": "integer",
                    "default": 25
                },
                "cursor": {
                    "type": "integer"
                }
            },
            "required": ["anchor"]
        }
    })
}

pub fn parse_args(args: &Value) -> Result<ListMemoryConflictsArgs, String> {
    serde_json::from_value(args.clone())
        .map_err(|error| format!("Invalid list_memory_conflicts arguments: {error}"))
}

pub fn execute(
    store: &MemoryStore,
    scope_filter: &ScopeFilter,
    applicable_checkout_id: Option<&str>,
    args: ListMemoryConflictsArgs,
) -> Result<ListMemoryConflictsExecution, String> {
    validate_anchor_authority(scope_filter, &args.anchor)?;
    let anchor_label = anchor_label(&args.anchor);
    let start = args.cursor.unwrap_or(0);
    let query_anchor = core_anchor(&args.anchor);
    let page = store
        .with_connection(|conn| {
            query_conflicts(
                conn,
                scope_filter,
                applicable_checkout_id,
                &query_anchor,
                start,
                args.limit,
            )
        })
        .map_err(|error| format!("Failed to inspect memory conflicts: {error}"))?;
    let conflicts: Vec<ConflictRecord> = page
        .records
        .into_iter()
        .map(|record| ConflictRecord {
            source: memory_identity(&record.source_workspace_id, &record.source_memory_id),
            target: memory_identity(&record.target_workspace_id, &record.target_memory_id),
            link_type: record.link_type,
            link_strength: record.link_strength,
            created_by: record.created_by,
            created_at: record.created_at,
            link_verification_status: parse_status(&record.verification_status),
            reason: record.reason,
        })
        .collect();
    let next_offset = start.checked_add(conflicts.len()).ok_or_else(|| {
        "Memory conflict cursor plus returned page length overflows usize".to_string()
    })?;
    let next_cursor = (next_offset < page.total).then_some(next_offset);
    let summary_lines = summarize_conflicts(&conflicts, args.render_mode);
    let surfaced_memory_ids = surfaced_memory_ids(&conflicts);
    Ok(ListMemoryConflictsExecution {
        response: ListMemoryConflictsResponse {
            anchor: anchor_label,
            conflicts,
            total: page.total,
            next_cursor,
            render_mode: args.render_mode,
            summary_lines,
        },
        surfaced_memory_ids,
    })
}

fn core_anchor(anchor: &ConflictAnchor) -> ConflictAnchorQuery {
    match anchor {
        ConflictAnchor::Memory(memory) => ConflictAnchorQuery::Memory(memory.ulid().to_string()),
        ConflictAnchor::File(file) => ConflictAnchorQuery::File(file.repo_relative_path.clone()),
        ConflictAnchor::Symbol(symbol) => {
            ConflictAnchorQuery::Symbol(symbol.qualified_name.clone())
        }
        ConflictAnchor::DocSection(section) => ConflictAnchorQuery::Doc(format!(
            "{}#{}",
            section.doc.repo_relative_path,
            section.heading_path.join(" > ")
        )),
    }
}

fn validate_anchor_authority(scope: &ScopeFilter, anchor: &ConflictAnchor) -> Result<(), String> {
    let requested_workspace = match anchor {
        ConflictAnchor::Memory(MemoryIdInput::Structured(memory)) => Some(&memory.workspace_id),
        ConflictAnchor::Memory(MemoryIdInput::Legacy(_)) => None,
        ConflictAnchor::File(file) => Some(&file.workspace_id),
        ConflictAnchor::Symbol(symbol) => Some(&symbol.file.workspace_id),
        ConflictAnchor::DocSection(section) => Some(&section.doc.workspace_id),
    };
    if requested_workspace.is_some_and(|workspace| workspace != &scope.workspace_id) {
        return Err("Conflict anchor is outside the active workspace scope".into());
    }
    Ok(())
}

fn summarize_conflicts(
    conflicts: &[ConflictRecord],
    render_mode: super::verify_explain_memory::VerifyExplainRenderMode,
) -> Vec<String> {
    let summaries: Vec<String> = conflicts
        .iter()
        .map(|conflict| {
            format!(
                "{} {} {} ({})",
                conflict.source, conflict.link_type, conflict.target, conflict.reason
            )
        })
        .collect();
    if matches!(
        render_mode,
        super::verify_explain_memory::VerifyExplainRenderMode::Compact
    ) {
        summaries.into_iter().take(5).collect()
    } else {
        summaries
    }
}

fn surfaced_memory_ids(conflicts: &[ConflictRecord]) -> Vec<MemoryId> {
    let mut seen = BTreeMap::new();
    for conflict in conflicts {
        for encoded in [&conflict.source, &conflict.target] {
            let decoded = parse_memory_identity(encoded);
            seen.entry(decoded.ulid.clone()).or_insert(decoded);
        }
    }
    seen.into_values().collect()
}

fn parse_memory_identity(encoded: &str) -> MemoryId {
    match lattice_core::identity::decode_identity(encoded).ok() {
        Some(lattice_core::identity::Identity::Memory(memory_id)) => memory_id,
        _ => MemoryId {
            workspace_id: "workspace-main".to_string(),
            ulid: encoded.to_string(),
        },
    }
}

fn memory_identity(workspace_id: &str, memory_id: &str) -> String {
    format!(
        "{}",
        MemoryId {
            workspace_id: workspace_id.to_string(),
            ulid: memory_id.to_string(),
        }
    )
}

fn anchor_label(anchor: &ConflictAnchor) -> String {
    match anchor {
        ConflictAnchor::Memory(memory_id) => format!(
            "memory:{}:{}",
            memory_id.workspace_id("workspace-main"),
            memory_id.ulid()
        ),
        ConflictAnchor::File(file) => format!("file:{}", file.repo_relative_path),
        ConflictAnchor::Symbol(symbol) => format!("symbol:{}", symbol.qualified_name),
        ConflictAnchor::DocSection(section) => format!(
            "doc_section:{}#{}",
            section.doc.repo_relative_path,
            section.heading_path.join(" > ")
        ),
    }
}

fn parse_status(value: &str) -> VerificationStatus {
    match value {
        "verified" => VerificationStatus::Verified,
        "in_review" => VerificationStatus::InReview,
        "stale" => VerificationStatus::Stale,
        "contradicted" => VerificationStatus::Contradicted,
        "superseded" => VerificationStatus::Superseded,
        "expired" => VerificationStatus::Expired,
        "invalidated" => VerificationStatus::Invalidated,
        _ => VerificationStatus::Unverified,
    }
}

fn default_limit() -> usize {
    25
}

fn default_render_mode() -> super::verify_explain_memory::VerifyExplainRenderMode {
    super::verify_explain_memory::VerifyExplainRenderMode::Full
}
