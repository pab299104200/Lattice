//! Identity resolver for Phase 1 stable ids.
//! This module implements the resolver deliverable required by
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 1: Unified Identity Model`. It also preserves the stable
//! follow-up contract from
//! `docs/architecture/2026-04-11-stable-follow-up-handles.md` `## Contract`
//! while using a small LRU justified by that note's
//! `## Assistant-Facing Surfaces`.
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use thiserror::Error;

use crate::graph::{CodeGraph, GraphNode};
use crate::identity::ambiguity::{
    section_disambiguation_hint, symbol_disambiguation_hint, test_disambiguation_hint,
    AmbiguityReport, ResolveOutcome,
};
use crate::storage::graph_store::FileIndexEntry;
use crate::symbols::{ParsedFile, SymbolKind};

use super::encoding::decode_identity;
use super::kinds::{DocId, EventId, FileId, Identity, SectionId, SymbolId};

pub type WorkspaceId = String;

const DEFAULT_CONTENT_HASH: &str = "00000000";
const DEFAULT_CACHE_CAPACITY: usize = 64;

/// Resolves user-facing or assistant-facing references into stable identities.
pub struct IdentityResolver<'a> {
    graph: &'a CodeGraph,
    file_index: &'a HashMap<String, FileIndexEntry>,
    parsed_files: &'a HashMap<String, ParsedFile>,
    default_workspace: WorkspaceId,
    latest_event_id: Option<EventId>,
    event_index: HashMap<String, EventId>,
    cache: RefCell<ResolverCache>,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("workspace `{workspace_id}` is not indexed")]
    WorkspaceNotIndexed { workspace_id: WorkspaceId },
    #[error("malformed {kind} reference `{query}`: {reason}")]
    Malformed {
        kind: &'static str,
        query: String,
        reason: &'static str,
    },
    #[error("no {kind} identity found for `{query}`")]
    NotFound { kind: &'static str, query: String },
    #[error(
        "event index is behind the requested reference; latest indexed event is {latest_event_id}"
    )]
    IndexLagBehind { latest_event_id: EventId },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum CacheKey {
    Path(WorkspaceId, String),
    Symbol(WorkspaceId, String),
    Section(WorkspaceId, String, String),
    Test(WorkspaceId, String),
    Event(WorkspaceId, String),
    LegacySymbol(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CacheValue {
    File(Result<FileId, ResolveError>),
    Symbol(ResolveOutcome<SymbolId>),
    Section(ResolveOutcome<SectionId>),
    Test(ResolveOutcome<SymbolId>),
    Event(Result<EventId, ResolveError>),
}

#[derive(Default)]
struct ResolverCache {
    capacity: usize,
    order: VecDeque<CacheKey>,
    values: HashMap<CacheKey, CacheValue>,
}

impl ResolverCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            order: VecDeque::new(),
            values: HashMap::new(),
        }
    }

    fn get(&mut self, key: &CacheKey) -> Option<CacheValue> {
        let value = self.values.get(key)?.clone();
        self.touch(key.clone());
        Some(value)
    }

    fn insert(&mut self, key: CacheKey, value: CacheValue) {
        self.values.insert(key.clone(), value);
        self.touch(key);
        while self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.values.remove(&oldest);
            }
        }
    }

    fn touch(&mut self, key: CacheKey) {
        self.order.retain(|existing| existing != &key);
        self.order.push_back(key);
    }
}

impl<'a> IdentityResolver<'a> {
    pub fn new(
        graph: &'a CodeGraph,
        file_index: &'a HashMap<String, FileIndexEntry>,
        parsed_files: &'a HashMap<String, ParsedFile>,
        default_workspace: WorkspaceId,
        event_ids: Vec<EventId>,
    ) -> Self {
        let latest_event_id = event_ids.iter().max_by_key(|event| &event.ulid).cloned();
        let event_index = event_ids
            .into_iter()
            .map(|event| (event.ulid.clone(), event))
            .collect();

        Self {
            graph,
            file_index,
            parsed_files,
            default_workspace,
            latest_event_id,
            event_index,
            cache: RefCell::new(ResolverCache::new(DEFAULT_CACHE_CAPACITY)),
        }
    }

    pub fn resolve_path(
        &self,
        workspace: &WorkspaceId,
        path: &str,
    ) -> Result<FileId, ResolveError> {
        let key = CacheKey::Path(workspace.clone(), path.to_string());
        if let Some(CacheValue::File(result)) = self.cache.borrow_mut().get(&key) {
            return result;
        }

        let result = self.resolve_path_uncached(workspace, path);
        self.cache
            .borrow_mut()
            .insert(key, CacheValue::File(result.clone()));
        result
    }

