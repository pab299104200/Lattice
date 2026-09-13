use super::*;
use crate::identity::EventId;

const REPO: &str = "repo-a";
const CHECKOUT: &str = "checkout-a";

fn memory(content: &str) -> Memory {
    Memory {
        id: String::new(),
        session_id: "session-a".into(),
        content: content.into(),
        memory_type: MemoryType::Observation,
        scope: MemoryScope::Repo,
        confidence: 1.0,
        linked_symbols: vec![],
        linked_files: vec![],
        workspace_id: Some(REPO.into()),
        branch: None,
        scope_organization_id: None,
        refresh_key: None,
        source_query: None,
        created_at: 1,
        last_accessed: 1,
        access_count: 0,
        is_stale: false,
        stale_reason: None,
        verification_status: MemoryVerificationStatus::Unverified,
    }
}

fn event(id: &str, kind: AttributionEventKind, sequence: u64, time: u64) -> AttributionEventFact {
    let suffix = match id {
        "tool" => 'A',
        "retrieval" => 'B',
        "terminal" => 'C',
        "terminal-retry" => 'D',
        _ => 'E',
    };
    AttributionEventFact {
        event_id: EventId {
            workspace_id: REPO.into(),
            ulid: format!("01ARZ3NDEKTSV4RRFFQ69G5FA{suffix}"),
        },
        kind,
        checkout_id: CHECKOUT.into(),
        session_id: "session-a".into(),
        branch: None,
        sequence,
        observed_at: time,
    }
}

fn input(memory_id: &str) -> AttributionRetrievalInput {
    AttributionRetrievalInput {
        retrieval_id: "retrieval-a".into(),
        repository_id: REPO.into(),
        checkout_id: CHECKOUT.into(),
        session_id: "session-a".into(),
        branch: None,
        tool_event: event("tool", AttributionEventKind::ToolCalled, 1, 10),
        retrieval_event: event("retrieval", AttributionEventKind::MemoryRetrieved, 2, 11),
        accessor: "context".into(),
        metric_client: "codex".into(),
        metric_channel: "mcp".into(),
        accesses: vec![AttributionAccessInput {
            access_id: "access-a".into(),
            local_memory_id: memory_id.into(),
            inclusion_reason: "exact path".into(),
        }],
    }
}

#[test]
fn retrieval_exact_replay_changed_retry_and_resolution_are_atomic() {
    let store = MemoryStore::open_in_memory().unwrap();
    let memory_id = store.store(memory("fact")).unwrap();
    let mut input = input(&memory_id);
    input.accesses.push(AttributionAccessInput {
        access_id: "access-b".into(),
        local_memory_id: memory_id.clone(),
        inclusion_reason: "secondary surface".into(),
    });
    let first = store.record_attribution_retrieval(&input).unwrap();
    assert!(!first.replayed);
    let pending = store.pending_attribution_metrics(None, 25).unwrap();
    assert_eq!(pending.items.len(), 1);
    assert!(matches!(
        pending.items[0],
        PendingAttributionMetric::Retrieval { .. }
    ));
    assert!(store.record_attribution_retrieval(&input).unwrap().replayed);
    let mut changed = input.clone();
    changed.accessor = "search".into();
    assert!(store
        .record_attribution_retrieval(&changed)
        .unwrap_err()
        .to_string()
        .contains("changed facts"));
    let resolved = store
        .resolve_attribution(
            &input.retrieval_id,
            &event("terminal", AttributionEventKind::WorkflowSucceeded, 3, 12),
            AttributionDisposition::Applied,
            &["access-a".into()],
        )
        .unwrap();
    assert_eq!(resolved.status, AttributionResolutionStatus::NewlyResolved);
    let replay = store
        .resolve_attribution(
            &input.retrieval_id,
            &event(
                "terminal-retry",
                AttributionEventKind::WorkflowSucceeded,
                4,
                13,
            ),
            AttributionDisposition::Applied,
            &["access-a".into()],
        )
        .unwrap();
    assert_eq!(replay.status, AttributionResolutionStatus::Idempotent);
    let accesses = store.list_memory_accesses(&memory_id).unwrap();
    assert_eq!(
        accesses
            .iter()
            .find(|a| a.access_id == "access-a")
            .unwrap()
            .was_used,
        Some(true)
    );
    assert_eq!(
        accesses
            .iter()
            .find(|a| a.access_id == "access-b")
            .unwrap()
            .was_used,
        Some(false)
    );
    assert!(store
        .resolve_attribution(
            &input.retrieval_id,
            &event("changed", AttributionEventKind::WorkflowFailed, 5, 14),
            AttributionDisposition::Rejected,
            &[]
        )
        .unwrap_err()
        .to_string()
        .contains("conflicting"));
}

