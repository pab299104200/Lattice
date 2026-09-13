use super::conflict_query::{query_conflicts_with_budget, ConflictAnchorQuery};
use super::{
    Memory, MemoryLinkRecord, MemoryScope, MemoryStore, MemoryType, MemoryVerificationStatus,
};
use crate::verification::ScopeFilter;
use rusqlite::params;

const WORKSPACE: &str = "workspace-conflicts";

fn memory(content: &str, workspace: &str, files: &[&str], symbols: &[&str]) -> Memory {
    Memory {
        id: String::new(),
        session_id: String::new(),
        content: content.into(),
        memory_type: MemoryType::Observation,
        scope: MemoryScope::Repo,
        confidence: 1.0,
        linked_symbols: symbols.iter().map(|value| (*value).into()).collect(),
        linked_files: files.iter().map(|value| (*value).into()).collect(),
        workspace_id: Some(workspace.into()),
        branch: None,
        scope_organization_id: None,
        refresh_key: None,
        source_query: None,
        created_at: 10,
        last_accessed: 10,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: MemoryVerificationStatus::Unverified,
    }
}

fn scope() -> ScopeFilter {
    ScopeFilter::new(WORKSPACE, None, None)
}

fn link(store: &MemoryStore, id: &str, source: &str, target: &str, created_at: u64) {
    store
        .insert_memory_link(&MemoryLinkRecord {
            link_id: id.into(),
            source_memory_id: source.into(),
            target_memory_id: target.into(),
            link_type: "contradicts".into(),
            reason: id.into(),
            created_at,
            verification_status: "contradicted".into(),
        })
        .expect("insert conflict");
}

#[test]
fn exact_file_anchor_ignores_ten_thousand_unrelated_memberships_and_pages_exactly() {
    let store = MemoryStore::open_in_memory().expect("store");
    store.with_connection(|conn| {
        conn.execute_batch(
            "WITH RECURSIVE n(value) AS (SELECT 1 UNION ALL SELECT value+1 FROM n WHERE value<10000)
             INSERT INTO memories(id,content,memory_type,scope,workspace_id,linked_files,created_at,last_accessed)
             SELECT printf('noise-%05d',value),'noise','observation','repo','workspace-conflicts',
                    json_array(printf('src/noise_%05d.rs',value)),value,value FROM n;"
        ).map_err(|error| crate::error::LatticeError::Storage(error.to_string()))?;
        Ok(())
    }).expect("seed noise");
    let anchor = store
        .store(memory("anchor", WORKSPACE, &["src/exact.rs"], &[]))
        .unwrap();
    let first = store.store(memory("first", WORKSPACE, &[], &[])).unwrap();
    let second = store.store(memory("second", WORKSPACE, &[], &[])).unwrap();
    link(&store, "older", &anchor, &first, 20);
    link(&store, "newer", &second, &anchor, 30);
    let first_page = store
        .with_connection(|conn| {
            query_conflicts_with_budget(
                conn,
                &scope(),
                None,
                &ConflictAnchorQuery::File("src/exact.rs".into()),
                0,
                1,
                5_000_000,
            )
        })
        .unwrap();
    assert_eq!(first_page.total, 2);
    assert_eq!(first_page.records.len(), 1);
    assert_eq!(first_page.records[0].created_at, 30);
    let second_page = store
        .with_connection(|conn| {
            query_conflicts_with_budget(
                conn,
                &scope(),
                None,
                &ConflictAnchorQuery::File("src/exact.rs".into()),
                1,
                1,
                5_000_000,
            )
        })
        .unwrap();
    assert_eq!(second_page.total, 2);
    assert_eq!(second_page.records[0].created_at, 20);
}

#[test]
fn doc_and_symbol_memberships_are_indexed_and_semantic_stale_records_remain_inspectable() {
    let store = MemoryStore::open_in_memory().expect("store");
    let anchor = store
        .store(memory("anchor", WORKSPACE, &[], &["crate::Thing"]))
        .unwrap();
    let target = store.store(memory("target", WORKSPACE, &[], &[])).unwrap();
    let mut fields = store.get_structured_fields(&anchor).unwrap().unwrap();
    fields.linked_docs = vec!["docs/design.md".into()];
    store.update_structured_fields(&anchor, &fields).unwrap();
    store
        .with_connection(|conn| {
            conn.execute(
                "UPDATE memories SET is_stale=1,verification_status='contradicted' WHERE id=?1",
                [&anchor],
            )
            .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))?;
            Ok(())
        })
        .unwrap();
    link(&store, "doc-conflict", &anchor, &target, 40);
    for query in [
        ConflictAnchorQuery::Doc("docs/design.md#Heading".into()),
        ConflictAnchorQuery::Symbol("crate::Thing".into()),
    ] {
        let page = store
            .with_connection(|conn| {
                query_conflicts_with_budget(conn, &scope(), None, &query, 0, 25, 5_000_000)
            })
            .unwrap();
        assert_eq!(page.total, 1);
    }
}

#[test]
fn out_of_scope_anchor_and_endpoint_are_rejected() {
    let store = MemoryStore::open_in_memory().expect("store");
    let anchor = store.store(memory("anchor", WORKSPACE, &[], &[])).unwrap();
    let outside = store
        .store(memory("outside", "other-workspace", &[], &[]))
        .unwrap();
    let error = store
        .with_connection(|conn| {
            query_conflicts_with_budget(
                conn,
                &scope(),
                None,
                &ConflictAnchorQuery::Memory(outside.clone()),
                0,
                25,
                5_000_000,
            )
        })
        .unwrap_err();
    assert!(error.to_string().contains("outside the active scope"));
    link(&store, "leak", &anchor, &outside, 50);
    let error = store
        .with_connection(|conn| {
            query_conflicts_with_budget(
                conn,
                &scope(),
                None,
                &ConflictAnchorQuery::Memory(anchor),
                0,
                25,
                5_000_000,
            )
        })
        .unwrap_err();
    assert!(error.to_string().contains("Conflict memory") && error.to_string().contains("outside"));
}