    pub fn resolve_symbol(
        &self,
        workspace: &WorkspaceId,
        qualified_name: &str,
    ) -> ResolveOutcome<SymbolId> {
        let key = CacheKey::Symbol(workspace.clone(), qualified_name.to_string());
        if let Some(CacheValue::Symbol(result)) = self.cache.borrow_mut().get(&key) {
            return result;
        }

        let result = self.resolve_symbol_uncached(workspace, qualified_name);
        self.cache
            .borrow_mut()
            .insert(key, CacheValue::Symbol(result.clone()));
        result
    }

    pub fn resolve_section(
        &self,
        workspace: &WorkspaceId,
        doc: &DocId,
        heading: &str,
    ) -> ResolveOutcome<SectionId> {
        let key = CacheKey::Section(
            workspace.clone(),
            doc.repo_relative_path.clone(),
            heading.to_string(),
        );
        if let Some(CacheValue::Section(result)) = self.cache.borrow_mut().get(&key) {
            return result;
        }

        let result = self.resolve_section_uncached(workspace, doc, heading);
        self.cache
            .borrow_mut()
            .insert(key, CacheValue::Section(result.clone()));
        result
    }

    pub fn resolve_test(
        &self,
        workspace: &WorkspaceId,
        test_name: &str,
    ) -> ResolveOutcome<SymbolId> {
        let key = CacheKey::Test(workspace.clone(), test_name.to_string());
        if let Some(CacheValue::Test(result)) = self.cache.borrow_mut().get(&key) {
            return result;
        }

        let result = self.resolve_test_uncached(workspace, test_name);
        self.cache
            .borrow_mut()
            .insert(key, CacheValue::Test(result.clone()));
        result
    }

    pub fn resolve_event_ref(
        &self,
        workspace: &WorkspaceId,
        ref_str: &str,
    ) -> Result<EventId, ResolveError> {
        let key = CacheKey::Event(workspace.clone(), ref_str.to_string());
        if let Some(CacheValue::Event(result)) = self.cache.borrow_mut().get(&key) {
            return result;
        }

        let result = self.resolve_event_ref_uncached(workspace, ref_str);
        self.cache
            .borrow_mut()
            .insert(key, CacheValue::Event(result.clone()));
        result
    }

    pub fn resolve_legacy_symbol_name(&self, name: &str) -> ResolveOutcome<SymbolId> {
        let key = CacheKey::LegacySymbol(name.to_string());
        if let Some(CacheValue::Symbol(result)) = self.cache.borrow_mut().get(&key) {
            return result;
        }

        let result = self.resolve_symbol(&self.default_workspace, name);
        self.cache
            .borrow_mut()
            .insert(key, CacheValue::Symbol(result.clone()));
        result
    }

    pub fn default_workspace_id(&self) -> &WorkspaceId {
        &self.default_workspace
    }

    fn resolve_path_uncached(
        &self,
        workspace: &WorkspaceId,
        path: &str,
    ) -> Result<FileId, ResolveError> {
        if let Some(file_id) = self.resolve_file_identity_compat(workspace, path)? {
            return Ok(file_id);
        }
        self.ensure_workspace_indexed(workspace)?;
        let wanted = normalize_path(path);
        if wanted.is_empty() {
            return Err(ResolveError::Malformed {
                kind: "path",
                query: path.to_string(),
                reason: "expected a non-empty repo-relative path",
            });
        }

        self.file_index
            .keys()
            .find(|stored_path| {
                self.relative_path_in_workspace(workspace, stored_path) == Some(wanted.clone())
            })
            .map(|stored_path| self.file_id_for_stored_path(workspace, stored_path))
            .transpose()?
            .ok_or_else(|| ResolveError::NotFound {
                kind: "file",
                query: path.to_string(),
            })
    }

    fn resolve_symbol_uncached(
        &self,
        workspace: &WorkspaceId,
        qualified_name: &str,
    ) -> ResolveOutcome<SymbolId> {
        if let Some(identity) = decode_identity(qualified_name).ok() {
            match identity {
                Identity::Symbol(symbol_id) if symbol_id.file.workspace_id == *workspace => {
                    return ResolveOutcome::Unique(symbol_id);
                }
                Identity::Symbol(symbol_id) => {
                    return ResolveOutcome::NotFound(ResolveError::NotFound {
                        kind: "symbol",
                        query: symbol_id.to_string(),
                    });
                }
                _ => {}
            }
        }
        if let Err(error) = self.ensure_workspace_indexed(workspace) {
            return ResolveOutcome::NotFound(error);
        }

        let (file_hint, symbol_hint) = split_symbol_target(qualified_name);
        let wanted = normalize_symbol_name(symbol_hint);
        if wanted.is_empty() {
            return ResolveOutcome::NotFound(ResolveError::Malformed {
                kind: "symbol",
                query: qualified_name.to_string(),
                reason: "expected a symbol name or stable symbol identity",
            });
        }

        let candidates = self.collect_symbol_candidates(workspace, file_hint, &wanted);
        unique_or_ambiguous_symbol(qualified_name, candidates)
    }

