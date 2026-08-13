//! Resolves the distinct identities a Git worktree needs at runtime.
//!
//! `checkout_root` is always the content boundary. `repository_id` and
//! `repository_lattice_dir` are shared only by checkouts of the same Git
//! common directory, so repository memory does not fragment by worktree.

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceIdentity {
    /// The requested checkout root. File reads, indexing, graphs, and watcher
    /// state remain rooted here even when repository memory is shared.
    pub(crate) checkout_root: PathBuf,
    /// Stable Git common-directory identity, or checkout root for non-Git
    /// workspaces. This is the only workspace id used for memory scope.
    pub(crate) repository_id: String,
    /// Stable opaque identity for checkout-owned derived state. It is never a
    /// repository-memory authority.
    pub(crate) checkout_id: String,
    /// The primary checkout associated with the Git common directory.
    pub(crate) repository_root: PathBuf,
    pub(crate) repository_lattice_dir: PathBuf,
}

impl WorkspaceIdentity {
    pub(crate) fn resolve(checkout_root: &Path) -> Result<Self> {
        let checkout_root = checkout_root.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize checkout root `{}`",
                checkout_root.display()
            )
        })?;

        let Some((common_git_dir, primary_checkout)) = git_identity(&checkout_root)? else {
            return Ok(Self::standalone(checkout_root));
        };
        let repository_id = encoded_id("repo", common_git_dir.as_os_str().as_encoded_bytes());
        let checkout_id = encoded_checkout_id(&repository_id, &checkout_root);
        Ok(Self {
            checkout_root,
            repository_id,
            checkout_id,
            repository_lattice_dir: primary_checkout.join(".lattice"),
            repository_root: primary_checkout,
        })
    }

    pub(crate) fn standalone(checkout_root: PathBuf) -> Self {
        let repository_id = encoded_id("standalone", checkout_root.as_os_str().as_encoded_bytes());
        let checkout_id = encoded_checkout_id(&repository_id, &checkout_root);
        Self {
            repository_lattice_dir: checkout_root.join(".lattice"),
            repository_root: checkout_root.clone(),
            checkout_root,
            repository_id,
            checkout_id,
        }
    }

    pub(crate) fn memories_path(&self) -> PathBuf {
        self.repository_lattice_dir.join("memories.db")
    }

    pub(crate) fn checkout_lattice_dir(&self) -> PathBuf {
        self.repository_lattice_dir
            .join("checkouts")
            .join(&self.checkout_id)
    }

    pub(crate) fn parsed_cache_path(&self) -> PathBuf {
        self.repository_lattice_dir.join("parsed-cache.db")
    }
}

