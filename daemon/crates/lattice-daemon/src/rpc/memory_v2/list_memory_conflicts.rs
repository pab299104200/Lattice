//! Conflict surfacing for contradiction and supersession review.
//!
//! This module implements the spec contracts from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## MCP Surface`,
//! `## Verification Engine`, and
//! `## MCP Tool Contract Principles`.

use std::collections::{BTreeMap, BTreeSet};

use lattice_core::identity::{FileId, MemoryId, SectionId, SymbolId};
use lattice_core::memory::{Memory, MemoryStore};
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
    args: ListMemoryConflictsArgs,
) -> Result<ListMemoryConflictsExecution, String> {
    let anchor_memories = resolve_anchor_memories(store, scope_filter, &args.anchor)?;
    let anchor_label = anchor_label(&args.anchor);
    let all_records = collect_conflicts(store, scope_filter, &anchor_memories)?;
    let start = args.cursor.unwrap_or(0);
    let limit = args.limit.max(1);
    let conflicts: Vec<ConflictRecord> = all_records
        .iter()
        .skip(start)
        .take(limit)
        .cloned()
        .collect();
    let next_cursor =
        (start + conflicts.len() < all_records.len()).then_some(start + conflicts.len());
    let summary_lines = summarize_conflicts(&conflicts, args.render_mode);
    let surfaced_memory_ids = surfaced_memory_ids(&conflicts);
    Ok(ListMemoryConflictsExecution {
        response: ListMemoryConflictsResponse {
            anchor: anchor_label,
            conflicts,
            total: all_records.len(),
            next_cursor,
            render_mode: args.render_mode,
            summary_lines,
        },
        surfaced_memory_ids,
    })
}

fn resolve_anchor_memories(
    store: &MemoryStore,
    scope_filter: &ScopeFilter,
    anchor: &ConflictAnchor,
) -> Result<Vec<Memory>, String> {
    match anchor {
        ConflictAnchor::Memory(memory_id) => {
            let scoped = store
                .get_by_id_scoped(memory_id.ulid(), scope_filter)
                .map_err(|error| format!("Failed to load scoped memory anchor: {error}"))?;
            if let Some(memory) = scoped {
                return Ok(vec![memory]);
            }
            let out_of_scope = store
                .get_by_id(memory_id.ulid())
                .map_err(|error| format!("Failed to inspect memory anchor scope: {error}"))?
                .is_some();
            if out_of_scope {
                return Err(format!(
                    "Memory `{}` is outside the active scope filter",
                    memory_id.ulid()
                ));
            }
            Err(format!("Memory `{}` was not found", memory_id.ulid()))
        }
        ConflictAnchor::File(file) => scoped_memories_matching(store, scope_filter, |memory| {
            memory
                .linked_files
                .iter()
                .any(|value| value == &file.repo_relative_path)
        }),
        ConflictAnchor::Symbol(symbol) => scoped_memories_matching(store, scope_filter, |memory| {
            memory
                .linked_symbols
                .iter()
                .any(|value| value == &symbol.qualified_name)
        }),
        ConflictAnchor::DocSection(section) => {
            let doc_path = &section.doc.repo_relative_path;
            let heading = section.heading_path.join(" > ");
            scoped_memories_matching(store, scope_filter, |memory| {
                store
                    .get_structured_fields(&memory.id)
                    .ok()
                    .flatten()
                    .is_some_and(|fields| {
                        fields.linked_docs.iter().any(|value| {
                            value == doc_path || value == &format!("{doc_path}#{heading}")
                        })
                    })
            })
        }
    }
}

fn scoped_memories_matching<F>(
    store: &MemoryStore,
    scope_filter: &ScopeFilter,
    predicate: F,
) -> Result<Vec<Memory>, String>
where
    F: Fn(&Memory) -> bool,
{
    let scoped = store
        .list_all_scoped(scope_filter)
        .map_err(|error| format!("Failed to list scoped memories: {error}"))?;
    let matches: Vec<Memory> = scoped.into_iter().filter(predicate).collect();
    if matches.is_empty() {
        return Err("No in-scope memories matched the requested anchor".to_string());
    }
    Ok(matches)
}

fn collect_conflicts(
    store: &MemoryStore,
    scope_filter: &ScopeFilter,
    anchor_memories: &[Memory],
) -> Result<Vec<ConflictRecord>, String> {
    let mut dedupe = BTreeSet::new();
    let mut records = Vec::new();
    for memory in anchor_memories {
        collect_link_records(store, scope_filter, memory, &mut dedupe, &mut records)?;
        collect_structured_conflicts(store, scope_filter, memory, &mut dedupe, &mut records)?;
    }
    records.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.source.cmp(&right.source))
            .then_with(|| left.target.cmp(&right.target))
    });
    Ok(records)
}