    fn resolve_section_uncached(
        &self,
        workspace: &WorkspaceId,
        doc: &DocId,
        heading: &str,
    ) -> ResolveOutcome<SectionId> {
        if let Err(error) = self.ensure_workspace_indexed(workspace) {
            return ResolveOutcome::NotFound(error);
        }
        if doc.workspace_id != *workspace {
            return ResolveOutcome::NotFound(ResolveError::NotFound {
                kind: "document",
                query: doc.to_string(),
            });
        }

        let Some(stored_path) = self.stored_path_for_doc(workspace, doc) else {
            return ResolveOutcome::NotFound(ResolveError::NotFound {
                kind: "document",
                query: doc.to_string(),
            });
        };

        let Some(parsed_file) = self.parsed_files.get(&stored_path) else {
            return ResolveOutcome::NotFound(ResolveError::NotFound {
                kind: "section",
                query: heading.to_string(),
            });
        };

        let wanted = normalize_heading(heading);
        let candidates = parsed_file
            .symbols
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::Section)
            .filter(|symbol| normalize_heading(&symbol.name) == wanted)
            .map(|symbol| SectionId {
                doc: doc.clone(),
                heading_path: vec![symbol.name.clone()],
                byte_offset: symbol.id.byte_offset,
            })
            .collect::<Vec<_>>();

