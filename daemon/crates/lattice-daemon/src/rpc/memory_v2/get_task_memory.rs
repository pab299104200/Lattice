use super::{
    checkout_state_for_memory, contradiction_state, detect_artifact_conflicts, evidence_strength,
    expansion_handle, freshness_status, memory_evidence_links, memory_recheck_commands,
    memory_requires_reverification, memory_risk_domains, memory_trust_reason, memory_trust_status,
    memory_workspace_conflict, memory_workspace_path_diagnostic, supersession_state, MemoryRecord,
    TaskMemoryBundle,
};
use lattice_core::memory::{Memory, MemoryScoreKind, MemoryScoreRecord, MemoryStore};
use lattice_core::working_memory::{CheckpointId, WorkingMemoryState};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Arguments for the `get_task_memory` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetTaskMemoryArgs {
    /// Task id whose active and durable memory should be loaded.
    pub task_id: String,
    /// Optional task statement used to seed a working-memory state when none exists yet.
    #[serde(default)]
    pub task_statement: Option<String>,
    /// Optional task-specific retrieval hint for durable memory recall.
    #[serde(default)]
    pub intent_hint: Option<String>,
    /// Optional focused files that should bias retrieval for this task.
    #[serde(default)]
    pub focus_files: Vec<String>,
    /// Optional focused directories that should bias retrieval for this task.
    #[serde(default)]
    pub focus_dirs: Vec<String>,
    /// Optional soft token budget for the response payload.
    #[serde(default)]
    pub budget_tokens: Option<usize>,
}

pub fn tool_definition() -> Value {
    json!({
        "name": "get_task_memory",
        "description": "Read working memory plus relevant durable memory for the current task, with inclusion reasons, verification status, freshness, contradiction state, and expansion handles.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Task id resolved through the task identity layer"
                },
                "task_statement": {
                    "type": "string",
                    "description": "Optional task statement used to seed working memory when no state exists yet"
                },
                "intent_hint": {
                    "type": "string",
                    "description": "Optional hint that biases durable memory retrieval toward the current operator intent"
                },
                "focus_files": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional focused files that should bias retrieval for this task"
                },
                "focus_dirs": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional focused directories that should bias retrieval for this task"
                },
                "budget_tokens": {
                    "type": "integer",
                    "description": "Optional soft budget for the returned bundle"
                }
            },
            "required": ["task_id"]
        }
    })
}

pub fn parse_args(args: &Value) -> Result<GetTaskMemoryArgs, String> {
    serde_json::from_value(args.clone())
        .map_err(|error| format!("Invalid get_task_memory arguments: {error}"))
}

pub fn build_bundle(
    store: &MemoryStore,
    workspace_id: &str,
    task_id: String,
    checkpoint_id: Option<CheckpointId>,
    working_state: &WorkingMemoryState,
    memories: Vec<(Memory, String, i64)>,
) -> Result<TaskMemoryBundle, String> {
    let mut records = Vec::with_capacity(memories.len());
    for (memory, inclusion_reason, _score) in memories {
        records.push(build_memory_record(
            store,
            workspace_id,
            &memory,
            inclusion_reason,
        )?);
    }
    Ok(TaskMemoryBundle {
        task_id,
        checkpoint_id,
        working_memory_verification_status: working_state
            .verification_status
            .status
            .as_str()
            .to_string(),
        memories: records,
    })
}

