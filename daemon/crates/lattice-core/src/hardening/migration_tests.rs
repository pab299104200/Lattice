//! Phase 11 migration hardening regressions.
//!
//! Cites `## Phase 3: Memory Graph Storage`, `## Phase 11: Hardening`,
//! `## MCP Tool Contract Principles`, and
//! `docs/architecture/2026-05-16-storage-migration-policy.md`
//! `## Migration order` / `## Rollback`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection};

use crate::events::EventStore;
use crate::memory_graph::migration::MigrationTableCounts;
use crate::memory_graph::{MemoryMigrator, MigrationPlan};

const STORAGE_POLICY: &str =
    include_str!("../../../../../docs/architecture/2026-05-16-storage-migration-policy.md");
const LARGE_LEGACY_ROWS: usize = 100_000;
const LARGE_DATASET_BUDGET: Duration = Duration::from_secs(120);

#[test]
fn legacy_memory_rows_migrate_to_memory_links_and_evidence() {
    let harness = MigrationHarness::with_rows(supported_legacy_rows());
    let report = harness.run();

    assert_eq!(report.migrated_rows, 8, "all supported legacy rows migrate");
    assert_eq!(
        harness.table_count("memories"),
        8,
        "one new row per legacy row"
    );
    assert_eq!(
        harness.table_count("memory_links"),
        4,
        "links are normalized"
    );
    assert_eq!(
        harness.table_count("memory_evidence"),
        1,
        "evidence is normalized"
    );

    let rows = harness.memory_rows();
    assert_memory(&rows, "memory:ws/observation", "observation", "unverified");
    assert_memory(&rows, "memory:ws/decision", "decision", "in_review");
    assert_memory(&rows, "memory:ws/pattern", "pattern", "verified");
    assert_memory(&rows, "memory:ws/anti", "anti_pattern", "stale");
    assert_memory(&rows, "memory:ws/open", "open_question", "contradicted");
    assert_memory(
        &rows,
        "memory:ws/workflow",
        "workflow_outcome",
        "superseded",
    );
    assert_memory(&rows, "memory:ws/constraint", "constraint", "verified");
}

#[test]
fn legacy_memory_with_malformed_json_is_quarantined_without_aborting() {
    let harness = MigrationHarness::with_rows(vec![
        legacy_row(
            "bad-json",
            "observation",
            "observation",
            "repo",
            "unverified",
        )
        .with_evidence("{not-json"),
        legacy_row("valid", "observation", "observation", "repo", "verified"),
    ]);
    let report = harness.run();

    assert_eq!(report.migrated_rows, 1, "valid row should still migrate");
    assert_eq!(report.skipped_rows.len(), 1, "malformed row is skipped");
    assert_eq!(
        harness.table_count("migration_quarantine"),
        1,
        "row quarantined"
    );
    assert!(
        harness
            .quarantine_reasons()
            .iter()
            .any(|reason| reason.contains("invalid legacy JSON")),
        "quarantine should preserve the malformed JSON reason"
    );
}

#[test]
fn idempotent_migration_second_run_is_noop() {
    let harness = MigrationHarness::with_rows(supported_legacy_rows());
    let first = harness.run();
    let second = harness.run();

    assert_eq!(first.migrated_rows, 8, "first run migrates source rows");
    assert_eq!(second.migrated_rows, 0, "second run is a no-op");
    assert_eq!(harness.table_count("memories"), 8, "no duplicate memories");
    assert_eq!(
        harness.table_count("migration_progress"),
        8,
        "no progress duplicates"
    );
}

#[test]
fn forward_migration_order_matches_policy_and_rejects_out_of_order() {
    let order = policy_migration_order();
    assert_eq!(
        order.first().map(String::as_str),
        Some("p1_001_identity_workspaces")
    );
    assert_eq!(
        order.last().map(String::as_str),
        Some("p7_001_verification_jobs")
    );

    let mut applied = Vec::new();
    assert!(apply_policy_migration(&order, &mut applied, "p1_001_identity_workspaces").is_ok());
    let error = apply_policy_migration(&order, &mut applied, "p1_003_identity_symbols")
        .expect_err("out-of-order migration should be rejected");
    assert!(error.contains("expected p1_002_identity_files"), "{error}");
}

