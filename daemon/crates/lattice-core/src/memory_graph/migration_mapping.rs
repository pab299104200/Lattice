use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use super::migration::{
    LegacyMemoryRow, MappedAccess, MappedEvidence, MappedLink, MappedMemoryRow, MappedScore,
    MIGRATION_GUIDE,
};
use crate::events::Actor;
use crate::identity::{EventId, FileId, Identity, MemoryId, SymbolId};
use crate::memory_graph::{
    classify_stream, default_policy, encode_identity_text, AssertionType, EvidenceAnchor,
    FreshnessKind, FreshnessPolicy, MemoryClass, MemoryGraphParseError, MemoryScope, ScoreKind,
    TestId, VerificationStatus,
};

pub(super) fn map_row(row: &LegacyMemoryRow) -> Result<MappedMemoryRow, String> {
    if row.is_invalidated != 0 {
        return Err("legacy row is invalidated".to_string());
    }
    let workspace_id = row
        .workspace_id
        .clone()
        .unwrap_or_else(|| "legacy".to_string());
    let class = map_class(&row.memory_type, &row.assertion_type)?;
    let assertion_type = map_assertion_type(&row.assertion_type, class)?;
    let stream = classify_stream(class, assertion_type);
    let scope = resolve_scope(&row.scope, &row.freshness_policy, stream)?;
    let policy = map_freshness_policy(&row.freshness_policy, stream);
    let memory_id = MemoryId {
        workspace_id: workspace_id.clone(),
        ulid: row.id.clone(),
    };
    let linked_files = parse_string_array(&row.linked_files, "linked_files")?;
    let linked_symbols = parse_string_array(&row.linked_symbols, "linked_symbols")?;
    let contradicts = parse_string_array(&row.contradicts_memory_ids, "contradicts_memory_ids")?;
    Ok(MappedMemoryRow {
        source_row_id: row.source_row_id,
        memory_id: memory_id.clone(),
        class,
        assertion_type,
        scope,
        scope_session_id: scope_session(scope, row),
        scope_branch: scope_branch(scope, row),
        scope_workspace_id: scope_workspace(scope, row, &workspace_id),
        verification_status: map_verification_status(row)?,
        confidence: row.confidence.clamp(0.0, 1.0),
        confidence_reason: confidence_reason(row),
        content: row.content.clone(),
        freshness_policy_json: json_string(&policy)?,
        validity_conditions_json: json_string(&Vec::<Value>::new())?,
        invalidation_triggers_json: json_string(&Vec::<Value>::new())?,
        provenance_event_ids_json: json_string(&Vec::<EventId>::new())?,
        evidence_references_json: json_string(&legacy_json(&row.evidence_json)?)?,
        linked_files_json: json_string(&file_ids(&workspace_id, &linked_files))?,
        linked_symbols_json: json_string(&symbol_ids(&workspace_id, &linked_symbols))?,
        linked_docs_json: json_string(&Vec::<Value>::new())?,
        linked_tests_json: json_string(&Vec::<TestId>::new())?,
        linked_memories_json: json_string(&memory_refs(
            &workspace_id,
            &contradicts,
            "contradicts",
        ))?,
        contradiction_links_json: json_string(&memory_refs(
            &workspace_id,
            &contradicts,
            "contradicts",
        ))?,
        supersession_links_json: json_string(&supersession_refs(row, &workspace_id))?,
        access_history_json: json_string(&access_history(row))?,
        usefulness_score: row.confidence.clamp(0.0, 1.0),
        created_at: row.created_at,
        updated_at: row.last_accessed.max(row.created_at),
        superseded_by: row.superseded_by_memory_id.as_ref().map(|id| MemoryId {
            workspace_id: workspace_id.clone(),
            ulid: id.clone(),
        }),
        links: map_links(
            row,
            &memory_id,
            &workspace_id,
            &linked_files,
            &linked_symbols,
            &contradicts,
        ),
        evidence: map_evidence(row, &memory_id, &workspace_id)?,
        accesses: map_accesses(row, &workspace_id),
        scores: map_scores(row),
    })
}

