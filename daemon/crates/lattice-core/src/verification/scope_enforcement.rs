//! Scope enforcement for durable memory retrieval.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 8. Verification Engine`:
//!
//! Verification checks:
//!
//! - linked files still exist
//! - linked symbols still exist
//! - cited docs still exist
//! - linked tests still exist
//! - evidence text still matches when exact spans were captured
//! - implementation still matches memory claim where deterministic checks are possible
//! - contradicted/superseded states remain coherent
//! - branch-scoped memory is not leaking into unrelated branches
//! - time-bound memory has expired
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Risks — Scope Leakage`:
//!
//! Risk: user/org/branch memories leak into wrong workspaces.
//!
//! Control: scope-aware queries, enforced filters in store APIs, and negative tests.

use crate::events::BranchRef;
use crate::identity::WorkspaceId;
use crate::memory::{Memory, MemoryScope};
use crate::verification::{VerificationStatus, VerificationVerdict};

pub type OrganizationId = String;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeFilter {
    pub workspace_id: WorkspaceId,
    pub branch: Option<BranchRef>,
    pub organization_id: Option<OrganizationId>,
    pub session_id: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScopeFilterError {
    #[error("scope filter requires a workspace id")]
    MissingWorkspaceId,
    #[error("scope filter has an empty branch name")]
    EmptyBranch,
    #[error("scope filter has an empty organization id")]
    EmptyOrganizationId,
    #[error("scope filter has an empty session id")]
    EmptySessionId,
}

impl ScopeFilter {
    pub fn new(
        workspace_id: impl Into<WorkspaceId>,
        branch: Option<BranchRef>,
        organization_id: Option<OrganizationId>,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            branch,
            organization_id,
            session_id: None,
        }
    }

    pub fn for_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn validate(&self) -> Result<(), ScopeFilterError> {
        if self.workspace_id.is_empty() {
            return Err(ScopeFilterError::MissingWorkspaceId);
        }
        if self
            .branch
            .as_ref()
            .is_some_and(|branch| branch.name.is_empty())
        {
            return Err(ScopeFilterError::EmptyBranch);
        }
        if self
            .organization_id
            .as_ref()
            .is_some_and(|organization_id| organization_id.is_empty())
        {
            return Err(ScopeFilterError::EmptyOrganizationId);
        }
        if self
            .session_id
            .as_ref()
            .is_some_and(|session_id| session_id.is_empty())
        {
            return Err(ScopeFilterError::EmptySessionId);
        }
        Ok(())
    }
}

pub fn allows(memory: &Memory, filter: &ScopeFilter) -> bool {
    if filter.validate().is_err() {
        return false;
    }

    match memory.scope {
        MemoryScope::Session => filter
            .session_id
            .as_ref()
            .is_some_and(|session_id| session_id == &memory.session_id),
        MemoryScope::Branch => {
            let Some(branch) = &filter.branch else {
                return false;
            };
            memory.workspace_id.as_ref() == Some(&filter.workspace_id)
                && memory.branch.as_ref() == Some(&branch.name)
        }
        MemoryScope::Repo => memory.workspace_id.as_ref() == Some(&filter.workspace_id),
        MemoryScope::Organization => {
            filter
                .organization_id
                .as_ref()
                .is_some_and(|organization_id| {
                    memory.scope_organization_id.as_ref() == Some(organization_id)
                })
        }
    }
}

pub struct ScopeEnforcement;

impl ScopeEnforcement {
    pub fn audit_memory(memory: &Memory, filter: &ScopeFilter) -> VerificationVerdict {
        if allows(memory, filter) {
            return VerificationVerdict::new(VerificationStatus::Verified, "ScopeHolds");
        }

        VerificationVerdict::new(VerificationStatus::Invalidated, "ScopeLeak")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryScopeFilteredEvent {
    pub memory_id: String,
    pub attempted_workspace_id: WorkspaceId,
    pub attempted_branch: Option<String>,
    pub memory_scope: MemoryScope,
}

impl MemoryScopeFilteredEvent {
    pub fn from_memory(memory: &Memory, filter: &ScopeFilter) -> Self {
        Self {
            memory_id: memory.id.clone(),
            attempted_workspace_id: filter.workspace_id.clone(),
            attempted_branch: filter.branch.as_ref().map(|branch| branch.name.clone()),
            memory_scope: memory.scope.clone(),
        }
    }
}
