use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::graph::model::{CodeGraph, EdgeKind, GraphNode};
use crate::memory::MemoryStore;
use crate::storage::VectorStore;
use crate::symbols::{SymbolId, SymbolKind};

use super::capsule::{CapsuleStats, ContextCapsule, ContextNode, PivotNode, QueryIntent};
use super::intent::{detect_intent, IntentParams};

/// Token estimation: ~4 characters per token.
const CHARS_PER_TOKEN: usize = 4;

/// Engine version for diagnosing binary freshness.
const ENGINE_VERSION: &str = "v31";

/// Stop words excluded from the negative keyword signal.
/// These are too generic to carry semantic meaning in symbol names
/// (e.g., "get_user" — "get" shouldn't penalize a match on "user").
/// Includes common verbs, prepositions, programming primitives, and
/// generic qualifiers that don't indicate domain subsystems.
const NAME_STOP_WORDS: &[&str] = &[
    // Common verbs / actions
    "get", "set", "new", "run", "do", "is", "has", "can", "to", "from", "add", "del", "put", "all",
    "try", "with", "into", "init", "make", "create", "update", "delete", "remove", "handle",
    "process", "check", "test", "build", "parse", "load", "save", "read", "write", "find", "list",
    "show", "send", "call", "start", "stop", "open", "close", "done", "apply", "emit", "register",
    "ensure", // Prepositions / articles / conjunctions
    "by", "in", "on", "of", "for", "the", "and", "or", "at", "as",
    // Generic qualifiers / modifiers
    "current", "info", "item", "self", "this", "that", "level", "data", "name", "id", "ids", "key",
    "val", "value", "result", "error", "endpoint", "params", "args", "options", "config", "state",
    "status", "count", "index", "size", "total", "raw", "base", "node", "entry", "record", "row",
    "col", "field", // Programming language keywords / primitives
    "type", "async", "await", "impl", "func", "def", "class", "pub", "fn", "mut", "ref", "var",
    "let", "const", "return", "export", "default", "int", "str", "bool", "num", "obj", "err",
    "ctx",
];

/// A candidate node with its computed score for ranking.
struct ScoredCandidate<'a> {
    node: &'a GraphNode,
    score: f64,
    _semantic_sim: f64,
    /// Whether this node was a direct seed hit (keyword/semantic match)
    /// vs reached only through graph traversal.
    is_seed_hit: bool,
    /// How this node was reached (e.g., "semantic_match: 0.91" or "called_by: loginUser").
    relationship_detail: String,
}

/// The query engine orchestrates intent detection, search, graph traversal,
/// ranking, and budget allocation to produce Context Capsules.
pub struct QueryEngine {
    graph: CodeGraph,
    vector_store: Option<VectorStore>,
    memory_store: Option<Arc<Mutex<MemoryStore>>>,
    query_history: HashMap<String, usize>,
}

impl QueryEngine {
    /// Create a new QueryEngine with a code graph, optional vector store,
    /// and optional memory store.
    pub fn new(
        graph: CodeGraph,
        vector_store: Option<VectorStore>,
        memory_store: Option<Arc<Mutex<MemoryStore>>>,
    ) -> Self {
        Self {
            graph,
            vector_store,
            memory_store,
            query_history: HashMap::new(),
        }
    }

    /// Execute a query and produce a Context Capsule.
    ///
    /// If `embedding` is provided and a vector store is available, semantic search
    /// is used. Otherwise, falls back to keyword matching on node names/signatures.
    pub fn query(
        &mut self,
        query_text: &str,
        embedding: Option<&[f32]>,
        focused: bool,
    ) -> ContextCapsule {
        // Record the query for frequency tracking (adaptive budget)
        self.record_query(query_text);

        // Step 0: Parse query filters (repo:, file:, lang:)
        let (filter, clean_query) = parse_query_filters(query_text);

        // Step 1: Detect intent (use clean query without filter tokens)
        let intent = detect_intent(&clean_query);
        let params = IntentParams::for_intent(intent);

        // Step 2: Semantic search or keyword fallback (use clean query for matching)
        let mut seed_hits = self.find_seed_hits(&clean_query, embedding, &params);

        // Step 2b: File-path seeding for Explore intent.
        // For understanding queries, augment seeds with top-centrality functions from
        // files whose path matches query words. This finds implementation logic like
        // begin_host_patching in patch_pipeline.py when the query mentions "patch" —
        // even when function names don't match any query word directly.
        // Seed scores are IDF-weighted: files matching rare query words score higher.
        if intent == QueryIntent::Explore {
            let q_lower = clean_query.to_lowercase();
            let q_cleaned: String = q_lower
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '_' {
                        c
                    } else {
                        ' '
                    }
                })
                .collect();
            let q_words: Vec<&str> = q_cleaned
                .split_whitespace()
                .filter(|w| w.len() > 2 && !STOP_WORDS.contains(w))
                .collect();

            if !q_words.is_empty() {
                let seed_ids: std::collections::HashSet<SymbolId> =
                    seed_hits.iter().map(|(id, _)| id.clone()).collect();

                // Compute IDF for file-path seed scoring
                let all_fp_nodes = self.graph.all_nodes();
                let fp_total = all_fp_nodes.len().max(1) as f64;
                let fp_word_idf: HashMap<&str, f64> = q_words
                    .iter()
                    .map(|w| {
                        let df = all_fp_nodes
                            .iter()
                            .filter(|n| {
                                let nl = n.name.to_lowercase();
                                nl.contains(*w) || n.signature.to_lowercase().contains(*w)
                            })
                            .count()
                            .max(1);
                        (*w, (fp_total / df as f64).ln().max(0.1))
                    })
                    .collect();
                let fp_max_idf_raw = fp_word_idf.values().cloned().fold(0.1f64, f64::max);
                let fp_idf_floor = fp_max_idf_raw * 0.3;
                let fp_word_idf_floored: HashMap<&str, f64> = fp_word_idf
                    .iter()
                    .map(|(w, idf)| (*w, idf.max(fp_idf_floor)))
                    .collect();
                let fp_max_idf = fp_word_idf_floored.values().cloned().fold(0.1f64, f64::max);

                // Group functions by matching file, scored by centrality.
                // Track per-file IDF-weighted match score for prioritization.
                let mut file_candidates: HashMap<String, Vec<(SymbolId, f64)>> = HashMap::new();
                let mut file_idf_scores: HashMap<String, f64> = HashMap::new();
                let mut file_match_counts: HashMap<String, usize> = HashMap::new();

                for node in &all_fp_nodes {
                    if seed_ids.contains(&node.id)
                        || is_lattice_own_source(&node.file)
                        || is_test_file(&node.file)
                    {
                        continue;
                    }
                    if !matches!(node.kind, SymbolKind::Function | SymbolKind::Method) {
                        continue;
                    }

                    let file_lower = node.file.to_lowercase();
                    let file_segments: Vec<&str> = file_lower
                        .split(|c: char| c == '/' || c == '_' || c == '-' || c == '.')
                        .filter(|p| p.len() >= 3)
                        .collect();

                    // Compute IDF-weighted match score for this file.
                    // Use prefix matching: a file segment like "auth" matches query word
                    // "authentication" (the file abbreviates the concept). Require the
                    // prefix to be >= 4 chars to prevent short matches like "user"→"users".
                    // Do NOT match segment.starts_with(word) — that's the v20 bug
                    // where "users" matched query word "user".
                    let mut file_score: f64 = 0.0;
                    let mut match_count: usize = 0;
                    for w in &q_words {
                        let matched = file_segments
                            .iter()
                            .any(|seg| seg == w || (seg.len() >= 4 && w.starts_with(seg)));
                        if matched {
                            let idf_f =
                                fp_word_idf_floored.get(*w).copied().unwrap_or(1.0) / fp_max_idf;
                            file_score += idf_f;
                            match_count += 1;
                        }
                    }

                    if match_count == 0 {
                        continue;
                    }

                    // For long queries (5+ content words), require 2+ file segment
                    // matches. A single "system" match on an 11-word auth query seeded
                    // Go system_collector.go, pulling Logger.Errorf via graph traversal.
                    // With many query words, a single file-path match is almost certainly
                    // coincidental. Shorter queries (2-4 words) keep the 1-match threshold
                    // since each word carries more signal.
                    if q_words.len() >= 5 && match_count < 2 {
                        continue;
                    }

                    let centrality = self.graph.centrality(&node.id);
                    file_candidates
                        .entry(node.file.clone())
                        .or_default()
                        .push((node.id.clone(), centrality));
                    file_idf_scores
                        .entry(node.file.clone())
                        .and_modify(|s| {
                            if file_score > *s {
                                *s = file_score;
                            }
                        })
                        .or_insert(file_score);
                    file_match_counts
                        .entry(node.file.clone())
                        .and_modify(|c| {
                            if match_count > *c {
                                *c = match_count;
                            }
                        })
                        .or_insert(match_count);
                }

                // Sort files by IDF-weighted score DESC. Files matching rare words
                // or multiple words rank higher than files matching one common word.
                let max_total = 20;
                let mut added = 0;

                let mut files: Vec<String> = file_candidates.keys().cloned().collect();
                files.sort_by(|a, b| {
                    let sa = file_idf_scores.get(a).copied().unwrap_or(0.0);
                    let sb = file_idf_scores.get(b).copied().unwrap_or(0.0);
                    sb.partial_cmp(&sa)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.cmp(b))
                });