fn collect_link_records(
    store: &MemoryStore,
    scope_filter: &ScopeFilter,
    memory: &Memory,
    dedupe: &mut BTreeSet<String>,
    records: &mut Vec<ConflictRecord>,
) -> Result<(), String> {
    for link in store
        .list_memory_links_from(&memory.id)
        .map_err(|error| format!("Failed to load outbound memory links: {error}"))?
        .into_iter()
        .filter(is_conflict_link)
    {
        let target = load_conflict_memory(store, scope_filter, &link.target_memory_id)?;
        insert_record(
            dedupe,
            records,
            build_record(
                memory_identity(memory),
                memory_identity(&target),
                &link.link_type,
                &link.reason,
                link.created_at,
                parse_status(&link.verification_status),
            ),
        );
    }
    for link in store
        .list_memory_links_to(&memory.id)
        .map_err(|error| format!("Failed to load inbound memory links: {error}"))?
        .into_iter()
        .filter(is_conflict_link)
    {
        let source = load_conflict_memory(store, scope_filter, &link.source_memory_id)?;
        insert_record(
            dedupe,
            records,
            build_record(
                memory_identity(&source),
                memory_identity(memory),
                &link.link_type,
                &link.reason,
                link.created_at,
                parse_status(&link.verification_status),
            ),
        );
    }
    Ok(())
}

fn collect_structured_conflicts(
    store: &MemoryStore,
    scope_filter: &ScopeFilter,
    memory: &Memory,
    dedupe: &mut BTreeSet<String>,
    records: &mut Vec<ConflictRecord>,
) -> Result<(), String> {
    let Some(fields) = store
        .get_structured_fields(&memory.id)
        .map_err(|error| format!("Failed to load structured conflict metadata: {error}"))?
    else {
        return Ok(());
    };
    for target_id in &fields.contradicted_by_memory_ids {
        let target = load_conflict_memory(store, scope_filter, target_id)?;
        insert_record(
            dedupe,
            records,
            build_record(
                memory_identity(&target),
                memory_identity(memory),
                "contradicts",
                "structured contradicted_by edge",
                memory.created_at,
                VerificationStatus::Contradicted,
            ),
        );
    }
    for target_id in &fields.contradicts_memory_ids {
        let target = load_conflict_memory(store, scope_filter, target_id)?;
        insert_record(
            dedupe,
            records,
            build_record(
                memory_identity(memory),
                memory_identity(&target),
                "contradicts",
                "structured contradicts edge",
                memory.created_at,
                VerificationStatus::Contradicted,
            ),
        );
    }
    if let Some(target_id) = fields.superseded_by_memory_id.as_deref() {
        let target = load_conflict_memory(store, scope_filter, target_id)?;
        insert_record(
            dedupe,
            records,
            build_record(
                memory_identity(&target),
                memory_identity(memory),
                "supersedes",
                "structured superseded_by edge",
                memory.created_at,
                VerificationStatus::Superseded,
            ),
        );
    }
    if let Some(target_id) = fields.supersedes_memory_id.as_deref() {
        let target = load_conflict_memory(store, scope_filter, target_id)?;
        insert_record(
            dedupe,
            records,
            build_record(
                memory_identity(memory),
                memory_identity(&target),
                "supersedes",
                "structured supersedes edge",
                memory.created_at,
                VerificationStatus::Superseded,
            ),
        );
    }
    Ok(())
}

fn load_conflict_memory(
    store: &MemoryStore,
    scope_filter: &ScopeFilter,
    memory_id: &str,
) -> Result<Memory, String> {
    if let Some(memory) = store
        .get_by_id_scoped(memory_id, scope_filter)
        .map_err(|error| format!("Failed to load scoped conflict memory: {error}"))?
    {
        return Ok(memory);
    }
    let out_of_scope = store
        .get_by_id(memory_id)
        .map_err(|error| format!("Failed to inspect conflict scope: {error}"))?
        .is_some();
    if out_of_scope {
        return Err(format!(
            "Conflict memory `{memory_id}` is outside the active scope filter"
        ));
    }
    Err(format!("Conflict memory `{memory_id}` was not found"))
}

fn insert_record(
    dedupe: &mut BTreeSet<String>,
    records: &mut Vec<ConflictRecord>,
    record: ConflictRecord,
) {
    let key = format!(
        "{}:{}:{}:{}",
        record.source, record.target, record.link_type, record.reason
    );
    if dedupe.insert(key) {
        records.push(record);
    }
}

fn build_record(
    source: String,
    target: String,
    link_type: &str,
    reason: &str,
    created_at: u64,
    status: VerificationStatus,
) -> ConflictRecord {
    ConflictRecord {
        source,
        target,
        link_type: link_type.to_string(),
        link_strength: 1.0,
        created_by: "system".to_string(),
        created_at,
        link_verification_status: status,
        reason: reason.to_string(),
    }
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

fn memory_identity(memory: &Memory) -> String {
    format!(
        "{}",
        MemoryId {
            workspace_id: memory
                .workspace_id
                .clone()
                .unwrap_or_else(|| "workspace-main".to_string()),
            ulid: memory.id.clone(),
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

fn is_conflict_link(link: &lattice_core::memory::MemoryLinkRecord) -> bool {
    matches!(link.link_type.as_str(), "contradicts" | "supersedes")
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
