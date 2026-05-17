use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection};

use super::{MemoryMigrator, MigrationError};

fn legacy_conn() -> Connection {
    let conn = Connection::open_in_memory().expect("source connection");
    conn.execute_batch(
        "CREATE TABLE memories (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL DEFAULT '',
            content TEXT NOT NULL,
            memory_type TEXT NOT NULL,
            scope TEXT NOT NULL DEFAULT 'session',
            confidence REAL NOT NULL DEFAULT 1.0,
            linked_symbols TEXT NOT NULL DEFAULT '[]',
            linked_files TEXT NOT NULL DEFAULT '[]',
            workspace_id TEXT,
            branch TEXT,
            refresh_key TEXT,
            source_query TEXT,
            assertion_type TEXT NOT NULL DEFAULT 'observation',
            verification_status TEXT NOT NULL DEFAULT 'unverified',
            confidence_reason TEXT,
            supersedes_memory_id TEXT,
            superseded_by_memory_id TEXT,
            contradicts_memory_ids TEXT NOT NULL DEFAULT '[]',
            contradicted_by_memory_ids TEXT NOT NULL DEFAULT '[]',
            freshness_policy TEXT NOT NULL DEFAULT 'session_scoped',
            freshness_policy_detail TEXT,
            provenance_json TEXT NOT NULL DEFAULT '[]',
            evidence_json TEXT NOT NULL DEFAULT '[]',
            created_at INTEGER NOT NULL,
            last_accessed INTEGER NOT NULL,
            access_count INTEGER NOT NULL DEFAULT 0,
            is_stale INTEGER NOT NULL DEFAULT 0,
            stale_reason TEXT,
            is_invalidated INTEGER NOT NULL DEFAULT 0
        );",
    )
    .expect("legacy schema");
    conn
}

fn dest_conn() -> Connection {
    Connection::open_in_memory().expect("destination connection")
}

fn migrator(source: Connection, dest: Connection, dry_run: bool) -> MemoryMigrator {
    MemoryMigrator::new(
        Arc::new(Mutex::new(source)),
        Arc::new(Mutex::new(dest)),
        3,
        dry_run,
    )
}

#[test]
fn empty_source_migrates_to_empty_destination() {
    let migrator = migrator(legacy_conn(), dest_conn(), false);
    let plan = migrator.plan().expect("plan");
    let report = migrator.run(&plan).expect("run");

    assert_eq!(plan.source_rows, 0);
    assert_eq!(report.migrated_rows, 0);
    assert_table_count(&migrator, "memories", 0);
}

#[test]
fn curated_legacy_rows_migrate_exactly_once() {
    let source = legacy_conn();
    insert_row(&source, "obs", "observation", "observation", "session");
    insert_row(&source, "decision", "decision", "decision", "repo");
    insert_row(&source, "pattern", "pattern", "pattern", "branch");
    insert_rich_row(&source);
    let migrator = migrator(source, dest_conn(), false);

    let plan = migrator.plan().expect("plan");
    let report = migrator.run(&plan).expect("run");

    assert_eq!(plan.source_rows, 4);
    assert_eq!(report.migrated_rows, 4);
    assert_eq!(report.destination_inserts.memories, 4);
    assert_eq!(
        report.destination_inserts.memory_links,
        plan.destination_inserts.memory_links
    );
    assert_table_count(&migrator, "memories", 4);
    assert_table_count(&migrator, "memory_evidence", 1);
    assert_table_count(&migrator, "memory_accesses", 1);
    assert_table_count(&migrator, "memory_scores", 4);
}

#[test]
fn second_run_after_success_is_noop() {
    let source = legacy_conn();
    insert_row(&source, "obs", "observation", "observation", "session");
    let migrator = migrator(source, dest_conn(), false);
    let first_plan = migrator.plan().expect("first plan");
    let first_report = migrator.run(&first_plan).expect("first run");
    let second_plan = migrator.plan().expect("second plan");
    let second_report = migrator.run(&second_plan).expect("second run");

    assert_eq!(first_report.migrated_rows, 1);
    assert_eq!(second_report.migrated_rows, 0);
    assert_table_count(&migrator, "memories", 1);
}

#[test]
fn failed_batch_rolls_back_and_follow_up_run_completes() {
    let source = legacy_conn();
    insert_row(&source, "obs", "observation", "observation", "session");
    insert_row(&source, "conflict", "decision", "decision", "repo");
    let dest = dest_conn();
    super::initialize_schema(&dest).expect("schema");
    dest.execute(
        "INSERT INTO memories
            (memory_id, content, class, assertion_type, scope, scope_workspace_id,
             verification_status, confidence, confidence_reason, freshness_policy_json,
             validity_conditions_json, invalidation_triggers_json, provenance_event_ids_json,
             evidence_references_json, linked_files_json, linked_symbols_json, linked_docs_json,
             linked_tests_json, linked_memories_json, contradiction_links_json,
             supersession_links_json, access_history_json, usefulness_score,
             usefulness_score_updated_at, created_at, created_by, updated_at, updated_by,
             schema_version)
         VALUES
            ('memory:ws/conflict', 'preexisting', 'decision', 'decision', 'repo', 'ws',
             'unverified', 1.0, 'preexisting', '{}', '[]', '[]', '[]', '[]', '[]', '[]',
             '[]', '[]', '[]', '[]', '[]', '[]', 0.0, 1, 1, 'test', 1, 'test', 1)",
        params![],
    )
    .expect("conflict row");
    let migrator = migrator(source, dest, false);
    let plan = migrator.plan().expect("plan");

    let error = migrator.run(&plan).expect_err("conflict");
    assert!(matches!(error, MigrationError::IdempotenceConflict { .. }));
    assert_table_count(&migrator, "memories", 1);

    delete_conflict(&migrator);
    let retry_plan = migrator.plan().expect("retry plan");
    let retry_report = migrator.run(&retry_plan).expect("retry run");
    assert_eq!(retry_report.migrated_rows, 2);
    assert_table_count(&migrator, "memories", 2);
}

