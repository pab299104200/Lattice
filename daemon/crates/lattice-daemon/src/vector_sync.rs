use anyhow::Result;
use lattice_core::embeddings::EmbeddingProvider;
use lattice_core::graph::{CodeGraph, GraphNode};
use lattice_core::storage::{VectorIndex, VectorScope};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

const MAX_EMBED_TEXT_CHARS: usize = 1800;
const MAX_BODY_SUMMARY_CHARS: usize = 420;
const MAX_SECTION_ITEMS: usize = 6;
const MAX_ANCHOR_CHARS: usize = 96;
const FILE_SUMMARY_VECTOR_NAME: &str = "file_summary";
const FILE_SUMMARY_VECTOR_OFFSET: usize = usize::MAX;

#[derive(Debug, Clone, Default)]
struct UpsertBatchStats {
    nodes_considered: usize,
    embedded_nodes: usize,
    failed_nodes: usize,
    payload_chars_total: usize,
    payload_chars_max: usize,
    memberships: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub(crate) struct VectorSyncStats {
    pub mode: &'static str,
    pub implementation: &'static str,
    pub graph_nodes: usize,
    pub files_requested: usize,
    pub files_deleted: usize,
    pub nodes_considered: usize,
    pub embedded_nodes: usize,
    pub failed_nodes: usize,
    pub payload_chars_total: usize,
    pub payload_chars_avg: usize,
    pub payload_chars_max: usize,
    pub elapsed_ms: u128,
}

impl VectorSyncStats {
    fn from_upsert(
        mode: &'static str,
        implementation: &'static str,
        graph_nodes: usize,
        files_requested: usize,
        files_deleted: usize,
        batch: UpsertBatchStats,
        elapsed_ms: u128,
    ) -> Self {
        let payload_chars_avg = if batch.nodes_considered == 0 {
            0
        } else {
            batch.payload_chars_total / batch.nodes_considered
        };
        Self {
            mode,
            implementation,
            graph_nodes,
            files_requested,
            files_deleted,
            nodes_considered: batch.nodes_considered,
            embedded_nodes: batch.embedded_nodes,
            failed_nodes: batch.failed_nodes,
            payload_chars_total: batch.payload_chars_total,
            payload_chars_avg,
            payload_chars_max: batch.payload_chars_max,
            elapsed_ms,
        }
    }