                let mut seeded_ids: std::collections::HashSet<SymbolId> = seed_ids.clone();

                for file in &files {
                    if added >= max_total {
                        break;
                    }
                    let file_score = file_idf_scores.get(file).copied().unwrap_or(0.0);
                    // Dynamic max_per_file: files matching 2+ query words are highly
                    // relevant and deserve more seed slots. auth_mgmt/helpers.py matching
                    // "auth" + "user" gets 5 slots; a file matching only 1 word gets 3.
                    let file_matches = file_match_counts.get(file).copied().unwrap_or(1);
                    let max_per_file = if file_matches >= 2 { 5 } else { 3 };
                    // Convert file IDF score to seed similarity: scale to 0.15..0.50 range.
                    // Max file_score is q_words.len() (if all words match with IDF=1.0).
                    let seed_sim = (0.15 + 0.35 * (file_score / q_words.len() as f64)).min(0.50);
                    if let Some(candidates) = file_candidates.get_mut(file) {
                        candidates.sort_by(|a, b| {
                            b.1.partial_cmp(&a.1)
                                .unwrap_or(std::cmp::Ordering::Equal)
                                .then_with(|| a.0.name.cmp(&b.0.name))
                        });
                        for (id, _centrality) in candidates.iter().take(max_per_file) {
                            if added >= max_total {
                                break;
                            }
                            seed_hits.push((id.clone(), seed_sim));
                            seeded_ids.insert(id.clone());
                            added += 1;
                        }
                    }
                }

