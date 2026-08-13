//! Anchor extraction and identity resolution for Retrieval V1.
//!
//! The spec requires literal anchors from the task text to be extracted first,
//! then resolved through the Phase 1 identity resolver instead of redoing path
//! or symbol normalization locally. The table below is the contract for each
//! anchor family:
//!
//! | Variant | Token shape | Identity family |
//! | --- | --- | --- |
//! | `PathAnchor` | `src/lib.rs`, `docs/guide.md#Heading`, `daemon\\src\\mod.rs` | file or section |
//! | `SymbolAnchor` | `login_user`, `AuthService::refresh`, `src/auth.rs::login_user()` | symbol |
//! | `ErrorAnchor` | `error[E0425]`, `thread 'main' panicked at ...`, `Traceback (most recent call last):` | file or symbol when stack frames provide one |
//! | `CommandAnchor` | `cargo test ...`, `$ rg ...`, `npm run ...` | file or symbol when command arguments include one |
//! | `ApiAnchor` | `GET /v1/files`, `workspace.resolve`, `prepare_change` | symbol when the API/tool name maps to code |
//! | `ConfigKeyAnchor` | `LATTICE_INDEX_ROOT`, `retrieval_v1.anchor.limit` | symbol when the config key maps to code |
//!
//! Behavior follows
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 7. Retrieval Engine` and `## Phase 1: Unified Identity Model`.

use serde::{Deserialize, Serialize};

use crate::identity::{
    AmbiguityReport, DocId, Identity, IdentityResolver, ResolveError, ResolveOutcome,
};

use super::{IntentClassification, IntentLabel};