#[test]
fn rollback_policy_documents_inverse_for_every_migration() {
    let order = policy_migration_order();
    let rollback = policy_rollback_commands();

    for migration_id in &order {
        let command = rollback
            .get(migration_id)
            .unwrap_or_else(|| panic!("missing rollback inverse for {migration_id}"));
        assert!(
            command.contains("--archive-newer") && command.contains(migration_id),
            "rollback for {migration_id} must preserve archive semantics"
        );
    }
    assert_eq!(
        rollback.len(),
        order.len(),
        "rollback table matches order table"
    );
}

#[test]
fn pre_event_log_state_bootstraps_event_schema_and_memory_migration() {
    let events = EventStore::open_in_memory().expect("event store should bootstrap absent schema");
    assert_eq!(events.event_count_after(0).expect("event count"), 0);
    events.with_connection(|conn| {
        assert!(table_exists(conn, "events"), "events table should exist");
        assert!(
            table_exists(conn, "event_payloads"),
            "event_payloads table should exist"
        );
    });

    let harness = MigrationHarness::with_rows(vec![legacy_row(
        "pre-events",
        "observation",
        "observation",
        "repo",
        "verified",
    )]);
    assert_eq!(harness.run().migrated_rows, 1);
}

#[test]
fn large_legacy_dataset_migrates_without_losing_rows_under_budget() {
    let harness = MigrationHarness::large(LARGE_LEGACY_ROWS);
    let started = Instant::now();
    let report = harness.run_with_plan(MigrationPlan {
        source_rows: LARGE_LEGACY_ROWS,
        already_migrated_rows: 0,
        destination_inserts: MigrationTableCounts {
            memories: LARGE_LEGACY_ROWS,
            memory_links: 0,
            memory_evidence: 0,
            memory_accesses: 0,
            memory_scores: LARGE_LEGACY_ROWS,
        },
        skipped_rows: Vec::new(),
    });
    let elapsed = started.elapsed();

    assert!(
        elapsed <= LARGE_DATASET_BUDGET,
        "large migration took {:?}, budget {:?}",
        elapsed,
        LARGE_DATASET_BUDGET
    );
    assert_eq!(report.source_rows, LARGE_LEGACY_ROWS);
    assert_eq!(
        report.migrated_rows + report.skipped_rows.len(),
        LARGE_LEGACY_ROWS
    );
    assert_eq!(harness.table_count("memories") as usize, LARGE_LEGACY_ROWS);
}

struct MigrationHarness {
    migrator: MemoryMigrator,
}

impl MigrationHarness {
    fn with_rows(rows: Vec<LegacySeed>) -> Self {
        let source = legacy_conn();
        for row in rows {
            insert_seed(&source, &row);
        }
        Self::new(source, 257)
    }

    fn large(count: usize) -> Self {
        let source = legacy_conn();
        let tx = source.unchecked_transaction().expect("large tx");
        for index in 0..count {
            let id = format!("legacy-{index}");
            insert_seed(
                &tx,
                &legacy_row(&id, "observation", "observation", "repo", "verified"),
            );
        }
        tx.commit().expect("large commit");
        Self::new(source, count)
    }

    fn new(source: Connection, batch_size: usize) -> Self {
        let migrator = MemoryMigrator::new(
            Arc::new(Mutex::new(source)),
            Arc::new(Mutex::new(Connection::open_in_memory().expect("dest"))),
            batch_size,
            false,
        );
        Self { migrator }
    }

    fn run(&self) -> crate::memory_graph::MigrationReport {
        let plan = self.migrator.plan().expect("migration plan");
        self.run_with_plan(plan)
    }

    fn run_with_plan(&self, plan: MigrationPlan) -> crate::memory_graph::MigrationReport {
        self.migrator.run(&plan).expect("migration run")
    }