fn build_memory_record(
    store: &MemoryStore,
    workspace_id: &str,
    memory: &Memory,
    inclusion_reason: String,
) -> Result<MemoryRecord, String> {
    let fields = store
        .get_structured_fields(&memory.id)
        .map_err(|error| format!("Failed to load structured memory fields: {error}"))?
        .unwrap_or_default();
    let links = store
        .list_memory_links_from(&memory.id)
        .map_err(|error| format!("Failed to load memory links: {error}"))?;
    let access_history = store
        .list_memory_accesses(&memory.id)
        .map_err(|error| format!("Failed to load memory access history: {error}"))?;
    let usefulness_scores = load_usefulness_scores(store, &memory.id)?;
    let last_verified_at = store
        .get_last_verified_at(&memory.id)
        .map_err(|error| format!("Failed to load last verified timestamp: {error}"))?;
    let last_verified_graph_snapshot_id = store
        .get_last_verified_graph_snapshot_id(&memory.id)
        .map_err(|error| format!("Failed to load last verified graph snapshot id: {error}"))?;
    let expires_at = store
        .get_expires_at(&memory.id)
        .map_err(|error| format!("Failed to load expiry timestamp: {error}"))?;
    let checkout_state = checkout_state_for_memory(memory, &fields, workspace_id);
    let trust_status = memory_trust_status(memory, &fields, &checkout_state).to_string();
    let trust_reason = memory_trust_reason(memory, &fields, &checkout_state).to_string();
    let risk_domains = memory_risk_domains(memory, &fields);
    let (requires_reverification, reverification_reason) = memory_requires_reverification(
        memory,
        &fields,
        &checkout_state,
        &risk_domains,
        last_verified_at,
    );
    Ok(MemoryRecord {
        id: memory.id.clone(),
        expansion_handle: expansion_handle(memory, workspace_id),
        content: memory.content.clone(),
        memory_class: fields.memory_class,
        assertion_type: fields.assertion_type,
        scope: memory.scope.as_str().to_string(),
        confidence: memory.confidence,
        confidence_reason: fields.confidence_reason.clone(),
        verification_status: fields.verification_status.as_str().to_string(),
        trust_status,
        trust_reason,
        risk_domains,
        requires_reverification,
        reverification_reason,
        freshness_status: freshness_status(memory, &fields, expires_at),
        contradiction_state: contradiction_state(&fields),
        supersession_state: supersession_state(&fields),
        inclusion_reason,
        evidence_strength: evidence_strength(&fields, &usefulness_scores, memory.access_count),
        linked_files: memory.linked_files.clone(),
        linked_symbols: memory.linked_symbols.clone(),
        linked_docs: fields.linked_docs.clone(),
        linked_tests: fields.linked_tests.clone(),
        linked_memories: fields.linked_memories.clone(),
        validity_conditions: fields.validity_conditions.clone(),
        invalidation_triggers: fields.invalidation_triggers.clone(),
        provenance: fields.provenance.clone(),
        evidence: fields.evidence.clone(),
        evidence_links: memory_evidence_links(memory, &fields),
        links,
        access_history,
        usefulness_scores,
        source_query: memory.source_query.clone(),
        branch: memory.branch.clone(),
        refresh_key: memory.refresh_key.clone(),
        last_verified_at,
        last_verified_graph_snapshot_id,
        checkout_state,
        workspace_conflict: memory_workspace_conflict(memory),
        workspace_path_diagnostic: memory_workspace_path_diagnostic(memory),
        recheck_commands: memory_recheck_commands(memory, &fields),
        artifact_conflicts: detect_artifact_conflicts(workspace_id, &fields),
    })
}

fn load_usefulness_scores(
    store: &MemoryStore,
    memory_id: &str,
) -> Result<Vec<MemoryScoreRecord>, String> {
    let mut scores = Vec::new();
    for kind in [
        MemoryScoreKind::UsefulnessPrior,
        MemoryScoreKind::RecentUsefulness,
        MemoryScoreKind::RetrievalAccuracy,
        MemoryScoreKind::RegressionRisk,
    ] {
        if let Some(score) = store
            .latest_memory_score(memory_id, kind)
            .map_err(|error| format!("Failed to load memory score: {error}"))?
        {
            scores.push(score);
        }
    }
    Ok(scores)
}

pub(crate) fn approximate_payload_cost(memory: &Memory) -> usize {
    60 + memory.content.len() / 4 + (memory.linked_files.len() + memory.linked_symbols.len()) * 12
}

pub(crate) fn clip_to_budget(
    memories: &[(Memory, String, i64)],
    budget_tokens: Option<usize>,
) -> Vec<(Memory, String, i64)> {
    let mut kept = Vec::new();
    let mut used = 0usize;
    let cap = budget_tokens.unwrap_or(1200);
    for item in memories {
        let next = approximate_payload_cost(&item.0);
        if !kept.is_empty() && used + next > cap {
            break;
        }
        used += next;
        kept.push(item.clone());
    }
    kept
}