pub(super) fn migration_actor(source_row_id: i64) -> String {
    let actor = Actor::Daemon;
    format!("MigrationFrom {{ source_row_id: {source_row_id}, actor: {actor:?} }}")
}

fn map_class(memory_type: &str, assertion_type: &str) -> Result<MemoryClass, String> {
    match memory_type {
        "observation" => Ok(MemoryClass::Observation),
        "decision" => Ok(MemoryClass::Decision),
        "exploration" => Ok(MemoryClass::OpenQuestion),
        "pattern" => Ok(MemoryClass::Pattern),
        "anti_pattern" => Ok(MemoryClass::AntiPattern),
        _ => match assertion_type {
            "workflow_outcome" => Ok(MemoryClass::WorkflowOutcome),
            "constraint" => Ok(MemoryClass::Constraint),
            _ => Err(format!("unknown legacy memory_type `{memory_type}`")),
        },
    }
}

fn map_assertion_type(value: &str, class: MemoryClass) -> Result<AssertionType, String> {
    match value {
        "observation" | "pattern" | "anti_pattern" | "exploration" => {
            Ok(AssertionType::Observation)
        }
        "decision" => Ok(AssertionType::Decision),
        "constraint" => Ok(AssertionType::Constraint),
        "workflow_outcome" => Ok(AssertionType::Outcome),
        other => AssertionType::from_str(other)
            .or_else(|_| default_assertion_for_class(class))
            .map_err(|error| error.to_string()),
    }
}

fn default_assertion_for_class(class: MemoryClass) -> Result<AssertionType, MemoryGraphParseError> {
    Ok(match class {
        MemoryClass::Decision | MemoryClass::ArchitectureInvariant => AssertionType::Decision,
        MemoryClass::Constraint | MemoryClass::DocsContract => AssertionType::Constraint,
        MemoryClass::Procedure => AssertionType::Procedure,
        MemoryClass::WorkflowOutcome => AssertionType::Outcome,
        MemoryClass::Preference => AssertionType::Preference,
        MemoryClass::OpenQuestion => AssertionType::Question,
        MemoryClass::CounterMemory => AssertionType::Counter,
        MemoryClass::AntiPattern => AssertionType::Hypothesis,
        _ => AssertionType::Observation,
    })
}

fn map_verification_status(row: &LegacyMemoryRow) -> Result<VerificationStatus, String> {
    if row.is_stale != 0 {
        return Ok(VerificationStatus::Stale);
    }
    match row.verification_status.as_str() {
        "unverified" => Ok(VerificationStatus::Unverified),
        "in_review" => Ok(VerificationStatus::InReview),
        "verified" => Ok(VerificationStatus::Verified),
        "stale" => Ok(VerificationStatus::Stale),
        "contradicted" => Ok(VerificationStatus::Contradicted),
        "superseded" => Ok(VerificationStatus::Superseded),
        other => Err(format!("unknown verification_status `{other}`")),
    }
}

fn resolve_scope(
    value: &str,
    freshness: &str,
    stream: crate::memory_graph::MemoryStream,
) -> Result<MemoryScope, String> {
    match value {
        "session" => Ok(MemoryScope::Session),
        "branch" => Ok(MemoryScope::Branch),
        "repo" => Ok(MemoryScope::Repo),
        "" => Ok(default_policy(stream).default_scope),
        _ if freshness == "repo_scoped" => Ok(MemoryScope::Repo),
        _ if freshness == "branch_scoped" => Ok(MemoryScope::Branch),
        other => Err(format!("unknown legacy scope `{other}`")),
    }
}

fn map_freshness_policy(value: &str, stream: crate::memory_graph::MemoryStream) -> FreshnessPolicy {
    match value {
        "session_scoped" => scoped_freshness(FreshnessKind::SessionScoped),
        "branch_scoped" => scoped_freshness(FreshnessKind::BranchScoped),
        "repo_scoped" => scoped_freshness(FreshnessKind::RepoScoped),
        "time_bound" => FreshnessPolicy {
            kind: FreshnessKind::TimeBound,
            ttl: Some(Duration::from_secs(30 * 24 * 60 * 60)),
            recheck_interval: Some(Duration::from_secs(7 * 24 * 60 * 60)),
        },
        _ => default_policy(stream).freshness_default,
    }
}