    fn table_count(&self, table: &str) -> i64 {
        let dest = self.migrator.dest_conn.lock().expect("dest lock");
        let sql = format!("SELECT COUNT(*) FROM {table}");
        dest.query_row(&sql, [], |row| row.get(0)).expect("count")
    }

    fn memory_rows(&self) -> HashMap<String, (String, String, String)> {
        let dest = self.migrator.dest_conn.lock().expect("dest lock");
        let mut statement = dest
            .prepare("SELECT memory_id, class, scope, verification_status FROM memories")
            .expect("memory rows query");
        statement
            .query_map([], |row| {
                Ok((row.get(0)?, (row.get(1)?, row.get(2)?, row.get(3)?)))
            })
            .expect("memory rows")
            .collect::<Result<HashMap<_, _>, _>>()
            .expect("memory row collect")
    }

    fn quarantine_reasons(&self) -> Vec<String> {
        let dest = self.migrator.dest_conn.lock().expect("dest lock");
        let mut statement = dest
            .prepare("SELECT reason FROM migration_quarantine ORDER BY source_row_id")
            .expect("quarantine query");
        statement
            .query_map([], |row| row.get(0))
            .expect("quarantine rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("quarantine collect")
    }
}

#[derive(Clone)]
struct LegacySeed {
    id: String,
    memory_type: String,
    assertion_type: String,
    scope: String,
    verification_status: String,
    linked_symbols: &'static str,
    linked_files: &'static str,
    evidence_json: &'static str,
    contradicts: &'static str,
    supersedes: Option<&'static str>,
    access_count: i64,
    is_stale: i64,
}

impl LegacySeed {
    fn with_evidence(mut self, evidence_json: &'static str) -> Self {
        self.evidence_json = evidence_json;
        self
    }
}

fn supported_legacy_rows() -> Vec<LegacySeed> {
    vec![
        legacy_row(
            "observation",
            "observation",
            "observation",
            "session",
            "unverified",
        ),
        legacy_row("decision", "decision", "decision", "repo", "in_review"),
        legacy_row("pattern", "pattern", "pattern", "branch", "verified"),
        legacy_row("anti", "anti_pattern", "anti_pattern", "branch", "verified").stale(),
        legacy_row(
            "open",
            "exploration",
            "exploration",
            "session",
            "contradicted",
        ),
        legacy_row(
            "workflow",
            "legacy",
            "workflow_outcome",
            "repo",
            "superseded",
        ),
        legacy_row("constraint", "legacy", "constraint", "repo", "verified"),
        legacy_row("rich", "observation", "observation", "repo", "verified").rich(),
    ]
}

fn legacy_row(
    id: &str,
    memory_type: &str,
    assertion_type: &str,
    scope: &str,
    verification_status: &str,
) -> LegacySeed {
    LegacySeed {
        id: id.to_string(),
        memory_type: memory_type.to_string(),
        assertion_type: assertion_type.to_string(),
        scope: scope.to_string(),
        verification_status: verification_status.to_string(),
        linked_symbols: "[]",
        linked_files: "[]",
        evidence_json: "[]",
        contradicts: "[]",
        supersedes: None,
        access_count: 0,
        is_stale: 0,
    }
}

trait SeedExt {
    fn rich(self) -> Self;
    fn stale(self) -> Self;
}

impl SeedExt for LegacySeed {
    fn rich(mut self) -> Self {
        self.linked_symbols = r#"["important_symbol"]"#;
        self.linked_files = r#"["src/lib.rs"]"#;
        self.evidence_json = r#"[{"kind":"note","reference":"test","captured_at":12}]"#;
        self.contradicts = r#"["observation"]"#;
        self.supersedes = Some("decision");
        self.access_count = 3;
        self
    }

