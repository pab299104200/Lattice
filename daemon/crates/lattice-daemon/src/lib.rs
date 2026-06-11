#![recursion_limit = "256"]

pub(crate) mod lifecycle_log;
pub(crate) mod repo_state;
pub mod rpc;
pub(crate) mod runtime_support;
#[allow(dead_code)]
pub(crate) mod watcher_health;
// The lib target exposes RPC for tests; the binary uses the incremental sync entry points.
#[allow(dead_code)]
pub mod vector_sync;

use std::path::{Path, PathBuf};

pub(crate) fn repo_name_for_root(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("default")
        .to_string()
}

pub(crate) fn prioritize_indexable_paths(root: &Path, files: &mut [PathBuf]) {
    files.sort_by(|a, b| {
        let a_rel = repo_relative_path(root, a);
        let b_rel = repo_relative_path(root, b);
        indexing_priority(&a_rel)
            .cmp(&indexing_priority(&b_rel))
            .then_with(|| a_rel.cmp(&b_rel))
    });
}

fn repo_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn indexing_priority(rel_path: &str) -> (u8, u8) {
    let normalized = rel_path.replace('\\', "/");
    let file_name = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
    let lower = normalized.to_ascii_lowercase();
    let is_markdown = lower.ends_with(".md");
    let is_doc_dir = lower.starts_with("docs/");
    let is_repo_guide = matches!(
        file_name,
        "README.md" | "CLAUDE.md" | "AGENTS.md" | "CONTRIBUTING.md"
    );

    if is_repo_guide || (is_markdown && is_doc_dir) {
        (0, 0)
    } else if is_markdown {
        (0, 1)
    } else if lower.ends_with(".py") || lower.ends_with(".pyi") {
        (1, 0)
    } else {
        (2, 0)
    }
}
