use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;

use crate::LatticeError;

pub const MAX_PROPOSAL_REFERENCE_PAYLOAD_BYTES: usize = 1024 * 1024;
pub const PROPOSAL_REFERENCE_BACKFILL_ROWS_PER_PASS: usize = 64;
pub const PROPOSAL_REFERENCE_BACKFILL_BYTES_PER_PASS: usize = 4 * 1024 * 1024;
pub const MAX_PROPOSAL_MEMORY_REFERENCES: usize = 4096;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProposalReferenceBackfillReport {
    pub rows_processed: usize,
    pub payload_bytes_processed: usize,
    pub complete: bool,
    pub blocked_proposal_id: Option<String>,
    pub blocked_payload_bytes: Option<usize>,
}

pub fn validate_proposal_payload(
    prior: &Value,
    proposed: &Value,
    evidence: &Value,
) -> Result<(), LatticeError> {
    let bytes = serde_json::to_vec(prior)
        .map_err(json_error)?
        .len()
        .saturating_add(serde_json::to_vec(proposed).map_err(json_error)?.len())
        .saturating_add(serde_json::to_vec(evidence).map_err(json_error)?.len());
    if bytes > MAX_PROPOSAL_REFERENCE_PAYLOAD_BYTES {
        return Err(LatticeError::Storage(format!(
            "consolidation proposal payload is {bytes} bytes; maximum is {MAX_PROPOSAL_REFERENCE_PAYLOAD_BYTES}"
        )));
    }
    let mut references = Vec::new();
    collect_strings(prior, &mut references, true);
    collect_strings(proposed, &mut references, true);
    collect_strings(evidence, &mut references, false);
    if references.len() > MAX_PROPOSAL_MEMORY_REFERENCES {
        return Err(LatticeError::Storage(format!(
            "consolidation proposal has {} memory references; maximum is {MAX_PROPOSAL_MEMORY_REFERENCES}", references.len()
        )));
    }
    Ok(())
}

