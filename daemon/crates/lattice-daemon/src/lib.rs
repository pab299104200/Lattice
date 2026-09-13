#![recursion_limit = "256"]

#[allow(dead_code)]
pub(crate) mod adoption_metrics;
pub(crate) mod disk_budget;
pub mod hook_session_binding;
pub(crate) mod hook_session_client;
pub mod hook_session_registry;
pub(crate) mod index_health;
#[allow(dead_code)]
pub(crate) mod index_work;
pub(crate) mod lifecycle_log;
pub(crate) mod memory_attribution;
pub(crate) mod memory_retention_runtime;
pub(crate) mod repo_state;
pub(crate) mod resource_budget;
pub mod rpc;
pub(crate) mod runtime_support;
pub(crate) mod storage_operator;
pub(crate) mod trusted_check_runner;
#[allow(dead_code)]
pub(crate) mod watcher_health;
pub(crate) mod workspace_identity;
pub(crate) mod worktree_base;
// The lib target exposes RPC for tests; the binary uses the incremental sync entry points.
#[allow(dead_code)]
pub mod vector_sync;

use std::path::Path;

pub(crate) fn repo_name_for_root(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("default")
        .to_string()
}
