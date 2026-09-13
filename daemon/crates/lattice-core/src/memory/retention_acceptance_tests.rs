//! Retention/proposal-reference acceptance coverage.
//!
//! These tests use the production SQLite schema and legacy-shaped rows.  They
//! pin the fail-closed migration contract: a purge cannot run until proposal
//! references have been indexed, and malformed or oversized audit rows remain
//! durable while reporting an actionable block.

use rusqlite::{params, Connection};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tempfile::tempdir;

use super::*;
use crate::consolidation::proposal_references::{
    advance_reference_backfill, MAX_PROPOSAL_REFERENCE_PAYLOAD_BYTES,
    PROPOSAL_REFERENCE_BACKFILL_ROWS_PER_PASS,
};
use crate::consolidation::review_queue::pending_manual_review_count_up_to;
use crate::memory::retention::{
    maintenance_needs_continuation, sweep, DeliveryBinding, MAX_REPLAY_AGE_SECS,
    RETENTION_DEPENDENCY_DELETE_BATCH,
};
use crate::memory::MemoryStore;

fn policy(batch_size: usize) -> RetentionPolicy {
    RetentionPolicy {
        stale_after_secs: 10,
        purge_after_secs: 20,
        sweep_interval_secs: 1,
        receipt_retention_secs: MAX_REPLAY_AGE_SECS,
        max_receipts: 1000,
        batch_size,
    }
}

fn store() -> (tempfile::TempDir, std::path::PathBuf, MemoryStore) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("memory.sqlite");
    let store = MemoryStore::open(&path).unwrap();
    store
        .with_connection(crate::consolidation::initialize_schema)
        .unwrap();
    (dir, path, store)
}

fn insert_memory(conn: &Connection, id: &str, created_at: u64) {
    conn.execute(
        "INSERT INTO memories(id,content,memory_type,created_at,last_accessed,retention_grace_until) VALUES(?1,?1,'fact',?2,?2,0)",
        params![id, created_at],
    ).unwrap();
}

fn insert_job_and_proposal(
    conn: &Connection,
    id: &str,
    target: Option<&str>,
    prior: &str,
    proposed: &str,
    evidence: &str,
) {
    conn.execute(
        "INSERT INTO consolidation_jobs(job_id,workspace_id,kind,mode,status,enqueued_at) VALUES(?1,'workspace','retention','background','proposed',1)",
        [format!("job-{id}")],
    ).unwrap();
    conn.execute(
        "INSERT INTO consolidation_proposals(proposal_id,job_id,target_memory_id,proposal_kind,prior_state,proposed_state,evidence,decision) VALUES(?1,?2,?3,'update_memory',?4,?5,?6,'pending')",
        params![id, format!("job-{id}"), target, prior, proposed, evidence],
    ).unwrap();
}

fn receipt_count_state(conn: &Connection) -> (String, i64, bool) {
    conn.query_row(
        "SELECT cursor,counted,complete FROM memory_receipt_count_state WHERE id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .unwrap()
}

fn receipt_count_oracle(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM memory_deletion_receipts", [], |row| {
        row.get(0)
    })
    .unwrap()
}

#[test]
fn stale_marking_is_batch_bounded_and_makes_deterministic_progress() {
    let (_dir, _path, store) = store();
    store
        .with_connection(|conn| {
            for index in 0..5 {
                insert_memory(conn, &format!("stale-{index}"), 1);
            }
            Ok(())
        })
        .unwrap();
    let mut p = policy(2);
    p.purge_after_secs = 1_000;
    let first = store.with_connection(|conn| sweep(conn, 100, &p)).unwrap();
    assert_eq!(first.stale, 2);
    store
        .with_connection(|conn| {
            conn.execute(
                "UPDATE memory_retention_control SET next_sweep=0 WHERE id=1",
                [],
            )
            .map(|_| ())
            .map_err(|e| crate::LatticeError::Storage(e.to_string()))
        })
        .unwrap();
    let second = store.with_connection(|conn| sweep(conn, 101, &p)).unwrap();
    assert_eq!(second.stale, 2);
    let stale: i64 = store
        .with_connection(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM memories WHERE retention_stale=1",
                [],
                |r| r.get(0),
            )
            .map_err(|e| crate::LatticeError::Storage(e.to_string()))
        })
        .unwrap();
    assert_eq!(stale, 4);
}