                // High-centrality promotion removed: with the parser now capturing
                // Depends() edges correctly, hub functions like get_current_user are
                // found through normal graph traversal from domain-specific seeds.
                // Blind centrality promotion caused get_db/get_current_user to leak
                // into every query regardless of relevance.
            }
        }

        // Step 3: Graph traversal — N hops from semantic hits, tracking relationship paths
        let mut candidate_ids: HashMap<SymbolId, f64> = HashMap::new();
        let mut relationship_paths: HashMap<SymbolId, String> = HashMap::new();
        let seed_hit_ids: std::collections::HashSet<SymbolId> =
            seed_hits.iter().map(|(id, _)| id.clone()).collect();

        for (id, sim) in &seed_hits {
            candidate_ids.insert(id.clone(), *sim);
            relationship_paths.insert(id.clone(), format!("semantic_match: {:.2}", sim));

            // Skip graph expansion from Variable/Constant nodes.
            // Variables are data declarations (e.g., `const token = localStorage.get(...)`)
            // whose graph edges are often coincidental name matches or import artifacts.
            // Expanding from them produces cross-domain noise (e.g., RemoteDesktop's
            // "token" variable connecting to vuln_matcher's _build_candidate_products).
            if let Some(seed_node) = self.graph.get_node(id) {
                if matches!(seed_node.kind, SymbolKind::Variable | SymbolKind::Constant) {
                    continue;
                }
            }

            // Build path-aware traversal from this seed hit.
            // Propagate decayed similarity so graph-traversed nodes get
            // meaningful scores instead of 0.0.
            let seed_node_info = self.graph.get_node(id);
            let seed_name = seed_node_info
                .map(|n| n.name.clone())
                .unwrap_or_else(|| id.name.clone());
            let seed_dir = seed_node_info
                .map(|n| file_directory(&n.file))
                .unwrap_or_default();
            let base_decay_1hop = sim * 0.6;
            let base_decay_2hop = sim * 0.3;

            // 1 hop: direct dependencies (callees + contained members)
            // Contains edges at hop 1 are fine — they discover the module/class
            // that a seed function belongs to, which is key for finding siblings.
            for (dep_node, edge_kind) in self.graph.get_dependencies(id) {
                // Cross-directory penalty: hops from snmp_engine.py to tunnel.go
                // cross a domain boundary. Penalize these so they can't reach pivot
                // scores. Same-directory hops (snmp_engine.py → snmp_models.py)
                // are domain-coherent and get full decay.
                let dep_dir = file_directory(&dep_node.file);
                let same_dir = dep_dir == seed_dir;
                let decay_1hop = if same_dir {
                    base_decay_1hop
                } else {
                    base_decay_1hop * 0.4
                };

                let edge_label = format_edge_kind(edge_kind);
                candidate_ids
                    .entry(dep_node.id.clone())
                    .and_modify(|s| {
                        if decay_1hop > *s {
                            *s = decay_1hop;
                        }
                    })
                    .or_insert(decay_1hop);
                relationship_paths
                    .entry(dep_node.id.clone())
                    .or_insert_with(|| {
                        format!("{}: {} (via {})", edge_label, seed_name, dep_node.name)
                    });

                // 2 hops from dependencies
                if params.hop_depth >= 2 {
                    for (dep2_node, edge_kind2) in self.graph.get_dependencies(&dep_node.id) {
                        let dep2_dir = file_directory(&dep2_node.file);
                        let same_dir_2 = dep2_dir == seed_dir;
                        let decay_2hop = if same_dir_2 {
                            base_decay_2hop
                        } else {
                            base_decay_2hop * 0.3
                        };

                        candidate_ids
                            .entry(dep2_node.id.clone())
                            .and_modify(|s| {
                                if decay_2hop > *s {
                                    *s = decay_2hop;
                                }
                            })
                            .or_insert(decay_2hop);
                        relationship_paths
                            .entry(dep2_node.id.clone())
                            .or_insert_with(|| {
                                format!(
                                    "{} -> {} -> {} (via {:?})",
                                    seed_name, dep_node.name, dep2_node.name, edge_kind2
                                )
                            });
                    }
                }
            }

            // 1 hop: direct dependents (callers + containers)
            // Contains dependents at hop 1 discover the class/module a method belongs to.
            // No 2-hop dependents: callers-of-callers are "things that use auth" not
            // "how auth works" — always noise for understanding queries. The dependency
            // direction (callees) already handles implementation chain discovery.
            //
            // Hub-node damping: functions with many callers (>5) are infrastructure
            // utilities (e.g., get_current_idp_user_id called by 20+ routers). Expanding
            // all callers floods results with unrelated domains. For hub nodes, only
            // include Contains-direction dependents (module/class containers) which are
            // always relevant, and skip Calls-direction dependents (consumer functions).
            let dependents = self.graph.get_dependents(id);
            let is_hub_node = dependents
                .iter()
                .filter(|(_, ek)| matches!(ek, EdgeKind::Calls))
                .count()
                > 5;

            for (caller_node, edge_kind) in &dependents {
                // For hub nodes, only follow Contains edges (module discovery),
                // skip Calls edges (consumer expansion).
                if is_hub_node && matches!(edge_kind, EdgeKind::Calls) {
                    continue;
                }
                let caller_dir = file_directory(&caller_node.file);
                let same_dir_caller = caller_dir == seed_dir;
                let caller_decay = if same_dir_caller {
                    base_decay_1hop
                } else {
                    base_decay_1hop * 0.4
                };

                let edge_label = format_edge_kind_reverse(*edge_kind);
                candidate_ids
                    .entry(caller_node.id.clone())
                    .and_modify(|s| {
                        if caller_decay > *s {
                            *s = caller_decay;
                        }
                    })
                    .or_insert(caller_decay);
                relationship_paths
                    .entry(caller_node.id.clone())
                    .or_insert_with(|| format!("{}: {}", edge_label, seed_name));
            }
        }

        // Step 3b: Deep intra-module call chain traversal for Explore intent.
        // Pipeline-shaped code (create_deployment → begin_patching → advance → calculate)
        // needs following Calls edges deeper than the standard 2 hops. We extend to
        // 4 hops but only within the same directory (module boundary) to prevent
        // cross-module noise from unrelated packages.
        if intent == QueryIntent::Explore {
            for (seed_id, seed_sim) in &seed_hits {
                let seed_node = match self.graph.get_node(seed_id) {
                    Some(n) => n,
                    None => continue,
                };
                let seed_dir = std::path::Path::new(&seed_node.file)
                    .parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or("");
                if seed_dir.is_empty() {
                    continue;
                }
                let seed_name = seed_node.name.clone();

                // BFS: follow Calls edges within same directory, up to 4 hops
                let mut current_layer: Vec<SymbolId> = vec![seed_id.clone()];
                let mut visited: std::collections::HashSet<SymbolId> =
                    std::collections::HashSet::new();
                visited.insert(seed_id.clone());

                for hop in 1..=4usize {
                    let mut next_layer: Vec<SymbolId> = Vec::new();
                    for id in &current_layer {
                        for (dep, ek) in self.graph.get_dependencies(id) {
                            if !matches!(ek, EdgeKind::Calls) {
                                continue;
                            }
                            if visited.contains(&dep.id) {
                                continue;
                            }
                            let dep_dir = std::path::Path::new(&dep.file)
                                .parent()
                                .and_then(|p| p.to_str())
                                .unwrap_or("");
                            if dep_dir != seed_dir {
                                continue;
                            }

                            visited.insert(dep.id.clone());
                            next_layer.push(dep.id.clone());

                            // Only add candidates for hops 3-4 (hops 1-2 already covered)
                            if hop >= 3 {
                                let decay = seed_sim * if hop == 3 { 0.15 } else { 0.10 };
                                candidate_ids.entry(dep.id.clone()).or_insert(decay);
                                relationship_paths.entry(dep.id.clone()).or_insert_with(|| {
                                    format!("deep_chain: {} ({} hops)", seed_name, hop)
                                });
                            }
                        }
                    }
                    current_layer = next_layer;
                    if current_layer.is_empty() {
                        break;
                    }
                }
            }
        }

        // Step 4: Rank candidates (apply query filters)
        let mut candidates: Vec<ScoredCandidate> = Vec::new();
        let all_node_ids: Vec<&SymbolId> = self.graph.all_node_ids();
        let nodes_evaluated = candidate_ids.len();

        // Pre-compute query words for keyword coherence check on graph-traversed nodes.
        let scoring_q_lower = clean_query.to_lowercase();
        let scoring_q_cleaned: String = scoring_q_lower
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '_' {
                    c
                } else {
                    ' '
                }
            })
            .collect();
        let scoring_q_words: Vec<&str> = scoring_q_cleaned
            .split_whitespace()
            .filter(|w| w.len() > 2 && !STOP_WORDS.contains(w))
            .collect();

        for (id, semantic_sim) in &candidate_ids {
            if let Some(node) = self.graph.get_node(id) {
                // Skip Lattice's own source and apply query filters
                if is_lattice_own_source(&node.file) || !filter.matches(node) {
                    continue;
                }

                let centrality = self.graph.centrality(id);

                // Recency: normalize last_modified to 0..1 range based on max
                let max_modified = all_node_ids
                    .iter()
                    .filter_map(|nid| self.graph.get_node(nid))
                    .map(|n| n.last_modified)
                    .max()
                    .unwrap_or(1)
                    .max(1);
                let recency = node.last_modified as f64 / max_modified as f64;

                // Caller count: number of incoming edges (dependents).
                // Use log compression so infrastructure hubs (get_db: 500+ deps,
                // get_current_user: 531 deps) don't dominate scoring. Without this,
                // caller_norm=1.0 × w_caller=0.2 = 0.2 free score for being popular,
                // enough to push generic functions into every query's pivots.
                // log(1+n)/log(1+max) compresses the range: 5 deps → ~0.5, 500 → ~0.9
                // instead of the linear 5→0.01, 500→1.0.
                let caller_count = self.graph.get_dependents(id).len() as f64;
                let max_callers = all_node_ids
                    .iter()
                    .map(|nid| self.graph.get_dependents(nid).len())
                    .max()
                    .unwrap_or(1)
                    .max(1);
                let caller_norm = (1.0 + caller_count).ln() / (1.0 + max_callers as f64).ln();

                // Hub dampening: symbols with extreme centrality (top 1% by dependents)
                // are infrastructure utilities (get_db, get_current_user, require_permission).
                // They're called by everything but carry no topical signal. Dampen their
                // centrality and caller contributions so they can't become pivots purely
                // on graph position.
                let hub_threshold = max_callers as f64 * 0.10; // top 10% by caller count
                let is_infra_hub = caller_count > hub_threshold && caller_count > 20.0;
                let effective_centrality = if is_infra_hub {
                    centrality * 0.3
                } else {
                    centrality
                };
                let effective_caller = if is_infra_hub {
                    caller_norm * 0.3
                } else {
                    caller_norm
                };

                let mut score = semantic_sim * params.w_semantic
                    + effective_centrality * params.w_centrality
                    + recency * params.w_recency
                    + effective_caller * params.w_caller;

                // Keyword coherence gate for graph-traversed nodes.
                // Nodes reached only through graph edges (not direct keyword matches)
                // must share at least one query word in their name, signature, or file
                // path. Without this, _get_lock (score 0.159) becomes an SNMP pivot
                // because snmp_poll_loop calls it — structurally true but semantically
                // irrelevant. _get_lock has zero keyword affinity with "SNMP polling
                // credential encryption".
                //
                // Seed hits (is_seed_hit) skip this check — they already matched keywords.
                // Graph-traversed nodes with zero keyword overlap get capped at 0.06,
                // low enough to stay out of pivot range but available as last-resort context.
                if !seed_hit_ids.contains(id) {
                    if !has_keyword_coherence(
                        &node.name,
                        &node.signature,
                        &node.file,
                        &scoring_q_words,
                    ) {
                        score = score.min(0.04);
                    }
                }

                // Negative keyword signal: symbols whose names contain 2+ strong
                // words absent from the query get capped at the coherence floor.
                // "verify_agent_flexible" has 2 unmatched strong parts ("agent",
                // "flexible") → capped. But "get_password_hash" has only 1 unmatched
                // ("hash") → no penalty. The 2+ threshold prevents collateral damage
                // to descriptive names while catching wrong-subsystem matches.
                if unmatched_strong_parts(&node.name, &scoring_q_words) >= 2 {
                    score = score.min(0.04);
                }

                // Deprioritize variable/constant declarations for pivot selection.
                // Variables like RemoteDesktop.tsx:token (a one-line localStorage.getItem())
                // keyword-match "token" but waste pivot slots with assignments instead
                // of implementation logic. Functions and methods are better pivots.
                if matches!(node.kind, SymbolKind::Variable | SymbolKind::Constant) {
                    score *= 0.25;
                }

                // Demote test files — useful as context but shouldn't dominate pivots.
                // Test fixtures like conftest.py:db() have artificially high centrality
                // because everything depends on them, but they're rarely what an LLM needs.
                if is_test_file(&node.file) {
                    score *= 0.3;
                }

                // Demote migration files (Alembic, Django, etc.). upgrade()/downgrade()
                // are generic function names that keyword-match any domain term appearing
                // in the migration filename (e.g. 042_fingerprint_system.py matching "system").
                if is_migration_file(&node.file) {
                    score *= 0.2;
                }

                // Schema deprioritization for Explore intent.
                // "How does X work" queries need implementation logic, not type defs.
                // Schemas/interfaces keyword-match well (DeploymentSchema matches
                // "deployment") but don't help LLMs understand execution flow.
                if intent == QueryIntent::Explore {
                    match node.kind {
                        SymbolKind::Interface | SymbolKind::TypeAlias => {
                            score *= 0.3;
                        }
                        SymbolKind::Enum => {
                            score *= 0.5;
                        }
                        SymbolKind::Class | SymbolKind::Struct => {
                            if is_schema_heavy(&node.body) {
                                score *= 0.4;
                            }
                        }
                        _ => {}
                    }
                }

                let rel_detail = relationship_paths
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| classify_relationship(*semantic_sim));

                candidates.push(ScoredCandidate {
                    node,
                    score,
                    _semantic_sim: *semantic_sim,
                    is_seed_hit: seed_hit_ids.contains(id),
                    relationship_detail: rel_detail,
                });
            }
        }

        // Sort by descending score, tiebreak by name for deterministic results
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.node.name.cmp(&b.node.name))
        });

        // Deduplicate: when multiple candidates share the same name and >80% body
        // overlap (e.g. 5 identical `interface Host` in different .tsx files), keep
        // only the highest-scoring one. This collapses re-exported type aliases and
        // copy-pasted interfaces that waste slots without adding information.
        {
            let mut seen_bodies: Vec<(String, String)> = Vec::new(); // (name, body)
            candidates.retain(|c| {
                let dominated = seen_bodies.iter().any(|(name, body)| {
                    c.node.name == *name && line_overlap_ratio(body, &c.node.body) > 0.80
                });
                if !dominated {
                    seen_bodies.push((c.node.name.clone(), c.node.body.to_string()));
                }
                !dominated
            });
        }

        // Step 5: Budget allocation (adaptive: repeated queries expand context)
        let repeat_count = self.query_history.get(query_text).copied().unwrap_or(0);
        let mut pivots = Vec::new();
        let mut context = Vec::new();
        let mut tokens_used: usize = 0;
        let budget = if focused {
            1500
        } else {
            params.base_token_budget + (repeat_count * 500)
        };
        // Cap context nodes to prevent 3rd-degree noise from flooding results.
        // Pivots (full source) are uncapped since they're budget-limited by token cost.
        // Context (signatures) are cheap, so without a count cap they can explode to 100+.
        let max_context_nodes: usize = if focused { 5 } else { 30 };
        let max_pivots: usize = if focused { 1 } else { usize::MAX };

        // Relative pivot threshold: if the best candidate scores 0.15, absolute
        // thresholds (0.20/0.35) would produce zero pivots. Use max_score * 0.55
        // as a floor — the top ~45% of candidates become pivot-eligible regardless
        // of absolute score, ensuring every query gets some full source.
        let max_candidate_score = candidates.first().map(|c| c.score).unwrap_or(0.0);
        let relative_pivot = (max_candidate_score * 0.55).max(0.12);

        for candidate in &candidates {
            if tokens_used >= budget {
                break;
            }

            // Pivot threshold: relative to max score OR absolute floor, whichever is lower.
            // This ensures pivots even when all scores are low (SNMP/credential queries),
            // while maintaining the absolute gate when scores are healthy.
            let absolute_threshold: f64 = if candidate.is_seed_hit { 0.20 } else { 0.35 };
            let pivot_threshold = absolute_threshold.min(relative_pivot);

            if candidate.score > pivot_threshold && pivots.len() < max_pivots {
                // Pivot: include full source for strong matches
                let source_tokens = candidate.node.body.len() / CHARS_PER_TOKEN;
                if tokens_used + source_tokens > budget {
                    // Try to fit as context instead
                    if context.len() < max_context_nodes {
                        let sig_tokens = candidate.node.signature.len() / CHARS_PER_TOKEN;
                        if tokens_used + sig_tokens <= budget {
                            context.push(ContextNode {
                                symbol: candidate.node.name.clone(),
                                kind: candidate.node.kind.short_code().to_string(),
                                file: candidate.node.file.clone(),
                                line: candidate.node.line,
                                skeleton: candidate.node.signature.to_string(),
                                relationship: candidate.relationship_detail.clone(),
                                score: candidate.score,
                            });
                            tokens_used += sig_tokens;
                        }
                    }
                    continue;
                }

                // Near-duplicate detection: if another pivot has >80% line overlap,
                // demote this one to context to save tokens. Common with copy-pasted
                // handlers (e.g., authenticate_websocket_token in terminal.py + file_browser.py).
                let is_duplicate = pivots.iter().any(|p: &PivotNode| {
                    line_overlap_ratio(&p.source, &candidate.node.body) > 0.80
                });

                if is_duplicate {
                    if context.len() < max_context_nodes {
                        let sig_tokens = candidate.node.signature.len() / CHARS_PER_TOKEN;
                        if tokens_used + sig_tokens <= budget {
                            let dup_of = pivots
                                .iter()
                                .find(|p: &&PivotNode| {
                                    line_overlap_ratio(&p.source, &candidate.node.body) > 0.80
                                })
                                .map(|p| p.symbol.clone())
                                .unwrap_or_default();
                            context.push(ContextNode {
                                symbol: candidate.node.name.clone(),
                                kind: candidate.node.kind.short_code().to_string(),
                                file: candidate.node.file.clone(),
                                line: candidate.node.line,
                                skeleton: candidate.node.signature.to_string(),
                                relationship: format!("near_duplicate_of: {}", dup_of),
                                score: candidate.score,
                            });
                            tokens_used += sig_tokens;
                        }
                    }
                    continue;
                }

                let reason = if candidate.is_seed_hit {
                    format!("seed match (score: {:.2})", candidate.score)
                } else {
                    format!(
                        "graph traversal: {} (score: {:.2})",
                        candidate.relationship_detail, candidate.score
                    )
                };

                pivots.push(PivotNode {
                    symbol: candidate.node.name.clone(),
                    kind: candidate.node.kind.short_code().to_string(),
                    file: candidate.node.file.clone(),
                    line: candidate.node.line,
                    source: candidate.node.body.to_string(),
                    score: candidate.score,
                    reason,
                });
                tokens_used += source_tokens;
            } else if candidate.score > 0.05 && context.len() < max_context_nodes {
                // Context: include skeleton (signature only) for weaker matches
                let sig_tokens = candidate.node.signature.len() / CHARS_PER_TOKEN;
                if tokens_used + sig_tokens > budget {
                    continue;
                }

                context.push(ContextNode {
                    symbol: candidate.node.name.clone(),
                    kind: candidate.node.kind.short_code().to_string(),
                    file: candidate.node.file.clone(),
                    line: candidate.node.line,
                    skeleton: candidate.node.signature.to_string(),
                    relationship: candidate.relationship_detail.clone(),
                    score: candidate.score,
                });
                tokens_used += sig_tokens;
            }
            // Scores <= 0.05 or context cap reached → excluded
        }

        // Step 5b: Same-file sibling completion (skipped in focused mode).
        // If we already included 2+ symbols from the same file, include a few
        // more siblings ranked by query relevance. Guards:
        //  - Skip files with 10+ total symbols (grab-bag files like conftest.py)
        //  - Cap at 5 siblings per file
        //  - Rank candidate siblings by keyword overlap with query
        //  - Separate 500-token mini-budget so siblings aren't blocked by main budget
        if !focused {
            let sibling_budget = 500usize;
            let mut sibling_tokens_used = 0usize;
            let max_siblings_per_file = 5usize;

            // Collect files that have at least 2 included symbols
            let mut file_counts: HashMap<String, usize> = HashMap::new();
            for p in &pivots {
                *file_counts.entry(p.file.clone()).or_insert(0) += 1;
            }
            for c in &context {
                *file_counts.entry(c.file.clone()).or_insert(0) += 1;
            }
            let included_symbols: std::collections::HashSet<String> = pivots
                .iter()
                .map(|p| p.symbol.clone())
                .chain(context.iter().map(|c| c.symbol.clone()))
                .collect();

            let mut sibling_files: Vec<String> = file_counts
                .into_iter()
                .filter(|(_, count)| *count >= 2)
                .map(|(file, _)| file)
                .collect();
            // Sort for deterministic iteration — file processing order affects
            // which siblings consume the shared mini-budget first.
            sibling_files.sort();

            // Reuse query words for ranking siblings
            let sib_query_lower = clean_query.to_lowercase();
            let sib_cleaned: String = sib_query_lower
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '_' {
                        c
                    } else {
                        ' '
                    }
                })
                .collect();
            let sib_words: Vec<&str> = sib_cleaned
                .split_whitespace()
                .filter(|w| w.len() > 2 && !STOP_WORDS.contains(w))
                .collect();

            for file in &sibling_files {
                // Skip large grab-bag files (conftest.py, utils, fixtures, etc.)
                let total_in_file = self
                    .graph
                    .all_nodes()
                    .iter()
                    .filter(|n| n.file == *file)
                    .count();
                if total_in_file >= 10 {
                    continue;
                }

                // Skip files whose path has no exact-word overlap with query words.
                // Prevents irrelevant files from getting sibling expansion just
                // because one symbol happened to match a generic keyword
                // (e.g., getLastLogins matching "login" in hostinfo/users_linux.go).
                // Uses exact segment matching: path "users_linux" splits to ["users", "linux"]
                // and only matches query word "users", not "user" (no substring matching).
                let file_lower = file.to_lowercase();
                let file_segments: Vec<&str> = file_lower
                    .split(|c: char| c == '/' || c == '_' || c == '-' || c == '.')
                    .filter(|p| p.len() >= 3)
                    .collect();
                let file_relevant = sib_words
                    .iter()
                    .any(|w| file_segments.iter().any(|seg| *seg == *w));
                if !file_relevant {
                    continue;
                }

                // Collect candidate siblings with relevance scores
                let mut candidates: Vec<(&GraphNode, f64)> = Vec::new();
                for node in self.graph.all_nodes() {
                    if node.file != *file || included_symbols.contains(&node.name) {
                        continue;
                    }
                    // Score by keyword overlap with query — uses the same matching
                    // strategy as keyword_fallback (bidirectional containment, prefix
                    // matching, signature matching) so siblings like verify_agent_simple
                    // get credit for "verification" containing "verify".
                    let name_lower = node.name.to_lowercase();
                    let name_parts = split_identifier(&name_lower);
                    let sig_lower = node.signature.to_lowercase();
                    let relevance: f64 = sib_words
                        .iter()
                        .filter(|w| {
                            // Direct containment
                            name_lower.contains(*w)
                            // Part matches: word contains part OR part contains word
                            || name_parts.iter().any(|p| {
                                (p.len() >= 3 && p.as_str() == **w)
                                || (p.len() >= 4 && w.starts_with(p.as_str()))
                                || (p.len() >= 3 && p.starts_with(**w))
                            })
                            // Signature match
                            || sig_lower.contains(*w)
                        })
                        .count() as f64;
                    candidates.push((node, relevance));
                }

                // Sort by relevance descending, name ascending for determinism
                candidates.sort_by(|a, b| {
                    b.1.partial_cmp(&a.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.0.name.cmp(&b.0.name))
                });

                let mut added = 0usize;
                for (node, rel) in &candidates {
                    if added >= max_siblings_per_file {
                        break;
                    }
                    // Skip siblings with zero keyword relevance — they matched no query
                    // words and are only here because their file has 2+ included symbols.
                    // Without this, Host and Organization interfaces waste context slots.
                    if *rel <= 0.0 {
                        continue;
                    }
                    let sig_tokens = node.signature.len() / CHARS_PER_TOKEN;
                    if sibling_tokens_used + sig_tokens > sibling_budget {
                        continue;
                    }
                    context.push(ContextNode {
                        symbol: node.name.clone(),
                        kind: node.kind.short_code().to_string(),
                        file: node.file.clone(),
                        line: node.line,
                        skeleton: node.signature.to_string(),
                        relationship: format!("same_file_sibling (relevance: {:.0})", rel),
                        score: 0.06 + rel * 0.02, // slightly above context threshold
                    });
                    sibling_tokens_used += sig_tokens;
                    added += 1;
                }
            }

            tokens_used += sibling_tokens_used;
        }

        // Step 5c: Dependency completion from PIVOT nodes only (skipped in focused mode).
        // Pivots are high-confidence matches (score > 0.20). Their direct Calls
        // dependencies are likely relevant helpers. Context nodes are weaker
        // matches and their deps would amplify noise (e.g. vuln_matcher deps).
        // Uses a separate 300-token mini-budget, capped at 5 total additions.
        if !focused {
            let dep_budget = 300usize;
            let mut dep_tokens_used = 0usize;
            let max_dep_additions = 5usize;
            let mut dep_added = 0usize;

            // Collect all currently included symbol names
            let included_ids: std::collections::HashSet<String> = pivots
                .iter()
                .map(|p| p.symbol.clone())
                .chain(context.iter().map(|c| c.symbol.clone()))
                .collect();

            // Only iterate dependencies of PIVOT nodes (high-confidence)
            let pivot_node_ids: Vec<SymbolId> = pivots
                .iter()
                .filter_map(|p| {
                    self.graph
                        .all_nodes()
                        .iter()
                        .find(|n| n.name == p.symbol && n.file == p.file)
                        .map(|n| n.id.clone())
                })
                .collect();

            for node_id in &pivot_node_ids {
                if dep_added >= max_dep_additions {
                    break;
                }
                // Sort dependencies by name for deterministic iteration order.
                // petgraph's neighbors_directed() order isn't guaranteed stable.
                let mut deps = self.graph.get_dependencies(node_id);
                deps.sort_by(|a, b| a.0.name.cmp(&b.0.name));

                for (dep_node, edge_kind) in &deps {
                    if dep_added >= max_dep_additions {
                        break;
                    }
                    // Only follow Calls edges (not Imports, TypeRef, etc.)
                    if !matches!(edge_kind, crate::graph::model::EdgeKind::Calls) {
                        continue;
                    }
                    if included_ids.contains(&dep_node.name) {
                        continue;
                    }
                    if is_lattice_own_source(&dep_node.file) || is_test_file(&dep_node.file) {
                        continue;
                    }
                    // Keyword coherence: only include dependencies that share query keywords.
                    // Without this, login → get_db and login → dispatch_webhook_event
                    // waste context slots despite zero topical relevance to the auth query.
                    if !has_keyword_coherence(
                        &dep_node.name,
                        &dep_node.signature,
                        &dep_node.file,
                        &scoring_q_words,
                    ) {
                        continue;
                    }
                    let sig_tokens = dep_node.signature.len() / CHARS_PER_TOKEN;
                    if dep_tokens_used + sig_tokens > dep_budget {
                        continue;
                    }
                    let caller_name = self
                        .graph
                        .get_node(node_id)
                        .map(|n| n.name.clone())
                        .unwrap_or_default();
                    context.push(ContextNode {
                        symbol: dep_node.name.clone(),
                        kind: dep_node.kind.short_code().to_string(),
                        file: dep_node.file.clone(),
                        line: dep_node.line,
                        skeleton: dep_node.signature.to_string(),
                        relationship: format!("called_by: {}", caller_name),
                        score: 0.08,
                    });
                    dep_tokens_used += sig_tokens;
                    dep_added += 1;
                }
            }

            tokens_used += dep_tokens_used;
        }

        // Calculate tokens saved
        let total_tokens_if_all: usize = candidates
            .iter()
            .map(|c| c.node.body.len() / CHARS_PER_TOKEN)
            .sum();
        let tokens_saved = total_tokens_if_all.saturating_sub(tokens_used);
        let nodes_included = pivots.len() + context.len();

        // Step 6: Retrieve relevant memories
        let memories = if let Some(ref ms) = self.memory_store {
            match ms.lock() {
                Ok(store) => store
                    .search_by_keyword(query_text)
                    .unwrap_or_default()
                    .into_iter()
                    .take(5)
                    .map(|m| {
                        serde_json::json!({
                            "content": m.content,
                            "type": m.memory_type.as_str(),
                        })
                    })
                    .collect(),
                Err(_) => vec![], // Mutex poisoned — skip memories gracefully
            }
        } else {
            vec![]
        };

        // Step 7: Assemble capsule
        ContextCapsule {
            query: query_text.to_string(),
            intent,
            pivots,
            context,
            memories,
            stats: CapsuleStats {
                tokens_used,
                tokens_saved,
                nodes_evaluated,
                nodes_included,
                engine_version: ENGINE_VERSION.to_string(),
                seed_count: seed_hits.len(),
                seed_symbols: seed_hits
                    .iter()
                    .take(15)
                    .map(|(id, _)| {
                        format!(
                            "{}:{}",
                            id.file.rsplit('/').next().unwrap_or(&id.file),
                            id.name
                        )
                    })
                    .collect(),
            },
        }
    }

    /// Get the underlying graph for direct operations.
    pub fn graph(&self) -> &CodeGraph {
        &self.graph
    }

    /// Get the underlying graph mutably for direct updates.
    pub fn graph_mut(&mut self) -> &mut CodeGraph {
        &mut self.graph
    }

    /// Get a reference to the vector store (if available).
    pub fn vector_store(&self) -> &Option<VectorStore> {
        &self.vector_store
    }

    /// Find a symbol by name (searches all nodes).
    pub fn find_symbol(&self, name: &str) -> Option<&GraphNode> {
        self.graph.all_nodes().into_iter().find(|n| n.name == name)
    }

    /// Find all symbols in a file.
    pub fn file_symbols(&self, file: &str) -> Vec<&GraphNode> {
        self.graph
            .all_nodes()
            .into_iter()
            .filter(|n| n.file == file)
            .collect()
    }

    /// Replace the code graph with a new one.
    pub fn update_graph(&mut self, graph: CodeGraph) {
        self.graph = graph;
    }

    /// Record a query for frequency tracking.
    pub fn record_query(&mut self, query: &str) {
        // Cap history size to prevent unbounded memory growth
        if self.query_history.len() >= 1000 {
            self.query_history.clear();
        }
        *self.query_history.entry(query.to_string()).or_insert(0) += 1;
    }

    /// Find seed hits using semantic search (vector store) or keyword fallback.
    fn find_seed_hits(
        &self,
        query_text: &str,
        embedding: Option<&[f32]>,
        params: &IntentParams,
    ) -> Vec<(SymbolId, f64)> {
        // Try semantic search first
        if let (Some(emb), Some(vs)) = (embedding, &self.vector_store) {
            if let Ok(results) = vs.search(emb, params.semantic_k) {
                let mut hits = Vec::new();
                for (name, file, byte_offset, similarity) in results {
                    let id = SymbolId {
                        file,
                        name,
                        byte_offset,
                    };
                    hits.push((id, similarity as f64));
                }
                if !hits.is_empty() {
                    return hits;
                }
            }
        }

        // Fallback: keyword matching on node names and signatures
        self.keyword_fallback(query_text, params.semantic_k)
    }

    /// Keyword-based fallback when no vector store or embedding is available.
    /// Matches query words against node names, signatures, and file paths.
    /// Uses IDF weighting so rare terms (e.g. "JWT") contribute more than
    /// common terms (e.g. "user") that appear in hundreds of symbols.
    fn keyword_fallback(&self, query_text: &str, top_k: usize) -> Vec<(SymbolId, f64)> {
        let query_lower = query_text.to_lowercase();
        let cleaned: String = query_lower
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '_' {
                    c
                } else {
                    ' '
                }
            })
            .collect();
        let query_words: Vec<&str> = cleaned
            .split_whitespace()
            .filter(|w| w.len() > 2 && !STOP_WORDS.contains(w))
            .collect();

        if query_words.is_empty() {
            return Vec::new();
        }

        // Compute IDF for each query word: how many symbols mention it?
        // Words appearing in many symbols (low IDF) contribute less to scores.
        let all_nodes = self.graph.all_nodes();
        let total_nodes = all_nodes.len().max(1) as f64;
        let word_idf: HashMap<&str, f64> = query_words
            .iter()
            .map(|w| {
                let df = all_nodes
                    .iter()
                    .filter(|n| {
                        let nl = n.name.to_lowercase();
                        nl.contains(*w) || n.signature.to_lowercase().contains(*w)
                    })
                    .count()
                    .max(1);
                (*w, (total_nodes / df as f64).ln().max(0.1))
            })
            .collect();
        let max_idf = word_idf.values().cloned().fold(0.1f64, f64::max);

        // Floor IDF at 30% of max. If a word is in the query, it's relevant by
        // definition — IDF should compress the range, not eliminate terms. Without
        // this, auth queries fail because "token"/"password"/"user" have near-zero
        // IDF in codebases where those words are ubiquitous.
        let idf_floor = max_idf * 0.3;
        let word_idf_floored: HashMap<&str, f64> = word_idf
            .iter()
            .map(|(w, idf)| (*w, idf.max(idf_floor)))
            .collect();
        let max_idf_floored = word_idf_floored.values().cloned().fold(0.1f64, f64::max);

        // Pre-compute total IDF (sum of all floored word IDFs) — constant across nodes
        let total_idf: f64 = query_words
            .iter()
            .map(|w| word_idf_floored.get(*w).copied().unwrap_or(1.0))
            .sum();

        let mut scored: Vec<(SymbolId, f64)> = Vec::new();

        for node in &all_nodes {
            // Skip Lattice's own source code — never relevant to user queries
            if is_lattice_own_source(&node.file) {
                continue;
            }

            let name_lower = node.name.to_lowercase();
            let sig_lower = node.signature.to_lowercase();
            let file_lower = node.file.to_lowercase();

            // Fast path: exact symbol name in query → high score.
            // IDF-weighted: exact match on a rare term scores higher than on a common one.
            if query_words.iter().any(|w| *w == name_lower) {
                let idf_factor = word_idf_floored
                    .get(name_lower.as_str())
                    .copied()
                    .unwrap_or(max_idf_floored)
                    / max_idf_floored;
                let exact_score = if query_words.len() >= 5 {
                    match node.kind {
                        SymbolKind::Variable | SymbolKind::Constant => 0.5 * idf_factor,
                        _ => idf_factor.max(0.5), // exact name match always strong
                    }
                } else {
                    1.0
                };
                scored.push((node.id.clone(), exact_score));
                continue;
            }

            // Split on original name (preserving camelCase boundaries).
            // split_identifier("GetSystemUsers") → ["get", "system", "users"]
            // Previously we passed name_lower which destroyed camelCase info,
            // producing ["getsystemusers"] — a single blob that matched "user"
            // via substring but missed that "users" ≠ "user".
            let name_parts = split_identifier(&node.name);
            // Pre-split file path into exact segments for matching
            let file_segments: Vec<&str> = file_lower
                .split(|c: char| c == '/' || c == '_' || c == '-' || c == '.')
                .filter(|p| p.len() >= 3)
                .collect();

            let mut idf_weighted_score: f64 = 0.0;
            let mut words_matched = 0usize;
            let mut has_file_match = false;

            for word in &query_words {
                let idf = word_idf_floored.get(*word).copied().unwrap_or(1.0);
                let idf_factor = idf / max_idf_floored; // 0..1 range, floored at 0.3
                let mut word_score: f64 = 0.0;

                // Name starts with query word (e.g. word="login", name="loginUser")
                if name_lower.starts_with(word) {
                    word_score = 0.8;
                }
                // Query word starts with name (e.g. word="authentication", name="auth")
                else if word.starts_with(&name_lower) && name_lower.len() >= 3 {
                    word_score = 0.6;
                }
                // Name contains query word at a word boundary
                else if name_lower.contains(word) {
                    let is_boundary = name_lower
                        .find(word)
                        .map(|pos| pos == 0 || name_lower.as_bytes().get(pos - 1) == Some(&b'_'))
                        .unwrap_or(false);
                    word_score = if is_boundary { 0.7 } else { 0.4 };
                }
                // Name part exactly matches query word or query word starts with name part.
                // Prefix match is valid: "auth" → "authentication". Substring match is not:
                // "info" ⊂ "verification" is coincidental, not semantic.
                // Require part.len() >= 4 for prefix matching to prevent "log"→"login"
                // (3 chars is too short — "log" ≠ "login" semantically). Exact matches
                // keep the >= 3 threshold since they're unambiguous.
                else if name_parts.iter().any(|part| {
                    (part.len() >= 3 && *word == part.as_str())
                        || (part.len() >= 4 && word.starts_with(part.as_str()))
                }) {
                    word_score = 0.5;
                }
                // Prefix match between name parts and query words
                else if name_parts.iter().any(|part| {
                    (part.len() >= 4 && word.starts_with(part.as_str()))
                        || (part.len() >= 3 && part.starts_with(word))
                }) {
                    word_score = 0.4;
                }
                // Signature-only match
                else if sig_lower.contains(word) {
                    word_score = 0.2;
                }

                // File path match — exact segment matching only.
                // "user" matches "user" segment but NOT "users" or "users_linux".
                if file_segments.iter().any(|seg| *seg == *word) {
                    has_file_match = true;
                    if word_score < 0.4 {
                        word_score = 0.4;
                    }
                }

                if word_score > 0.0 {
                    words_matched += 1;
                    // Weight this word's contribution by its IDF
                    idf_weighted_score += word_score * idf_factor;
                }
            }

            if words_matched == 0 {
                continue;
            }

            // For queries with 3+ words, require at least 2 matched words.
            // Single-word matches on common terms are almost always noise.
            if query_words.len() >= 3 && words_matched < 2 {
                continue;
            }

            // Score: IDF-weighted sum normalized by word count, with coverage scaling.
            // This naturally suppresses nodes matching only common terms while boosting
            // those matching rare terms or multiple terms.
            let coverage = words_matched as f64 / query_words.len() as f64;
            let file_bonus = if has_file_match { 0.1 } else { 0.0 };
            let max_possible_score = total_idf / max_idf_floored; // max if all words matched at 1.0
            let mut normalized = if max_possible_score > 0.0 {
                let raw = idf_weighted_score / max_possible_score.min(query_words.len() as f64);
                (raw * (0.5 + 0.5 * coverage) + file_bonus).min(1.0)
            } else {
                0.0
            };

            // Penalty for single-match in multi-word queries.
            // Scale by query length: longer queries are more specific, so a single
            // word match is increasingly likely to be noise.
            // 2 words: × 0.35 (AuthenticationMiddleware survives)
            // 5 words: × 0.15 (very unlikely to be relevant)
            // 8+ words: × 0.05 (essentially excluded)
            if query_words.len() >= 2 && words_matched == 1 {
                let penalty = (0.50 / query_words.len() as f64).max(0.05);
                normalized *= penalty;
            }

            if normalized > 0.05 {
                scored.push((node.id.clone(), normalized));
            }
        }

        // Sort by score descending, then by name ascending for deterministic ordering.
        // Without a tiebreaker, symbols at the same score flap in/out across runs.
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.name.cmp(&b.0.name))
        });
        scored.truncate(top_k);

        // Cap test file seeds at 3 total. Test functions match query keywords
        // (test_authenticate_user matches "authenticate" + "user") but provide
        // test logic, not implementation. Without a cap, 11 of 15 seeds can be
        // tests from one file, starving real implementation files of seed slots.
        {
            let max_test_seeds = 3;
            let test_count = scored
                .iter()
                .filter(|(id, _)| is_test_file(&id.file))
                .count();
            if test_count > max_test_seeds {
                // Collect non-test overflow candidates from the full scored list
                let mut overflow_candidates: Vec<(SymbolId, f64)> = Vec::new();
                for node in &all_nodes {
                    if is_test_file(&node.file) || is_lattice_own_source(&node.file) {
                        continue;
                    }
                    // Check if already in scored
                    if scored.iter().any(|(id, _)| *id == node.id) {
                        continue;
                    }
                    let name_lower = node.name.to_lowercase();
                    let name_parts = split_identifier(&name_lower);
                    let mut idf_ws: f64 = 0.0;
                    let mut wm = 0usize;
                    for word in &query_words {
                        let idf_f =
                            word_idf_floored.get(*word).copied().unwrap_or(1.0) / max_idf_floored;
                        let mut ws: f64 = 0.0;
                        if name_lower.starts_with(word) {
                            ws = 0.8;
                        } else if name_lower.contains(word) {
                            ws = 0.4;
                        } else if name_parts.iter().any(|p| {
                            (p.len() >= 3 && *word == p.as_str())
                                || (p.len() >= 4 && word.starts_with(p.as_str()))
                        }) {
                            ws = 0.5;
                        }
                        if ws > 0.0 {
                            wm += 1;
                            idf_ws += ws * idf_f;
                        }
                    }
                    if wm == 0 {
                        continue;
                    }
                    let cov = wm as f64 / query_words.len() as f64;
                    let max_possible = total_idf / max_idf_floored;
                    let norm = if max_possible > 0.0 {
                        (idf_ws / max_possible.min(query_words.len() as f64) * (0.5 + 0.5 * cov))
                            .min(1.0)
                    } else {
                        0.0
                    };
                    if norm > 0.05 {
                        overflow_candidates.push((node.id.clone(), norm));
                    }
                }
                overflow_candidates
                    .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

                // Remove excess test seeds (keep highest-scoring 3), replace with overflow
                let mut test_kept = 0;
                let mut to_remove = Vec::new();
                for (i, (id, _)) in scored.iter().enumerate() {
                    if is_test_file(&id.file) {
                        test_kept += 1;
                        if test_kept > max_test_seeds {
                            to_remove.push(i);
                        }
                    }
                }
                // Replace excess test seeds with overflow candidates
                let mut oc_idx = 0;
                for &idx in &to_remove {
                    if oc_idx < overflow_candidates.len() {
                        scored[idx] = overflow_candidates[oc_idx].clone();
                        oc_idx += 1;
                    }
                }
                // Re-sort after replacements
                scored.sort_by(|a, b| {
                    b.1.partial_cmp(&a.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.0.name.cmp(&b.0.name))
                });
                scored.truncate(top_k);
            }
        }

        // File diversity: ensure at least one representative from each file
        // that scored well but got crowded out of top_k. Uses same IDF weighting
        // and exact segment matching as the main scoring loop.
        if scored.len() == top_k {
            let selected_files: std::collections::HashSet<&str> =
                scored.iter().map(|(id, _)| id.file.as_str()).collect();

            let mut unrepresented: Vec<(SymbolId, f64)> = Vec::new();
            for node in &all_nodes {
                if selected_files.contains(node.file.as_str()) || is_lattice_own_source(&node.file)
                {
                    continue;
                }
                let name_lower = node.name.to_lowercase();
                let file_lower = node.file.to_lowercase();
                // Exact segment matching for file relevance
                let file_segs: Vec<&str> = file_lower
                    .split(|c: char| c == '/' || c == '_' || c == '-' || c == '.')
                    .filter(|p| p.len() >= 3)
                    .collect();
                let file_relevant = query_words
                    .iter()
                    .any(|w| file_segs.iter().any(|seg| *seg == *w));
                if !file_relevant {
                    continue;
                }
                // IDF-weighted scoring (same logic as main loop)
                let mut idf_ws: f64 = 0.0;
                let mut wm = 0usize;
                let name_parts = split_identifier(&name_lower);
                for word in &query_words {
                    let idf_f =
                        word_idf_floored.get(*word).copied().unwrap_or(1.0) / max_idf_floored;
                    let mut ws: f64 = 0.0;
                    if name_lower.starts_with(word) {
                        ws = 0.8;
                    } else if word.starts_with(&name_lower) && name_lower.len() >= 3 {
                        ws = 0.6;
                    } else if name_lower.contains(word) {
                        ws = 0.4;
                    } else if name_parts.iter().any(|p| {
                        (p.len() >= 3 && *word == p.as_str())
                            || (p.len() >= 4 && word.starts_with(p.as_str()))
                            || (p.len() >= 3 && p.starts_with(word))
                    }) {
                        ws = 0.5;
                    }
                    // File path: exact segment match only
                    if file_segs.iter().any(|seg| *seg == *word) {
                        ws = ws.max(0.4);
                    }
                    if ws > 0.0 {
                        wm += 1;
                        idf_ws += ws * idf_f;
                    }
                }
                if wm == 0 {
                    continue;
                }
                if query_words.len() >= 3 && wm < 2 {
                    continue;
                }
                let cov = wm as f64 / query_words.len() as f64;
                let max_possible = total_idf / max_idf_floored;
                let norm = if max_possible > 0.0 {
                    let raw = idf_ws / max_possible.min(query_words.len() as f64);
                    (raw * (0.5 + 0.5 * cov) + 0.1).min(1.0)
                } else {
                    0.0
                };
                if norm > 0.2 {
                    unrepresented.push((node.id.clone(), norm));
                }
            }

            // Group by file, take best per file
            let mut best_per_file: HashMap<String, (SymbolId, f64)> = HashMap::new();
            for (id, score) in unrepresented {
                let file = id.file.clone();
                let entry = best_per_file
                    .entry(file)
                    .or_insert_with(|| (id.clone(), 0.0));
                if score > entry.1 {
                    *entry = (id, score);
                }
            }

            // Swap in up to 5 unrepresented file representatives for the lowest seed hits
            let mut swaps: Vec<(SymbolId, f64)> = best_per_file.into_values().collect();
            swaps.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.0.file.cmp(&b.0.file))
            });
            let max_swaps = 5.min(swaps.len());
            for i in 0..max_swaps {
                let swap_idx = scored.len() - 1 - i;
                if swaps[i].1 > scored[swap_idx].1 * 0.6 {
                    scored[swap_idx] = swaps[i].clone();
                }
            }

            scored.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.0.name.cmp(&b.0.name))
            });
        }

        scored
    }
}