pub fn advance_reference_backfill(
    c: &Connection,
) -> Result<ProposalReferenceBackfillReport, LatticeError> {
    let tx =
        rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate).map_err(storage)?;
    let (cursor, complete, blocked, blocked_bytes): (i64, bool, Option<String>, Option<i64>) = tx.query_row(
        "SELECT next_rowid,complete,blocked_proposal_id,blocked_payload_bytes FROM consolidation_proposal_ref_backfill WHERE id=1",
        [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
    ).map_err(storage)?;
    if complete {
        tx.commit().map_err(storage)?;
        return Ok(ProposalReferenceBackfillReport {
            complete,
            blocked_proposal_id: blocked,
            blocked_payload_bytes: blocked_bytes.and_then(|v| usize::try_from(v).ok()),
            ..Default::default()
        });
    }
    if let Some(id) = blocked {
        let current_bytes:Option<usize>=tx.query_row("SELECT octet_length(prior_state)+octet_length(proposed_state)+octet_length(evidence) FROM consolidation_proposals WHERE proposal_id=?1",[&id],|r|r.get(0)).optional().map_err(storage)?;
        let remediated=match current_bytes { None=>true, Some(bytes) if bytes<=MAX_PROPOSAL_REFERENCE_PAYLOAD_BYTES=>tx.query_row("SELECT json_valid(prior_state) AND json_valid(proposed_state) AND json_valid(evidence) FROM consolidation_proposals WHERE proposal_id=?1",[&id],|r|r.get(0)).map_err(storage)?, Some(_)=>false };
        if !remediated {
            tx.commit().map_err(storage)?;
            return Ok(ProposalReferenceBackfillReport {
                blocked_proposal_id: Some(id),
                blocked_payload_bytes: blocked_bytes.and_then(|v| usize::try_from(v).ok()),
                ..Default::default()
            });
        }
        tx.execute("UPDATE consolidation_proposal_ref_backfill SET blocked_proposal_id=NULL,blocked_payload_bytes=NULL,last_error=NULL WHERE id=1",[]).map_err(storage)?;
    }
    let sql="SELECT rowid,proposal_id,target_memory_id,octet_length(prior_state),octet_length(proposed_state),octet_length(evidence) FROM consolidation_proposals WHERE rowid>?1 ORDER BY rowid LIMIT ?2";
    let mut statement = tx.prepare(&sql).map_err(storage)?;
    let mut rows = statement
        .query(params![
            cursor,
            PROPOSAL_REFERENCE_BACKFILL_ROWS_PER_PASS as i64
        ])
        .map_err(storage)?;
    let mut report = ProposalReferenceBackfillReport::default();
    let mut next = cursor;
    let mut blocked_row: Option<(String, usize, String)> = None;
    while let Some(row) = rows.next().map_err(storage)? {
        let rowid: i64 = row.get(0).map_err(storage)?;
        let proposal_id: String = row.get(1).map_err(storage)?;
        let target: Option<String> = row.get(2).map_err(storage)?;
        let bytes = row
            .get::<_, usize>(3)
            .map_err(storage)?
            .saturating_add(row.get::<_, usize>(4).map_err(storage)?)
            .saturating_add(row.get::<_, usize>(5).map_err(storage)?);
        if bytes > MAX_PROPOSAL_REFERENCE_PAYLOAD_BYTES {
            let message = format!("legacy consolidation proposal '{proposal_id}' is {bytes} bytes; purge is blocked until it is remediated below {MAX_PROPOSAL_REFERENCE_PAYLOAD_BYTES} bytes");
            blocked_row = Some((proposal_id, bytes, message));
            break;
        }
        if report.rows_processed > 0
            && report.payload_bytes_processed.saturating_add(bytes)
                > PROPOSAL_REFERENCE_BACKFILL_BYTES_PER_PASS
        {
            break;
        }
        let (prior,proposed,evidence):(Vec<u8>,Vec<u8>,Vec<u8>)=tx.query_row("SELECT CAST(prior_state AS BLOB),CAST(proposed_state AS BLOB),CAST(evidence AS BLOB) FROM consolidation_proposals WHERE rowid=?1",[rowid],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(storage)?;
        let parsed = match (
            serde_json::from_slice::<Value>(&prior),
            serde_json::from_slice::<Value>(&proposed),
            serde_json::from_slice::<Value>(&evidence),
        ) {
            (Ok(prior), Ok(proposed), Ok(evidence)) => (prior, proposed, evidence),
            values => {
                let detail = [values.0.err(), values.1.err(), values.2.err()]
                    .into_iter()
                    .flatten()
                    .next()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "unknown JSON error".into());
                let message=format!("legacy consolidation proposal '{proposal_id}' contains malformed JSON; purge is blocked: {detail}");
                blocked_row = Some((proposal_id, bytes, message));
                break;
            }
        };
        let (prior, proposed, evidence) = parsed;
        let workspace:String=tx.query_row("SELECT j.workspace_id FROM consolidation_jobs j JOIN consolidation_proposals p ON p.job_id=j.job_id WHERE p.proposal_id=?1",[&proposal_id],|r|r.get(0)).map_err(storage)?;
        let authority = evidence.get("repository_id").and_then(Value::as_str);
        let scope = proposed
            .pointer("/memory/scope")
            .or_else(|| proposed.get("scope"))
            .or_else(|| prior.pointer("/memory/scope"))
            .or_else(|| prior.get("scope"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        tx.execute(
            "DELETE FROM consolidation_proposal_admission WHERE proposal_id=?1",
            [&proposal_id],
        )
        .map_err(storage)?;
        tx.execute("INSERT INTO consolidation_proposal_admission(proposal_id,workspace_id,authority_repository_id,manual_review) SELECT ?1,?2,?3,?4 WHERE EXISTS(SELECT 1 FROM consolidation_proposals WHERE proposal_id=?1 AND decision='pending')",params![proposal_id,workspace,authority,matches!(scope.as_str(),"repo"|"organization")]).map_err(storage)?;
        tx.execute(
            "DELETE FROM consolidation_proposal_memory_refs WHERE proposal_id=?1",
            [&proposal_id],
        )
        .map_err(storage)?;
        let mut reference_count = usize::from(target.is_some());
        if let Some(id) = target {
            insert_ref(&tx, &proposal_id, &id, "target")?;
        }
        for (kind, value) in [
            ("prior_state", &prior),
            ("proposed_state", &proposed),
            ("evidence", &evidence),
        ] {
            let mut refs = Vec::new();
            collect_strings(value, &mut refs, kind != "evidence");
            reference_count = reference_count.saturating_add(refs.len());
            if reference_count > MAX_PROPOSAL_MEMORY_REFERENCES {
                let message=format!("legacy consolidation proposal '{proposal_id}' has {reference_count} memory references; purge is blocked until it is reduced below {MAX_PROPOSAL_MEMORY_REFERENCES}");
                blocked_row = Some((proposal_id, bytes, message));
                break;
            }
            for id in refs {
                insert_ref(&tx, &proposal_id, id, kind)?;
            }
        }
        if blocked_row.is_some() {
            break;
        }
        next = rowid;
        report.rows_processed += 1;
        report.payload_bytes_processed += bytes;
    }
    drop(rows);
    drop(statement);
    if let Some((proposal_id, bytes, message)) = blocked_row {
        tx.execute("UPDATE consolidation_proposal_ref_backfill SET blocked_proposal_id=?1,blocked_payload_bytes=?2,last_error=?3 WHERE id=1",params![proposal_id,bytes as i64,message]).map_err(storage)?;
        report.blocked_proposal_id = Some(proposal_id);
        report.blocked_payload_bytes = Some(bytes);
        tx.commit().map_err(storage)?;
        return Ok(report);
    }
    let more: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM consolidation_proposals WHERE rowid>?1)",
            [next],
            |r| r.get(0),
        )
        .map_err(storage)?;
    report.complete = !more;
    tx.execute("UPDATE consolidation_proposal_ref_backfill SET next_rowid=?1,complete=?2,last_error=NULL WHERE id=1",params![next,report.complete]).map_err(storage)?;
    tx.commit().map_err(storage)?;
    Ok(report)
}