#[test]
fn legacy_nested_references_backfill_across_restartable_pages() {
    let (_dir, path, store) = store();
    store.with_connection(|conn| {
        for index in 0..=PROPOSAL_REFERENCE_BACKFILL_ROWS_PER_PASS {
            insert_job_and_proposal(conn, &format!("nested-{index}"), Some("target-id"),
                r#"{"deep":{"memory_id":"prior-id"}}"#,
                r#"{"deep":[{"memory_id":"proposed-id"},{"memory_id":"nested-id"}]}"#,
                r#"{"evidence":{"memory_id":"evidence-id"}}"#);
        }
        conn.execute("UPDATE consolidation_proposal_ref_backfill SET next_rowid=0,complete=0,blocked_proposal_id=NULL,blocked_payload_bytes=NULL,last_error=NULL WHERE id=1", []).unwrap();
        Ok(())
    }).unwrap();
    let first = store.with_connection(advance_reference_backfill).unwrap();
    assert_eq!(
        first.rows_processed,
        PROPOSAL_REFERENCE_BACKFILL_ROWS_PER_PASS
    );
    assert!(!first.complete);
    drop(store);
    let reopened = MemoryStore::open(&path).unwrap();
    let second = reopened
        .with_connection(advance_reference_backfill)
        .unwrap();
    assert_eq!(second.rows_processed, 1);
    assert!(second.complete);
    reopened.with_connection(|conn| {
        let kinds = conn.prepare("SELECT reference_kind FROM consolidation_proposal_memory_refs WHERE proposal_id='nested-0' ORDER BY reference_kind")
            .map_err(|e| crate::LatticeError::Storage(e.to_string()))?
            .query_map([], |r| r.get::<_, String>(0)).map_err(|e| crate::LatticeError::Storage(e.to_string()))?
            .collect::<rusqlite::Result<Vec<_>>>().map_err(|e| crate::LatticeError::Storage(e.to_string()))?;
        assert_eq!(kinds, ["evidence", "prior_state", "proposed_state", "proposed_state", "target"]);
        Ok(())
    }).unwrap();
}

#[test]
fn incomplete_or_oversized_legacy_reference_backfill_blocks_purge_without_receipt() {
    let (_dir, _path, store) = store();
    store
        .with_connection(|conn| {
            insert_memory(conn, "purge-candidate", 1);
            let oversized = format!(
                "{{\"payload\":\"{}\"}}",
                "x".repeat(MAX_PROPOSAL_REFERENCE_PAYLOAD_BYTES)
            );
            conn.execute(
                "DROP TRIGGER consolidation_proposal_payload_limit_insert",
                [],
            )
            .unwrap();
            insert_job_and_proposal(conn, "oversized", None, "{}", "{}", &oversized);
            conn.execute(
                "UPDATE consolidation_proposal_ref_backfill SET next_rowid=0,complete=0 WHERE id=1",
                [],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
    let error = store
        .with_connection(|conn| sweep(conn, 100, &policy(1)))
        .expect_err("oversized legacy proposal blocks purge");
    assert!(error.to_string().contains("purge blocked"));
    store.with_connection(|conn| {
        let memory_count: i64 = conn.query_row("SELECT COUNT(*) FROM memories WHERE id='purge-candidate'", [], |r| r.get(0)).unwrap();
        let receipt_count: i64 = conn.query_row("SELECT COUNT(*) FROM memory_deletion_receipts WHERE memory_id='purge-candidate'", [], |r| r.get(0)).unwrap();
        let blocked: Option<String> = conn.query_row("SELECT blocked_proposal_id FROM consolidation_proposal_ref_backfill WHERE id=1", [], |r| r.get(0)).unwrap();
        assert_eq!(memory_count, 1);
        assert_eq!(receipt_count, 0);
        assert_eq!(blocked.as_deref(), Some("oversized"));
        Ok(())
    }).unwrap();
}

#[test]
fn malformed_legacy_json_commits_block_diagnostic_and_never_starts_purge() {
    let (_dir, _path, store) = store();
    store.with_connection(|conn| {
        insert_memory(conn, "malformed-victim", 1);
        conn.execute_batch("DROP TRIGGER consolidation_proposal_refs_insert; DROP TRIGGER consolidation_proposal_refs_update; DROP TRIGGER consolidation_proposal_payload_limit_insert; DROP TRIGGER consolidation_proposal_payload_limit_update; DROP TRIGGER consolidation_proposal_reference_limit_insert; DROP TRIGGER consolidation_proposal_reference_limit_update; PRAGMA ignore_check_constraints=ON;").unwrap();
        insert_job_and_proposal(conn, "malformed", None, "{}", "{}", "{}");
        conn.execute("UPDATE consolidation_proposals SET prior_state='{not-json' WHERE proposal_id='malformed'", []).unwrap();
        conn.execute_batch("PRAGMA ignore_check_constraints=OFF; UPDATE consolidation_proposal_ref_backfill SET next_rowid=0,complete=0 WHERE id=1;").unwrap();
        Ok(())
    }).unwrap();
    let error = store
        .with_connection(|conn| sweep(conn, 100, &policy(1)))
        .expect_err("malformed legacy row blocks purge");
    assert!(error.to_string().contains("purge blocked"));
    store.with_connection(|conn| {
        let blocked: Option<String> = conn.query_row("SELECT blocked_proposal_id FROM consolidation_proposal_ref_backfill WHERE id=1", [], |r| r.get(0)).unwrap();
        let receipts: i64 = conn.query_row("SELECT COUNT(*) FROM memory_deletion_receipts WHERE memory_id='malformed-victim'", [], |r| r.get(0)).unwrap();
        assert_eq!(blocked.as_deref(), Some("malformed"));
        assert_eq!(receipts, 0);
        Ok(())
    }).unwrap();
}

#[test]
fn proposal_reference_triggers_update_delete_and_rollback_atomically() {
    let (_dir, _path, store) = store();
    store.with_connection(|conn| {
        insert_job_and_proposal(conn, "triggered", Some("target-a"), r#"{"id":"prior-a"}"#, "{}", r#"{"memory_id":"evidence-a"}"#);
        let count = |id: &str| conn.query_row("SELECT COUNT(*) FROM consolidation_proposal_memory_refs WHERE proposal_id='triggered' AND memory_id=?1", [id], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(count("prior-a"), 1);
        conn.execute("UPDATE consolidation_proposals SET evidence='{\"memory_id\":\"evidence-b\"}' WHERE proposal_id='triggered'", []).unwrap();
        assert_eq!(count("evidence-a"), 0);
        assert_eq!(count("evidence-b"), 1);
        let tx = conn.unchecked_transaction().unwrap();
        tx.execute("UPDATE consolidation_proposals SET target_memory_id='target-b' WHERE proposal_id='triggered'", []).unwrap();
        tx.rollback().unwrap();
        assert_eq!(count("target-a"), 1);
        assert_eq!(count("target-b"), 0);
        conn.execute("DELETE FROM consolidation_proposals WHERE proposal_id='triggered'", []).unwrap();
        assert_eq!(count("prior-a"), 0);
        Ok(())
    }).unwrap();
}

#[test]
fn indexed_purge_removes_only_referencing_proposal_and_its_outbox() {
    let (_dir, _path, store) = store();
    store.with_connection(|conn| {
        insert_memory(conn, "victim", 1);
        insert_job_and_proposal(conn, "dependent", Some("victim"), "{}", "{}", "{}");
        insert_job_and_proposal(conn, "unrelated", Some("other"), "{}", "{}", "{}");
        conn.execute("INSERT INTO consolidation_event_outbox(outbox_id,proposal_id,transition,workspace_id,event_uuid,event_ts_unix_micros,envelope_json,created_at) VALUES('outbox-dependent','dependent','applied','workspace','01ARZ3NDEKTSV4RRFFQ69G5FAV',1,'{}',1)", []).unwrap();
        Ok(())
    }).unwrap();
    let report = store
        .with_connection(|conn| sweep(conn, 100, &policy(1)))
        .unwrap();
    assert_eq!(report.purged, 1);
    store
        .with_connection(|conn| {
            let count = |table: &str, id: &str| {
                conn.query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {table} WHERE {}=?1",
                        if table == "consolidation_event_outbox" {
                            "proposal_id"
                        } else {
                            "proposal_id"
                        }
                    ),
                    [id],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
            };
            assert_eq!(count("consolidation_proposals", "dependent"), 0);
            assert_eq!(count("consolidation_event_outbox", "dependent"), 0);
            assert_eq!(count("consolidation_proposals", "unrelated"), 1);
            let receipt: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM memory_deletion_receipts WHERE memory_id='victim'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(receipt, 1);
            Ok(())
        })
        .unwrap();
}

#[test]
fn indexed_reference_lookup_stays_selective_with_thousands_of_large_unrelated_proposals() {
    let (_dir, _path, store) = store();
    store
        .with_connection(|conn| {
            insert_memory(conn, "indexed-victim", 1);
            let evidence = format!("{{\"padding\":\"{}\"}}", "z".repeat(8 * 1024));
            for index in 0..1_000 {
                insert_job_and_proposal(
                    conn,
                    &format!("unrelated-{index}"),
                    Some(&format!("other-{index}")),
                    "{}",
                    "{}",
                    &evidence,
                );
            }
            insert_job_and_proposal(
                conn,
                "indexed-dependent",
                Some("indexed-victim"),
                "{}",
                "{}",
                "{}",
            );
            Ok(())
        })
        .unwrap();
    store.with_connection(|conn| {
        let plan = conn.prepare("EXPLAIN QUERY PLAN SELECT proposal_id FROM consolidation_proposal_memory_refs INDEXED BY idx_consolidation_proposal_memory_refs_memory WHERE memory_id=?1")
            .unwrap().query_map(["indexed-victim"], |r| r.get::<_, String>(3)).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap().join(" ");
        assert!(plan.contains("idx_consolidation_proposal_memory_refs_memory"));
        Ok(())
    }).unwrap();
    let report = store
        .with_connection(|conn| sweep(conn, 100, &policy(1)))
        .unwrap();
    assert_eq!(report.purged, 1);
    store.with_connection(|conn| {
        let dependent: i64 = conn.query_row("SELECT COUNT(*) FROM consolidation_proposals WHERE proposal_id='indexed-dependent'", [], |r| r.get(0)).unwrap();
        let unrelated: i64 = conn.query_row("SELECT COUNT(*) FROM consolidation_proposals WHERE proposal_id='unrelated-999'", [], |r| r.get(0)).unwrap();
        assert_eq!(dependent, 0);
        assert_eq!(unrelated, 1);
        Ok(())
    }).unwrap();
}

#[test]
fn scan_cursor_bounds_sqlite_vm_steps_with_thousands_of_ineligible_grace_rows() {
    let (_dir, _path, store) = store();
    store.with_connection(|conn| {
        for index in 0..3_000 {
            conn.execute("INSERT INTO memories(id,content,memory_type,created_at,last_accessed,retention_grace_until) VALUES(?1,'grace','fact',1,1,999999)", [format!("a-grace-{index:04}")]).unwrap();
        }
        insert_memory(conn, "z-eligible", 1);
        Ok(())
    }).unwrap();
    let ticks = Arc::new(AtomicUsize::new(0));
    let observer = ticks.clone();
    let report = store
        .with_connection(|conn| {
            conn.progress_handler(
                100,
                Some(move || {
                    observer.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            );
            let result = sweep(conn, 100, &policy(1));
            conn.progress_handler(0, None::<fn() -> bool>);
            result
        })
        .unwrap();
    assert_eq!(report.stale, 0);
    assert!(
        ticks.load(Ordering::Relaxed) * 100 <= 250_000,
        "one batch must have a fixed VM-work ceiling"
    );
    let cursor: String = store
        .with_connection(|conn| {
            conn.query_row(
                "SELECT stale_scan_cursor FROM memory_retention_control WHERE id=1",
                [],
                |r| r.get(0),
            )
            .map_err(|e| crate::LatticeError::Storage(e.to_string()))
        })
        .unwrap();
    assert_eq!(cursor, "a-grace-0000");
}

#[test]
fn high_fanout_cleanup_is_staged_and_reopens_until_memory_can_be_purged() {
    let (_dir, path, store) = store();
    store
        .with_connection(|conn| {
            insert_memory(conn, "fanout-victim", 1);
            for index in 0..(RETENTION_DEPENDENCY_DELETE_BATCH * 3 + 1) {
                insert_job_and_proposal(
                    conn,
                    &format!("fanout-{index:03}"),
                    Some("fanout-victim"),
                    "{}",
                    "{}",
                    "{}",
                );
            }
            Ok(())
        })
        .unwrap();
    let first = store
        .with_connection(|conn| sweep(conn, 100, &policy(1)))
        .unwrap();
    assert_eq!(
        first.purged, 0,
        "first pass may remove only one bounded dependency page"
    );
    let remaining = store.with_connection(|conn| conn.query_row("SELECT COUNT(*) FROM consolidation_proposals WHERE target_memory_id='fanout-victim'", [], |r| r.get::<_, i64>(0)).map_err(|e| crate::LatticeError::Storage(e.to_string()))).unwrap();
    assert_eq!(
        remaining,
        (RETENTION_DEPENDENCY_DELETE_BATCH * 2 + 1) as i64
    );
    drop(store);
    let reopened = MemoryStore::open(&path).unwrap();
    for now in 101..110 {
        reopened
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE memory_retention_control SET next_sweep=0 WHERE id=1",
                    [],
                )
                .map(|_| ())
                .map_err(|e| crate::LatticeError::Storage(e.to_string()))
            })
            .unwrap();
        let _ = reopened
            .with_connection(|conn| sweep(conn, now, &policy(1)))
            .unwrap();
        let remains: i64 = reopened
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM memories WHERE id='fanout-victim'",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| crate::LatticeError::Storage(e.to_string()))
            })
            .unwrap();
        if remains == 0 {
            break;
        }
    }
    assert_eq!(
        reopened
            .with_connection(|conn| conn
                .query_row::<i64, _, _>(
                    "SELECT COUNT(*) FROM memories WHERE id='fanout-victim'",
                    [],
                    |r| r.get(0)
                )
                .map_err(|e| crate::LatticeError::Storage(e.to_string())))
            .unwrap(),
        0
    );
    assert_eq!(reopened.with_connection(|conn| conn.query_row("SELECT COUNT(*) FROM consolidation_proposals WHERE target_memory_id='fanout-victim'", [], |r| r.get::<_, i64>(0)).map_err(|e| crate::LatticeError::Storage(e.to_string()))).unwrap(), 0);
}

#[test]
fn staged_purge_fence_blocks_ack_updates_and_replay_until_reopened_cleanup_finishes() {
    // A first dependency page is destructive, so it must establish the replay
    // fence before deletion.  The still-durable row is intentionally not a
    // recallable memory while later sweeps finish bounded fanout cleanup.
    let (_dir, path, store) = store();
    let binding = DeliveryBinding {
        delivery_id: "purge-pending-delivery",
        repository_id: "repo",
        session_id: "session",
        payload_hash: "payload",
    };
    store
        .with_connection(|conn| {
            insert_memory(conn, "pending-victim", 1);
            for index in 0..(RETENTION_DEPENDENCY_DELETE_BATCH * 3 + 1) {
                insert_job_and_proposal(
                    conn,
                    &format!("pending-fanout-{index:03}"),
                    Some("pending-victim"),
                    "{}",
                    "{}",
                    "{}",
                );
            }
            Ok(())
        })
        .unwrap();
    store
        .attempt_memory_delivery(&binding, &["pending-victim".to_owned()], 90)
        .unwrap();
    let replay_attempt = store.get_by_id("pending-victim").unwrap().unwrap();

    let first = store
        .with_connection(|conn| sweep(conn, 100, &policy(1)))
        .unwrap();
    assert_eq!(first.purged, 0);
    store.with_connection(|conn| {
        let pending: bool = conn.query_row("SELECT purge_pending FROM memories WHERE id='pending-victim'", [], |r| r.get(0)).unwrap();
        let receipt: i64 = conn.query_row("SELECT COUNT(*) FROM memory_deletion_receipts WHERE memory_id='pending-victim'", [], |r| r.get(0)).unwrap();
        let remaining: i64 = conn.query_row("SELECT COUNT(*) FROM consolidation_proposals WHERE target_memory_id='pending-victim'", [], |r| r.get(0)).unwrap();
        assert!(pending);
        assert_eq!(receipt, 1);
        assert_eq!(remaining, (RETENTION_DEPENDENCY_DELETE_BATCH * 2 + 1) as i64);
        Ok(())
    }).unwrap();

    assert_eq!(store.acknowledge_memory_delivery(&binding, 101).unwrap(), 0);
    assert!(!store
        .memory_delivery_acknowledgement_was_recorded(&binding, 101)
        .unwrap());
    assert!(store
        .update_content("pending-victim", "must-not-renew")
        .is_err());
    assert!(store
        .with_connection(|conn| conn
            .execute(
                "UPDATE memories SET content='must-not-renew' WHERE id='pending-victim'",
                []
            )
            .map(|_| ())
            .map_err(|e| crate::LatticeError::Storage(e.to_string())))
        .is_err());
    assert!(
        store.store(replay_attempt).is_err(),
        "the durable pending receipt fences same-ID replay"
    );
    assert!(store
        .with_connection(maintenance_needs_continuation)
        .unwrap());

    // Move far beyond receipt retention while the staged dependency set is
    // still nonempty.  A receipt may age only after the pending row is gone.
    let second = store
        .with_connection(|conn| sweep(conn, 100 + MAX_REPLAY_AGE_SECS + 1, &policy(1)))
        .unwrap();
    assert_eq!(second.purged, 0);
    store.with_connection(|conn| {
        let receipt: i64 = conn.query_row("SELECT COUNT(*) FROM memory_deletion_receipts WHERE memory_id='pending-victim'", [], |r| r.get(0)).unwrap();
        assert_eq!(receipt, 1);
        Ok(())
    }).unwrap();

    drop(store);
    let reopened = MemoryStore::open(&path).unwrap();
    for now in (100 + MAX_REPLAY_AGE_SECS + 2)..(100 + MAX_REPLAY_AGE_SECS + 12) {
        reopened
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE memory_retention_control SET next_sweep=0 WHERE id=1",
                    [],
                )
                .map(|_| ())
                .map_err(|e| crate::LatticeError::Storage(e.to_string()))
            })
            .unwrap();
        let _ = reopened
            .with_connection(|conn| sweep(conn, now, &policy(1)))
            .unwrap();
        let remains: i64 = reopened
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM memories WHERE id='pending-victim'",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| crate::LatticeError::Storage(e.to_string()))
            })
            .unwrap();
        if remains == 0 {
            break;
        }
    }
    assert!(
        reopened
            .with_connection(|conn| conn
                .query_row::<i64, _, _>(
                    "SELECT COUNT(*) FROM memories WHERE id='pending-victim'",
                    [],
                    |r| r.get(0)
                )
                .map_err(|e| crate::LatticeError::Storage(e.to_string())))
            .unwrap()
            == 0
    );
    assert!(!reopened
        .with_connection(maintenance_needs_continuation)
        .unwrap());
}

