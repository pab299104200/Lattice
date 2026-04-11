pub mod manager;
#[cfg(test)]
mod tests;
pub use manager::{repo_rel_path, CrossRepoEdge, RepoStats, WorkspaceManager};