/// Parsed query filters extracted from prefix tokens like `repo:name`, `file:pattern`, `lang:language`.
#[derive(Default, Debug, Clone)]
pub struct QueryFilter {
    pub repo: Option<String>,
    pub file_pattern: Option<String>,
    pub language: Option<String>,
}

impl QueryFilter {
    /// Returns true if the given graph node passes all active filters.
    pub fn matches(&self, node: &GraphNode) -> bool {
        if let Some(ref repo) = self.repo {
            // Match repo name against the beginning of the file path
            if !node.file.starts_with(repo) && !node.file.contains(&format!("/{}/", repo)) {
                return false;
            }
        }
        if let Some(ref pattern) = self.file_pattern {
            if !node.file.contains(pattern) {
                return false;
            }
        }
        if let Some(ref lang) = self.language {
            let lang_lower = lang.to_lowercase();
            let node_lang = format!("{:?}", node.language).to_lowercase();
            if node_lang != lang_lower {
                return false;
            }
        }
        true
    }
}

/// Parse query filters like `repo:frontend how does auth work?`
/// Returns (filter, clean_query) where clean_query has the filter tokens removed.
pub fn parse_query_filters(query: &str) -> (QueryFilter, String) {
    let mut filter = QueryFilter::default();
    let mut clean_parts = Vec::new();

    for word in query.split_whitespace() {
        if let Some(repo) = word.strip_prefix("repo:") {
            filter.repo = Some(repo.to_string());
        } else if let Some(file) = word.strip_prefix("file:") {
            filter.file_pattern = Some(file.to_string());
        } else if let Some(lang) = word.strip_prefix("lang:") {
            filter.language = Some(lang.to_string());
        } else {
            clean_parts.push(word);
        }
    }

    (filter, clean_parts.join(" "))
}