    pub fn throughput_nodes_per_sec(&self) -> f64 {
        if self.elapsed_ms == 0 {
            return self.embedded_nodes as f64;
        }
        self.embedded_nodes as f64 / (self.elapsed_ms as f64 / 1000.0)
    }
}

pub(crate) fn sync_full_graph_embeddings(
    graph: &CodeGraph,
    embedding_engine: &dyn EmbeddingProvider,
    vector_index: &dyn VectorIndex,
) -> Result<VectorSyncStats> {
    let _publication = embedding_engine.publication_lease()?;
    let _vector_publication = vector_index.begin_publication()?;
    if let Some(identity) = embedding_engine.storage_identity()? {
        vector_index.bind_embedding_identity(&identity)?;
    }
    let started = Instant::now();
    let graph_nodes = graph.node_count();
    vector_index.clear_all()?;
    let batch = upsert_graph_nodes(
        graph.all_nodes().into_iter(),
        embedding_engine,
        vector_index,
    )?;
    vector_index.flush()?;
    embedding_engine.publish_membership(&batch.memberships, true, &[])?;
    let stats = VectorSyncStats::from_upsert(
        "full",
        vector_index.implementation_name(),
        graph_nodes,
        0,
        0,
        batch,
        started.elapsed().as_millis(),
    );
    tracing::info!(
        target: "lattice.vector_sync",
        mode = stats.mode,
        implementation = stats.implementation,
        graph_nodes = stats.graph_nodes,
        nodes_considered = stats.nodes_considered,
        embedded_nodes = stats.embedded_nodes,
        failed_nodes = stats.failed_nodes,
        payload_chars_total = stats.payload_chars_total,
        payload_chars_avg = stats.payload_chars_avg,
        payload_chars_max = stats.payload_chars_max,
        elapsed_ms = stats.elapsed_ms as u64,
        throughput_nodes_per_sec = stats.throughput_nodes_per_sec(),
        "Semantic vector sync complete"
    );
    Ok(stats)
}

pub(crate) fn sync_changed_files_embeddings(
    graph: &CodeGraph,
    changed_files: &[String],
    embedding_engine: &dyn EmbeddingProvider,
    vector_index: &dyn VectorIndex,
) -> Result<VectorSyncStats> {
    let _publication = embedding_engine.publication_lease()?;
    let _vector_publication = vector_index.begin_publication()?;
    if let Some(identity) = embedding_engine.storage_identity()? {
        vector_index.bind_embedding_identity(&identity)?;
    }
    let started = Instant::now();
    let graph_nodes = graph.node_count();
    let files_requested = changed_files.len();
    if changed_files.is_empty() {
        return Ok(VectorSyncStats::from_upsert(
            "changed",
            vector_index.implementation_name(),
            graph_nodes,
            files_requested,
            0,
            UpsertBatchStats::default(),
            0,
        ));
    }

    let changed: HashSet<&str> = changed_files.iter().map(|file| file.as_str()).collect();
    for file in &changed {
        vector_index.delete_by_file(file)?;
    }

    let batch = upsert_graph_nodes(
        graph
            .all_nodes()
            .into_iter()
            .filter(|node| changed.contains(node.file.as_str())),
        embedding_engine,
        vector_index,
    )?;
    vector_index.flush()?;
    let remove_prefixes: Vec<String> = changed
        .iter()
        .map(|file| membership_file_prefix(file))
        .collect();
    embedding_engine.publish_membership(&batch.memberships, false, &remove_prefixes)?;
    let stats = VectorSyncStats::from_upsert(
        "changed",
        vector_index.implementation_name(),
        graph_nodes,
        files_requested,
        changed.len(),
        batch,
        started.elapsed().as_millis(),
    );
    tracing::info!(
        target: "lattice.vector_sync",
        mode = stats.mode,
        implementation = stats.implementation,
        graph_nodes = stats.graph_nodes,
        files_requested = stats.files_requested,
        files_deleted = stats.files_deleted,
        nodes_considered = stats.nodes_considered,
        embedded_nodes = stats.embedded_nodes,
        failed_nodes = stats.failed_nodes,
        payload_chars_total = stats.payload_chars_total,
        payload_chars_avg = stats.payload_chars_avg,
        payload_chars_max = stats.payload_chars_max,
        elapsed_ms = stats.elapsed_ms as u64,
        throughput_nodes_per_sec = stats.throughput_nodes_per_sec(),
        "Semantic vector sync complete"
    );
    Ok(stats)
}

fn upsert_graph_nodes<'a>(
    nodes: impl IntoIterator<Item = &'a GraphNode>,
    embedding_engine: &dyn EmbeddingProvider,
    vector_index: &dyn VectorIndex,
) -> Result<UpsertBatchStats> {
    let collected_nodes: Vec<&GraphNode> = nodes.into_iter().collect();
    let mut stats = UpsertBatchStats::default();
    let mut targets: Vec<(String, String, String, usize, VectorScope)> = Vec::new();
    for node in &collected_nodes {
        let text = build_embedding_text(node);
        let payload_chars = text.chars().count();
        stats.nodes_considered += 1;
        stats.payload_chars_total += payload_chars;
        stats.payload_chars_max = stats.payload_chars_max.max(payload_chars);
        targets.push((
            text,
            node.file.clone(),
            node.name.clone(),
            node.id.byte_offset,
            VectorScope::Symbol,
        ));
    }

    let mut file_nodes: BTreeMap<&str, Vec<&GraphNode>> = BTreeMap::new();
    for node in &collected_nodes {
        file_nodes
            .entry(node.file.as_str())
            .or_default()
            .push(*node);
    }
    for (file, mut nodes) in file_nodes {
        nodes.sort_by(|a, b| {
            a.line
                .cmp(&b.line)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.id.byte_offset.cmp(&b.id.byte_offset))
        });
        let text = build_file_summary_embedding_text(file, &nodes);
        let payload_chars = text.chars().count();
        stats.nodes_considered += 1;
        stats.payload_chars_total += payload_chars;
        stats.payload_chars_max = stats.payload_chars_max.max(payload_chars);
        targets.push((
            text,
            file.to_string(),
            FILE_SUMMARY_VECTOR_NAME.to_string(),
            FILE_SUMMARY_VECTOR_OFFSET,
            VectorScope::FileSummary,
        ));
    }

    let texts: Vec<&str> = targets.iter().map(|target| target.0.as_str()).collect();
    match embedding_engine.embed_batch(&texts) {
        Ok(vectors) if vectors.len() == targets.len() => {
            for ((_, file, name, offset, scope), vector) in targets.iter().zip(vectors) {
                vector_index.upsert_vector_in_scope(file, name, *offset, *scope, &vector)?;
                stats.embedded_nodes += 1;
            }
            for (text, file, name, offset, scope) in &targets {
                if let Some(key) = embedding_engine.object_key(text)? {
                    stats.memberships.insert(
                        format!(
                            "{}{:?}:{name}:{offset}",
                            membership_file_prefix(file),
                            scope
                        ),
                        key,
                    );
                }
            }
        }
        Ok(vectors) => {
            stats.failed_nodes += targets.len();
            tracing::warn!(expected = targets.len(), actual = vectors.len(), "Embedding provider returned an incomplete batch; semantic vectors were not published and lexical retrieval remains available");
        }
        Err(err) => {
            stats.failed_nodes += targets.len();
            tracing::warn!(error = %err, failed_nodes = targets.len(), "Embedding batch failed; lexical retrieval remains available");
        }
    }
    Ok(stats)
}