#[test]
fn ordinary_text_values_do_not_create_false_proposal_references_or_delete_audit_rows() {
    let (_dir, _path, store) = store();
    store
        .with_connection(|conn| {
            insert_memory(conn, "ordinary-memory", 1);
            insert_job_and_proposal(
                conn,
                "ordinary-text",
                Some("different-memory"),
                "{}",
                "{}",
                r#"{"note":"main verified ordinary-memory"}"#,
            );
            Ok(())
        })
        .unwrap();
    let report = store
        .with_connection(|conn| sweep(conn, 100, &policy(1)))
        .unwrap();
    assert_eq!(report.purged, 1);
    store.with_connection(|conn| {
        let refs: i64 = conn.query_row("SELECT COUNT(*) FROM consolidation_proposal_memory_refs WHERE proposal_id='ordinary-text' AND memory_id='ordinary-memory'", [], |r| r.get(0)).unwrap();
        let proposal: i64 = conn.query_row("SELECT COUNT(*) FROM consolidation_proposals WHERE proposal_id='ordinary-text'", [], |r| r.get(0)).unwrap();
        assert_eq!(refs, 0);
        assert_eq!(proposal, 1);
        Ok(())
    }).unwrap();
}

#[test]
fn blocked_oversized_legacy_row_recovers_after_shrink_and_allows_later_purge() {
    let (_dir, _path, store) = store();
    store.with_connection(|conn| {
        insert_memory(conn, "recover-victim", 1);
        conn.execute_batch("DROP TRIGGER consolidation_proposal_refs_insert; DROP TRIGGER consolidation_proposal_payload_limit_insert; DROP TRIGGER consolidation_proposal_reference_limit_insert;").unwrap();
        let oversized = format!("{{\"memory_id\":\"{}\"}}", "x".repeat(MAX_PROPOSAL_REFERENCE_PAYLOAD_BYTES));
        insert_job_and_proposal(conn, "recover-oversized", None, "{}", "{}", &oversized);
        conn.execute("UPDATE consolidation_proposal_ref_backfill SET next_rowid=0,complete=0 WHERE id=1", []).unwrap();
        Ok(())
    }).unwrap();
    assert!(store
        .with_connection(|conn| sweep(conn, 100, &policy(1)))
        .is_err());
    store.with_connection(|conn| conn.execute("UPDATE consolidation_proposals SET evidence='{}' WHERE proposal_id='recover-oversized'", []).map(|_| ()).map_err(|e| crate::LatticeError::Storage(e.to_string()))).unwrap();
    store
        .with_connection(|conn| {
            conn.execute(
                "UPDATE memory_retention_control SET next_sweep=0 WHERE id=1",
                [],
            )
            .map(|_| ())
            .map_err(|e| crate::LatticeError::Storage(e.to_string()))
        })
        .unwrap();
    let recovered = store
        .with_connection(|conn| sweep(conn, 101, &policy(1)))
        .unwrap();
    assert_eq!(recovered.purged, 1);
    assert!(store.get_by_id("recover-victim").unwrap().is_none());
    assert_eq!(
        store
            .with_connection(
                |conn| crate::consolidation::proposal_references::backfill_status(conn)
            )
            .unwrap()
            .blocked_proposal_id,
        None
    );
}

