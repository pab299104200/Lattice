use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::graph::model::{CodeGraph, EdgeKind, GraphNode};
use crate::symbols::{Language, SymbolId, SymbolKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocsTargetKind {
    Auto,
    File,
    Symbol,
    Doc,
    Section,
}

impl DocsTargetKind {
    pub fn from_str(value: Option<&str>) -> Self {
        match value.unwrap_or("auto") {
            "file" => Self::File,
            "symbol" => Self::Symbol,
            "doc" | "document" => Self::Doc,
            "section" => Self::Section,
            _ => Self::Auto,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DocHit {
    pub symbol: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub end_line: usize,
    pub summary: String,
    pub score: f64,
    pub reasons: Vec<String>,
    pub backlink_count: usize,
    pub outgoing_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelatedDocSymbol {
    pub symbol: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub mention_count: usize,
    pub mentioned_from: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocsCapsuleStats {
    pub doc_candidates: usize,
    pub docs_returned: usize,
    pub related_symbols_returned: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocsCapsule {
    pub query: String,
    pub docs: Vec<DocHit>,
    pub related_symbols: Vec<RelatedDocSymbol>,
    pub stats: DocsCapsuleStats,
}

#[derive(Debug, Clone, Serialize)]
pub struct LinkReference {
    pub symbol: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub relationship: String,
    pub preview: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BacklinksReport {
    pub requested_target: String,
    pub resolved_target: String,
    pub resolved_kind: String,
    pub file: Option<String>,
    pub line: Option<usize>,
    pub backlinks: Vec<LinkReference>,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutgoingLinksReport {
    pub requested_target: String,
    pub resolved_target: String,
    pub resolved_kind: String,
    pub file: Option<String>,
    pub line: Option<usize>,
    pub links: Vec<LinkReference>,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct StaleDocHit {
    pub symbol: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub end_line: usize,
    pub summary: String,
    pub score: f64,
    pub reasons: Vec<String>,
    pub matched_files: Vec<String>,
    pub matched_symbols: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StaleDocsStats {
    pub requested_files: usize,
    pub requested_symbols: usize,
    pub resolved_files: usize,
    pub resolved_symbols: usize,
    pub candidate_docs: usize,
    pub docs_returned: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct StaleDocsReport {
    pub requested_files: Vec<String>,
    pub requested_symbols: Vec<String>,
    pub resolved_files: Vec<String>,
    pub resolved_symbols: Vec<String>,
    pub docs: Vec<StaleDocHit>,
    pub stats: StaleDocsStats,
}

#[derive(Debug, Clone)]
struct ResolvedTarget {
    requested_target: String,
    resolved_target: String,
    resolved_kind: String,
    file: Option<String>,
    line: Option<usize>,
    node_ids: Vec<SymbolId>,
}

#[derive(Debug, Clone)]
struct StaleDocCandidate {
    node: GraphNode,
    score: f64,
    reasons: Vec<String>,
    reason_keys: HashSet<String>,
    matched_files: HashSet<String>,
    matched_symbols: HashSet<String>,
}

pub fn get_docs_capsule(
    graph: &CodeGraph,
    query: &str,
    files: &[String],
    symbols: &[String],
    limit: usize,
) -> DocsCapsule {
    let query_tokens = tokenize(query);
    let anchor_symbols: HashSet<String> = resolve_anchor_symbols(graph, symbols)
        .into_iter()
        .map(|value| value.to_lowercase())
        .collect();
    let anchor_files: HashSet<String> = resolve_anchor_files(graph, files)
        .into_iter()
        .map(|value| value.to_lowercase())
        .collect();
    let all_nodes = graph.all_nodes();
    let doc_nodes: Vec<&GraphNode> = all_nodes
        .into_iter()
        .filter(|node| is_doc_node(node))
        .collect();

    let mut docs = Vec::new();
    for node in &doc_nodes {
        let (score, reasons) =
            score_doc_node(graph, node, &query_tokens, &anchor_files, &anchor_symbols);
        if score <= 0.0 {
            continue;
        }

        let backlink_count = graph
            .get_dependents(&node.id)
            .into_iter()
            .filter(|(source, edge)| {
                source.language == Language::Markdown && *edge == EdgeKind::LinksTo
            })
            .count();
        let outgoing_count = graph
            .get_dependencies(&node.id)
            .into_iter()
            .filter(|(_, edge)| matches!(edge, EdgeKind::LinksTo | EdgeKind::Mentions))
            .count();

        docs.push(DocHit {
            symbol: node.name.clone(),
            kind: node.kind.short_code().to_string(),
            file: node.file.clone(),
            line: node.line,
            end_line: node.end_line,
            summary: node_preview(node),
            score,
            reasons,
            backlink_count,
            outgoing_count,
        });
    }

    docs.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    docs.truncate(limit.max(1));

    let mut related_symbols: HashMap<(String, String, usize), RelatedDocSymbol> = HashMap::new();
    for hit in &docs {
        let Some(doc_node) = graph
            .all_nodes()
            .into_iter()
            .find(|node| node.file == hit.file && node.name == hit.symbol && node.line == hit.line)
        else {
            continue;
        };

        for (dep, edge) in graph.get_dependencies(&doc_node.id) {
            if edge != EdgeKind::Mentions || is_doc_node(dep) {
                continue;
            }

            let key = (dep.file.clone(), dep.name.clone(), dep.line);
            let entry = related_symbols
                .entry(key)
                .or_insert_with(|| RelatedDocSymbol {
                    symbol: dep.name.clone(),
                    kind: dep.kind.short_code().to_string(),
                    file: dep.file.clone(),
                    line: dep.line,
                    mention_count: 0,
                    mentioned_from: Vec::new(),
                });
            entry.mention_count += 1;
            let source_label = format!("{}:{}", hit.file, hit.symbol);
            if !entry.mentioned_from.contains(&source_label) {
                entry.mentioned_from.push(source_label);
            }
        }
    }

    let mut related_symbols: Vec<RelatedDocSymbol> = related_symbols.into_values().collect();
    related_symbols.sort_by(|a, b| {
        b.mention_count
            .cmp(&a.mention_count)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    related_symbols.truncate(8);

    let docs_returned = docs.len();
    let related_symbols_returned = related_symbols.len();

    DocsCapsule {
        query: query.to_string(),
        docs,
        stats: DocsCapsuleStats {
            doc_candidates: doc_nodes.len(),
            docs_returned,
            related_symbols_returned,
        },
        related_symbols,
    }
}

pub fn get_backlinks(
    graph: &CodeGraph,
    target: &str,
    kind: DocsTargetKind,
    limit: usize,
) -> Option<BacklinksReport> {
    let resolved = resolve_target(graph, target, kind)?;
    let mut backlinks = Vec::new();
    let mut seen: HashSet<(SymbolId, EdgeKind)> = HashSet::new();

    for node_id in &resolved.node_ids {
        let node = graph.get_node(node_id)?;
        let wants_markdown_links = node.language == Language::Markdown;
        for (source, edge) in graph.get_dependents(node_id) {
            let allowed = if wants_markdown_links {
                source.language == Language::Markdown && edge == EdgeKind::LinksTo
            } else {
                source.language == Language::Markdown && edge == EdgeKind::Mentions
            };

            if !allowed || !seen.insert((source.id.clone(), edge)) {
                continue;
            }

            backlinks.push(link_reference(source, edge));
        }
    }

    backlinks.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    backlinks.truncate(limit.max(1));

    Some(BacklinksReport {
        requested_target: resolved.requested_target,
        resolved_target: resolved.resolved_target,
        resolved_kind: resolved.resolved_kind,
        file: resolved.file,
        line: resolved.line,
        count: backlinks.len(),
        backlinks,
    })
}

pub fn get_outgoing_links(
    graph: &CodeGraph,
    target: &str,
    kind: DocsTargetKind,
    limit: usize,
) -> Option<OutgoingLinksReport> {
    let resolved = resolve_target(graph, target, kind)?;
    let mut links = Vec::new();
    let mut seen: HashSet<(SymbolId, EdgeKind)> = HashSet::new();

    for node_id in &resolved.node_ids {
        let node = graph.get_node(node_id)?;
        if node.language != Language::Markdown {
            continue;
        }

        for (dep, edge) in graph.get_dependencies(node_id) {
            if !matches!(edge, EdgeKind::LinksTo | EdgeKind::Mentions)
                || !seen.insert((dep.id.clone(), edge))
            {
                continue;
            }

            links.push(link_reference(dep, edge));
        }
    }

    links.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    links.truncate(limit.max(1));

    Some(OutgoingLinksReport {
        requested_target: resolved.requested_target,
        resolved_target: resolved.resolved_target,
        resolved_kind: resolved.resolved_kind,
        file: resolved.file,
        line: resolved.line,
        count: links.len(),
        links,
    })
}

pub fn find_stale_docs(
    graph: &CodeGraph,
    files: &[String],
    symbols: &[String],
    limit: usize,
) -> StaleDocsReport {
    let requested_files = normalized_values(files);
    let requested_symbols = normalized_values(symbols);
    let mut resolved_files = HashSet::new();
    let mut resolved_symbols = HashSet::new();
    let mut candidates: HashMap<SymbolId, StaleDocCandidate> = HashMap::new();

    for file in &requested_files {
        let resolved_file_matches = match_graph_files(graph, file);
        if resolved_file_matches.is_empty() {
            continue;
        }

        for resolved_file in resolved_file_matches {
            resolved_files.insert(resolved_file.clone());
            let Some(nodes) = nodes_for_file(graph, &resolved_file) else {
                continue;
            };

            for node in nodes {
                if is_doc_node(node) {
                    for (source, edge) in graph.get_dependents(&node.id) {
                        if source.language != Language::Markdown || edge != EdgeKind::LinksTo {
                            continue;
                        }

                        record_stale_candidate(
                            &mut candidates,
                            source,
                            format!("doc:{}:{}", resolved_file, node.line),
                            format!("links to changed doc {}", stale_target_label(node)),
                            1.8,
                            Some(&resolved_file),
                            None,
                        );
                    }
                    continue;
                }

                for (source, edge) in graph.get_dependents(&node.id) {
                    if source.language != Language::Markdown || edge != EdgeKind::Mentions {
                        continue;
                    }

                    record_stale_candidate(
                        &mut candidates,
                        source,
                        format!("file-symbol:{}:{}", resolved_file, node.name),
                        format!(
                            "mentions changed symbol {} from {}",
                            node.name, resolved_file
                        ),
                        2.4,
                        Some(&resolved_file),
                        Some(&node.name),
                    );
                }
            }
        }
    }

    for symbol in &requested_symbols {
        let resolved_nodes = resolve_symbol_nodes(graph, symbol);
        if resolved_nodes.is_empty() {
            continue;
        }

        for node in resolved_nodes {
            resolved_symbols.insert(node.name.clone());
            for (source, edge) in graph.get_dependents(&node.id) {
                if source.language != Language::Markdown || edge != EdgeKind::Mentions {
                    continue;
                }

                record_stale_candidate(
                    &mut candidates,
                    source,
                    format!("symbol:{}:{}", node.name, node.file),
                    format!("mentions changed symbol {}", node.name),
                    3.2,
                    Some(&node.file),
                    Some(&node.name),
                );
            }
        }
    }

    let mut docs: Vec<StaleDocHit> = candidates
        .into_values()
        .map(|candidate| {
            let mut reasons = candidate.reasons;
            reasons.sort();

            let mut matched_files: Vec<String> = candidate.matched_files.into_iter().collect();
            matched_files.sort();

            let mut matched_symbols: Vec<String> = candidate.matched_symbols.into_iter().collect();
            matched_symbols.sort();

            let score = candidate.score
                + if candidate.node.kind == SymbolKind::Section {
                    0.35
                } else {
                    0.1
                };

            StaleDocHit {
                symbol: candidate.node.name.clone(),
                kind: candidate.node.kind.short_code().to_string(),
                file: candidate.node.file.clone(),
                line: candidate.node.line,
                end_line: candidate.node.end_line,
                summary: node_preview(&candidate.node),
                score,
                reasons,
                matched_files,
                matched_symbols,
            }
        })
        .collect();

    docs.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.matched_symbols.len().cmp(&a.matched_symbols.len()))
            .then_with(|| b.matched_files.len().cmp(&a.matched_files.len()))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    let candidate_docs = docs.len();
    docs.truncate(limit.max(1));

    let resolved_files = sorted_values(resolved_files);
    let resolved_symbols = sorted_values(resolved_symbols);
    let docs_returned = docs.len();

    StaleDocsReport {
        requested_files: requested_files.clone(),
        requested_symbols: requested_symbols.clone(),
        resolved_files: resolved_files.clone(),
        resolved_symbols: resolved_symbols.clone(),
        stats: StaleDocsStats {
            requested_files: requested_files.len(),
            requested_symbols: requested_symbols.len(),
            resolved_files: resolved_files.len(),
            resolved_symbols: resolved_symbols.len(),
            candidate_docs,
            docs_returned,
        },
        docs,
    }
}

fn score_doc_node(
    graph: &CodeGraph,
    node: &GraphNode,
    query_tokens: &[String],
    anchor_files: &HashSet<String>,
    anchor_symbols: &HashSet<String>,
) -> (f64, Vec<String>) {
    let mut score = if node.kind == SymbolKind::Section {
        0.4
    } else {
        0.1
    };
    let mut reasons = Vec::new();
    let name_lower = node.name.to_lowercase();
    let file_lower = node.file.to_lowercase();
    let body_lower = node.body.to_lowercase();

    let mut title_hits = 0;
    let mut body_hits = 0;
    for token in query_tokens {
        if name_lower.contains(token) || file_lower.contains(token) {
            score += 2.5;
            title_hits += 1;
        } else if body_lower.contains(token) {
            score += 0.9;
            body_hits += 1;
        }
    }

    if title_hits > 0 {
        reasons.push(format!("matches {} title/path token(s)", title_hits));
    }
    if body_hits > 0 {
        reasons.push(format!("matches {} body token(s)", body_hits));
    }

    let mut file_anchor_hits = 0;
    let mut symbol_anchor_hits = 0;
    for (dep, edge) in graph.get_dependencies(&node.id) {
        if edge != EdgeKind::Mentions {
            continue;
        }
        if anchor_files.contains(&dep.file.to_lowercase()) {
            score += 2.8;
            file_anchor_hits += 1;
        }
        if anchor_symbols.contains(&dep.name.to_lowercase()) {
            score += 3.2;
            symbol_anchor_hits += 1;
        }
    }

    if file_anchor_hits > 0 {
        reasons.push(format!(
            "mentions {} anchor file symbol(s)",
            file_anchor_hits
        ));
    }
    if symbol_anchor_hits > 0 {
        reasons.push(format!("mentions {} anchor symbol(s)", symbol_anchor_hits));
    }

    let backlinks = graph
        .get_dependents(&node.id)
        .into_iter()
        .filter(|(source, edge)| {
            source.language == Language::Markdown && *edge == EdgeKind::LinksTo
        })
        .count();
    if backlinks > 0 {
        score += (backlinks as f64).min(3.0) * 0.15;
        reasons.push(format!("referenced by {} other doc node(s)", backlinks));
    }

    if query_tokens.is_empty() && file_anchor_hits == 0 && symbol_anchor_hits == 0 {
        score = 0.0;
    }

    (score, reasons)
}

fn resolve_target(graph: &CodeGraph, target: &str, kind: DocsTargetKind) -> Option<ResolvedTarget> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return None;
    }

    match kind {
        DocsTargetKind::Auto => resolve_file_target(graph, trimmed)
            .or_else(|| resolve_section_target(graph, trimmed))
            .or_else(|| resolve_symbol_target(graph, trimmed))
            .or_else(|| resolve_doc_target(graph, trimmed)),
        DocsTargetKind::File => resolve_file_target(graph, trimmed),
        DocsTargetKind::Symbol => resolve_symbol_target(graph, trimmed),
        DocsTargetKind::Doc => resolve_doc_target(graph, trimmed),
        DocsTargetKind::Section => resolve_section_target(graph, trimmed),
    }
}

fn resolve_file_target(graph: &CodeGraph, target: &str) -> Option<ResolvedTarget> {
    let (file, anchor) = split_target_anchor(target);
    let resolved_files = match_graph_files(graph, file);
    if resolved_files.len() != 1 {
        return None;
    }
    let resolved_file = &resolved_files[0];
    let mut nodes = nodes_for_file(graph, resolved_file)?;

    if nodes.is_empty() {
        return None;
    }

    if let Some(anchor) = anchor {
        let sections: Vec<&GraphNode> = nodes
            .iter()
            .copied()
            .filter(|node| {
                node.kind == SymbolKind::Section
                    && normalize_anchor(&node.name) == normalize_anchor(anchor)
            })
            .collect();
        if !sections.is_empty() {
            return Some(make_resolved_target(
                target,
                format!("{}#{}", resolved_file, normalize_anchor(anchor)),
                "section",
                Some(resolved_file.to_string()),
                sections,
            ));
        }
    }

    nodes.sort_by_key(|node| node.line);
    Some(make_resolved_target(
        target,
        resolved_file.to_string(),
        "file",
        Some(resolved_file.to_string()),
        nodes,
    ))
}

fn resolve_symbol_target(graph: &CodeGraph, target: &str) -> Option<ResolvedTarget> {
    let nodes = resolve_symbol_nodes(graph, target);

    if nodes.is_empty() {
        return None;
    }

    Some(make_resolved_target(
        target,
        nodes[0].name.clone(),
        "symbol",
        nodes.first().map(|node| node.file.clone()),
        nodes,
    ))
}

fn resolve_doc_target(graph: &CodeGraph, target: &str) -> Option<ResolvedTarget> {
    let file_target = resolve_file_target(graph, target)?;
    let doc_nodes: Vec<SymbolId> = file_target
        .node_ids
        .into_iter()
        .filter(|id| {
            graph
                .get_node(id)
                .map(|node| node.kind == SymbolKind::Document)
                .unwrap_or(false)
        })
        .collect();

    if !doc_nodes.is_empty() {
        return Some(ResolvedTarget {
            requested_target: file_target.requested_target,
            resolved_target: file_target.resolved_target,
            resolved_kind: "document".to_string(),
            file: file_target.file,
            line: file_target.line,
            node_ids: doc_nodes,
        });
    }

    let nodes: Vec<&GraphNode> = graph
        .all_nodes()
        .into_iter()
        .filter(|node| node.kind == SymbolKind::Document && node.name == target)
        .collect();
    if nodes.is_empty() {
        return None;
    }

    Some(make_resolved_target(
        target,
        target.to_string(),
        "document",
        nodes.first().map(|node| node.file.clone()),
        nodes,
    ))
}

fn resolve_section_target(graph: &CodeGraph, target: &str) -> Option<ResolvedTarget> {
    let (path_or_name, anchor) = split_target_anchor(target);
    let wanted_anchor = anchor.unwrap_or(path_or_name);
    let wanted_anchor = normalize_anchor(wanted_anchor);
    let file_filter = if anchor.is_some() {
        let matches = match_graph_files(graph, path_or_name);
        if matches.is_empty() {
            return None;
        }
        Some(matches)
    } else {
        None
    };

    let nodes: Vec<&GraphNode> = graph
        .all_nodes()
        .into_iter()
        .filter(|node| {
            node.kind == SymbolKind::Section
                && normalize_anchor(&node.name) == wanted_anchor
                && file_filter
                    .as_ref()
                    .map(|files| files.iter().any(|file| node.file == *file))
                    .unwrap_or(true)
        })
        .collect();

    if nodes.is_empty() {
        return None;
    }

    Some(make_resolved_target(
        target,
        nodes[0].name.clone(),
        "section",
        nodes.first().map(|node| node.file.clone()),
        nodes,
    ))
}

fn make_resolved_target(
    requested_target: &str,
    resolved_target: String,
    resolved_kind: &str,
    file: Option<String>,
    nodes: Vec<&GraphNode>,
) -> ResolvedTarget {
    let line = nodes.first().map(|node| node.line);
    let node_ids = nodes.into_iter().map(|node| node.id.clone()).collect();

    ResolvedTarget {
        requested_target: requested_target.to_string(),
        resolved_target,
        resolved_kind: resolved_kind.to_string(),
        file,
        line,
        node_ids,
    }
}

fn split_target_anchor(target: &str) -> (&str, Option<&str>) {
    match target.split_once('#') {
        Some((file, anchor)) => (file, Some(anchor)),
        None => (target, None),
    }
}

fn resolve_anchor_files(graph: &CodeGraph, files: &[String]) -> Vec<String> {
    let mut resolved = HashSet::new();
    for file in files {
        for matched in match_graph_files(graph, file) {
            resolved.insert(matched);
        }
    }
    sorted_values(resolved)
}

fn resolve_anchor_symbols(graph: &CodeGraph, symbols: &[String]) -> Vec<String> {
    let mut resolved = HashSet::new();
    for symbol in symbols {
        for node in resolve_symbol_nodes(graph, symbol) {
            resolved.insert(node.name.clone());
        }
    }
    sorted_values(resolved)
}

fn nodes_for_file<'a>(graph: &'a CodeGraph, file: &str) -> Option<Vec<&'a GraphNode>> {
    let nodes: Vec<&GraphNode> = graph
        .all_nodes()
        .into_iter()
        .filter(|node| node.file == file)
        .collect();
    if nodes.is_empty() {
        None
    } else {
        Some(nodes)
    }
}

fn match_graph_files(graph: &CodeGraph, target: &str) -> Vec<String> {
    let normalized_target = normalize_file_path(target);
    if normalized_target.is_empty() {
        return Vec::new();
    }

    let mut files: Vec<String> = graph
        .all_nodes()
        .into_iter()
        .map(|node| node.file.clone())
        .collect();
    files.sort();
    files.dedup();

    let exact: Vec<String> = files
        .iter()
        .filter(|file| normalize_file_path(file) == normalized_target)
        .cloned()
        .collect();
    if !exact.is_empty() {
        return exact;
    }

    let suffix: Vec<String> = files
        .iter()
        .filter(|file| {
            let normalized_file = normalize_file_path(file);
            normalized_target == normalized_file
                || normalized_target.ends_with(&format!("/{}", normalized_file))
                || normalized_file.ends_with(&format!("/{}", normalized_target))
        })
        .cloned()
        .collect();
    if !suffix.is_empty() {
        return suffix;
    }

    let target_basename = normalized_target
        .rsplit('/')
        .next()
        .unwrap_or(&normalized_target);
    files
        .into_iter()
        .filter(|file| normalize_file_path(file).rsplit('/').next() == Some(target_basename))
        .collect()
}

fn resolve_symbol_nodes<'a>(graph: &'a CodeGraph, target: &str) -> Vec<&'a GraphNode> {
    let (file_hint, symbol_hint) = split_symbol_target(target);
    let wanted_symbol = normalize_symbol_name(symbol_hint);
    if wanted_symbol.is_empty() {
        return Vec::new();
    }

    let matched_files = file_hint.map(|hint| match_graph_files(graph, hint));
    let mut nodes: Vec<&GraphNode> = graph
        .all_nodes()
        .into_iter()
        .filter(|node| {
            !is_doc_node(node)
                && matched_files
                    .as_ref()
                    .map(|files| files.iter().any(|file| node.file == *file))
                    .unwrap_or(true)
        })
        .collect();

    let exact: Vec<&GraphNode> = nodes
        .iter()
        .copied()
        .filter(|node| normalize_symbol_name(&node.name) == wanted_symbol)
        .collect();
    if !exact.is_empty() {
        return exact;
    }

    let wanted_lower = wanted_symbol.to_lowercase();
    nodes.retain(|node| normalize_symbol_name(&node.name).to_lowercase() == wanted_lower);
    nodes
}

fn split_symbol_target(target: &str) -> (Option<&str>, &str) {
    let trimmed = target.trim();

    if let Some((file, symbol)) = trimmed.rsplit_once("::") {
        if !file.trim().is_empty() && !symbol.trim().is_empty() {
            return (Some(file), symbol);
        }
    }

    if let Some((file, symbol)) = trimmed.rsplit_once(':') {
        if !file.trim().is_empty()
            && !symbol.trim().is_empty()
            && (file.contains('/') || file.contains('\\') || file.contains('.'))
        {
            return (Some(file), symbol);
        }
    }

    (None, trimmed)
}

fn link_reference(node: &GraphNode, edge: EdgeKind) -> LinkReference {
    LinkReference {
        symbol: node.name.clone(),
        kind: node.kind.short_code().to_string(),
        file: node.file.clone(),
        line: node.line,
        relationship: edge_relationship(edge).to_string(),
        preview: node_preview(node),
    }
}

fn edge_relationship(edge: EdgeKind) -> &'static str {
    match edge {
        EdgeKind::LinksTo => "links_to",
        EdgeKind::Mentions => "mentions",
        EdgeKind::Calls => "calls",
        EdgeKind::Imports => "imports",
        EdgeKind::Implements => "implements",
        EdgeKind::Extends => "extends",
        EdgeKind::TypeRef => "type_ref",
        EdgeKind::Contains => "contains",
        EdgeKind::CoChanges => "co_changes",
    }
}

fn node_preview(node: &GraphNode) -> String {
    if node.language == Language::Markdown {
        summarize_markdown(node.body.as_ref(), 160)
    } else {
        truncate(node.signature.as_ref().trim(), 160)
    }
}

fn summarize_markdown(body: &str, max_len: usize) -> String {
    let mut parts = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("```")
            || trimmed.starts_with("~~~")
        {
            continue;
        }
        parts.push(trimmed);
        if parts.join(" ").len() >= max_len {
            break;
        }
    }

    truncate(parts.join(" ").trim(), max_len)
}

fn truncate(value: &str, max_len: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= max_len {
        value.to_string()
    } else {
        let visible_len = max_len.saturating_sub(3);
        let truncated: String = value.chars().take(visible_len).collect();
        format!("{}...", truncated)
    }
}

fn is_doc_node(node: &GraphNode) -> bool {
    matches!(node.kind, SymbolKind::Document | SymbolKind::Section)
        || node.language == Language::Markdown
}

fn tokenize(value: &str) -> Vec<String> {
    let mut tokens: Vec<String> = value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(|part| part.trim().to_lowercase())
        .filter(|part| part.len() >= 2)
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens
}

fn normalize_anchor(value: &str) -> String {
    let mut normalized = String::new();
    let mut last_was_dash = false;

    for ch in value.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch);
            last_was_dash = false;
        } else if ch.is_ascii_whitespace() || ch == '-' || ch == '_' {
            if !last_was_dash && !normalized.is_empty() {
                normalized.push('-');
                last_was_dash = true;
            }
        }
    }

    normalized.trim_matches('-').to_string()
}

fn normalize_file_path(value: &str) -> String {
    value
        .trim()
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

fn normalize_symbol_name(value: &str) -> String {
    let trimmed = value.trim().trim_matches('`');
    trimmed
        .strip_suffix("()")
        .unwrap_or(trimmed)
        .trim()
        .to_string()
}

fn normalized_values(values: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();

    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() || !seen.insert(trimmed.to_string()) {
            continue;
        }
        normalized.push(trimmed.to_string());
    }

    normalized
}

fn sorted_values(values: HashSet<String>) -> Vec<String> {
    let mut items: Vec<String> = values.into_iter().collect();
    items.sort();
    items
}

fn record_stale_candidate(
    candidates: &mut HashMap<SymbolId, StaleDocCandidate>,
    source: &GraphNode,
    reason_key: String,
    reason: String,
    score_delta: f64,
    matched_file: Option<&str>,
    matched_symbol: Option<&str>,
) {
    let candidate = candidates
        .entry(source.id.clone())
        .or_insert_with(|| StaleDocCandidate {
            node: source.clone(),
            score: 0.0,
            reasons: Vec::new(),
            reason_keys: HashSet::new(),
            matched_files: HashSet::new(),
            matched_symbols: HashSet::new(),
        });

    if !candidate.reason_keys.insert(reason_key) {
        return;
    }

    candidate.score += score_delta;
    candidate.reasons.push(reason);
    if let Some(file) = matched_file {
        candidate.matched_files.insert(file.to_string());
    }
    if let Some(symbol) = matched_symbol {
        candidate.matched_symbols.insert(symbol.to_string());
    }
}

fn stale_target_label(node: &GraphNode) -> String {
    if node.kind == SymbolKind::Section {
        format!("{}#{}", node.file, node.name)
    } else {
        node.file.clone()
    }
}