fn membership_file_prefix(file: &str) -> String {
    format!("{:x}:", Sha256::digest(file.as_bytes()))
}

fn build_embedding_text(node: &GraphNode) -> String {
    let mut sections = Vec::new();
    sections.push(format!("symbol: {}", normalize_whitespace(&node.name)));
    if !node.signature.trim().is_empty() {
        sections.push(format!(
            "signature: {}",
            normalize_whitespace(&node.signature)
        ));
    }
    sections.push(format!("file: {}", normalize_whitespace(&node.file)));

    let body_summary = summarize_body(&node.body);
    if !body_summary.is_empty() {
        sections.push(format!("body_summary: {}", body_summary));
    }

    let mut comments_docstrings = extract_comment_and_doc_anchors(&node.body);
    dedupe_and_limit(&mut comments_docstrings, MAX_SECTION_ITEMS);
    if !comments_docstrings.is_empty() {
        sections.push(format!(
            "comments_docstrings: {}",
            comments_docstrings.join(" | ")
        ));
    }

    let literals = extract_string_literals(&node.body);
    let mut error_strings = Vec::new();
    let mut config_keys = Vec::new();
    let mut route_names = Vec::new();
    for literal in literals {
        if is_error_literal(&literal) {
            push_unique(&mut error_strings, literal.clone());
        }
        if is_config_literal(&literal) {
            push_unique(&mut config_keys, literal.clone());
        }
        if is_route_literal(&literal) {
            push_unique(&mut route_names, literal);
        }
    }

    dedupe_and_limit(&mut error_strings, MAX_SECTION_ITEMS);
    dedupe_and_limit(&mut config_keys, MAX_SECTION_ITEMS);
    dedupe_and_limit(&mut route_names, MAX_SECTION_ITEMS);

    if !error_strings.is_empty() {
        sections.push(format!("error_strings: {}", error_strings.join(" | ")));
    }
    if !config_keys.is_empty() {
        sections.push(format!("config_keys: {}", config_keys.join(" | ")));
    }
    if !route_names.is_empty() {
        sections.push(format!("routes: {}", route_names.join(" | ")));
    }

    let combined = sections.join("\n");
    truncate_chars(&combined, MAX_EMBED_TEXT_CHARS)
}

