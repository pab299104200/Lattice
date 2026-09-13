//! Bounded consumption of durable verification requests. This never runs checks
//! or acknowledges memory delivery; behavioral observations require explicit
//! trusted execution through the separate runner.
use super::verify_explain_memory;
use lattice_core::{
    indexer::Indexer, memory::MemoryStore, storage::GraphStore, verification::ScopeFilter,
};
use rusqlite::params;
use std::{collections::HashMap, path::Path};

pub(crate) fn drain(
    store: &MemoryStore,
    indexer: &Indexer,
    graph: &GraphStore,
    root: &Path,
    scope: &ScopeFilter,
    checkout_id: &str,
    now_micros: i64,
) -> Result<usize, String> {
    if scope.workspace_id.trim().is_empty() || checkout_id.trim().is_empty() {
        return Err(
            "Verification queue requires explicit repository and checkout authority".to_string(),
        );
    }
    let branch = scope.branch.as_ref().map(|branch| branch.name.as_str());
    let candidates = store.with_connection(|connection| {
        let mut statement = connection.prepare(
            "SELECT j.job_id,j.target_memory_id FROM verification_jobs j JOIN memories m ON m.id=j.target_memory_id
             WHERE j.workspace_id=?1 AND m.workspace_id=?1
               AND m.is_invalidated=0 AND (m.applicable_checkout_id IS NULL OR m.applicable_checkout_id=?5)
               AND (m.scope='repo' OR (m.scope='branch' AND m.branch=?2) OR (m.scope='session' AND m.session_id=?3))
               AND (j.status='queued' OR (j.status='running' AND j.started_at < ?4))
             ORDER BY j.queued_at,j.job_id LIMIT 8",
        ).map_err(sql_error)?;
        let rows = statement.query_map(params![scope.workspace_id,branch,scope.session_id,now_micros.saturating_sub(300_000_000),checkout_id], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))
            .map_err(sql_error)?.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_error)?;
        Ok(rows)
    }).map_err(|error| error.to_string())?;
    let mut processed = 0;
    for (job, memory) in candidates {
        let claimed = store.with_connection(|connection| connection.execute(
            "UPDATE verification_jobs SET status='running',started_at=?2,finished_at=NULL WHERE job_id=?1 AND (status='queued' OR (status='running' AND started_at < ?3))",
            params![job,now_micros,now_micros.saturating_sub(300_000_000)],
        ).map_err(sql_error)).map_err(|error| error.to_string())?;
        if claimed == 0 {
            continue;
        }
        let args = verify_explain_memory::parse_args(
            &serde_json::json!({"memory_id": memory,"mode":"verify"}),
        )?;
        // Reports are ephemeral here: maintenance must not grow a presentation
        // cache or create a delivery receipt as a side effect of verification.
        let result = verify_explain_memory::execute_with_behavioral_validations(
            store,
            indexer,
            graph,
            root,
            scope,
            &mut HashMap::new(),
            args,
            &[],
            Some(&scope.workspace_id),
            Some(checkout_id),
            None,
            None,
        );
        let (state, verdict, reason) = match &result {
            Ok(execution) => ("completed", Some(execution.report.status.as_str()), None),
            Err(error) => ("failed", None, Some(error.as_str())),
        };
        store.with_connection(|connection| connection.execute(
            "UPDATE verification_jobs SET status=?3,verdict=?4,reason=?5,finished_at=?2 WHERE job_id=?1 AND status='running' AND started_at=?2",
            params![job,now_micros,state,verdict,reason],
        ).map_err(sql_error)).map_err(|error| error.to_string())?;
        processed += 1;
    }
    Ok(processed)
}