pub(crate) fn rank_memories(
    store: &MemoryStore,
    workspace_id: &str,
    working_state: &WorkingMemoryState,
    candidates: Vec<Memory>,
    intent_hint: Option<&str>,
    focus_files: &[String],
    focus_dirs: &[String],
    preferred_branch: Option<&str>,
) -> Result<Vec<(Memory, String, i64)>, String> {
    let active_file_hints: Vec<&str> = working_state
        .active_files
        .iter()
        .map(|item| item.repo_relative_path.as_str())
        .collect();
    let active_symbol_hints: Vec<&str> = working_state
        .active_symbols
        .iter()
        .map(|item| item.qualified_name.as_str())
        .collect();
    let query_terms = build_query_terms(&working_state.task_statement, intent_hint);
    let exact_terms = structured_query_terms(&working_state.task_statement, intent_hint);
    let mut ranked = Vec::new();

    for memory in candidates {
        if memory
            .workspace_id
            .as_deref()
            .is_some_and(|memory_workspace| memory_workspace != workspace_id)
        {
            continue;
        }
        let fields = store
            .get_structured_fields(&memory.id)
            .map_err(|error| format!("Failed to load structured memory fields: {error}"))?
            .unwrap_or_default();
        let mut score = (memory.confidence * 1000.0).round() as i64;
        let mut reasons = Vec::new();

        if working_state
            .selected_memories
            .iter()
            .any(|item| item.identity.to_string().contains(memory.id.as_str()))
        {
            score += 1200;
            reasons.push("selected in working memory".to_string());
        }

        for symbol in &memory.linked_symbols {
            if active_symbol_hints
                .iter()
                .any(|hint| symbol.eq_ignore_ascii_case(hint))
            {
                score += 650;
                reasons.push(format!("linked to active symbol `{symbol}`"));
            }
        }

        for file in &memory.linked_files {
            if active_file_hints.iter().any(|hint| file.ends_with(hint)) {
                score += 500;
                reasons.push(format!("linked to active file `{file}`"));
            }
            if focus_files.iter().any(|hint| file.ends_with(hint)) {
                score += 420;
                reasons.push(format!("matches focused file `{file}`"));
            }
            if let Some(matched_dir) = focus_dirs
                .iter()
                .find(|hint| normalized_dir_match(file, hint))
            {
                score += 280;
                reasons.push(format!("matches focused directory `{matched_dir}`"));
            }
        }

        let haystack = durable_memory_search_text(&memory, &fields).to_ascii_lowercase();
        let mut matched_terms = Vec::new();
        for term in &query_terms {
            if haystack.contains(term.as_str()) {
                score += if is_structured_remediation_token(term) {
                    700
                } else if term.contains('/') || term.contains('.') {
                    500
                } else {
                    180
                };
                matched_terms.push(term.clone());
            }
        }
        if !exact_terms.is_empty()
            && !matched_terms
                .iter()
                .any(|term| is_structured_remediation_token(term))
        {
            continue;
        }
        if !matched_terms.is_empty() {
            reasons.push(format!("matches task terms: {}", matched_terms.join(", ")));
        }

        if reasons.is_empty() {
            continue;
        }

        score += match fields.verification_status {
            lattice_core::memory::MemoryVerificationStatus::Verified => 500,
            lattice_core::memory::MemoryVerificationStatus::InReview => 350,
            lattice_core::memory::MemoryVerificationStatus::Unverified => 200,
            lattice_core::memory::MemoryVerificationStatus::Superseded => -200,
            lattice_core::memory::MemoryVerificationStatus::Contradicted => -450,
            lattice_core::memory::MemoryVerificationStatus::Stale => -500,
            lattice_core::memory::MemoryVerificationStatus::Expired
            | lattice_core::memory::MemoryVerificationStatus::Invalidated => -700,
        };

        if memory.workspace_id.as_deref().unwrap_or(workspace_id) == workspace_id {
            score += 50;
        }
        if preferred_branch.is_some() && memory.branch.as_deref() == preferred_branch {
            score += 90;
        }
        score += i64::from(memory.access_count.min(10)) * 8;
        ranked.push((memory, reasons.join("; "), score));
    }

    ranked.sort_by(|left, right| {
        right
            .2
            .cmp(&left.2)
            .then_with(|| right.0.created_at.cmp(&left.0.created_at))
    });
    Ok(ranked)
}

fn normalized_dir_match(file: &str, directory: &str) -> bool {
    let normalized = directory.trim().trim_matches('/');
    !normalized.is_empty()
        && (file == normalized
            || file.starts_with(&format!("{normalized}/"))
            || file.ends_with(&format!("/{normalized}")))
}