fn build_file_summary_embedding_text(file: &str, nodes: &[&GraphNode]) -> String {
    let mut sections = vec![
        "granularity: file_summary".to_string(),
        format!("file: {}", normalize_whitespace(file)),
        format!("symbol_count: {}", nodes.len()),
    ];

    let mut top_symbols = Vec::new();
    for node in nodes.iter().take(16) {
        let item = format!(
            "{}({})",
            normalize_whitespace(&node.name),
            node.kind.short_code()
        );
        push_unique(&mut top_symbols, truncate_chars(&item, MAX_ANCHOR_CHARS));
    }
    dedupe_and_limit(&mut top_symbols, MAX_SECTION_ITEMS);
    if !top_symbols.is_empty() {
        sections.push(format!("top_symbols: {}", top_symbols.join(" | ")));
    }

    let mut comments_docstrings = Vec::new();
    let mut error_strings = Vec::new();
    let mut config_keys = Vec::new();
    let mut route_names = Vec::new();
    for node in nodes.iter().take(24) {
        for anchor in extract_comment_and_doc_anchors(&node.body) {
            push_unique(&mut comments_docstrings, anchor);
        }
        for literal in extract_string_literals(&node.body) {
            if is_error_literal(&literal) {
                push_unique(&mut error_strings, literal.clone());
            }
            if is_config_literal(&literal) {
                push_unique(&mut config_keys, literal.clone());
            }
            if is_route_literal(&literal) {
                push_unique(&mut route_names, literal);
            }
        }
    }
    dedupe_and_limit(&mut comments_docstrings, MAX_SECTION_ITEMS);
    dedupe_and_limit(&mut error_strings, MAX_SECTION_ITEMS);
    dedupe_and_limit(&mut config_keys, MAX_SECTION_ITEMS);
    dedupe_and_limit(&mut route_names, MAX_SECTION_ITEMS);

    if !comments_docstrings.is_empty() {
        sections.push(format!(
            "comments_docstrings: {}",
            comments_docstrings.join(" | ")
        ));
    }
    if !error_strings.is_empty() {
        sections.push(format!("error_strings: {}", error_strings.join(" | ")));
    }
    if !config_keys.is_empty() {
        sections.push(format!("config_keys: {}", config_keys.join(" | ")));
    }
    if !route_names.is_empty() {
        sections.push(format!("routes: {}", route_names.join(" | ")));
    }

    truncate_chars(&sections.join("\n"), MAX_EMBED_TEXT_CHARS)
}

fn summarize_body(body: &str) -> String {
    let mut parts = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || is_comment_line(trimmed) {
            continue;
        }
        if matches!(trimmed, "{" | "}" | "(" | ")" | "[" | "]") {
            continue;
        }
        let normalized = truncate_chars(&normalize_whitespace(trimmed), MAX_ANCHOR_CHARS);
        if normalized.is_empty() {
            continue;
        }
        push_unique(&mut parts, normalized);
        if parts.len() >= 6 {
            break;
        }
    }
    truncate_chars(&parts.join(" "), MAX_BODY_SUMMARY_CHARS)
}

fn extract_comment_and_doc_anchors(body: &str) -> Vec<String> {
    let mut anchors = extract_docstring_anchors(body);
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(stripped) = strip_comment_prefix(trimmed) {
            let normalized = truncate_chars(&normalize_whitespace(stripped), MAX_ANCHOR_CHARS);
            if !normalized.is_empty() {
                push_unique(&mut anchors, normalized);
            }
        }
    }
    anchors
}