#[test]
fn blocked_oversized_legacy_row_recovers_after_delete_and_allows_later_purge() {
    let (_dir, _path, store) = store();
    store.with_connection(|conn| {
        insert_memory(conn, "delete-recover-victim", 1);
        conn.execute_batch("DROP TRIGGER consolidation_proposal_refs_insert; DROP TRIGGER consolidation_proposal_payload_limit_insert; DROP TRIGGER consolidation_proposal_reference_limit_insert;").unwrap();
        let oversized = format!("{{\"memory_id\":\"{}\"}}", "x".repeat(MAX_PROPOSAL_REFERENCE_PAYLOAD_BYTES));
        insert_job_and_proposal(conn, "delete-recover-oversized", None, "{}", "{}", &oversized);
        conn.execute("UPDATE consolidation_proposal_ref_backfill SET next_rowid=0,complete=0 WHERE id=1", []).unwrap();
        Ok(())
    }).unwrap();
    assert!(store
        .with_connection(|conn| sweep(conn, 100, &policy(1)))
        .is_err());
    store
        .with_connection(|conn| {
            conn.execute(
                "DELETE FROM consolidation_proposals WHERE proposal_id='delete-recover-oversized'",
                [],
            )
            .map(|_| ())
            .map_err(|e| crate::LatticeError::Storage(e.to_string()))
        })
        .unwrap();
    store
        .with_connection(|conn| {
            conn.execute(
                "UPDATE memory_retention_control SET next_sweep=0 WHERE id=1",
                [],
            )
            .map(|_| ())
            .map_err(|e| crate::LatticeError::Storage(e.to_string()))
        })
        .unwrap();
    let recovered = store
        .with_connection(|conn| sweep(conn, 101, &policy(1)))
        .unwrap();
    assert_eq!(recovered.purged, 1);
    assert!(store.get_by_id("delete-recover-victim").unwrap().is_none());
    assert_eq!(
        store
            .with_connection(
                |conn| crate::consolidation::proposal_references::backfill_status(conn)
            )
            .unwrap()
            .blocked_proposal_id,
        None
    );
}