fn collect_strings<'a>(v: &'a Value, out: &mut Vec<&'a str>, include_root_id: bool) {
    collect_semantic(v, out, include_root_id, None)
}
fn collect_semantic<'a>(v: &'a Value, out: &mut Vec<&'a str>, root: bool, parent: Option<&str>) {
    match v {
        Value::String(s) if parent.is_some_and(reference_key) => out.push(s),
        Value::Array(a) => a
            .iter()
            .for_each(|v| collect_semantic(v, out, false, parent)),
        Value::Object(o) => {
            for (key, value) in o {
                if (root || parent == Some("memory")) && key == "id" {
                    if let Some(s) = value.as_str() {
                        out.push(s)
                    }
                } else {
                    collect_semantic(value, out, false, Some(key));
                }
            }
        }
        _ => {}
    }
}
fn reference_key(key: &str) -> bool {
    matches!(
        key,
        "memory_id"
            | "memory_ids"
            | "source_memory_id"
            | "source_memory_ids"
            | "replacement_memory_id"
            | "supersedes_memory_id"
            | "superseded_by_memory_id"
            | "contradicts_memory_ids"
            | "contradicted_by_memory_ids"
            | "linked_memories"
            | "source"
            | "target"
    )
}
fn insert_ref(c: &Connection, p: &str, m: &str, k: &str) -> Result<(), LatticeError> {
    c.execute("INSERT OR IGNORE INTO consolidation_proposal_memory_refs(proposal_id,memory_id,reference_kind)VALUES(?1,?2,?3)",params![p,m,k]).map(|_|()).map_err(storage)
}
fn storage(e: rusqlite::Error) -> LatticeError {
    LatticeError::Storage(format!("proposal reference maintenance failed: {e}"))
}
fn json_error(e: serde_json::Error) -> LatticeError {
    LatticeError::Storage(format!("serialize consolidation proposal payload: {e}"))
}
pub fn backfill_status(c: &Connection) -> Result<ProposalReferenceBackfillReport, LatticeError> {
    c.query_row("SELECT complete,blocked_proposal_id,blocked_payload_bytes FROM consolidation_proposal_ref_backfill WHERE id=1",[],|r|Ok(ProposalReferenceBackfillReport{complete:r.get(0)?,blocked_proposal_id:r.get(1)?,blocked_payload_bytes:r.get::<_,Option<i64>>(2)?.and_then(|v|usize::try_from(v).ok()),..Default::default()})).optional().map_err(storage)?.ok_or_else(||LatticeError::Storage("proposal reference backfill state is missing".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_limit_accepts_exact_boundary_and_rejects_one_byte_over() {
        let empty = serde_json::json!({});
        let exact = Value::String("x".repeat(MAX_PROPOSAL_REFERENCE_PAYLOAD_BYTES - 6));
        validate_proposal_payload(&exact, &empty, &empty).unwrap();
        let over = Value::String("x".repeat(MAX_PROPOSAL_REFERENCE_PAYLOAD_BYTES - 5));
        assert!(validate_proposal_payload(&over, &empty, &empty).is_err());
    }

    #[test]
    fn semantic_references_exclude_ordinary_strings() {
        let value = serde_json::json!({"branch":"main","status":"verified","memory":{"id":"kept"},"source_memory_ids":["source"]});
        let mut references = Vec::new();
        collect_strings(&value, &mut references, true);
        assert_eq!(references, vec!["kept", "source"]);
    }
}