    fn stale(mut self) -> Self {
        self.is_stale = 1;
        self
    }
}

fn legacy_conn() -> Connection {
    let conn = Connection::open_in_memory().expect("source connection");
    conn.execute_batch(
        "CREATE TABLE memories (
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL DEFAULT '', content TEXT NOT NULL,
            memory_type TEXT NOT NULL, scope TEXT NOT NULL DEFAULT 'session',
            confidence REAL NOT NULL DEFAULT 1.0, linked_symbols TEXT NOT NULL DEFAULT '[]',
            linked_files TEXT NOT NULL DEFAULT '[]', workspace_id TEXT, branch TEXT,
            refresh_key TEXT, source_query TEXT, assertion_type TEXT NOT NULL DEFAULT 'observation',
            verification_status TEXT NOT NULL DEFAULT 'unverified', confidence_reason TEXT,
            supersedes_memory_id TEXT, superseded_by_memory_id TEXT,
            contradicts_memory_ids TEXT NOT NULL DEFAULT '[]',
            contradicted_by_memory_ids TEXT NOT NULL DEFAULT '[]',
            freshness_policy TEXT NOT NULL DEFAULT 'session_scoped',
            freshness_policy_detail TEXT, provenance_json TEXT NOT NULL DEFAULT '[]',
            evidence_json TEXT NOT NULL DEFAULT '[]', created_at INTEGER NOT NULL,
            last_accessed INTEGER NOT NULL, access_count INTEGER NOT NULL DEFAULT 0,
            is_stale INTEGER NOT NULL DEFAULT 0, stale_reason TEXT,
            is_invalidated INTEGER NOT NULL DEFAULT 0
        );",
    )
    .expect("legacy schema");
    conn
}

fn insert_seed(conn: &Connection, row: &LegacySeed) {
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
             'source query', ?7, ?8, NULL, ?9, NULL, ?10, '[]', 'session_scoped',
             NULL, '[]', ?11, 10, 20, ?12, ?13, NULL, 0)",
        params![
            row.id,
            format!("content for {}", row.id),
            row.memory_type,
            row.scope,
            row.linked_symbols,
            row.linked_files,
            row.assertion_type,
            row.verification_status,
            row.supersedes,
            row.contradicts,
            row.evidence_json,
            row.access_count,
            row.is_stale,
        ],
    )
    .expect("insert legacy row");
}

fn assert_memory(
    rows: &HashMap<String, (String, String, String)>,
    id: &str,
    class: &str,
    verification_status: &str,
) {
    let (actual_class, _scope, actual_status) = rows.get(id).expect("memory row exists");
    assert_eq!(actual_class, class, "class for {id}");
    assert_eq!(actual_status, verification_status, "verification for {id}");
}

fn policy_migration_order() -> Vec<String> {
    table_rows("## Migration order")
        .into_iter()
        .filter_map(|row| markdown_code_cells(&row).first().cloned())
        .collect()
}

fn policy_rollback_commands() -> HashMap<String, String> {
    table_rows("## Rollback")
        .into_iter()
        .filter_map(|row| {
            let cells = markdown_code_cells(&row);
            Some((cells.first()?.clone(), cells.last()?.clone()))
        })
        .collect()
}

fn table_rows(heading: &str) -> Vec<String> {
    STORAGE_POLICY
        .split(heading)
        .nth(1)
        .expect("heading exists")
        .lines()
        .skip_while(|line| !line.starts_with('|'))
        .take_while(|line| line.starts_with('|'))
        .filter(|line| line.contains('`') && !line.contains("---"))
        .map(str::to_string)
        .collect()
}

fn markdown_code_cells(row: &str) -> Vec<String> {
    row.split('`')
        .enumerate()
        .filter_map(|(index, value)| (index % 2 == 1).then(|| value.to_string()))
        .collect()
}

fn apply_policy_migration(
    order: &[String],
    applied: &mut Vec<String>,
    migration_id: &str,
) -> Result<(), String> {
    let applied_set: HashSet<&str> = applied.iter().map(String::as_str).collect();
    let position = order
        .iter()
        .position(|id| id == migration_id)
        .ok_or_else(|| format!("unknown migration {migration_id}"))?;
    if let Some(expected) = order[..position]
        .iter()
        .find(|id| !applied_set.contains(id.as_str()))
    {
        return Err(format!(
            "cannot apply {migration_id} out of order; expected {expected}"
        ));
    }
    applied.push(migration_id.to_string());
    Ok(())
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![table],
        |_| Ok(()),
    )
    .is_ok()
}
