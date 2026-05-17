use std::panic::{catch_unwind, AssertUnwindSafe};

use super::{ScopeEnforcement, ScopeFilter, VerificationStatus};
use crate::events::BranchRef;
use crate::memory::{Memory, MemoryScope, MemoryStore, MemoryType};

#[test]
fn branch_scope_memory_does_not_list_for_unrelated_branch() {
    let store = MemoryStore::open_in_memory().expect("memory store opens");
    let memory_id = store
        .store(scoped_memory(
            "branch-memory",
            MemoryScope::Branch,
            "session-a",
            Some("workspace-a"),
            Some("feat/foo"),
            None,
        ))
        .expect("branch memory stores");

    let visible = store
        .list_all_scoped(&filter("workspace-a", Some("feat/bar"), None, None))
        .expect("scoped list succeeds");

    assert!(visible.iter().all(|memory| memory.id != memory_id));
}

#[test]
fn repo_scope_memory_does_not_list_for_unrelated_workspace() {
    let store = MemoryStore::open_in_memory().expect("memory store opens");
    let memory_id = store
        .store(scoped_memory(
            "repo-memory",
            MemoryScope::Repo,
            "session-a",
            Some("workspace-a"),
            None,
            None,
        ))
        .expect("repo memory stores");

    let visible = store
        .list_all_scoped(&filter("workspace-b", Some("main"), None, None))
        .expect("scoped list succeeds");

    assert!(visible.iter().all(|memory| memory.id != memory_id));
}

#[test]
fn organization_scope_requires_matching_organization() {
    let store = MemoryStore::open_in_memory().expect("memory store opens");
    let memory_id = store
        .store(scoped_memory(
            "organization-memory",
            MemoryScope::Organization,
            "session-a",
            None,
            None,
            Some("org-a"),
        ))
        .expect("organization memory stores");

    let missing_org = store
        .list_all_scoped(&filter("workspace-a", Some("main"), None, None))
        .expect("missing organization filter succeeds");
    let different_org = store
        .list_all_scoped(&filter("workspace-a", Some("main"), Some("org-b"), None))
        .expect("different organization filter succeeds");

    assert!(missing_org.iter().all(|memory| memory.id != memory_id));
    assert!(different_org.iter().all(|memory| memory.id != memory_id));
}

#[test]
fn session_scope_memory_does_not_list_for_different_session() {
    let store = MemoryStore::open_in_memory().expect("memory store opens");
    let memory_id = store
        .store(scoped_memory(
            "session-memory",
            MemoryScope::Session,
            "session-a",
            Some("workspace-a"),
            Some("main"),
            None,
        ))
        .expect("session memory stores");

    let visible = store
        .list_all_scoped(&filter(
            "workspace-a",
            Some("main"),
            None,
            Some("session-b"),
        ))
        .expect("scoped list succeeds");

    assert!(visible.iter().all(|memory| memory.id != memory_id));
}

#[test]
fn audit_memory_invalidates_leaked_memory() {
    let memory = scoped_memory(
        "leaked-branch-memory",
        MemoryScope::Branch,
        "session-a",
        Some("workspace-a"),
        Some("feat/foo"),
        None,
    );

    let verdict = ScopeEnforcement::audit_memory(
        &memory,
        &filter("workspace-a", Some("feat/bar"), None, None),
    );

    assert_eq!(verdict.status, VerificationStatus::Invalidated);
    assert_eq!(verdict.reason, "ScopeLeak");
}

#[cfg(debug_assertions)]
#[test]
fn debug_guard_panics_when_leaked_row_reaches_boundary() {
    let store = MemoryStore::open_in_memory().expect("memory store opens");
    let leaked = scoped_memory(
        "leaked-repo-memory",
        MemoryScope::Repo,
        "session-a",
        Some("workspace-a"),
        None,
        None,
    );

    let result = catch_unwind(AssertUnwindSafe(|| {
        store
            .enforce_scope_boundary(
                vec![leaked],
                &filter("workspace-b", Some("main"), None, None),
                "test bypass",
            )
            .expect("scope boundary should panic before returning");
    }));

    assert!(result.is_err());
}

#[test]
fn scope_filter_event_is_emitted_for_each_store_boundary_drop() {
    let store = MemoryStore::open_in_memory().expect("memory store opens");
    store
        .store(scoped_memory(
            "branch-memory",
            MemoryScope::Branch,
            "session-a",
            Some("workspace-a"),
            Some("feat/foo"),
            None,
        ))
        .expect("branch memory stores");
    store
        .store(scoped_memory(
            "repo-memory",
            MemoryScope::Repo,
            "session-a",
            Some("workspace-b"),
            None,
            None,
        ))
        .expect("repo memory stores");

    let visible = store
        .list_all_scoped(&filter("workspace-a", Some("feat/bar"), None, None))
        .expect("scoped list succeeds");
    let events = store
        .scope_filter_events()
        .expect("scope filter events load");

    assert!(visible.is_empty());
    assert_eq!(events.len(), 2);
    assert!(events
        .iter()
        .all(|event| event.attempted_workspace_id == "workspace-a"));
}

fn filter(
    workspace_id: &str,
    branch: Option<&str>,
    organization_id: Option<&str>,
    session_id: Option<&str>,
) -> ScopeFilter {
    let branch = branch.map(|name| BranchRef {
        name: name.to_string(),
    });
    let filter = ScopeFilter::new(workspace_id, branch, organization_id.map(str::to_string));
    if let Some(session_id) = session_id {
        return filter.for_session(session_id);
    }
    filter
}

fn scoped_memory(
    id: &str,
    scope: MemoryScope,
    session_id: &str,
    workspace_id: Option<&str>,
    branch: Option<&str>,
    organization_id: Option<&str>,
) -> Memory {
    Memory {
        id: id.to_string(),
        session_id: session_id.to_string(),
        content: format!("{id} content"),
        memory_type: MemoryType::Observation,
        scope,
        confidence: 1.0,
        linked_symbols: Vec::new(),
        linked_files: Vec::new(),
        workspace_id: workspace_id.map(str::to_string),
        branch: branch.map(str::to_string),
        scope_organization_id: organization_id.map(str::to_string),
        refresh_key: None,
        source_query: None,
        created_at: 1,
        last_accessed: 1,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: crate::memory::MemoryVerificationStatus::Unverified,
    }
}