#[test]
fn dense_conflicts_fail_with_actionable_budget_and_handler_is_removed() {
    let store = MemoryStore::open_in_memory().expect("store");
    let anchor = store.store(memory("anchor", WORKSPACE, &[], &[])).unwrap();
    let target = store.store(memory("target", WORKSPACE, &[], &[])).unwrap();
    store.with_connection(|conn| {
        conn.execute(
            "WITH RECURSIVE n(value) AS (SELECT 1 UNION ALL SELECT value+1 FROM n WHERE value<2000)
             INSERT INTO memory_links(link_id,source_memory_id,target_memory_id,link_type,reason,created_at,verification_status)
             SELECT printf('dense-%05d',value),?1,?2,'contradicts',printf('reason-%05d',value),value,'contradicted' FROM n",
            params![anchor, target],
        ).map_err(|error| crate::error::LatticeError::Storage(error.to_string()))?;
        Ok(())
    }).unwrap();
    let error = store
        .with_connection(|conn| {
            query_conflicts_with_budget(
                conn,
                &scope(),
                None,
                &ConflictAnchorQuery::Memory(anchor.clone()),
                0,
                25,
                1,
            )
        })
        .unwrap_err();
    assert!(error.to_string().contains("bounded SQLite work allowance"));
    let page = store
        .with_connection(|conn| {
            query_conflicts_with_budget(
                conn,
                &scope(),
                None,
                &ConflictAnchorQuery::Memory(anchor),
                0,
                1,
                5_000_000,
            )
        })
        .unwrap();
    assert_eq!(page.total, 2000);
}

#[test]
fn cursor_and_limit_overflow_are_rejected_before_sql() {
    let store = MemoryStore::open_in_memory().expect("store");
    let anchor = store.store(memory("anchor", WORKSPACE, &[], &[])).unwrap();
    for (offset, limit, expected) in [
        (0, 0, "limit"),
        (0, 4097, "limit"),
        (usize::MAX, 1, "overflows"),
    ] {
        let error = store
            .with_connection(|conn| {
                query_conflicts_with_budget(
                    conn,
                    &scope(),
                    None,
                    &ConflictAnchorQuery::Memory(anchor.clone()),
                    offset,
                    limit,
                    5_000_000,
                )
            })
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn structured_supersession_is_visible_from_both_endpoints_and_checkout_is_authorized() {
    let store = MemoryStore::open_in_memory().expect("store");
    let older = store.store(memory("older", WORKSPACE, &[], &[])).unwrap();
    let newer = store.store(memory("newer", WORKSPACE, &[], &[])).unwrap();
    let mut older_fields = store.get_structured_fields(&older).unwrap().unwrap();
    older_fields.superseded_by_memory_id = Some(newer.clone());
    store
        .update_structured_fields(&older, &older_fields)
        .unwrap();
    store
        .with_connection(|conn| {
            conn.execute(
                "UPDATE memories SET applicable_checkout_id='checkout-a' WHERE id IN (?1,?2)",
                params![older, newer],
            )
            .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))?;
            Ok(())
        })
        .unwrap();
    for anchor in [&older, &newer] {
        let denied = store
            .with_connection(|conn| {
                query_conflicts_with_budget(
                    conn,
                    &scope(),
                    Some("checkout-b"),
                    &ConflictAnchorQuery::Memory(anchor.clone()),
                    0,
                    25,
                    5_000_000,
                )
            })
            .unwrap_err();
        assert!(denied.to_string().contains("outside the active scope"));
        let page = store
            .with_connection(|conn| {
                query_conflicts_with_budget(
                    conn,
                    &scope(),
                    Some("checkout-a"),
                    &ConflictAnchorQuery::Memory(anchor.clone()),
                    0,
                    25,
                    5_000_000,
                )
            })
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.records[0].source_memory_id, newer);
        assert_eq!(page.records[0].target_memory_id, older);
    }
}

#[test]
fn historical_doc_membership_backfill_advances_in_durable_pages() {
    let store = MemoryStore::open_in_memory().expect("store");
    store.with_connection(|conn| {
        conn.execute_batch(
            "WITH RECURSIVE n(value) AS (SELECT 1 UNION ALL SELECT value+1 FROM n WHERE value<300)
             INSERT INTO memories(id,content,memory_type,scope,workspace_id,linked_docs_json,created_at,last_accessed)
             SELECT printf('doc-%05d',value),'doc','observation','repo','workspace-conflicts',
                    json_array(printf('docs/%05d.md',value)),value,value FROM n;
             DELETE FROM memory_retrieval_docs;
             DELETE FROM memory_retrieval_metadata WHERE key IN ('membership-docs-v1','membership-docs-cursor-v1');"
        ).map_err(|error| crate::error::LatticeError::Storage(error.to_string()))?;
        super::retrieval::initialize(conn)?;
        let indexed: i64 = conn.query_row("SELECT count(DISTINCT memory_id) FROM memory_retrieval_docs", [], |row| row.get(0))
            .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))?;
        assert_eq!(indexed, 256);
        let complete: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM memory_retrieval_metadata WHERE key='membership-docs-v1')", [], |row| row.get(0))
            .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))?;
        assert!(!complete);
        super::retrieval::initialize(conn)?;
        let indexed: i64 = conn.query_row("SELECT count(DISTINCT memory_id) FROM memory_retrieval_docs", [], |row| row.get(0))
            .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))?;
        assert_eq!(indexed, 300);
        Ok(())
    }).unwrap();
}