        match candidates.len() {
            0 => ResolveOutcome::NotFound(ResolveError::NotFound {
                kind: "section",
                query: heading.to_string(),
            }),
            1 => ResolveOutcome::Unique(candidates[0].clone()),
            _ => ResolveOutcome::Ambiguous(AmbiguityReport::new(
                heading,
                candidates,
                section_disambiguation_hint(),
            )),
        }
    }

    fn resolve_test_uncached(
        &self,
        workspace: &WorkspaceId,
        test_name: &str,
    ) -> ResolveOutcome<SymbolId> {
        if let Err(error) = self.ensure_workspace_indexed(workspace) {
            return ResolveOutcome::NotFound(error);
        }
        let wanted = normalize_symbol_name(test_name);
        if wanted.is_empty() {
            return ResolveOutcome::NotFound(ResolveError::Malformed {
                kind: "test",
                query: test_name.to_string(),
                reason: "expected a non-empty test name",
            });
        }

        let candidates = self
            .graph
            .all_nodes()
            .into_iter()
            .filter(|node| {
                self.relative_path_in_workspace(workspace, &node.file)
                    .is_some()
            })
            .filter(|node| is_test_node(node))
            .filter(|node| normalize_symbol_name(&node.name) == wanted)
            .filter_map(|node| self.symbol_id_for_node(workspace, node, Some("test")))
            .collect::<Vec<_>>();

        match candidates.len() {
            0 => ResolveOutcome::NotFound(ResolveError::NotFound {
                kind: "test",
                query: test_name.to_string(),
            }),
            1 => ResolveOutcome::Unique(candidates[0].clone()),
            _ => ResolveOutcome::Ambiguous(AmbiguityReport::new(
                test_name,
                candidates,
                test_disambiguation_hint(),
            )),
        }
    }

    fn resolve_event_ref_uncached(
        &self,
        workspace: &WorkspaceId,
        ref_str: &str,
    ) -> Result<EventId, ResolveError> {
        if let Some(identity) = decode_identity(ref_str).ok() {
            match identity {
                Identity::Event(event_id) if event_id.workspace_id == *workspace => {
                    return Ok(event_id);
                }
                Identity::Event(event_id) => {
                    return Err(ResolveError::NotFound {
                        kind: "event",
                        query: event_id.to_string(),
                    });
                }
                _ => {}
            }
        }
        if !self
            .event_index
            .values()
            .any(|event_id| event_id.workspace_id == *workspace)
        {
            return Err(ResolveError::WorkspaceNotIndexed {
                workspace_id: workspace.clone(),
            });
        }
        let trimmed = ref_str.trim();
        if !is_canonical_ulid(trimmed) {
            return Err(ResolveError::Malformed {
                kind: "event",
                query: ref_str.to_string(),
                reason: "expected a canonical ULID or stable event identity",
            });
        }
        if let Some(event_id) = self.event_index.get(trimmed) {
            return Ok(event_id.clone());
        }
        if self.event_ref_appears_ahead_of_index(workspace, trimmed) {
            return Err(ResolveError::IndexLagBehind {
                latest_event_id: self.latest_event_id.clone().expect("checked above"),
            });
        }
        Err(ResolveError::NotFound {
            kind: "event",
            query: ref_str.to_string(),
        })
    }

    fn collect_symbol_candidates(
        &self,
        workspace: &WorkspaceId,
        file_hint: Option<&str>,
        wanted: &str,
    ) -> Vec<SymbolId> {
        let wanted_lower = wanted.to_lowercase();
        self.graph
            .all_nodes()
            .into_iter()
            .filter(|node| {
                self.relative_path_in_workspace(workspace, &node.file)
                    .is_some()
            })
            .filter(|node| node.kind != SymbolKind::Document && node.kind != SymbolKind::Section)
            .filter(|node| {
                file_hint
                    .map(|hint| self.node_matches_file_hint(workspace, node, hint))
                    .unwrap_or(true)
            })
            .filter(|node| normalize_symbol_name(&node.name).to_lowercase() == wanted_lower)
            .filter_map(|node| self.symbol_id_for_node(workspace, node, None))
            .collect()
    }

    fn node_matches_file_hint(
        &self,
        workspace: &WorkspaceId,
        node: &GraphNode,
        file_hint: &str,
    ) -> bool {
        let wanted = normalize_path(file_hint);
        let Some(relative_path) = self.relative_path_in_workspace(workspace, &node.file) else {
            return false;
        };
        relative_path == wanted
            || relative_path.ends_with(&format!("/{wanted}"))
            || relative_path.rsplit('/').next() == Some(wanted.as_str())
    }

    fn ensure_workspace_indexed(&self, workspace: &WorkspaceId) -> Result<(), ResolveError> {
        if self
            .file_index
            .keys()
            .any(|path| self.relative_path_in_workspace(workspace, path).is_some())
            || self.workspace_has_nodes(workspace)
        {
            Ok(())
        } else {
            Err(ResolveError::WorkspaceNotIndexed {
                workspace_id: workspace.clone(),
            })
        }
    }

    fn workspace_has_nodes(&self, workspace: &WorkspaceId) -> bool {
        self.graph.all_nodes().into_iter().any(|node| {
            self.relative_path_in_workspace(workspace, &node.file)
                .is_some()
        })
    }

    fn event_ref_appears_ahead_of_index(&self, workspace: &WorkspaceId, ulid: &str) -> bool {
        self.latest_event_id
            .as_ref()
            .filter(|latest| latest.workspace_id == *workspace)
            .map(|latest| ulid > latest.ulid.as_str())
            .unwrap_or(false)
    }

    fn file_id_for_stored_path(
        &self,
        workspace: &WorkspaceId,
        stored_path: &str,
    ) -> Result<FileId, ResolveError> {
        let Some(repo_relative_path) = self.relative_path_in_workspace(workspace, stored_path)
        else {
            return Err(ResolveError::NotFound {
                kind: "file",
                query: stored_path.to_string(),
            });
        };
        Ok(FileId {
            workspace_id: workspace.clone(),
            repo_relative_path,
            content_hash: self
                .file_index
                .get(stored_path)
                .map(|entry| entry.content_hash.clone())
                .unwrap_or_else(|| DEFAULT_CONTENT_HASH.to_string()),
        })
    }

    fn symbol_id_for_node(
        &self,
        workspace: &WorkspaceId,
        node: &GraphNode,
        forced_kind: Option<&str>,
    ) -> Option<SymbolId> {
        let file = self.file_id_for_stored_path(workspace, &node.file).ok()?;
        Some(SymbolId {
            file,
            qualified_name: node.name.clone(),
            byte_offset: node.id.byte_offset,
            kind: forced_kind
                .map(str::to_string)
                .unwrap_or_else(|| format!("{:?}", node.kind).to_lowercase()),
        })
    }

    fn stored_path_for_doc(&self, workspace: &WorkspaceId, doc: &DocId) -> Option<String> {
        self.file_index.keys().find_map(|stored_path| {
            let relative_path = self.relative_path_in_workspace(workspace, stored_path)?;
            if relative_path == doc.repo_relative_path {
                Some(stored_path.clone())
            } else {
                None
            }
        })
    }

    fn relative_path_in_workspace(
        &self,
        workspace: &WorkspaceId,
        stored_path: &str,
    ) -> Option<String> {
        let normalized = normalize_path(stored_path);
        let prefix = format!("{workspace}/");
        if normalized == *workspace {
            return Some(String::new());
        }
        if let Some(stripped) = normalized.strip_prefix(&prefix) {
            return Some(stripped.to_string());
        }
        if workspace == &self.default_workspace && !self.is_namespaced_path(&normalized) {
            return Some(normalized);
        }
        None
    }

    fn is_namespaced_path(&self, path: &str) -> bool {
        let first_segment = path.split('/').next().unwrap_or(path);
        first_segment == self.default_workspace
            || self
                .file_index
                .keys()
                .filter_map(|item| normalize_path(item).split('/').next().map(str::to_string))
                .any(|segment| segment == first_segment)
    }

    fn resolve_file_identity_compat(
        &self,
        workspace: &WorkspaceId,
        query: &str,
    ) -> Result<Option<FileId>, ResolveError> {
        let Ok(identity) = decode_identity(query) else {
            return Ok(None);
        };
        let Identity::File(file_id) = identity else {
            return Ok(None);
        };
        if file_id.workspace_id != *workspace {
            return Err(ResolveError::NotFound {
                kind: "file",
                query: file_id.to_string(),
            });
        }
        if let Some(current) = self.find_current_file_by_hash(workspace, &file_id) {
            return Ok(Some(current));
        }
        Err(ResolveError::NotFound {
            kind: "file",
            query: query.to_string(),
        })
    }

    fn find_current_file_by_hash(
        &self,
        workspace: &WorkspaceId,
        legacy: &FileId,
    ) -> Option<FileId> {
        let mut matches = self
            .file_index
            .iter()
            .filter(|(stored_path, entry)| {
                entry.content_hash == legacy.content_hash
                    && self
                        .relative_path_in_workspace(workspace, stored_path)
                        .is_some()
            })
            .filter_map(|(stored_path, _)| {
                self.file_id_for_stored_path(workspace, stored_path).ok()
            })
            .collect::<Vec<_>>();
        if matches.len() == 1 {
            return matches.pop();
        }
        matches
            .into_iter()
            .find(|candidate| candidate.repo_relative_path == legacy.repo_relative_path)
    }
}