#[test]
fn dry_run_matches_real_report_but_writes_nothing() {
    let dry_source = legacy_conn();
    insert_rich_row(&dry_source);
    let real_source = legacy_conn();
    insert_rich_row(&real_source);
    let dry = migrator(dry_source, dest_conn(), true);
    let real = migrator(real_source, dest_conn(), false);

    let dry_plan = dry.plan().expect("dry plan");
    let dry_report = dry.run(&dry_plan).expect("dry run");
    let real_plan = real.plan().expect("real plan");
    let real_report = real.run(&real_plan).expect("real run");

    assert_eq!(dry_report.migrated_rows, real_report.migrated_rows);
    assert_eq!(
        dry_report.destination_inserts,
        real_report.destination_inserts
    );
    assert_table_count(&dry, "memories", 0);
    assert_table_count(&real, "memories", 1);
}

#[test]
fn unmappable_rows_are_reported_and_do_not_halt_migration() {
    let source = legacy_conn();
    insert_bad_row(&source);
    insert_row(&source, "obs", "observation", "observation", "session");
    let migrator = migrator(source, dest_conn(), false);
    let plan = migrator.plan().expect("plan");
    let report = migrator.run(&plan).expect("run");

    assert_eq!(plan.skipped_rows.len(), 1);
    assert_eq!(report.skipped_rows.len(), 1);
    assert_eq!(report.migrated_rows, 1);
    assert_table_count(&migrator, "memories", 1);
}

fn insert_row(conn: &Connection, id: &str, memory_type: &str, assertion: &str, scope: &str) {
    insert_memory(conn, id, memory_type, assertion, scope, "[]", "[]", 0);
}

fn insert_rich_row(conn: &Connection) {
    insert_memory(
        conn,
        "rich",
        "anti_pattern",
        "anti_pattern",
        "branch",
        r#"["src/lib.rs"]"#,
        r#"["important_symbol"]"#,
        2,
    );
    conn.execute(
        "UPDATE memories
         SET evidence_json = ?2, contradicts_memory_ids = ?3, supersedes_memory_id = ?4
         WHERE id = ?1",
        params![
            "rich",
            r#"[{"kind":"note","reference":"test","captured_at":12}]"#,
            r#"["obs"]"#,
            "decision"
        ],
    )
    .expect("rich metadata");
}

fn insert_bad_row(conn: &Connection) {
    insert_memory(
        conn,
        "bad",
        "unknown_type",
        "unknown",
        "session",
        "[]",
        "[]",
        0,
    );
}

fn insert_memory(
    conn: &Connection,
    id: &str,
    memory_type: &str,
    assertion: &str,
    scope: &str,
    linked_files: &str,
    linked_symbols: &str,
    access_count: i64,
) {
    conn.execute(
        "INSERT INTO memories
            (id, session_id, content, memory_type, scope, confidence, linked_symbols,
             linked_files, workspace_id, branch, refresh_key, source_query, assertion_type,
             verification_status, confidence_reason, supersedes_memory_id,
             superseded_by_memory_id, contradicts_memory_ids, contradicted_by_memory_ids,
             freshness_policy, freshness_policy_detail, provenance_json, evidence_json,
             created_at, last_accessed, access_count, is_stale, stale_reason, is_invalidated)
         VALUES
            (?1, 'session-a', ?2, ?3, ?4, 0.8, ?5, ?6, 'ws', 'main', NULL,
             'source query', ?7, 'unverified', NULL, NULL, NULL, '[]', '[]',
             'session_scoped', NULL, '[]', '[]', 10, 20, ?8, 0, NULL, 0)",
        params![
            id,
            format!("content for {id}"),
            memory_type,
            scope,
            linked_symbols,
            linked_files,
            assertion,
            access_count,
        ],
    )
    .expect("insert legacy row");
}

fn assert_table_count(migrator: &MemoryMigrator, table: &str, expected: i64) {
    let dest = migrator.dest_conn.lock().expect("dest lock");
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let count: i64 = dest
        .query_row(&sql, params![], |row| row.get(0))
        .expect("count");
    assert_eq!(count, expected, "{table}");
}

fn delete_conflict(migrator: &MemoryMigrator) {
    let dest = migrator.dest_conn.lock().expect("dest lock");
    dest.execute(
        "DELETE FROM memories WHERE memory_id = 'memory:ws/conflict'",
        params![],
    )
    .expect("delete conflict");
}