fn scoped_freshness(kind: FreshnessKind) -> FreshnessPolicy {
    FreshnessPolicy {
        kind,
        ttl: None,
        recheck_interval: None,
    }
}

fn map_links(
    row: &LegacyMemoryRow,
    memory_id: &MemoryId,
    workspace_id: &str,
    files: &[String],
    symbols: &[String],
    contradicts: &[String],
) -> Vec<MappedLink> {
    let mut links = Vec::new();
    for file in files {
        links.push(link(
            row,
            memory_id,
            "file",
            file_identity(workspace_id, file),
            "applies_to",
        ));
    }
    for symbol in symbols {
        links.push(link(
            row,
            memory_id,
            "symbol",
            symbol_identity(workspace_id, symbol),
            "applies_to",
        ));
    }
    if let Some(target) = &row.supersedes_memory_id {
        links.push(link(
            row,
            memory_id,
            "memory",
            memory_identity(workspace_id, target),
            "supersedes",
        ));
    }
    for target in contradicts {
        links.push(link(
            row,
            memory_id,
            "memory",
            memory_identity(workspace_id, target),
            "contradicts",
        ));
    }
    links
}

fn link(
    row: &LegacyMemoryRow,
    memory_id: &MemoryId,
    target_kind: &'static str,
    target_id: String,
    link_type: &'static str,
) -> MappedLink {
    MappedLink {
        link_id: format!(
            "migration:{}:{}:{}",
            row.source_row_id, memory_id.ulid, target_id
        ),
        target_kind,
        target_id,
        link_type,
        reason: format!("Migrated from legacy memories. See {MIGRATION_GUIDE}."),
    }
}

fn map_evidence(
    row: &LegacyMemoryRow,
    memory_id: &MemoryId,
    workspace_id: &str,
) -> Result<Vec<MappedEvidence>, String> {
    let values = legacy_array(&row.evidence_json)?;
    let mut evidence = Vec::new();
    for (index, value) in values.iter().enumerate() {
        let event = EventId {
            workspace_id: workspace_id.to_string(),
            ulid: format!("legacy-evidence-{}-{index}", row.source_row_id),
        };
        evidence.push(MappedEvidence {
            evidence_id: format!(
                "migration:{}:{}:evidence:{index}",
                row.source_row_id, memory_id.ulid
            ),
            event_id: Some(encode_identity_text(&Identity::Event(event.clone()))),
            anchor_kind: "event_reference",
            anchor_json: json_string(&EvidenceAnchor::EventReference(event))?,
            captured_at: value
                .get("captured_at")
                .and_then(Value::as_i64)
                .unwrap_or(row.created_at),
        });
    }
    Ok(evidence)
}

fn map_accesses(row: &LegacyMemoryRow, workspace_id: &str) -> Vec<MappedAccess> {
    if row.access_count <= 0 {
        return Vec::new();
    }
    let event = EventId {
        workspace_id: workspace_id.to_string(),
        ulid: format!("legacy-access-{}", row.source_row_id),
    };
    vec![MappedAccess {
        access_id: format!("migration:{}:access", row.source_row_id),
        accessed_at: row.last_accessed,
        accessed_in_event: encode_identity_text(&Identity::Event(event)),
        inclusion_reason: "Legacy memory last_accessed/access_count metadata".to_string(),
        was_used: Some(1),
    }]
}

fn map_scores(row: &LegacyMemoryRow) -> Vec<MappedScore> {
    vec![MappedScore {
        score_kind: ScoreKind::UsefulnessPrior.as_str(),
        value: row.confidence.clamp(0.0, 1.0),
        computed_at: row.created_at,
        computed_from_window_secs: 0,
        sample_size: row.access_count.max(0),
    }]
}

fn parse_string_array(encoded: &str, column: &'static str) -> Result<Vec<String>, String> {
    serde_json::from_str(encoded).map_err(|error| format!("invalid {column} JSON: {error}"))
}

fn legacy_array(encoded: &str) -> Result<Vec<Value>, String> {
    serde_json::from_str(encoded).map_err(|error| format!("invalid legacy evidence JSON: {error}"))
}