fn git_identity(checkout_root: &Path) -> Result<Option<(PathBuf, PathBuf)>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(checkout_root)
        .args(["rev-parse", "--git-common-dir", "--show-toplevel"])
        .output()
        .with_context(|| format!("failed to execute git for `{}`", checkout_root.display()))?;
    if !output.status.success() {
        if checkout_root.join(".git").exists() {
            return Err(anyhow!(
                "Git metadata for checkout `{}` is invalid or inaccessible: {}",
                checkout_root.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        return Ok(None);
    }
    let output =
        String::from_utf8(output.stdout).context("git returned a non-UTF-8 repository identity")?;
    let mut lines = output.lines();
    let raw_common_dir = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or_else(|| anyhow!("git did not return a common directory"))?;
    let raw_toplevel = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or_else(|| anyhow!("git did not return a checkout root"))?;
    let common_dir = absolute_from_checkout(checkout_root, raw_common_dir)?;
    let toplevel = absolute_from_checkout(checkout_root, raw_toplevel)?;

    // In a normal linked-worktree repository, the common git directory is
    // `<primary>/.git`; its parent is the only safe canonical home for the
    // repository-owned Lattice directory. Bare or unusual layouts have no
    // `.git` leaf, so retain Git's concrete checkout root instead of guessing.
    let primary_checkout = if common_dir.file_name().is_some_and(|name| name == ".git") {
        common_dir
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("Git common directory has no repository parent"))?
    } else {
        toplevel
    };
    Ok(Some((common_dir, primary_checkout)))
}

fn encoded_checkout_id(repository_id: &str, checkout_root: &Path) -> String {
    let mut input = repository_id.as_bytes().to_vec();
    input.push(0);
    input.extend_from_slice(checkout_root.as_os_str().as_encoded_bytes());
    encoded_id("checkout", &input)
}

fn encoded_id(kind: &str, bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{kind}_{hex}")
}

fn absolute_from_checkout(checkout_root: &Path, raw: &str) -> Result<PathBuf> {
    let path = PathBuf::from(raw);
    let absolute = if path.is_absolute() {
        path
    } else {
        checkout_root.join(path)
    };
    absolute.canonicalize().with_context(|| {
        format!(
            "Git reported inaccessible repository path `{}` from checkout `{}`",
            raw,
            checkout_root.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::WorkspaceIdentity;
    use crate::runtime_support::{
        build_incremental_index_for_roots_with_cache, ParsedCacheRuntime,
    };
    use lattice_core::memory::{
        Memory, MemoryScope, MemoryStore, MemoryType, MemoryVerificationStatus,
    };
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("lattice-{name}-{nonce}"))
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("git executable must be available for worktree fixture");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn real_worktree_shares_repository_memory_identity_but_keeps_checkout_root() {
        let root = unique_temp_dir("worktree-identity");
        let worktree = root.with_extension("worktree");
        fs::create_dir_all(&root).expect("create fixture repository");
        git(&root, &["init"]);
        git(&root, &["config", "user.email", "lattice@example.test"]);
        git(&root, &["config", "user.name", "Lattice Test"]);
        for index in 0..10 {
            fs::write(
                root.join(format!("fixture_{index}.rs")),
                format!("pub fn shared_{index}() {{}}\n"),
            )
            .expect("write fixture source");
        }
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "fixture"]);
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "fixture-worktree",
                worktree.to_str().expect("utf8 worktree path"),
            ],
        );

        let primary = WorkspaceIdentity::resolve(&root).expect("resolve primary checkout");
        let linked = WorkspaceIdentity::resolve(&worktree).expect("resolve linked worktree");
        assert_eq!(primary.repository_id, linked.repository_id);
        assert_ne!(primary.checkout_id, linked.checkout_id);
        assert_eq!(
            primary.repository_lattice_dir,
            linked.repository_lattice_dir
        );
        assert_ne!(primary.checkout_root, linked.checkout_root);
        assert_eq!(linked.repository_root, primary.checkout_root);
        assert_ne!(
            primary.checkout_lattice_dir(),
            linked.checkout_lattice_dir()
        );
        assert_eq!(primary.parsed_cache_path(), linked.parsed_cache_path());

        fs::create_dir_all(&primary.repository_lattice_dir).expect("create shared Lattice store");
        let primary_store =
            MemoryStore::open(&primary.memories_path()).expect("open primary memory store");
        primary_store
            .store(Memory {
                id: String::new(),
                session_id: "fixture".to_string(),
                content: "worktree memory is repository scoped".to_string(),
                memory_type: MemoryType::Observation,
                scope: MemoryScope::Repo,
                confidence: 1.0,
                linked_symbols: vec![],
                linked_files: vec![],
                workspace_id: Some(primary.repository_id.clone()),
                branch: None,
                scope_organization_id: None,
                refresh_key: None,
                source_query: None,
                created_at: 0,
                last_accessed: 0,
                access_count: 0,
                is_stale: false,
                stale_reason: None,
                verification_status: MemoryVerificationStatus::Unverified,
            })
            .expect("store repository memory");
        let linked_store =
            MemoryStore::open(&linked.memories_path()).expect("open linked memory store");
        assert_eq!(
            linked_store.list_all().expect("read shared memories").len(),
            1
        );
        linked_store
            .store(Memory {
                id: String::new(),
                session_id: "linked".to_string(),
                content: "linked checkout writes are visible to primary".to_string(),
                memory_type: MemoryType::Observation,
                scope: MemoryScope::Repo,
                confidence: 1.0,
                linked_symbols: vec![],
                linked_files: vec![],
                workspace_id: Some(linked.repository_id.clone()),
                branch: None,
                scope_organization_id: None,
                refresh_key: None,
                source_query: None,
                created_at: 0,
                last_accessed: 0,
                access_count: 0,
                is_stale: false,
                stale_reason: None,
                verification_status: MemoryVerificationStatus::Unverified,
            })
            .expect("write linked memory");
        assert_eq!(
            primary_store.list_all().expect("read both memories").len(),
            2
        );
        let mut memory_writers = Vec::new();
        for writer in 0..2 {
            let path = primary.memories_path();
            let repository_id = primary.repository_id.clone();
            memory_writers.push(std::thread::spawn(move || {
                let store = MemoryStore::open(&path).unwrap();
                for index in 0..5 {
                    store
                        .store(Memory {
                            id: String::new(),
                            session_id: format!("writer-{writer}"),
                            content: format!("concurrent memory {writer}-{index}"),
                            memory_type: MemoryType::Observation,
                            scope: MemoryScope::Repo,
                            confidence: 1.0,
                            linked_symbols: vec![],
                            linked_files: vec![],
                            workspace_id: Some(repository_id.clone()),
                            branch: None,
                            scope_organization_id: None,
                            refresh_key: None,
                            source_query: None,
                            created_at: 0,
                            last_accessed: 0,
                            access_count: 0,
                            is_stale: false,
                            stale_reason: None,
                            verification_status: MemoryVerificationStatus::Unverified,
                        })
                        .unwrap();
                }
            }));
        }
        for writer in memory_writers {
            writer.join().unwrap();
        }
        assert_eq!(primary_store.list_all().unwrap().len(), 12);

        let cache = ParsedCacheRuntime::open(&primary.parsed_cache_path()).unwrap();
        let primary_index = build_incremental_index_for_roots_with_cache(
            &[root.clone()],
            None,
            Default::default(),
            &cache,
        );
        assert_eq!(primary_index.parsed_cache.writes, 10);
        let linked_index = build_incremental_index_for_roots_with_cache(
            &[worktree.clone()],
            None,
            Default::default(),
            &cache,
        );
        assert_eq!(linked_index.parsed_cache.hits, 10);
        assert!(
            linked_index.parsed_cache.hits * 100 / linked_index.index_report.requested_count as u64
                >= 90
        );
        let full = build_incremental_index_for_roots_with_cache(
            &[worktree.clone()],
            None,
            Default::default(),
            &ParsedCacheRuntime::in_memory(),
        );
        assert_eq!(graph_facts(&linked_index.graph), graph_facts(&full.graph));

        fs::create_dir_all(primary.checkout_lattice_dir()).unwrap();
        fs::create_dir_all(linked.checkout_lattice_dir()).unwrap();
        let primary_graph_path = primary.checkout_lattice_dir().join("graph.db");
        let linked_graph_path = linked.checkout_lattice_dir().join("graph.db");
        assert_ne!(primary_graph_path, linked_graph_path);
        let primary_graph = lattice_core::storage::GraphStore::open(&primary_graph_path).unwrap();
        primary_graph.save_graph(&primary_index.graph).unwrap();
        fs::write(worktree.join("fixture_0.rs"), "pub fn linked_only() {}\n").unwrap();
        let divergent = build_incremental_index_for_roots_with_cache(
            &[worktree.clone()],
            None,
            Default::default(),
            &cache,
        );
        let linked_graph = lattice_core::storage::GraphStore::open(&linked_graph_path).unwrap();
        linked_graph.save_graph(&divergent.graph).unwrap();
        assert!(primary_graph
            .load_graph()
            .unwrap()
            .all_nodes()
            .iter()
            .any(|node| node.name == "shared_0"));
        assert!(!linked_graph
            .load_graph()
            .unwrap()
            .all_nodes()
            .iter()
            .any(|node| node.name == "shared_0"));

        let _ = fs::remove_dir_all(&worktree);
        let _ = fs::remove_dir_all(&root);
    }

    fn graph_facts(graph: &lattice_core::graph::CodeGraph) -> (Vec<String>, Vec<String>) {
        let mut nodes = graph
            .all_nodes()
            .into_iter()
            .map(|node| serde_json::to_string(node).unwrap())
            .collect::<Vec<_>>();
        let mut edges = graph
            .all_edges()
            .into_iter()
            .map(|(from, to, kind)| {
                format!(
                    "{}:{}:{}->{:?}->{}:{}:{}",
                    from.file,
                    from.name,
                    from.id.byte_offset,
                    kind,
                    to.file,
                    to.name,
                    to.id.byte_offset
                )
            })
            .collect::<Vec<_>>();
        nodes.sort();
        edges.sort();
        (nodes, edges)
    }

    #[test]
    fn non_git_identity_is_stable_and_checkout_namespaced() {
        let root = unique_temp_dir("standalone-identity");
        fs::create_dir_all(&root).unwrap();
        let first = WorkspaceIdentity::resolve(&root).unwrap();
        let second = WorkspaceIdentity::resolve(&root).unwrap();
        assert_eq!(first.repository_id, second.repository_id);
        assert_eq!(first.checkout_id, second.checkout_id);
        assert!(first.repository_id.starts_with("standalone_"));
        assert!(first.checkout_id.starts_with("checkout_"));
        assert!(first
            .checkout_lattice_dir()
            .starts_with(first.checkout_root.join(".lattice/checkouts")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_git_pointer_is_an_actionable_error() {
        let root = unique_temp_dir("malformed-gitdir");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join(".git"),
            "gitdir: /definitely/missing/lattice/gitdir\n",
        )
        .unwrap();
        let error = WorkspaceIdentity::resolve(&root).unwrap_err().to_string();
        assert!(error.contains("invalid or inaccessible"), "{error}");
        assert!(error.contains(root.to_string_lossy().as_ref()), "{error}");
        let _ = fs::remove_dir_all(root);
    }
}