fn extract_docstring_anchors(body: &str) -> Vec<String> {
    let mut anchors = Vec::new();
    for delimiter in ["\"\"\"", "'''"] {
        let mut tail = body;
        while let Some(start) = tail.find(delimiter) {
            tail = &tail[start + delimiter.len()..];
            let Some(end) = tail.find(delimiter) else {
                break;
            };
            let block = &tail[..end];
            for line in block.lines() {
                let normalized = truncate_chars(&normalize_whitespace(line), MAX_ANCHOR_CHARS);
                if !normalized.is_empty() {
                    push_unique(&mut anchors, normalized);
                }
            }
            tail = &tail[end + delimiter.len()..];
        }
    }
    anchors
}

fn strip_comment_prefix(line: &str) -> Option<&str> {
    for prefix in ["///", "//!", "//", "#", "--", "/*", "*", "*/"] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some(rest.trim());
        }
    }
    None
}

fn is_comment_line(line: &str) -> bool {
    strip_comment_prefix(line).is_some()
}

fn extract_string_literals(body: &str) -> Vec<String> {
    let mut anchors = Vec::new();
    let mut iter = body.chars().peekable();

    while let Some(ch) = iter.next() {
        if ch != '"' && ch != '\'' && ch != '`' {
            continue;
        }

        let delimiter = ch;
        let mut escaped = false;
        let mut literal = String::new();

        while let Some(next) = iter.next() {
            if escaped {
                literal.push(next);
                escaped = false;
                continue;
            }
            if next == '\\' {
                escaped = true;
                continue;
            }
            if next == delimiter {
                break;
            }
            if next == '\n' && delimiter != '`' {
                break;
            }
            literal.push(next);
            if literal.chars().count() > MAX_ANCHOR_CHARS * 2 {
                break;
            }
        }

        let normalized = truncate_chars(&normalize_whitespace(&literal), MAX_ANCHOR_CHARS);
        if normalized.chars().count() < 3 {
            continue;
        }
        if is_error_literal(&normalized)
            || is_config_literal(&normalized)
            || is_route_literal(&normalized)
        {
            push_unique(&mut anchors, normalized);
        }
    }

    anchors
}