fn unique_or_ambiguous_symbol(query: &str, candidates: Vec<SymbolId>) -> ResolveOutcome<SymbolId> {
    match candidates.len() {
        0 => ResolveOutcome::NotFound(ResolveError::NotFound {
            kind: "symbol",
            query: query.to_string(),
        }),
        1 => ResolveOutcome::Unique(candidates[0].clone()),
        _ => {
            let files = candidates
                .iter()
                .map(|candidate| candidate.file.repo_relative_path.clone())
                .collect::<Vec<_>>();
            let distinct_files = files.iter().collect::<std::collections::HashSet<_>>().len();
            ResolveOutcome::Ambiguous(AmbiguityReport::new(
                query,
                candidates,
                symbol_disambiguation_hint(distinct_files > 1),
            ))
        }
    }
}

fn split_symbol_target(target: &str) -> (Option<&str>, &str) {
    let trimmed = target.trim();
    if let Some((file, symbol)) = trimmed.rsplit_once("::") {
        if looks_like_path(file) {
            return (Some(file), symbol);
        }
    }
    if let Some((file, symbol)) = trimmed.rsplit_once(':') {
        if looks_like_path(file) {
            return (Some(file), symbol);
        }
    }
    (None, trimmed)
}

fn looks_like_path(value: &str) -> bool {
    value.contains('/') || value.contains('\\') || value.contains('.')
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

fn normalize_symbol_name(value: &str) -> String {
    value.trim().trim_matches(':').to_string()
}

fn normalize_heading(value: &str) -> String {
    value.trim().to_lowercase()
}

fn is_test_node(node: &GraphNode) -> bool {
    let lower_name = node.name.to_lowercase();
    let lower_file = node.file.to_lowercase();
    lower_name.starts_with("test")
        || lower_name.ends_with("test")
        || lower_file.contains("/tests/")
        || lower_file.contains("_test.")
        || lower_file.contains(".test.")
}

fn is_canonical_ulid(value: &str) -> bool {
    value.len() == 26
        && value.chars().all(|ch| {
            matches!(ch, '0'..='9' | 'A'..='Z') && ch != 'I' && ch != 'L' && ch != 'O' && ch != 'U'
        })
}

#[cfg(test)]
mod tests;