/// Check if a symbol has keyword overlap with query words.
///
/// Uses word-boundary matching (split_identifier parts) instead of substring
/// matching to prevent false positives like "dispatch" containing "patch".
/// Also checks signature (substring OK there — it's structured) and file path
/// segments.
///
/// Rules for name part matching:
/// - Exact match: part.len() >= 3 (e.g., "auth" == "auth")
/// - Query word starts with part: part.len() >= 4 (e.g., "authentication".starts_with("auth"))
/// - Part starts with query word: part.len() >= 3 (e.g., "authenticate".starts_with("auth"))
fn has_keyword_coherence(name: &str, signature: &str, file: &str, query_words: &[&str]) -> bool {
    let name_parts = split_identifier(name);
    let sig_lower = signature.to_lowercase();
    let file_lower = file.to_lowercase();

    query_words.iter().any(|w| {
        sig_lower.contains(w)
            || file_lower
                .split(|c: char| c == '/' || c == '_' || c == '-' || c == '.')
                .any(|seg| seg == *w || (seg.len() >= 4 && w.starts_with(seg)))
            || name_parts.iter().any(|p| {
                (p.len() >= 3 && p.as_str() == *w)
                    || (p.len() >= 4 && w.starts_with(p.as_str()))
                    || (p.len() >= 3 && p.starts_with(w))
            })
    })
}