#[test]
fn ignored_feedback_never_marks_even_a_cited_access_used() {
    let store = MemoryStore::open_in_memory().unwrap();
    let id = store.store(memory("fact")).unwrap();
    let input = input(&id);
    store.record_attribution_retrieval(&input).unwrap();
    store
        .resolve_attribution(
            &input.retrieval_id,
            &event("terminal", AttributionEventKind::WorkflowSucceeded, 3, 12),
            AttributionDisposition::Ignored,
            &["access-a".into()],
        )
        .unwrap();
    assert_eq!(
        store.list_memory_accesses(&id).unwrap()[0].was_used,
        Some(false)
    );
}

#[test]
fn mixed_invalid_access_rolls_back_and_foreign_authority_is_rejected() {
    let store = MemoryStore::open_in_memory().unwrap();
    let id = store.store(memory("fact")).unwrap();
    let mut invalid = input(&id);
    invalid.accesses.push(AttributionAccessInput {
        access_id: "bad".into(),
        local_memory_id: "missing".into(),
        inclusion_reason: "bad".into(),
    });
    assert!(store.record_attribution_retrieval(&invalid).is_err());
    assert!(store
        .load_attribution_retrieval(&invalid.retrieval_id)
        .unwrap()
        .is_none());
    let mut foreign = input(&id);
    foreign.repository_id = "repo-b".into();
    assert!(store.record_attribution_retrieval(&foreign).is_err());
    store
        .with_connection(|conn| {
            conn.execute(
                "UPDATE memories SET applicable_checkout_id='checkout-b' WHERE id=?1",
                [&id],
            )
            .map_err(|e| crate::error::LatticeError::Storage(e.to_string()))?;
            Ok(())
        })
        .unwrap();
    let mut wrong_checkout = input(&id);
    wrong_checkout.retrieval_id = "wrong-checkout".into();
    assert!(store.record_attribution_retrieval(&wrong_checkout).is_err());
}

#[test]
fn purge_cascades_journal_and_leaves_expiry_receipt() {
    let store = MemoryStore::open_in_memory().unwrap();
    let id = store.store(memory("fact")).unwrap();
    let input = input(&id);
    store.record_attribution_retrieval(&input).unwrap();
    store
        .with_connection(|conn| {
            conn.execute("DELETE FROM memories WHERE id=?1", [id])
                .map_err(|e| crate::error::LatticeError::Storage(e.to_string()))?;
            Ok(())
        })
        .unwrap();
    assert!(store
        .load_attribution_retrieval(&input.retrieval_id)
        .unwrap()
        .is_none());
    assert!(store
        .record_attribution_retrieval(&input)
        .unwrap_err()
        .to_string()
        .contains("expired"));
}

#[test]
fn prune_is_bounded_and_reports_metric_dead_letter() {
    let store = MemoryStore::open_in_memory().unwrap();
    let id = store.store(memory("fact")).unwrap();
    let input = input(&id);
    store.record_attribution_retrieval(&input).unwrap();
    let retained = store
        .prune_attribution_journal(
            25,
            AttributionPrunePolicy {
                max_resolved_age_secs: 10,
                max_pending_age_secs: 10,
                max_metric_pending_age_secs: 20,
                max_resolved_retrievals: 10,
                batch_limit: 1,
            },
        )
        .unwrap();
    assert_eq!(retained.retrievals_pruned, 0);
    let outcome = store
        .prune_attribution_journal(
            1000,
            AttributionPrunePolicy {
                max_resolved_age_secs: 10,
                max_pending_age_secs: 10,
                max_metric_pending_age_secs: 20,
                max_resolved_retrievals: 10,
                batch_limit: 1,
            },
        )
        .unwrap();
    assert_eq!(outcome.retrievals_pruned, 1);
    assert_eq!(outcome.metric_dead_letters, 1);
    assert_eq!(store.list_all().unwrap().len(), 1);
}