const HTTP_METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "HEAD"];
const COMMAND_PREFIXES: [&str; 9] = [
    "cargo ", "npm ", "pnpm ", "yarn ", "git ", "rg ", "pytest ", "python ", "uv ",
];
const MCP_TOOL_NAMES: [&str; 10] = [
    "prepare_change",
    "get_context_capsule",
    "summarize_subsystem",
    "expand_context",
    "find_relevant_tests",
    "diagnose_failure",
    "get_working_set_context",
    "get_impact_graph",
    "search_symbols",
    "search_logic_flow",
];
const ROOT_FILE_NAMES: [&str; 7] = [
    "AGENTS.md",
    "CLAUDE.md",
    "README",
    "README.md",
    "Cargo.toml",
    "Makefile",
    ".gitignore",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum AnchorKind {
    Path,
    Symbol,
    Error,
    Command,
    Api,
    ConfigKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathAnchor {
    pub path: String,
    pub heading: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolAnchor {
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackFrameAnchor {
    pub file_path: Option<String>,
    pub symbol_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorAnchor {
    pub message: String,
    pub stack_frames: Vec<StackFrameAnchor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandAnchor {
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiAnchor {
    pub name: String,
    pub http_method: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigKeyAnchor {
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RawAnchorData {
    Path(PathAnchor),
    Symbol(SymbolAnchor),
    Error(ErrorAnchor),
    Command(CommandAnchor),
    Api(ApiAnchor),
    ConfigKey(ConfigKeyAnchor),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawAnchor {
    pub kind: AnchorKind,
    pub anchor_text: String,
    pub source_span: SourceSpan,
    pub data: RawAnchorData,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnchorResolution {
    Resolved(Identity),
    Ambiguous {
        candidates: Vec<Identity>,
        reason: String,
    },
    Unresolved {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedAnchor {
    pub kind: AnchorKind,
    pub anchor_text: String,
    pub source_span: SourceSpan,
    pub resolution: AnchorResolution,
}

pub type AnchorDiagnostic = (String, Option<Identity>, Option<String>, SourceSpan);

impl ResolvedAnchor {
    pub fn diagnostics(&self) -> AnchorDiagnostic {
        let identity = match &self.resolution {
            AnchorResolution::Resolved(identity) => Some(identity.clone()),
            _ => None,
        };
        let reason = match &self.resolution {
            AnchorResolution::Resolved(_) => None,
            AnchorResolution::Ambiguous { reason, .. }
            | AnchorResolution::Unresolved { reason } => Some(reason.clone()),
        };
        (self.anchor_text.clone(), identity, reason, self.source_span)
    }
}

pub fn extract_anchors(task_text: &str, intent: &IntentClassification) -> Vec<RawAnchor> {
    let mut anchors = Vec::new();
    let mut line_offset = 0usize;
    for line in task_text.lines() {
        anchors.extend(extract_line_anchors(line, line_offset));
        line_offset += line.len() + 1;
    }
    anchors.sort_by(|left, right| compare_anchors(left, right, intent));
    dedupe_anchors(anchors)
}

pub fn resolve_anchors(
    raws: Vec<RawAnchor>,
    resolver: &IdentityResolver<'_>,
) -> Vec<ResolvedAnchor> {
    raws.into_iter()
        .map(|raw| resolve_anchor(raw, resolver))
        .collect()
}

fn resolve_anchor(raw: RawAnchor, resolver: &IdentityResolver<'_>) -> ResolvedAnchor {
    let resolution = match &raw.data {
        RawAnchorData::Path(anchor) => resolve_path_anchor(anchor, resolver),
        RawAnchorData::Symbol(anchor) => resolve_symbol_query(&anchor.query, resolver),
        RawAnchorData::Error(anchor) => resolve_error_anchor(anchor, resolver),
        RawAnchorData::Command(anchor) => resolve_command_anchor(anchor, resolver),
        RawAnchorData::Api(anchor) => resolve_api_anchor(anchor, resolver),
        RawAnchorData::ConfigKey(anchor) => resolve_symbol_query(&anchor.key, resolver),
    };
    ResolvedAnchor {
        kind: raw.kind,
        anchor_text: raw.anchor_text,
        source_span: raw.source_span,
        resolution,
    }
}

fn compare_anchors(
    left: &RawAnchor,
    right: &RawAnchor,
    intent: &IntentClassification,
) -> std::cmp::Ordering {
    anchor_priority(right.kind, intent)
        .cmp(&anchor_priority(left.kind, intent))
        .then_with(|| left.source_span.start.cmp(&right.source_span.start))
        .then_with(|| left.anchor_text.cmp(&right.anchor_text))
}

fn anchor_priority(kind: AnchorKind, intent: &IntentClassification) -> u8 {
    let mut priority = match kind {
        AnchorKind::Path => 60,
        AnchorKind::Symbol => 70,
        AnchorKind::Error => 55,
        AnchorKind::Command => 40,
        AnchorKind::Api => 45,
        AnchorKind::ConfigKey => 35,
    };

    match intent.primary_label {
        IntentLabel::Debug if kind == AnchorKind::Error => priority += 40,
        IntentLabel::Debug if kind == AnchorKind::Path => priority += 15,
        IntentLabel::Refactor | IntentLabel::AddFeature | IntentLabel::ModifyFeature
            if kind == AnchorKind::Symbol =>
        {
            priority += 30
        }
        IntentLabel::UpdateDocs if kind == AnchorKind::Path => priority += 35,
        _ => {}
    }

    priority
}

fn dedupe_anchors(anchors: Vec<RawAnchor>) -> Vec<RawAnchor> {
    let mut deduped: Vec<RawAnchor> = Vec::new();
    for anchor in anchors {
        let duplicate = deduped.iter().any(|existing| {
            existing.kind == anchor.kind
                && existing.source_span == anchor.source_span
                && existing.anchor_text == anchor.anchor_text
        });
        if !duplicate {
            deduped.push(anchor);
        }
    }
    deduped
}

fn extract_line_anchors(line: &str, line_offset: usize) -> Vec<RawAnchor> {
    let mut anchors = Vec::new();
    anchors.extend(extract_error_anchor(line, line_offset));
    anchors.extend(extract_command_anchor(line, line_offset));
    anchors.extend(extract_http_api_anchors(line, line_offset));
    anchors.extend(extract_tool_api_anchors(line, line_offset));

    for token in tokenize_with_spans(line, line_offset) {
        anchors.extend(classify_token_anchors(line, token));
    }
    anchors
}

fn extract_error_anchor(line: &str, line_offset: usize) -> Option<RawAnchor> {
    let trimmed = line.trim();
    if !is_error_line(trimmed) {
        return None;
    }
    let stack_frames = tokenize_with_spans(line, line_offset)
        .into_iter()
        .filter_map(|token| stack_frame_from_token(&token.text))
        .collect::<Vec<_>>();
    Some(RawAnchor {
        kind: AnchorKind::Error,
        anchor_text: trimmed.to_string(),
        source_span: SourceSpan {
            start: line_offset + line.find(trimmed).unwrap_or(0),
            end: line_offset + line.find(trimmed).unwrap_or(0) + trimmed.len(),
        },
        data: RawAnchorData::Error(ErrorAnchor {
            message: trimmed.to_string(),
            stack_frames,
        }),
    })
}

fn extract_command_anchor(line: &str, line_offset: usize) -> Option<RawAnchor> {
    let trimmed = line.trim();
    let command = trimmed.strip_prefix("$ ").unwrap_or(trimmed);
    let mut matched = command.to_string();
    if !looks_like_command(command) {
        matched = find_embedded_command(line)?;
    }
    Some(RawAnchor {
        kind: AnchorKind::Command,
        anchor_text: matched.clone(),
        source_span: SourceSpan {
            start: line_offset + line.find(&matched).unwrap_or(0),
            end: line_offset + line.find(&matched).unwrap_or(0) + matched.len(),
        },
        data: RawAnchorData::Command(CommandAnchor { command: matched }),
    })
}

fn extract_http_api_anchors(line: &str, line_offset: usize) -> Vec<RawAnchor> {
    let mut anchors = Vec::new();
    let tokens = line.split_whitespace().collect::<Vec<_>>();
    for pair in tokens.windows(2) {
        if !HTTP_METHODS.contains(&pair[0]) || !pair[1].starts_with('/') {
            continue;
        }
        let text = format!("{} {}", pair[0], trim_token_edge(pair[1]));
        if let Some(start) = line.find(&text) {
            anchors.push(RawAnchor {
                kind: AnchorKind::Api,
                anchor_text: text.clone(),
                source_span: SourceSpan {
                    start: line_offset + start,
                    end: line_offset + start + text.len(),
                },
                data: RawAnchorData::Api(ApiAnchor {
                    name: trim_token_edge(pair[1]).to_string(),
                    http_method: Some(pair[0].to_string()),
                }),
            });
        }
    }
    anchors
}

fn extract_tool_api_anchors(line: &str, line_offset: usize) -> Vec<RawAnchor> {
    let mut anchors = Vec::new();
    for tool in MCP_TOOL_NAMES {
        if let Some(index) = line.find(tool) {
            anchors.push(RawAnchor {
                kind: AnchorKind::Api,
                anchor_text: tool.to_string(),
                source_span: SourceSpan {
                    start: line_offset + index,
                    end: line_offset + index + tool.len(),
                },
                data: RawAnchorData::Api(ApiAnchor {
                    name: tool.to_string(),
                    http_method: None,
                }),
            });
        }
    }
    anchors
}

fn classify_token_anchors(line: &str, token: TokenSpan) -> Vec<RawAnchor> {
    // A structural target carries two useful identities. Keep the literal
    // file anchor (for exact entry-file retrieval) and the qualified symbol
    // anchor (for graph lookup), rather than treating the whole target as a
    // malformed path.
    if let Some((path, symbol, path_len)) = structural_symbol_parts(&token.text) {
        let path_span = SourceSpan {
            start: token.span.start,
            end: token.span.start + path_len,
        };
        let symbol_query = format!("{path}::{symbol}");
        return vec![
            path_anchor(&path, path_span),
            RawAnchor {
                kind: AnchorKind::Symbol,
                anchor_text: symbol_query.clone(),
                source_span: token.span,
                data: RawAnchorData::Symbol(SymbolAnchor {
                    query: symbol_query,
                }),
            },
        ];
    }
    if let Some(anchor) = path_anchor_from_token(&token.text, token.span) {
        return vec![anchor];
    }
    if let Some(anchor) = config_anchor_from_token(&token.text, token.span) {
        return vec![anchor];
    }
    if let Some(anchor) = symbol_anchor_from_token(line, &token.text, token.span) {
        return vec![anchor];
    }
    Vec::new()
}

fn path_anchor_from_token(token: &str, span: SourceSpan) -> Option<RawAnchor> {
    let trimmed = trim_token_edge(token);
    let (path, heading) = normalized_path_parts(trimmed)?;
    if !looks_like_path(&path) {
        return None;
    }
    Some(path_anchor(&path, span).with_heading(heading))
}

fn path_anchor(path: &str, span: SourceSpan) -> RawAnchor {
    RawAnchor {
        kind: AnchorKind::Path,
        anchor_text: path.to_string(),
        source_span: span,
        data: RawAnchorData::Path(PathAnchor {
            path: path.to_string(),
            heading: None,
        }),
    }
}

trait PathAnchorExt {
    fn with_heading(self, heading: Option<String>) -> RawAnchor;
}

impl PathAnchorExt for RawAnchor {
    fn with_heading(mut self, heading: Option<String>) -> RawAnchor {
        if let RawAnchorData::Path(path) = &mut self.data {
            path.heading = heading;
            if let Some(heading) = &path.heading {
                self.anchor_text = format!("{}#{heading}", path.path);
            }
        }
        self
    }
}

fn normalized_path_parts(token: &str) -> Option<(String, Option<String>)> {
    let token = token.replace('\\', "/");
    let (path, heading) = split_markdown_heading(&token);
    let path = split_path_line_column(&path)
        .map(|(path, _, _)| path.to_string())
        .unwrap_or(path);
    let path = path.trim().trim_start_matches("./").trim_start_matches('/');
    (!path.is_empty()).then(|| (path.to_string(), heading))
}

fn structural_symbol_parts(token: &str) -> Option<(String, String, usize)> {
    let trimmed = trim_token_edge(token);
    let (raw_path, symbol) = trimmed.rsplit_once("::")?;
    let (path, _) = normalized_path_parts(raw_path)?;
    if !looks_like_path(&path) || !looks_like_symbol(symbol) {
        return None;
    }
    Some((path, normalize_symbol_token(symbol), raw_path.len()))
}

fn config_anchor_from_token(token: &str, span: SourceSpan) -> Option<RawAnchor> {
    let trimmed = trim_token_edge(token);
    if !looks_like_config_key(trimmed) {
        return None;
    }
    Some(RawAnchor {
        kind: AnchorKind::ConfigKey,
        anchor_text: trimmed.to_string(),
        source_span: span,
        data: RawAnchorData::ConfigKey(ConfigKeyAnchor {
            key: trimmed.to_string(),
        }),
    })
}

fn symbol_anchor_from_token(line: &str, token: &str, span: SourceSpan) -> Option<RawAnchor> {
    let trimmed = normalize_symbol_token(token);
    if trimmed.is_empty() || looks_like_path(&trimmed) || looks_like_config_key(&trimmed) {
        return None;
    }
    if !looks_like_symbol(trimmed.as_str()) {
        return None;
    }
    if line.contains("tool ") && MCP_TOOL_NAMES.contains(&trimmed.as_str()) {
        return None;
    }
    Some(RawAnchor {
        kind: AnchorKind::Symbol,
        anchor_text: trimmed.clone(),
        source_span: span,
        data: RawAnchorData::Symbol(SymbolAnchor { query: trimmed }),
    })
}

fn resolve_path_anchor(anchor: &PathAnchor, resolver: &IdentityResolver<'_>) -> AnchorResolution {
    let workspace = resolver.default_workspace_id().clone();
    let file = match resolver.resolve_path(&workspace, &anchor.path) {
        Ok(file) => file,
        Err(error) => return unresolved_from_error(error),
    };
    if let Some(heading) = &anchor.heading {
        let doc = DocId {
            workspace_id: file.workspace_id.clone(),
            repo_relative_path: file.repo_relative_path.clone(),
            content_hash: file.content_hash.clone(),
        };
        return outcome_to_resolution(resolver.resolve_section(&workspace, &doc, heading));
    }
    AnchorResolution::Resolved(Identity::File(file))
}

fn resolve_symbol_query(query: &str, resolver: &IdentityResolver<'_>) -> AnchorResolution {
    let workspace = resolver.default_workspace_id().clone();
    let outcome = if query.contains("::") || query.contains(':') {
        resolver.resolve_symbol(&workspace, query)
    } else {
        resolver.resolve_legacy_symbol_name(query)
    };
    outcome_to_resolution(outcome)
}

fn resolve_error_anchor(anchor: &ErrorAnchor, resolver: &IdentityResolver<'_>) -> AnchorResolution {
    for frame in &anchor.stack_frames {
        if let Some(path) = &frame.file_path {
            let path_anchor = PathAnchor {
                path: path.clone(),
                heading: None,
            };
            let resolution = resolve_path_anchor(&path_anchor, resolver);
            if !matches!(resolution, AnchorResolution::Unresolved { .. }) {
                return resolution;
            }
        }
        if let Some(symbol) = &frame.symbol_hint {
            let resolution = resolve_symbol_query(symbol, resolver);
            if !matches!(resolution, AnchorResolution::Unresolved { .. }) {
                return resolution;
            }
        }
    }
    AnchorResolution::Unresolved {
        reason: "error anchor did not include a resolvable stack frame".to_string(),
    }
}

fn resolve_command_anchor(
    anchor: &CommandAnchor,
    resolver: &IdentityResolver<'_>,
) -> AnchorResolution {
    for part in anchor.command.split_whitespace() {
        let token = trim_token_edge(part);
        if looks_like_path(token) {
            let resolution = resolve_path_anchor(
                &PathAnchor {
                    path: split_markdown_heading(token).0,
                    heading: None,
                },
                resolver,
            );
            if !matches!(resolution, AnchorResolution::Unresolved { .. }) {
                return resolution;
            }
        }
        let symbol = normalize_symbol_token(token);
        if looks_like_symbol(&symbol) {
            let resolution = resolve_symbol_query(&symbol, resolver);
            if !matches!(resolution, AnchorResolution::Unresolved { .. }) {
                return resolution;
            }
        }
    }
    AnchorResolution::Unresolved {
        reason: "command anchor did not reference a resolvable file or symbol".to_string(),
    }
}

fn resolve_api_anchor(anchor: &ApiAnchor, resolver: &IdentityResolver<'_>) -> AnchorResolution {
    if anchor.http_method.is_some() {
        return AnchorResolution::Unresolved {
            reason: "HTTP route anchors require endpoint identities beyond Phase 1".to_string(),
        };
    }
    resolve_symbol_query(&anchor.name, resolver)
}

fn outcome_to_resolution<T>(outcome: ResolveOutcome<T>) -> AnchorResolution
where
    T: Into<Identity> + Clone,
{
    match outcome {
        ResolveOutcome::Unique(identity) => AnchorResolution::Resolved(identity.into()),
        ResolveOutcome::Ambiguous(report) => ambiguous_resolution(report),
        ResolveOutcome::NotFound(error) => unresolved_from_error(error),
    }
}

fn ambiguous_resolution<T>(report: AmbiguityReport<T>) -> AnchorResolution
where
    T: Into<Identity>,
{
    AnchorResolution::Ambiguous {
        candidates: report.candidates.into_iter().map(Into::into).collect(),
        reason: report.disambiguation_hint,
    }
}

fn unresolved_from_error(error: ResolveError) -> AnchorResolution {
    AnchorResolution::Unresolved {
        reason: error.to_string(),
    }
}

fn tokenize_with_spans(line: &str, line_offset: usize) -> Vec<TokenSpan> {
    let mut spans = Vec::new();
    let mut current_start = None;
    for (index, ch) in line.char_indices() {
        if ch.is_whitespace() {
            push_token_span(line, line_offset, &mut spans, current_start.take(), index);
            continue;
        }
        if current_start.is_none() {
            current_start = Some(index);
        }
    }
    push_token_span(line, line_offset, &mut spans, current_start, line.len());
    spans
}

fn push_token_span(
    line: &str,
    line_offset: usize,
    spans: &mut Vec<TokenSpan>,
    start: Option<usize>,
    end: usize,
) {
    let Some(start) = start else {
        return;
    };
    let raw = &line[start..end];
    let trimmed = trim_token_edge(raw);
    if trimmed.is_empty() {
        return;
    }
    let leading = raw.find(trimmed).unwrap_or(0);
    let token_start = start + leading;
    spans.push(TokenSpan {
        text: trimmed.to_string(),
        span: SourceSpan {
            start: line_offset + token_start,
            end: line_offset + token_start + trimmed.len(),
        },
    });
}

fn is_error_line(line: &str) -> bool {
    line.contains("error[E")
        || line.contains("panicked at")
        || line.contains("Traceback (most recent call last):")
        || line.contains("Exception:")
}

fn looks_like_command(text: &str) -> bool {
    COMMAND_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

fn find_embedded_command(line: &str) -> Option<String> {
    COMMAND_PREFIXES.iter().find_map(|prefix| {
        let start = line.find(prefix)?;
        let rest = &line[start..];
        let end = rest.find('`').unwrap_or(rest.len());
        Some(trim_token_edge(&rest[..end]).to_string())
    })
}

fn looks_like_path(text: &str) -> bool {
    let lowered = text.to_lowercase();
    let basename = lowered.rsplit('/').next().unwrap_or(&lowered);
    let has_known_extension = [
        ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".java", ".md", ".json", ".toml",
        ".yaml", ".yml",
    ]
    .iter()
    .any(|extension| basename.ends_with(extension));
    has_known_extension
        || ROOT_FILE_NAMES
            .iter()
            .any(|name| basename == name.to_lowercase())
        || lowered.starts_with("src/")
        || lowered.starts_with("docs/")
}

fn looks_like_config_key(text: &str) -> bool {
    is_env_key(text) || is_dotted_config_key(text)
}

fn is_env_key(text: &str) -> bool {
    text.len() > 4
        && text.contains('_')
        && text
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

fn is_dotted_config_key(text: &str) -> bool {
    text.contains('.')
        && !looks_like_path(text)
        && text
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

fn looks_like_symbol(text: &str) -> bool {
    if text.is_empty() || HTTP_METHODS.contains(&text) {
        return false;
    }
    let bare = text.trim_matches(':');
    if bare.contains("::") {
        return true;
    }
    if bare.ends_with("()") {
        return true;
    }
    if bare.contains('_') {
        return bare
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    }
    let mut saw_alpha = false;
    let mut saw_lower = false;
    for ch in bare.chars() {
        if ch.is_ascii_alphabetic() {
            saw_alpha = true;
            saw_lower |= ch.is_ascii_lowercase();
        } else if !ch.is_ascii_digit() {
            return false;
        }
    }
    saw_alpha && saw_lower
}

fn stack_frame_from_token(token: &str) -> Option<StackFrameAnchor> {
    let candidate = trim_token_edge(token);
    if let Some((path, _, _)) = split_path_line_column(candidate) {
        return Some(StackFrameAnchor {
            file_path: Some(path.to_string()),
            symbol_hint: None,
        });
    }
    let symbol = normalize_symbol_token(candidate);
    if looks_like_symbol(&symbol) {
        return Some(StackFrameAnchor {
            file_path: None,
            symbol_hint: Some(symbol),
        });
    }
    None
}

fn split_markdown_heading(text: &str) -> (String, Option<String>) {
    if let Some((path, heading)) = text.split_once(".md#") {
        return (
            format!("{path}.md"),
            Some(heading.trim().trim_matches('#').to_string()),
        );
    }
    (text.to_string(), None)
}

fn split_path_line_column(text: &str) -> Option<(&str, usize, usize)> {
    let mut parts = text.rsplitn(3, ':');
    let column = parts.next()?.parse::<usize>().ok()?;
    let line = parts.next()?.parse::<usize>().ok()?;
    let path = parts.next()?;
    Some((path, line, column))
}

fn normalize_symbol_token(token: &str) -> String {
    trim_token_edge(token)
        .trim_end_matches("()")
        .trim_matches(':')
        .to_string()
}

fn trim_token_edge(token: &str) -> &str {
    token.trim_matches(|ch: char| {
        matches!(
            ch,
            '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
        )
    })
}

#[derive(Debug, Clone)]
struct TokenSpan {
    text: String,
    span: SourceSpan,
}