/// Count the number of strong (non-stop-word, >= 3 chars) name parts that
/// don't match any query word. Used to detect wrong-subsystem symbols:
/// "verify_agent_flexible" has 2 unmatched ("agent", "flexible") while
/// "get_password_hash" has only 1 ("hash").
fn unmatched_strong_parts(name: &str, query_words: &[&str]) -> usize {
    let name_parts = split_identifier(name);
    let mut count = 0usize;

    for part in &name_parts {
        if part.len() < 3 {
            continue; // Too short to carry meaning
        }
        if NAME_STOP_WORDS.contains(&part.as_str()) {
            continue; // Generic verb/preposition/qualifier
        }
        // Check if this name part matches any query word
        let matches_query = query_words.iter().any(|w| {
            (part.len() >= 3 && part.as_str() == *w)
                || (part.len() >= 4 && w.starts_with(part.as_str()))
                || (part.len() >= 3 && part.starts_with(w))
        });
        if !matches_query {
            count += 1;
        }
    }

    count
}

/// Format an edge kind for display in dependency direction (outgoing).
/// e.g., Calls -> "calls", Imports -> "imports"
fn format_edge_kind(kind: EdgeKind) -> String {
    match kind {
        EdgeKind::Calls => "calls".to_string(),
        EdgeKind::Imports => "imports".to_string(),
        EdgeKind::Implements => "implements".to_string(),
        EdgeKind::Extends => "extends".to_string(),
        EdgeKind::TypeRef => "type_ref_of".to_string(),
        EdgeKind::Contains => "contains".to_string(),
        EdgeKind::LinksTo => "links_to".to_string(),
        EdgeKind::Mentions => "mentions".to_string(),
        EdgeKind::CoChanges => "co_changes_with".to_string(),
    }
}

