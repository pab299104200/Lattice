//! Transactional replacement of historical repository authorities.
use crate::error::LatticeError;
use rusqlite::{params, Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct IdentityMigrationReport {
    pub migrated_memories: usize,
    pub migrated_dependents: usize,
    pub migrated_serialized_states: usize,
    pub isolated_identities: Vec<(String, usize)>,
    pub before_checksum: String,
    pub after_checksum: String,
}

/// `proven_ids` must come exclusively from canonical local Git metadata. Call
/// this while holding the repository initialization lease.
pub fn migrate_identities(
    conn: &Connection,
    repository_id: &str,
    proven_ids: &BTreeSet<String>,
) -> Result<IdentityMigrationReport, LatticeError> {
    if !repository_id.starts_with("repo_") || repository_id.len() != 69 {
        return Err(LatticeError::Storage(
            "identity migration requires a canonical Git repository id".into(),
        ));
    }
    (|| -> rusqlite::Result<_> {
        let tx=rusqlite::Transaction::new_unchecked(conn,TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS memory_identity_migrations(old_identity TEXT NOT NULL,repository_id TEXT NOT NULL,table_name TEXT NOT NULL,row_key TEXT NOT NULL,before_hash TEXT NOT NULL,after_hash TEXT NOT NULL,migrated_at INTEGER NOT NULL DEFAULT(unixepoch()),PRIMARY KEY(old_identity,repository_id,table_name,row_key)); CREATE TABLE IF NOT EXISTS memory_identity_migration_runs(repository_id TEXT NOT NULL,proof_hash TEXT NOT NULL,before_checksum TEXT NOT NULL,after_checksum TEXT NOT NULL,migrated_memories INTEGER NOT NULL,migrated_dependents INTEGER NOT NULL,migrated_serialized_states INTEGER NOT NULL,completed_at INTEGER NOT NULL DEFAULT(unixepoch()),PRIMARY KEY(repository_id,proof_hash));")?;
        let old=proven_ids.iter().filter(|v|v.as_str()!=repository_id).cloned().collect::<BTreeSet<_>>();
        let before=authority_checksum(&tx)?; let mut report=IdentityMigrationReport{before_checksum:before,..Default::default()};
        for old_id in &old { for &(table,column,key) in columns() {
            if !has_column(&tx,table,column)? {continue}
            let keys=tx.prepare(&format!("SELECT CAST({key} AS TEXT) FROM {table} WHERE {column}=?1 ORDER BY {key}"))?.query_map([old_id],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for row_key in keys { let bh=fingerprint(table,&row_key,old_id); let ah=fingerprint(table,&row_key,repository_id); tx.execute(&format!("UPDATE {table} SET {column}=?1 WHERE {key}=?2 AND {column}=?3"),params![repository_id,row_key,old_id])?; tx.execute("INSERT OR IGNORE INTO memory_identity_migrations VALUES(?1,?2,?3,?4,?5,?6,unixepoch())",params![old_id,repository_id,table,row_key,bh,ah])?; if table=="memories"{report.migrated_memories+=1}else{report.migrated_dependents+=1} }
        }}
        report.migrated_serialized_states+=rewrite_json(&tx,"working_memory_checkpoints","checkpoint_id","state_json",Some("state_hash"),&old,repository_id)?;
        for column in ["prior_state","proposed_state"] { report.migrated_serialized_states+=rewrite_json(&tx,"consolidation_proposals","proposal_id",column,None,&old,repository_id)?; }
        if has_column(&tx,"consolidation_jobs","workspace_id")? { tx.execute("UPDATE consolidation_proposals SET decision='rejected',decision_reason='historical repository identity migrated; regenerate under current authority',decided_at=unixepoch() WHERE decision='pending' AND job_id IN(SELECT job_id FROM consolidation_jobs WHERE workspace_id=?1)",[repository_id])?; }
        report.isolated_identities=tx.prepare("SELECT workspace_id,COUNT(*) FROM memories WHERE workspace_id IS NOT NULL AND workspace_id!=?1 GROUP BY workspace_id ORDER BY workspace_id")?.query_map([repository_id],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        report.after_checksum=authority_checksum(&tx)?; let proof=hash(old.iter().flat_map(|v|v.as_bytes()).copied().collect::<Vec<_>>().as_slice());
        tx.execute("INSERT OR IGNORE INTO memory_identity_migration_runs(repository_id,proof_hash,before_checksum,after_checksum,migrated_memories,migrated_dependents,migrated_serialized_states)VALUES(?1,?2,?3,?4,?5,?6,?7)",params![repository_id,proof,report.before_checksum,report.after_checksum,report.migrated_memories as i64,report.migrated_dependents as i64,report.migrated_serialized_states as i64])?;
        tx.commit()?; Ok(report)
    })().map_err(|e|LatticeError::Storage(format!("historical identity migration rolled back: {e}")))
}

fn columns() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        ("memories", "workspace_id", "id"),
        ("consolidation_jobs", "workspace_id", "job_id"),
        ("verification_jobs", "workspace_id", "job_id"),
        (
            "working_memory_checkpoints",
            "workspace_id",
            "checkpoint_id",
        ),
        (
            "working_memory_checkpoint_memory_refs",
            "memory_id",
            "checkpoint_id",
        ),
        (
            "consolidation_capture_receipts",
            "repository_id",
            "delivery_key",
        ),
        ("session_digest_deliveries", "repository_id", "delivery_key"),
        (
            "session_capture_tombstones",
            "repository_id",
            "delivery_key",
        ),
        (
            "memory_scope_filter_events",
            "attempted_workspace_id",
            "event_id",
        ),
    ]
}
fn has_column(c: &Connection, t: &str, col: &str) -> rusqlite::Result<bool> {
    if !c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [t],
        |r| r.get(0),
    )? {
        return Ok(false);
    }
    let v = c
        .prepare(&format!("PRAGMA table_info({t})"))?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(v.iter().any(|x| x == col))
}
fn rewrite_json(
    c: &Connection,
    t: &str,
    key: &str,
    col: &str,
    hash_col: Option<&str>,
    old: &BTreeSet<String>,
    new: &str,
) -> rusqlite::Result<usize> {
    if !has_column(c, t, col)? {
        return Ok(0);
    }
    let rows = c
        .prepare(&format!(
            "SELECT CAST({key} AS TEXT),{col} FROM {t} ORDER BY {key}"
        ))?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut n = 0;
    for (k, s) in rows {
        let mut v: serde_json::Value = serde_json::from_str(&s)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        if rewrite(&mut v, old, new) {
            let encoded = serde_json::to_string(&v)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            if let Some(h) = hash_col {
                c.execute(
                    &format!("UPDATE {t} SET {col}=?1,{h}=?2 WHERE {key}=?3"),
                    params![encoded, hash(encoded.as_bytes()), k],
                )?;
            } else {
                c.execute(
                    &format!("UPDATE {t} SET {col}=?1 WHERE {key}=?2"),
                    params![encoded, k],
                )?;
            }
            n += 1
        }
    }
    Ok(n)
}
fn rewrite(v: &mut serde_json::Value, old: &BTreeSet<String>, new: &str) -> bool {
    rewrite_at_key(v, old, new, None)
}
fn rewrite_at_key(
    v: &mut serde_json::Value,
    old: &BTreeSet<String>,
    new: &str,
    key: Option<&str>,
) -> bool {
    match v {
        serde_json::Value::String(s)
            if key.is_some_and(|key| {
                matches!(key, "authority" | "workspace_id" | "repository_id")
            }) && old.contains(s) =>
        {
            *s = new.into();
            true
        }
        serde_json::Value::Array(a) => a
            .iter_mut()
            .fold(false, |x, v| rewrite_at_key(v, old, new, key) || x),
        serde_json::Value::Object(m) => m.iter_mut().fold(false, |x, (key, v)| {
            rewrite_at_key(v, old, new, Some(key)) || x
        }),
        _ => false,
    }
}
fn authority_checksum(c: &Connection) -> rusqlite::Result<String> {
    let mut b = Vec::new();
    for &(t, col, key) in columns() {
        if !has_column(c, t, col)? {
            continue;
        }
        let rows = c
            .prepare(&format!(
                "SELECT CAST({key} AS TEXT),COALESCE({col},'') FROM {t} ORDER BY {key}"
            ))?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (k, v) in rows {
            b.extend_from_slice(format!("{t}\0{k}\0{v}\n").as_bytes())
        }
    }
    Ok(hash(&b))
}
fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn fingerprint(t: &str, k: &str, i: &str) -> String {
    hash(format!("{t}\0{k}\0{i}").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;
    #[test]
    fn atomic_idempotent_and_foreign_isolated() {
        let s = MemoryStore::open_in_memory().unwrap();
        let repo = format!("repo_{}", "a".repeat(64));
        s.with_connection(|c|{c.execute("INSERT INTO memories(id,content,memory_type,scope,workspace_id,branch,created_at,last_accessed,access_count)VALUES('owned','sentinel','pattern','branch','/repo/.git','feature',1,1,0),('foreign','private','pattern','branch','/foreign/.git','main',1,1,0)",[]).unwrap();let p=BTreeSet::from(["/repo/.git".into()]);let r=migrate_identities(c,&repo,&p)?;assert_eq!(r.migrated_memories,1);assert_eq!(r.isolated_identities,vec![("/foreign/.git".into(),1)]);assert_eq!(migrate_identities(c,&repo,&p)?.migrated_memories,0);Ok(())}).unwrap()
    }
    #[test]
    fn rollback_then_restart() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("m.db");
        let repo = format!("repo_{}", "b".repeat(64));
        let s = MemoryStore::open(&p).unwrap();
        s.with_connection(|c|{c.execute("INSERT INTO memories(id,content,memory_type,workspace_id,created_at,last_accessed,access_count)VALUES('a','x','fact','/repo',1,1,0)",[]).unwrap();c.execute_batch("CREATE TRIGGER fail_identity BEFORE UPDATE OF workspace_id ON memories BEGIN SELECT RAISE(ABORT,'injected');END;").unwrap();assert!(migrate_identities(c,&repo,&BTreeSet::from(["/repo".into()])).is_err());assert_eq!(c.query_row::<String,_,_>("SELECT workspace_id FROM memories",[],|r|r.get(0)).unwrap(),"/repo");c.execute_batch("DROP TRIGGER fail_identity").unwrap();Ok(())}).unwrap();
        drop(s);
        let s = MemoryStore::open(&p).unwrap();
        s.with_connection(|c| {
            assert_eq!(
                migrate_identities(c, &repo, &BTreeSet::from(["/repo".into()]))?.migrated_memories,
                1
            );
            Ok(())
        })
        .unwrap()
    }
    #[test]
    fn checkpoint_and_pending_proposal_authority_is_rewritten_or_isolated() {
        let s = MemoryStore::open_in_memory().unwrap();
        let repo = format!("repo_{}", "c".repeat(64));
        let old = "/repo/.git";
        s.with_connection(|c| { let state=serde_json::json!({"authority":old,"nested":[old,"keep"]}).to_string(); c.execute("INSERT INTO working_memory_checkpoints(workspace_id,session_id,task_id,checkpoint_name,created_at,state_version,state_json,state_hash)VALUES(?1,'s','t','c',1,1,?2,'old-hash')",params![old,state]).unwrap(); c.execute("INSERT INTO consolidation_jobs(job_id,workspace_id,kind,mode,status,enqueued_at)VALUES('j',?1,'refresh','manual_review','proposed',1)",[old]).unwrap(); c.execute("INSERT INTO consolidation_proposals(proposal_id,job_id,proposal_kind,prior_state,proposed_state,evidence,decision)VALUES('p','j','refresh',?1,?1,'[]','pending')",[serde_json::json!({"workspace_id":old}).to_string()]).unwrap(); let r=migrate_identities(c,&repo,&BTreeSet::from([old.into()]))?; assert!(r.migrated_serialized_states>=3); let (workspace,json,digest):(String,String,String)=c.query_row("SELECT workspace_id,state_json,state_hash FROM working_memory_checkpoints",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap(); assert_eq!(workspace,repo); assert!(json.contains(&repo)); assert_eq!(digest,hash(json.as_bytes())); let decision:String=c.query_row("SELECT decision FROM consolidation_proposals",[],|r|r.get(0)).unwrap(); assert_eq!(decision,"rejected"); Ok(()) }).unwrap();
    }
}