#[test]
fn pending_metric_cursor_pages_colon_ids_without_skipping_accesses() {
    let store = MemoryStore::open_in_memory().unwrap();
    let memory_id = store.store(memory("fact")).unwrap();

    for index in 0..260 {
        let mut retrieval = input(&memory_id);
        retrieval.retrieval_id = format!("memory_retrieval:{index:04}");
        retrieval.accesses[0].access_id = format!("memory_access:{index:04}");
        store.record_attribution_retrieval(&retrieval).unwrap();
        store
            .mark_attribution_retrieval_metric_recorded(&retrieval.retrieval_id)
            .unwrap();
        store
            .resolve_attribution(
                &retrieval.retrieval_id,
                &event("terminal", AttributionEventKind::WorkflowSucceeded, 3, 12),
                AttributionDisposition::Applied,
                std::slice::from_ref(&retrieval.accesses[0].access_id),
            )
            .unwrap();
    }

    let mut cursor = None;
    let mut seen = Vec::new();
    loop {
        let page = store
            .pending_attribution_metrics(cursor.as_ref(), 37)
            .unwrap();
        for item in page.items {
            match item {
                PendingAttributionMetric::Access {
                    access_id,
                    session_id,
                    ..
                } => {
                    assert_eq!(session_id, "session-a");
                    seen.push(access_id);
                }
                PendingAttributionMetric::Retrieval { .. } => {
                    panic!("retrieval metric was already acknowledged")
                }
            }
        }
        let Some(next) = page.next_cursor else { break };
        cursor = Some(next);
    }

    assert_eq!(seen.len(), 260);
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 260);
    assert_eq!(seen.first().unwrap(), "memory_access:0000");
    assert_eq!(seen.last().unwrap(), "memory_access:0259");
}

#[test]
fn prune_aborts_without_mutation_when_ineligible_outbox_backlog_exceeds_budget() {
    let store = MemoryStore::open_in_memory().unwrap();
    let memory_id = store.store(memory("fact")).unwrap();
    let retrieval = input(&memory_id);
    store.record_attribution_retrieval(&retrieval).unwrap();
    store
        .resolve_attribution(
            &retrieval.retrieval_id,
            &event("terminal", AttributionEventKind::WorkflowSucceeded, 3, 12),
            AttributionDisposition::Applied,
            &["access-a".into()],
        )
        .unwrap();

    store
        .with_connection(|conn| {
            conn.execute_batch(
                "WITH RECURSIVE n(value) AS (SELECT 1 UNION ALL SELECT value+1 FROM n WHERE value<10001)
                 INSERT INTO memory_attribution_retrievals(
                   retrieval_id,repository_id,checkout_id,session_id,branch,
                   tool_event_id,tool_event_kind,tool_event_sequence,tool_event_observed_at,
                   retrieval_event_id,retrieval_event_kind,retrieval_event_sequence,retrieval_event_observed_at,
                   accessor,retrieved_count,metric_client,metric_channel,payload_hash,created_at,
                   terminal_event_id,terminal_event_kind,terminal_event_sequence,terminal_event_observed_at,
                   disposition,cited_access_ids_json,resolved_at,retrieval_metric_recorded)
                 SELECT printf('backlog:%05d',value),repository_id,checkout_id,session_id,branch,
                   tool_event_id,tool_event_kind,tool_event_sequence,tool_event_observed_at,
                   retrieval_event_id,retrieval_event_kind,retrieval_event_sequence,retrieval_event_observed_at,
                   accessor,retrieved_count,metric_client,metric_channel,payload_hash,created_at,
                   terminal_event_id,terminal_event_kind,terminal_event_sequence,terminal_event_observed_at,
                   disposition,cited_access_ids_json,resolved_at,0
                 FROM memory_attribution_retrievals,n WHERE retrieval_id='retrieval-a'",
            )
            .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))?;
            let before: i64 = conn
                .query_row("SELECT count(*) FROM memory_attribution_retrievals", [], |row| {
                    row.get(0)
                })
                .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))?;
            let tx = conn
                .unchecked_transaction()
                .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))?;
            let error = super::attribution::prune_in_transaction_with_budget(
                &tx,
                100,
                AttributionPrunePolicy {
                    max_resolved_age_secs: 10,
                    max_pending_age_secs: 10,
                    max_metric_pending_age_secs: 90,
                    max_resolved_retrievals: 100_000,
                    batch_limit: 10,
                },
                5_000,
            )
            .unwrap_err();
            assert!(error.to_string().contains("bounded SQLite work allowance"));
            let after: i64 = tx
                .query_row("SELECT count(*) FROM memory_attribution_retrievals", [], |row| {
                    row.get(0)
                })
                .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))?;
            assert_eq!(before, after);
            tx.rollback()
                .map_err(|error| crate::error::LatticeError::Storage(error.to_string()))?;
            Ok(())
        })
        .unwrap();
}