/// Format an edge kind for display in dependent direction (incoming / reverse).
/// e.g., Calls -> "called_by", Imports -> "imported_by"
fn format_edge_kind_reverse(kind: EdgeKind) -> String {
    match kind {
        EdgeKind::Calls => "called_by".to_string(),
        EdgeKind::Imports => "imported_by".to_string(),
        EdgeKind::Implements => "implemented_by".to_string(),
        EdgeKind::Extends => "extended_by".to_string(),
        EdgeKind::TypeRef => "type_referenced_by".to_string(),
        EdgeKind::Contains => "contained_in".to_string(),
        EdgeKind::LinksTo => "linked_from".to_string(),
        EdgeKind::Mentions => "mentioned_by".to_string(),
        EdgeKind::CoChanges => "co_changes_with".to_string(),
    }
}

/// Classify the relationship of a node based on its semantic similarity.
fn classify_relationship(semantic_sim: f64) -> String {
    if semantic_sim > 0.8 {
        "direct match".to_string()
    } else if semantic_sim > 0.5 {
        "closely related".to_string()
    } else if semantic_sim > 0.2 {
        "related via graph".to_string()
    } else {
        "transitive dependency".to_string()
    }
}

// classify_why was replaced by the detailed score/path-based why_included format in Fix 18.