#[test]
fn legacy_receipt_counter_backfills_64_ids_per_sweep_across_reopen() {
    let (_dir, path, store) = store();
    store
        .with_connection(|conn| {
            for index in 0..130 {
                conn.execute(
                    "INSERT INTO memory_deletion_receipts(memory_id,deleted_at) VALUES(?1,1)",
                    [format!("legacy-receipt-{index:03}")],
                )
                .unwrap();
            }
            conn.execute(
                "UPDATE memory_receipt_count_state SET cursor='',counted=0,complete=0 WHERE id=1",
                [],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
    assert!(store
        .with_connection(|conn| sweep(conn, 100, &policy(1)))
        .is_err());
    store
        .with_connection(|conn| {
            let (cursor, counted, complete) = receipt_count_state(conn);
            let prefix: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM memory_deletion_receipts WHERE memory_id<=?1",
                    [&cursor],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(counted, 64);
            assert_eq!(
                counted, prefix,
                "partial counter matches its durable cursor prefix"
            );
            assert!(!complete);
            Ok(())
        })
        .unwrap();
    drop(store);
    let reopened = MemoryStore::open(&path).unwrap();
    assert!(reopened
        .with_connection(|conn| sweep(conn, 101, &policy(1)))
        .is_err());
    let completed = reopened
        .with_connection(|conn| sweep(conn, 102, &policy(1)))
        .unwrap();
    assert!(!completed.skipped);
    reopened
        .with_connection(|conn| {
            let (_, counted, complete) = receipt_count_state(conn);
            assert!(complete);
            assert_eq!(counted, receipt_count_oracle(conn));
            assert_eq!(counted, 130);
            Ok(())
        })
        .unwrap();
}

#[test]
fn receipt_counter_triggers_track_before_after_cursor_and_upsert_without_double_counting() {
    let (_dir, path, store) = store();
    store
        .with_connection(|conn| {
            for index in 0..130 {
                conn.execute(
                    "INSERT INTO memory_deletion_receipts(memory_id,deleted_at) VALUES(?1,1)",
                    [format!("receipt-{index:03}")],
                )
                .unwrap();
            }
            conn.execute(
                "UPDATE memory_receipt_count_state SET cursor='',counted=0,complete=0 WHERE id=1",
                [],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
    assert!(store
        .with_connection(|conn| sweep(conn, 100, &policy(1)))
        .is_err());
    store.with_connection(|conn| {
        let (cursor, before, complete) = receipt_count_state(conn);
        assert!(!complete);
        conn.execute("INSERT INTO memory_deletion_receipts(memory_id,deleted_at) VALUES('a-before-cursor',1)", []).unwrap();
        conn.execute("INSERT INTO memory_deletion_receipts(memory_id,deleted_at) VALUES('z-after-cursor',1)", []).unwrap();
        let (_, after_inserts, _) = receipt_count_state(conn);
        assert_eq!(after_inserts, before + 1);
        conn.execute("DELETE FROM memory_deletion_receipts WHERE memory_id='receipt-000'", []).unwrap();
        conn.execute("DELETE FROM memory_deletion_receipts WHERE memory_id='receipt-100'", []).unwrap();
        let (_, after_deletes, _) = receipt_count_state(conn);
        assert_eq!(after_deletes, before);
        let prefix: i64 = conn.query_row("SELECT COUNT(*) FROM memory_deletion_receipts WHERE memory_id<=?1", [&cursor], |row| row.get(0)).unwrap();
        assert_eq!(after_deletes, prefix);
        Ok(())
    }).unwrap();
    drop(store);
    let reopened = MemoryStore::open(&path).unwrap();
    assert!(reopened
        .with_connection(|conn| sweep(conn, 101, &policy(1)))
        .is_err());
    let _ = reopened
        .with_connection(|conn| sweep(conn, 102, &policy(1)))
        .unwrap();
    reopened.with_connection(|conn| {
        let (_, counted, complete) = receipt_count_state(conn);
        assert!(complete);
        assert_eq!(counted, receipt_count_oracle(conn));
        conn.execute("INSERT INTO memory_deletion_receipts(memory_id,deleted_at) VALUES('stable-upsert',1)", []).unwrap();
        let (_, after_insert, _) = receipt_count_state(conn);
        conn.execute("INSERT INTO memory_deletion_receipts(memory_id,deleted_at) VALUES('stable-upsert',2) ON CONFLICT(memory_id) DO UPDATE SET deleted_at=excluded.deleted_at", []).unwrap();
        assert_eq!(receipt_count_state(conn).1, after_insert, "receipt UPSERT updates time without incrementing the singleton counter");
        conn.execute("DELETE FROM memory_deletion_receipts WHERE memory_id='stable-upsert'", []).unwrap();
        assert_eq!(receipt_count_state(conn).1, receipt_count_oracle(conn));
        Ok(())
    }).unwrap();
}

#[test]
fn pending_receipt_is_pinned_past_both_age_and_overflow_pruning() {
    let (_dir, _path, store) = store();
    let mut limited = policy(1);
    limited.max_receipts = 1;
    store
        .with_connection(|conn| {
            insert_memory(conn, "pinned-receipt", 1);
            for index in 0..(RETENTION_DEPENDENCY_DELETE_BATCH * 2 + 1) {
                insert_job_and_proposal(
                    conn,
                    &format!("pinned-{index:03}"),
                    Some("pinned-receipt"),
                    "{}",
                    "{}",
                    "{}",
                );
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(
        store
            .with_connection(|conn| sweep(conn, 100, &limited))
            .unwrap()
            .purged,
        0
    );
    store.with_connection(|conn| {
        conn.execute("INSERT INTO memory_deletion_receipts(memory_id,deleted_at) VALUES('old-unpinned',1)", []).unwrap();
        conn.execute("INSERT INTO memory_deletion_receipts(memory_id,deleted_at) VALUES(?1,?2)", params!["fresh-unpinned", 100 + MAX_REPLAY_AGE_SECS + 1]).unwrap();
        Ok(())
    }).unwrap();
    store
        .with_connection(|conn| {
            conn.execute(
                "UPDATE memory_retention_control SET next_sweep=0 WHERE id=1",
                [],
            )
            .map(|_| ())
            .map_err(|e| crate::LatticeError::Storage(e.to_string()))
        })
        .unwrap();
    let report = store
        .with_connection(|conn| sweep(conn, 100 + MAX_REPLAY_AGE_SECS + 1, &limited))
        .unwrap();
    assert!(
        report.receipts_expired >= 2,
        "age deletes old unpinned receipts and overflow deletes fresh ones"
    );
    store.with_connection(|conn| {
        let pinned: i64 = conn.query_row("SELECT COUNT(*) FROM memory_deletion_receipts WHERE memory_id='pinned-receipt'", [], |row| row.get(0)).unwrap();
        assert_eq!(pinned, 1);
        assert_eq!(receipt_count_state(conn).1, receipt_count_oracle(conn));
        Ok(())
    }).unwrap();
}

#[test]
fn receipt_overflow_prunes_one_indexed_candidate_with_bounded_vm_work() {
    let (_dir, _path, store) = store();
    let mut limited = policy(1);
    limited.max_receipts = 1;
    store
        .with_connection(|conn| {
            for index in 0..3_000 {
                conn.execute(
                    "INSERT INTO memory_deletion_receipts(memory_id,deleted_at) VALUES(?1,100)",
                    [format!("overflow-{index:04}")],
                )
                .unwrap();
            }
            Ok(())
        })
        .unwrap();
    let ticks = Arc::new(AtomicUsize::new(0));
    let observer = ticks.clone();
    let report = store
        .with_connection(|conn| {
            conn.progress_handler(
                100,
                Some(move || {
                    observer.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            );
            let result = sweep(conn, 100, &limited);
            conn.progress_handler(0, None::<fn() -> bool>);
            result
        })
        .unwrap();
    assert_eq!(report.receipts_expired, 1);
    assert!(
        ticks.load(Ordering::Relaxed) * 100 <= 250_000,
        "overflow pruning must select a bounded indexed candidate without a full anti-join"
    );
    store
        .with_connection(|conn| {
            assert_eq!(receipt_count_state(conn).1, receipt_count_oracle(conn));
            assert_eq!(receipt_count_oracle(conn), 2_999);
            Ok(())
        })
        .unwrap();
}

#[test]
fn admission_projection_ignores_completed_and_foreign_rows_with_bounded_vm_work() {
    let (_dir, _path, store) = store();
    store.with_connection(|conn| {
        for index in 0..1_500 {
            let id = format!("completed-{index:04}");
            conn.execute("INSERT INTO consolidation_jobs(job_id,workspace_id,kind,mode,status,enqueued_at) VALUES(?1,'workspace','retention','background','proposed',1)", [format!("job-{id}")]).unwrap();
            conn.execute("INSERT INTO consolidation_proposals(proposal_id,job_id,proposal_kind,prior_state,proposed_state,evidence,decision) VALUES(?1,?2,'update_memory','{}','{\"scope\":\"repo\"}','{\"repository_id\":\"workspace\"}','applied')", params![id, format!("job-{id}")]).unwrap();
        }
        for index in 0..1_500 {
            let id = format!("foreign-{index:04}");
            conn.execute("INSERT INTO consolidation_jobs(job_id,workspace_id,kind,mode,status,enqueued_at) VALUES(?1,'foreign-workspace','retention','background','proposed',1)", [format!("job-{id}")]).unwrap();
            conn.execute("INSERT INTO consolidation_proposals(proposal_id,job_id,proposal_kind,prior_state,proposed_state,evidence,decision) VALUES(?1,?2,'update_memory','{}','{\"scope\":\"repo\"}','{\"repository_id\":\"foreign\"}','pending')", params![id, format!("job-{id}")]).unwrap();
        }
        for index in 0..3 {
            let id = format!("local-{index}");
            conn.execute("INSERT INTO consolidation_jobs(job_id,workspace_id,kind,mode,status,enqueued_at) VALUES(?1,'workspace','retention','background','proposed',1)", [format!("job-{id}")]).unwrap();
            conn.execute("INSERT INTO consolidation_proposals(proposal_id,job_id,proposal_kind,prior_state,proposed_state,evidence,decision) VALUES(?1,?2,'update_memory','{}','{\"scope\":\"repo\"}','{\"repository_id\":\"workspace\"}','pending')", params![id, format!("job-{id}")]).unwrap();
        }
        Ok(())
    }).unwrap();
    let ticks = Arc::new(AtomicUsize::new(0));
    let observer = ticks.clone();
    let count = store
        .with_connection(|conn| {
            conn.progress_handler(
                100,
                Some(move || {
                    observer.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            );
            let result = pending_manual_review_count_up_to(conn, "workspace", 8);
            conn.progress_handler(0, None::<fn() -> bool>);
            result
        })
        .unwrap();
    assert_eq!(count, 3);
    assert!(ticks.load(Ordering::Relaxed) * 100 <= 250_000);
    store
        .with_connection(|conn| {
            conn.execute(
                "UPDATE consolidation_proposals SET decision='applied' WHERE proposal_id='local-0'",
                [],
            )
            .map(|_| ())
            .map_err(|e| crate::LatticeError::Storage(e.to_string()))
        })
        .unwrap();
    assert_eq!(
        store
            .with_connection(|conn| pending_manual_review_count_up_to(conn, "workspace", 8))
            .unwrap(),
        2
    );
}