fn is_error_literal(value: &str) -> bool {
    let lower = value.to_lowercase();
    [
        "error",
        "failed",
        "failure",
        "invalid",
        "unauthorized",
        "forbidden",
        "denied",
        "exception",
        "timeout",
        "not found",
        "missing",
        "conflict",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_config_literal(value: &str) -> bool {
    if value.starts_with('/') || value.contains(' ') {
        return false;
    }
    let has_upper_underscore = value.contains('_')
        && value
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_');
    let has_dotted_key = value.contains('.') && value.chars().any(|ch| ch.is_ascii_alphabetic());
    has_upper_underscore || has_dotted_key
}

fn is_route_literal(value: &str) -> bool {
    value.starts_with('/') && !value.contains(' ')
}

fn dedupe_and_limit(values: &mut Vec<String>, max_items: usize) {
    let mut deduped = Vec::new();
    for value in values.drain(..) {
        let truncated = truncate_chars(value.trim(), MAX_ANCHOR_CHARS);
        if !truncated.is_empty() {
            push_unique(&mut deduped, truncated);
        }
        if deduped.len() >= max_items {
            break;
        }
    }
    *values = deduped;
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !value.is_empty() && !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn normalize_whitespace(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut pending_space = false;
    for ch in value.chars() {
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        out.push(ch);
    }
    out.trim().to_string()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::{build_embedding_text, build_file_summary_embedding_text, MAX_EMBED_TEXT_CHARS};
    use lattice_core::graph::GraphNode;
    use lattice_core::symbols::{Language, SymbolId, SymbolKind};
    use std::sync::Arc;

    fn sample_node(body: &str) -> GraphNode {
        GraphNode {
            id: SymbolId {
                file: "routers/certificates.py".to_string(),
                name: "_verify_org_access".to_string(),
                byte_offset: 66,
            },
            kind: SymbolKind::Function,
            name: "_verify_org_access".to_string(),
            signature: Arc::from("def _verify_org_access(org_id):"),
            body: Arc::from(body.to_string()),
            file: "routers/certificates.py".to_string(),
            line: 66,
            end_line: 84,
            is_exported: false,
            language: Language::Python,
            last_modified: 0,
        }
    }

    fn sample_helper_node() -> GraphNode {
        GraphNode {
            id: SymbolId {
                file: "routers/certificates.py".to_string(),
                name: "upsert_renewal_policy".to_string(),
                byte_offset: 380,
            },
            kind: SymbolKind::Function,
            name: "upsert_renewal_policy".to_string(),
            signature: Arc::from("def upsert_renewal_policy(org_id, payload):"),
            body: Arc::from(
                r#"
def upsert_renewal_policy(org_id, payload):
    """Store certificate renewal policy."""
    route = "/api/v1/certificates/{cert_id}/renewal-policy"
    logger.error("renewal policy update failed")
"#
                .to_string(),
            ),
            file: "routers/certificates.py".to_string(),
            line: 380,
            end_line: 410,
            is_exported: false,
            language: Language::Python,
            last_modified: 0,
        }
    }

    #[test]
    fn test_build_embedding_text_includes_high_signal_anchors() {
        let node = sample_node(
            r#"
/// Validate organization access for renewal requests.
def _verify_org_access(org_id):
    """Ensure tenant isolation for certificate renewal routes."""
    route = "/api/v1/certificates/{cert_id}/renew"
    timeout_key = "CERT_RENEWAL_TIMEOUT_SECONDS"
    raise HTTPException(status_code=403, detail="organization access denied")
"#,
        );

        let text = build_embedding_text(&node);
        assert!(text.contains("symbol: _verify_org_access"));
        assert!(text.contains("file: routers/certificates.py"));
        assert!(text.contains("comments_docstrings:"));
        assert!(text.contains("error_strings:"));
        assert!(text.contains("config_keys:"));
        assert!(text.contains("routes:"));
        assert!(text.contains("/api/v1/certificates/{cert_id}/renew"));
        assert!(text.contains("CERT_RENEWAL_TIMEOUT_SECONDS"));
    }

    #[test]
    fn test_build_embedding_text_is_bounded() {
        let mut long_body = String::new();
        for _ in 0..200 {
            long_body.push_str(
                "raise RuntimeError(\"organization access denied due to missing account role\")\n",
            );
            long_body.push_str("route = \"/api/v1/certificates/{cert_id}/renew\"\n");
            long_body.push_str("config = \"CERT_RENEWAL_TIMEOUT_SECONDS\"\n");
        }

        let node = sample_node(&long_body);
        let text = build_embedding_text(&node);
        assert!(text.chars().count() <= MAX_EMBED_TEXT_CHARS);
        assert!(text.contains("error_strings:"));
        assert!(text.contains("routes:"));
        assert!(text.contains("config_keys:"));
    }

    #[test]
    fn test_build_file_summary_embedding_text_includes_file_granularity_signals() {
        let first = sample_node(
            r#"
/// Validate organization access for renewal requests.
def _verify_org_access(org_id):
    """Ensure tenant isolation for certificate renewal routes."""
    route = "/api/v1/certificates/{cert_id}/renew"
    timeout_key = "CERT_RENEWAL_TIMEOUT_SECONDS"
    raise HTTPException(status_code=403, detail="organization access denied")
"#,
        );
        let second = sample_helper_node();
        let text = build_file_summary_embedding_text("routers/certificates.py", &[&first, &second]);

        assert!(text.contains("granularity: file_summary"));
        assert!(text.contains("file: routers/certificates.py"));
        assert!(text.contains("symbol_count: 2"));
        assert!(text.contains("top_symbols:"));
        assert!(text.contains("upsert_renewal_policy"));
        assert!(text.contains("routes:"));
    }
}