/// Extract the directory portion of a file path for co-directory checks.
/// e.g., "src/auth/helpers.py" → "src/auth", "agent/tunnels/tunnel.go" → "agent/tunnels"
fn file_directory(file_path: &str) -> String {
    std::path::Path::new(file_path)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("")
        .to_string()
}

/// Detect if a file path belongs to the Lattice daemon's own source code.
/// These should be excluded from query results when the user is querying
/// a different project's codebase — the daemon's own symbols are never relevant.
fn is_lattice_own_source(file_path: &str) -> bool {
    let lower = file_path.to_lowercase();
    lower.contains("/lattice-core/")
        || lower.contains("/lattice-daemon/")
        || lower.contains("/lattice/daemon/")
        || lower.contains("/lattice/extension/")
}

/// Detect if a file path is a test file based on common naming conventions.
/// Covers Python (test_*, *_test.py, conftest.py), JS/TS (*.test.*, *.spec.*),
/// Go (*_test.go), and common test directories (tests/, __tests__/, test/).
fn is_test_file(file_path: &str) -> bool {
    let lower = file_path.to_lowercase();
    let filename = std::path::Path::new(&lower)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("");

    filename.starts_with("test_")
        || filename.starts_with("test.")
        || filename.contains("_test.")
        || filename.contains(".test.")
        || filename.contains(".spec.")
        || filename == "conftest.py"
        || lower.contains("/tests/")
        || lower.contains("/__tests__/")
}

/// Detect if a file path is a database migration file (Alembic, Django, Sequelize, etc.).
/// Migration files contain generic upgrade()/downgrade() functions whose names match
/// any query but whose content is DDL, not application logic.
fn is_migration_file(file_path: &str) -> bool {
    let lower = file_path.to_lowercase();
    // Alembic: versions/042_some_name.py (digits followed by underscore)
    // Django: migrations/0001_initial.py
    // Common migration directories
    lower.contains("/versions/")
        || lower.contains("/migrations/")
        || lower.contains("/alembic/")
        || lower.contains("/migrate/")
}

/// Split an identifier into constituent words (handles snake_case and camelCase).
/// e.g., "authenticate_user" → ["authenticate", "user"]
///       "getUserAuth" → ["get", "user", "auth"]
///       "SystemCollector.GetSystemInfo" → ["system", "collector", "get", "system", "info"]
fn split_identifier(name: &str) -> Vec<String> {
    let mut parts = Vec::new();
    // Split on underscores and dots (Go methods use Class.Method notation)
    for segment in name.split(|c: char| c == '_' || c == '.') {
        if segment.is_empty() {
            continue;
        }
        // Then split camelCase
        let mut current = String::new();
        for ch in segment.chars() {
            if ch.is_uppercase() && !current.is_empty() {
                parts.push(current.to_lowercase());
                current = String::new();
            }
            current.push(ch);
        }
        if !current.is_empty() {
            parts.push(current.to_lowercase());
        }
    }
    parts
}

/// Compute the fraction of non-empty lines shared between two source strings.
/// Used for near-duplicate detection (e.g., copy-pasted handlers in different files).
/// Returns 0.0..1.0 where 1.0 means identical content.
fn line_overlap_ratio(a: &str, b: &str) -> f64 {
    let lines_a: std::collections::HashSet<&str> = a
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    let lines_b: std::collections::HashSet<&str> = b
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if lines_a.is_empty() || lines_b.is_empty() {
        return 0.0;
    }
    let intersection = lines_a.intersection(&lines_b).count();
    let smaller = lines_a.len().min(lines_b.len());
    intersection as f64 / smaller as f64
}

/// Detect if a symbol's body is primarily field/type declarations (schema-heavy)
/// vs implementation logic. Used to deprioritize data classes for Explore intent.
/// Returns true if < 15% of non-empty, non-comment lines contain logic indicators.
fn is_schema_heavy(body: &str) -> bool {
    let lines: Vec<&str> = body
        .lines()
        .map(|l| l.trim())
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with('#')
                && !l.starts_with("//")
                && !l.starts_with("/*")
                && !l.starts_with('*')
                && *l != "}"
                && *l != "{"
                && *l != ")"
                && *l != "]"
        })
        .collect();
    if lines.len() < 3 {
        return false;
    }

    let logic_count = lines
        .iter()
        .filter(|l| {
            l.contains("if ")
                || l.contains("for ")
                || l.contains("while ")
                || l.contains("return ")
                || l.contains("await ")
                || l.contains("yield ")
                || l.contains("raise ")
                || l.contains("throw ")
                || l.contains("match ")
                || l.contains("else {")
                || l.contains("else:")
                || (l.contains('(')
                    && !l.starts_with("class ")
                    && !l.starts_with("def ")
                    && !l.starts_with("fn ")
                    && !l.starts_with("func ")
                    && !l.starts_with("pub fn")
                    && !l.starts_with("pub(")
                    && !l.starts_with("type ")
                    && !l.starts_with("interface ")
                    && !l.starts_with("struct ")
                    && !l.starts_with("enum "))
        })
        .count();

    let logic_ratio = logic_count as f64 / lines.len() as f64;
    logic_ratio < 0.15
}

/// Common English stop words filtered from keyword queries to avoid noisy matches.
const STOP_WORDS: &[&str] = &[
    "how", "does", "what", "where", "when", "why", "which", "who", "the", "this", "that", "these",
    "those", "with", "from", "into", "for", "and", "but", "not", "are", "was", "were", "been",
    "being", "have", "has", "had", "will", "would", "could", "should", "can", "may", "might",
    "shall", "must", "need", "use", "used", "using", "work", "works", "working", "make", "made",
    "get", "set", "all", "any", "each", "every", "some", "about", "also", "then", "than", "very",
    "just", "only", "more", "most", "other", "new", "old",
];