fn sql_error(error: rusqlite::Error) -> lattice_core::LatticeError {
    lattice_core::LatticeError::Storage(format!("verification queue: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lattice_core::events::BranchRef;

    #[test]
    fn queue_is_bounded_scoped_and_does_not_record_recall() {
        let root = tempfile::tempdir().unwrap();
        let store = MemoryStore::open_in_memory().unwrap();
        let indexer = Indexer::new(root.path().to_path_buf());
        let graph = GraphStore::open_in_memory().unwrap();
        for number in 0..12 {
            let id = format!("queued-{number}");
            store.with_connection(|connection| {
                connection.execute("INSERT INTO memories(id,content,memory_type,scope,workspace_id,created_at,last_accessed,access_count) VALUES(?1,'assertion','observation','repo','repo',1,0,0)",[&id]).map_err(sql_error)?;
                Ok(())
            }).unwrap();
            store.enqueue_verification_job("repo", &id).unwrap();
        }
        store.with_connection(|connection| {
            connection.execute("INSERT INTO memories(id,content,memory_type,scope,workspace_id,branch,created_at,last_accessed,access_count) VALUES('wrong-branch','assertion','observation','branch','repo','other',1,0,0)",[]).map_err(sql_error)?;
            Ok(())
        }).unwrap();
        store
            .enqueue_verification_job("repo", "wrong-branch")
            .unwrap();
        let scope = ScopeFilter::new(
            "repo",
            Some(BranchRef {
                name: "main".into(),
            }),
            None,
        );
        assert_eq!(
            drain(
                &store,
                &indexer,
                &graph,
                root.path(),
                &scope,
                "checkout",
                1_000_000
            )
            .unwrap(),
            8
        );
        assert_eq!(
            drain(
                &store,
                &indexer,
                &graph,
                root.path(),
                &scope,
                "checkout",
                2_000_000
            )
            .unwrap(),
            4
        );
        assert_eq!(
            drain(
                &store,
                &indexer,
                &graph,
                root.path(),
                &scope,
                "checkout",
                3_000_000
            )
            .unwrap(),
            0
        );
        store
            .with_connection(|connection| {
                assert_eq!(
                    connection
                        .query_row(
                            "SELECT COUNT(*) FROM verification_jobs WHERE status='completed'",
                            [],
                            |row| row.get::<_, usize>(0)
                        )
                        .unwrap(),
                    12
                );
                assert_eq!(
                    connection
                        .query_row(
                            "SELECT COUNT(*) FROM verification_jobs WHERE status='queued'",
                            [],
                            |row| row.get::<_, usize>(0)
                        )
                        .unwrap(),
                    1
                );
                assert_eq!(
                    connection
                        .query_row("SELECT SUM(access_count) FROM memories", [], |row| row
                            .get::<_, usize>(0))
                        .unwrap(),
                    0
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn abandoned_running_job_is_retried_only_after_lease_deadline() {
        let root = tempfile::tempdir().unwrap();
        let store = MemoryStore::open_in_memory().unwrap();
        store.with_connection(|connection| {
            connection.execute("INSERT INTO memories(id,content,memory_type,scope,workspace_id,created_at,last_accessed,access_count) VALUES('retry','assertion','observation','repo','repo',1,0,0)",[]).map_err(sql_error)?;
            Ok(())
        }).unwrap();
        let job = store.enqueue_verification_job("repo", "retry").unwrap();
        store.with_connection(|connection| {
            connection.execute("UPDATE verification_jobs SET status='running',started_at=1 WHERE job_id=?1",[job]).map_err(sql_error)?;
            Ok(())
        }).unwrap();
        let indexer = Indexer::new(root.path().to_path_buf());
        let graph = GraphStore::open_in_memory().unwrap();
        let scope = ScopeFilter::new("repo", None, None);
        assert_eq!(
            drain(
                &store,
                &indexer,
                &graph,
                root.path(),
                &scope,
                "checkout",
                300_000_001
            )
            .unwrap(),
            0
        );
        assert_eq!(
            drain(
                &store,
                &indexer,
                &graph,
                root.path(),
                &scope,
                "checkout",
                300_000_002
            )
            .unwrap(),
            1
        );
    }
    #[test]
    fn queue_preserves_foreign_checkout_jobs_and_verifies_current_checkout() {
        let root = tempfile::tempdir().unwrap();
        let store = MemoryStore::open_in_memory().unwrap();
        let indexer = Indexer::new(root.path().to_path_buf());
        let graph = GraphStore::open_in_memory().unwrap();
        for (id, checkout) in [("local", "checkout"), ("foreign", "elsewhere")] {
            store.with_connection(|conn| {
                conn.execute("INSERT INTO memories(id,content,memory_type,scope,workspace_id,applicable_checkout_id,created_at,last_accessed,access_count) VALUES(?1,'assertion','observation','repo','repo',?2,1,0,0)", params![id,checkout]).map_err(sql_error)?;
                Ok(())
            }).unwrap();
            store.enqueue_verification_job("repo", id).unwrap();
        }
        let scope = ScopeFilter::new("repo", None, None);
        assert!(drain(&store, &indexer, &graph, root.path(), &scope, "", 1_000_000).is_err());
        assert_eq!(
            drain(
                &store,
                &indexer,
                &graph,
                root.path(),
                &scope,
                "checkout",
                1_000_000
            )
            .unwrap(),
            1
        );
        store.with_connection(|conn| {
            for (id, expected) in [("local", "completed"), ("foreign", "queued")] {
                let (status, recalled): (String, Option<i64>) = conn.query_row(
                    "SELECT j.status,m.last_recalled_at FROM verification_jobs j JOIN memories m ON m.id=j.target_memory_id WHERE m.id=?1",
                    [id], |row| Ok((row.get(0)?,row.get(1)?)),
                ).map_err(sql_error)?;
                let reason: Option<String> = conn.query_row(
                    "SELECT reason FROM verification_jobs WHERE target_memory_id=?1", [id], |row| row.get(0),
                ).map_err(sql_error)?;
                assert_eq!(status, expected, "{id}: {reason:?}");
                assert_eq!(recalled, None);
            }
            Ok(())
        }).unwrap();
    }
}