fn build_query_terms(task_statement: &str, intent_hint: Option<&str>) -> Vec<String> {
    let mut terms = Vec::new();
    for source in [Some(task_statement), intent_hint] {
        if let Some(source) = source {
            for chunk in source.split_whitespace() {
                let normalized = normalize_query_token(chunk);
                if normalized.len() >= 3
                    && !is_stopword(&normalized)
                    && !terms.contains(&normalized)
                {
                    terms.push(normalized);
                }
                for part in chunk.split(|c: char| !c.is_ascii_alphanumeric()) {
                    let token = part.to_ascii_lowercase();
                    if token.len() >= 3 && !is_stopword(&token) && !terms.contains(&token) {
                        terms.push(token);
                    }
                }
            }
        }
    }
    terms.sort_by_key(|term| std::cmp::Reverse(query_term_priority(term)));
    terms.truncate(16);
    terms
}

pub(crate) fn structured_query_terms(query: &str, intent_hint: Option<&str>) -> Vec<String> {
    build_query_terms(query, intent_hint)
        .into_iter()
        .filter(|term| is_structured_remediation_token(term))
        .collect()
}

pub(crate) fn durable_memory_search_text(
    memory: &Memory,
    fields: &lattice_core::memory::MemoryStructuredFields,
) -> String {
    let evidence = fields
        .evidence
        .iter()
        .filter_map(|item| serde_json::to_string(item).ok())
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{} {} {} {} {} {} {} {}",
        memory.content,
        memory.linked_symbols.join(" "),
        memory.linked_files.join(" "),
        memory.refresh_key.as_deref().unwrap_or_default(),
        memory.source_query.as_deref().unwrap_or_default(),
        fields.linked_docs.join(" "),
        fields.linked_tests.join(" "),
        evidence
    )
}

pub(crate) fn matching_query_terms(text: &str, query: &str) -> Vec<String> {
    let haystack = text.to_ascii_lowercase();
    let mut matches = Vec::new();
    for term in build_query_terms(query, None) {
        if haystack.contains(&term) && !matches.contains(&term) {
            matches.push(term);
        }
    }
    matches
}

pub(crate) fn search_match_score(matches: &[String]) -> i64 {
    matches
        .iter()
        .map(|term| {
            if is_structured_remediation_token(term) {
                5000
            } else if is_code_identifier_token(term) {
                1800
            } else if term.contains('-') {
                1400
            } else if term.contains('/') || term.contains('.') {
                1200
            } else {
                100
            }
        })
        .sum()
}

fn normalize_query_token(raw: &str) -> String {
    raw.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | '`' | ',' | ';' | ':' | ')' | '(' | '[' | ']' | '{' | '}'
        )
    })
    .to_ascii_lowercase()
}

fn is_stopword(token: &str) -> bool {
    matches!(
        token,
        "and"
            | "for"
            | "the"
            | "this"
            | "that"
            | "with"
            | "from"
            | "current"
            | "state"
            | "next"
            | "steps"
            | "after"
            | "prior"
            | "fixes"
            | "lattice"
            | "memory"
            | "memories"
            | "determine"
            | "remediation"
            | "run"
            | "audit"
            | "contract"
            | "claims"
    )
}

pub(crate) fn is_structured_remediation_token(token: &str) -> bool {
    let upper = token.to_ascii_uppercase();
    for prefix in ["IU-", "PX-", "IM-"] {
        if let Some(suffix) = upper.strip_prefix(prefix) {
            return suffix.chars().all(|ch| ch.is_ascii_digit());
        }
    }
    for prefix in ["IU", "PX", "IM"] {
        if let Some(suffix) = upper.strip_prefix(prefix) {
            return suffix.len() >= 2 && suffix.chars().all(|ch| ch.is_ascii_digit());
        }
    }
    false
}

fn is_code_identifier_token(token: &str) -> bool {
    token.contains('_')
        || token.contains("::")
        || (token.len() >= 10 && token.chars().all(|ch| ch.is_ascii_alphanumeric()))
        || token
            .chars()
            .any(|ch| ch.is_ascii_alphabetic() && ch.is_ascii_uppercase())
}

fn query_term_priority(term: &str) -> i32 {
    if is_structured_remediation_token(term) {
        5
    } else if is_code_identifier_token(term) {
        4
    } else if term.contains('-') {
        3
    } else if term.contains('/') || term.contains('.') {
        2
    } else {
        1
    }
}