fn legacy_json(encoded: &str) -> Result<Value, String> {
    serde_json::from_str(encoded).map_err(|error| format!("invalid legacy JSON: {error}"))
}

fn json_string<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
}

fn file_ids(workspace_id: &str, files: &[String]) -> Vec<FileId> {
    files
        .iter()
        .map(|file| file_id(workspace_id, file))
        .collect()
}

fn symbol_ids(workspace_id: &str, symbols: &[String]) -> Vec<SymbolId> {
    symbols
        .iter()
        .map(|symbol| symbol_id(workspace_id, symbol))
        .collect()
}

fn file_id(workspace_id: &str, file: &str) -> FileId {
    FileId {
        workspace_id: workspace_id.to_string(),
        repo_relative_path: file.to_string(),
        content_hash: "legacy".to_string(),
    }
}

fn symbol_id(workspace_id: &str, symbol: &str) -> SymbolId {
    SymbolId {
        file: file_id(workspace_id, "legacy-symbols"),
        qualified_name: symbol.to_string(),
        byte_offset: 0,
        kind: "unknown".to_string(),
    }
}

fn file_identity(workspace_id: &str, file: &str) -> String {
    encode_identity_text(&Identity::File(file_id(workspace_id, file)))
}

fn symbol_identity(workspace_id: &str, symbol: &str) -> String {
    encode_identity_text(&Identity::Symbol(symbol_id(workspace_id, symbol)))
}

fn memory_identity(workspace_id: &str, memory_id: &str) -> String {
    encode_identity_text(&Identity::Memory(MemoryId {
        workspace_id: workspace_id.to_string(),
        ulid: memory_id.to_string(),
    }))
}

fn memory_refs(workspace_id: &str, ids: &[String], reason: &str) -> Vec<BTreeMap<String, String>> {
    ids.iter()
        .map(|id| {
            BTreeMap::from([
                ("memory_id".to_string(), memory_identity(workspace_id, id)),
                ("reason".to_string(), reason.to_string()),
            ])
        })
        .collect()
}

fn supersession_refs(row: &LegacyMemoryRow, workspace_id: &str) -> Vec<BTreeMap<String, String>> {
    row.supersedes_memory_id
        .iter()
        .map(|id| {
            BTreeMap::from([
                ("memory_id".to_string(), memory_identity(workspace_id, id)),
                ("reason".to_string(), "supersedes".to_string()),
            ])
        })
        .collect()
}

fn access_history(row: &LegacyMemoryRow) -> Vec<BTreeMap<String, Value>> {
    if row.access_count <= 0 {
        return Vec::new();
    }
    vec![BTreeMap::from([
        ("accessed_at".to_string(), json!(row.last_accessed)),
        ("accessor".to_string(), json!("legacy")),
        ("purpose".to_string(), json!("migrated access metadata")),
    ])]
}

fn confidence_reason(row: &LegacyMemoryRow) -> String {
    row.confidence_reason
        .clone()
        .or_else(|| row.source_query.clone())
        .or_else(|| row.stale_reason.clone())
        .unwrap_or_else(|| format!("Migrated from legacy memories. See {MIGRATION_GUIDE}."))
}

fn scope_session(scope: MemoryScope, row: &LegacyMemoryRow) -> Option<String> {
    (scope == MemoryScope::Session).then(|| {
        if row.session_id.is_empty() {
            "legacy-session".to_string()
        } else {
            row.session_id.clone()
        }
    })
}

fn scope_branch(scope: MemoryScope, row: &LegacyMemoryRow) -> Option<String> {
    (scope == MemoryScope::Branch)
        .then(|| row.branch.clone().unwrap_or_else(|| "legacy".to_string()))
}

fn scope_workspace(
    scope: MemoryScope,
    row: &LegacyMemoryRow,
    workspace_id: &str,
) -> Option<String> {
    matches!(scope, MemoryScope::Branch | MemoryScope::Repo).then(|| {
        row.workspace_id
            .clone()
            .unwrap_or_else(|| workspace_id.to_string())
    })
}
