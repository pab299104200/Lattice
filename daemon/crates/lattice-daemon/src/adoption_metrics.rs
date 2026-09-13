use anyhow::{Context, Result};
use lattice_core::storage::{managed_sqlite::ManagedSqlite, SecureDir};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const LEDGER_VERSION: u32 = 2;
const RETENTION_DAYS: u64 = 90;
const FOLLOW_THROUGH_WINDOW_SECS: u64 = 60 * 60;
const MAX_SUGGESTED_FILES: usize = 32;
const SECS_PER_DAY: u64 = 86_400;
const MAX_CAPTURE_SCHEMA_VERSION: u32 = 64;
const MAX_CAPTURE_EXTRACTOR_VERSION: u32 = 64;
const DATABASE_FILE: &str = "adoption_metrics.sqlite3";
const LEGACY_LEDGER_FILE: &str = "adoption_metrics.jsonl";
const MIGRATION_MARKER: &str = "legacy_jsonl_import_v1";
const MIGRATION_CURSOR: &str = "legacy_jsonl_cursor_v2";
const CAPTURE_AGGREGATE_MARKER: &str = "capture_outcome_daily_v1";
const CAPTURE_AGGREGATE_CURSOR: &str = "capture_outcome_daily_cursor_v1";
const CAPTURE_AGGREGATE_PAGE_ROWS: usize = 1024;
const CAPTURE_EVENT_JSON_BYTES: usize = 4096;
const LEGACY_PAGE_RECORDS: usize = 1024;
const LEGACY_PAGE_BYTES: u64 = 8 * 1024 * 1024;
const LEGACY_RECORD_BYTES: u64 = 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct LegacyImportCursor {
    signature: String,
    offset: u64,
    lines: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolCallSource {
    pub(crate) client: String,
    pub(crate) channel: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolCallRecord {
    pub(crate) session_id: String,
    pub(crate) client: String,
    pub(crate) channel: String,
    pub(crate) tool: String,
    pub(crate) latency_ms: u64,
    /// Files the tool actually returned as relevant. Follow-through is only
    /// credited after the watcher subsequently observes one of these files.
    pub(crate) suggested_files: Vec<String>,
}

/// A durable, append-only observation that a memory retrieval returned one or
/// more records to an assistant. `retrieval_id` is supplied by the memory
/// layer so a later citation/use can be joined without guessing from prose.
#[derive(Debug, Clone)]
pub(crate) struct MemoryRetrievalRecord {
    pub(crate) session_id: String,
    pub(crate) client: String,
    pub(crate) channel: String,
    pub(crate) retrieval_id: String,
    pub(crate) retrieved_count: u64,
}

/// A durable observation that one or more records from a prior retrieval were
/// used. The caller must supply the retrieval id returned at retrieval time;
/// this deliberately prevents unrelated later actions from being credited.
#[derive(Debug, Clone)]
pub(crate) struct MemoryUseRecord {
    pub(crate) retrieval_id: String,
    pub(crate) used_count: u64,
}

/// A durable observation that a hook or workflow presented memories to an
/// assistant. The injection id lets a subsequent action be attributed to the
/// exact presentation rather than to all memories in a session.
#[derive(Debug, Clone)]
pub(crate) struct MemoryInjectionRecord {
    pub(crate) session_id: String,
    pub(crate) client: String,
    pub(crate) channel: String,
    pub(crate) injection_id: String,
    pub(crate) shown_count: u64,
}

/// A durable observation that an assistant acted on a prior injection.
#[derive(Debug, Clone)]
pub(crate) struct MemoryInjectionActionRecord {
    pub(crate) injection_id: String,
    pub(crate) acted_count: u64,
}

/// A durable observation that a response carried health evidence about
/// specific files (spec H4.5).
///
/// The cited paths are recorded so a later edit can be attributed to the
/// evidence that named the file, reusing the same edit-follow-through join the
/// `context` and `impact` rows already use rather than a parallel pipeline.
#[derive(Debug, Clone)]
pub(crate) struct HealthEvidenceRecord {
    pub(crate) session_id: String,
    pub(crate) client: String,
    pub(crate) channel: String,
    pub(crate) tool: String,
    pub(crate) cited_files: Vec<String>,
}

/// Content-free terminal or deferred state for one session-capture attempt.
/// These names are deliberately a fixed allowlist so capture telemetry cannot
/// become an unbounded diagnostic or payload channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CaptureOutcome {
    Captured,
    PartiallyCaptured,
    Rejected,
    DaemonUnavailable,
    StoreUnavailable,
    Queued,
    Skipped,
}

impl CaptureOutcome {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Captured => "captured",
            Self::PartiallyCaptured => "partially_captured",
            Self::Rejected => "rejected",
            Self::DaemonUnavailable => "daemon_unavailable",
            Self::StoreUnavailable => "store_unavailable",
            Self::Queued => "queued",
            Self::Skipped => "skipped",
        }
    }
}

/// The only dimensions persisted for capture observability. It intentionally
/// excludes session or delivery identity, payload values, paths, error text,
/// capability material, and hashes/fingerprints.
#[derive(Debug, Clone)]
pub(crate) struct CaptureMetricRecord {
    pub(crate) integration: String,
    pub(crate) schema_version: u32,
    pub(crate) extractor_version: u32,
    pub(crate) outcome: CaptureOutcome,
}

/// A content-free aggregate suitable for doctor output. Individual capture
/// events are never surfaced through this type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CaptureHealth {
    pub(crate) total_attempts: u64,
    pub(crate) outcomes: BTreeMap<String, u64>,
}

impl CaptureHealth {
    pub(crate) fn has_concerning_outcomes(&self) -> bool {
        self.outcomes.iter().any(|(outcome, count)| {
            *count > 0
                && matches!(
                    outcome.as_str(),
                    "partially_captured" | "rejected" | "daemon_unavailable" | "store_unavailable"
                )
        })
    }
}

#[derive(Debug)]
pub(crate) struct AdoptionMetricsStore {
    path: PathBuf,
    legacy_path: PathBuf,
    home: std::result::Result<SecureDir, String>,
    // Serializes setup and migration for instances in this process. SQLite's
    // BEGIN IMMEDIATE transaction serializes independent instances/processes.
    lock: Mutex<()>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct AdoptionCounter {
    pub(crate) calls: u64,
    pub(crate) total_latency_ms: u64,
    pub(crate) max_latency_ms: u64,
    pub(crate) follow_through_edits: u64,
    pub(crate) memory_retrievals: u64,
    pub(crate) memory_retrieved_items: u64,
    pub(crate) memory_used_items: u64,
    pub(crate) memory_injections: u64,
    pub(crate) memory_injected_items: u64,
    pub(crate) memory_injection_actions: u64,
    /// Responses that carried health evidence about at least one file.
    #[serde(default)]
    pub(crate) health_evidence_injections: u64,
    /// Distinct files those responses cited.
    #[serde(default)]
    pub(crate) health_evidence_cited_files: u64,
    /// Injections followed by an edit to a file the evidence named.
    #[serde(default)]
    pub(crate) health_evidence_followed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AdoptionLedger {
    version: u32,
    days: BTreeMap<String, BTreeMap<String, BTreeMap<String, BTreeMap<String, AdoptionCounter>>>>,
    #[serde(default)]
    capture: CaptureLedger,
}

impl Default for AdoptionLedger {
    fn default() -> Self {
        Self {
            version: LEDGER_VERSION,
            days: BTreeMap::new(),
            capture: CaptureLedger::default(),
        }
    }
}

/// Bounded capture counters. Integrations collapse to a fixed vocabulary and
/// unsupported schema/extractor versions collapse to `other`, so untrusted
/// hook input cannot create unbounded metric cardinality.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CaptureLedger {
    total_attempts: u64,
    outcomes: BTreeMap<String, u64>,
    by_integration: BTreeMap<String, BTreeMap<String, BTreeMap<String, BTreeMap<String, u64>>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AdoptionEvent {
    ToolCall {
        timestamp_secs: u64,
        session_id: String,
        client: String,
        channel: String,
        tool: String,
        latency_ms: u64,
        suggested_files: Vec<String>,
    },
    ObservedEdit {
        timestamp_secs: u64,
        session_id: String,
        file: String,
    },
    MemoryRetrieval {
        timestamp_secs: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metric_id: Option<String>,
        session_id: String,
        client: String,
        channel: String,
        retrieval_id: String,
        retrieved_count: u64,
    },
    MemoryUse {
        timestamp_secs: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metric_id: Option<String>,
        retrieval_id: String,
        used_count: u64,
    },
    MemoryInjection {
        timestamp_secs: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metric_id: Option<String>,
        session_id: String,
        client: String,
        channel: String,
        injection_id: String,
        shown_count: u64,
    },
    MemoryInjectionAction {
        timestamp_secs: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metric_id: Option<String>,
        injection_id: String,
        acted_count: u64,
    },
    HealthEvidenceInjection {
        timestamp_secs: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metric_id: Option<String>,
        session_id: String,
        client: String,
        channel: String,
        tool: String,
        cited_files: BTreeSet<String>,
    },
    CaptureOutcome {
        timestamp_secs: u64,
        integration: String,
        schema_version: u32,
        extractor_version: u32,
        outcome: CaptureOutcome,
    },
}

#[derive(Debug, Clone)]
struct PendingAssistance {
    timestamp_secs: u64,
    session_id: String,
    suggested_files: BTreeSet<String>,
    counter_key: CounterKey,
    credited: bool,
}

#[derive(Debug, Clone)]
struct CounterKey {
    day: String,
    client: String,
    channel: String,
    tool: String,
}

#[derive(Debug, Clone)]
struct PendingMemoryRetrieval {
    counter_key: CounterKey,
    retrieved_count: u64,
    used_count: u64,
}

#[derive(Debug, Clone)]
struct PendingMemoryInjection {
    counter_key: CounterKey,
    shown_count: u64,
    acted_count: u64,
}

impl AdoptionMetricsStore {
    pub(crate) fn new(workspace_root: &Path) -> Self {
        let resolved = Self::resolve_home(workspace_root, true)
            .and_then(|home| home.context("repository telemetry home absent after creation"));
        Self::from_home(workspace_root, resolved)
    }

    fn resolve_home(workspace_root: &Path, create: bool) -> Result<Option<SecureDir>> {
        let identity = crate::workspace_identity::WorkspaceIdentity::resolve(workspace_root)?;
        let parent = identity
            .repository_lattice_dir
            .parent()
            .context("repository telemetry home has no parent")?;
        let leaf = identity
            .repository_lattice_dir
            .file_name()
            .and_then(|leaf| leaf.to_str())
            .context("repository telemetry home is not UTF-8")?;
        let parent =
            SecureDir::open(parent).context("failed to pin repository telemetry parent")?;
        if create {
            return parent
                .create_dir(leaf)
                .map(Some)
                .context("failed to pin repository telemetry home");
        }
        match parent.open_dir(Path::new(leaf)) {
            Ok(home) => Ok(Some(home)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("failed to pin existing repository telemetry home"),
        }
    }

    fn from_home(workspace_root: &Path, resolved: Result<SecureDir>) -> Self {
        // These fallback paths are diagnostic only. No I/O is allowed until
        // home() succeeds; an invalid Git identity never becomes standalone.
        let display_home = resolved
            .as_ref()
            .map(|home| home.path().to_path_buf())
            .unwrap_or_else(|_| workspace_root.join(".lattice"));
        Self {
            path: display_home.join(DATABASE_FILE),
            legacy_path: display_home.join(LEGACY_LEDGER_FILE),
            home: resolved
                .map_err(|error| format!("repository telemetry authority unavailable: {error:#}")),
            lock: Mutex::new(()),
        }
    }

    fn home(&self) -> Result<&SecureDir> {
        self.home
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.clone()))
    }

    pub(crate) fn record(&self, record: ToolCallRecord) -> Result<()> {
        self.append_event(AdoptionEvent::ToolCall {
            timestamp_secs: now_secs(),
            session_id: clean_key(&record.session_id, "unknown-session"),
            client: clean_key(&record.client, "unknown-client"),
            channel: clean_key(&record.channel, "unknown-channel"),
            tool: clean_key(&record.tool, "unknown-tool"),
            latency_ms: record.latency_ms,
            suggested_files: normalize_suggested_files(record.suggested_files),
        })
    }

    /// Records an actual filesystem change emitted by the watcher. Attribution
    /// is resolved by replaying events, never by caller-supplied prose.
    pub(crate) fn record_observed_edit(&self, session_id: &str, file: &str) -> Result<()> {
        self.append_event(AdoptionEvent::ObservedEdit {
            timestamp_secs: now_secs(),
            session_id: clean_key(session_id, "unknown-session"),
            file: clean_file(file).unwrap_or_default(),
        })
    }

    /// Records a memory retrieval in the same 90-day append-only ledger as
    /// adoption events. Empty retrievals are retained: they are an important
    /// distinction from an unavailable metric and make recall misses visible.
    pub(crate) fn record_memory_retrieval(&self, record: MemoryRetrievalRecord) -> Result<()> {
        self.append_event(AdoptionEvent::MemoryRetrieval {
            timestamp_secs: now_secs(),
            metric_id: None,
            session_id: clean_key(&record.session_id, "unknown-session"),
            client: clean_key(&record.client, "unknown-client"),
            channel: clean_key(&record.channel, "unknown-channel"),
            retrieval_id: clean_identifier(&record.retrieval_id, "unknown-retrieval"),
            retrieved_count: record.retrieved_count,
        })
    }

    /// Attributes use to a previously recorded retrieval. The replay path
    /// rejects unknown ids, so a malformed or stale producer cannot inflate
    /// memory-value metrics.
    pub(crate) fn record_memory_use(&self, record: MemoryUseRecord) -> Result<()> {
        self.append_event(AdoptionEvent::MemoryUse {
            timestamp_secs: now_secs(),
            metric_id: None,
            retrieval_id: clean_identifier(&record.retrieval_id, "unknown-retrieval"),
            used_count: record.used_count,
        })
    }

    /// Appends a retrieval metric exactly once for a caller-supplied stable
    /// metric id. Returns true only when this call appends the event; a
    /// recovered retry of the same event returns false.
    ///
    /// The id lookup and append occur under both the in-process mutex and a
    /// repository-owned SQLite immediate transaction. The event is synced before return,
    /// so a process crash after the append is safe to retry.
    pub(crate) fn record_memory_retrieval_once(
        &self,
        metric_id: &str,
        record: MemoryRetrievalRecord,
    ) -> Result<bool> {
        let metric_id = stable_metric_id(metric_id)?;
        self.append_event_once(
            metric_id.clone(),
            AdoptionEvent::MemoryRetrieval {
                timestamp_secs: now_secs(),
                metric_id: Some(metric_id),
                session_id: clean_key(&record.session_id, "unknown-session"),
                client: clean_key(&record.client, "unknown-client"),
                channel: clean_key(&record.channel, "unknown-channel"),
                retrieval_id: clean_identifier(&record.retrieval_id, "unknown-retrieval"),
                retrieved_count: record.retrieved_count,
            },
        )
    }

    /// Appends a use metric exactly once for a caller-supplied stable metric
    /// id. It has the same atomic recovery and return-value contract as
    /// record_memory_retrieval_once.
    pub(crate) fn record_memory_use_once(
        &self,
        metric_id: &str,
        record: MemoryUseRecord,
    ) -> Result<bool> {
        let metric_id = stable_metric_id(metric_id)?;
        self.append_event_once(
            metric_id.clone(),
            AdoptionEvent::MemoryUse {
                timestamp_secs: now_secs(),
                metric_id: Some(metric_id),
                retrieval_id: clean_identifier(&record.retrieval_id, "unknown-retrieval"),
                used_count: record.used_count,
            },
        )
    }

    pub(crate) fn record_memory_injection(&self, record: MemoryInjectionRecord) -> Result<()> {
        self.append_event(AdoptionEvent::MemoryInjection {
            timestamp_secs: now_secs(),
            metric_id: None,
            session_id: clean_key(&record.session_id, "unknown-session"),
            client: clean_key(&record.client, "unknown-client"),
            channel: clean_key(&record.channel, "unknown-channel"),
            injection_id: clean_identifier(&record.injection_id, "unknown-injection"),
            shown_count: record.shown_count,
        })
    }

    pub(crate) fn record_memory_injection_action(
        &self,
        record: MemoryInjectionActionRecord,
    ) -> Result<()> {
        self.append_event(AdoptionEvent::MemoryInjectionAction {
            timestamp_secs: now_secs(),
            metric_id: None,
            injection_id: clean_identifier(&record.injection_id, "unknown-injection"),
            acted_count: record.acted_count,
        })
    }

    /// Appends an injection metric exactly once for a caller-supplied stable
    /// metric id. A retry of the same payload returns false; reusing the id
    /// for a different payload fails without appending anything.
    pub(crate) fn record_memory_injection_once(
        &self,
        metric_id: &str,
        record: MemoryInjectionRecord,
    ) -> Result<bool> {
        let metric_id = stable_metric_id(metric_id)?;
        self.append_event_once(
            metric_id.clone(),
            AdoptionEvent::MemoryInjection {
                timestamp_secs: now_secs(),
                metric_id: Some(metric_id),
                session_id: clean_key(&record.session_id, "unknown-session"),
                client: clean_key(&record.client, "unknown-client"),
                channel: clean_key(&record.channel, "unknown-channel"),
                injection_id: clean_identifier(&record.injection_id, "unknown-injection"),
                shown_count: record.shown_count,
            },
        )
    }

    /// Appends an injection-action metric exactly once for a caller-supplied
    /// stable metric id, with the same retry and conflict contract as
    /// record_memory_injection_once.
    /// Records that a response carried health evidence naming specific files.
    ///
    /// Best-effort like every other write here: a metrics failure must never
    /// change a response or fail a tool call.
    pub(crate) fn record_health_evidence(&self, record: HealthEvidenceRecord) -> Result<()> {
        let cited_files: BTreeSet<String> = normalize_suggested_files(record.cited_files)
            .into_iter()
            .collect();
        if cited_files.is_empty() {
            return Ok(());
        }
        self.append_event(AdoptionEvent::HealthEvidenceInjection {
            timestamp_secs: now_secs(),
            metric_id: None,
            session_id: clean_key(&record.session_id, "unknown-session"),
            client: clean_key(&record.client, "unknown-client"),
            channel: clean_key(&record.channel, "unknown-channel"),
            tool: clean_key(&record.tool, "unknown-tool"),
            cited_files,
        })
    }

    pub(crate) fn record_memory_injection_action_once(
        &self,
        metric_id: &str,
        record: MemoryInjectionActionRecord,
    ) -> Result<bool> {
        let metric_id = stable_metric_id(metric_id)?;
        self.append_event_once(
            metric_id.clone(),
            AdoptionEvent::MemoryInjectionAction {
                timestamp_secs: now_secs(),
                metric_id: Some(metric_id),
                injection_id: clean_identifier(&record.injection_id, "unknown-injection"),
                acted_count: record.acted_count,
            },
        )
    }

    /// Records one content-free capture outcome when its authenticated
    /// delivery was new. The registry's replay bit is deliberately supplied
    /// separately: persisting a delivery/session identity just to deduplicate
    /// metrics would violate the capture telemetry boundary.
    pub(crate) fn record_capture_outcome(
        &self,
        record: CaptureMetricRecord,
        idempotent_replay: bool,
    ) -> Result<bool> {
        if idempotent_replay {
            return Ok(false);
        }
        self.append_event(AdoptionEvent::CaptureOutcome {
            timestamp_secs: now_secs(),
            integration: capture_integration(&record.integration),
            schema_version: bounded_capture_version(
                record.schema_version,
                MAX_CAPTURE_SCHEMA_VERSION,
            ),
            extractor_version: bounded_capture_version(
                record.extractor_version,
                MAX_CAPTURE_EXTRACTOR_VERSION,
            ),
            outcome: record.outcome,
        })?;
        Ok(true)
    }

    pub(crate) fn render_table(&self, days: usize) -> Result<String> {
        let ledger = self.read_ledger()?;
        Ok(render_table_from_ledger(&ledger, days))
    }

    pub(crate) fn read_json(&self) -> Result<serde_json::Value> {
        let ledger = self.read_ledger()?;
        serde_json::to_value(ledger).map_err(Into::into)
    }

    fn capture_health(&self) -> Result<CaptureHealth> {
        let _guard = self.lock_state()?;
        let mut connection = self.open_connection()?;
        let transaction = begin_write(&mut connection)?;
        prune_expired(&transaction)?;
        if !reconcile_capture_aggregate(&transaction)? {
            transaction.commit()?;
            anyhow::bail!("capture health aggregation migration is in progress; retry")
        }
        prune_capture_days(&transaction)?;
        let start_day = capture_health_start_day();
        let mut statement = transaction.prepare(
            "SELECT outcome,SUM(total) FROM capture_outcome_daily WHERE day_utc>=?1
             GROUP BY outcome ORDER BY outcome",
        )?;
        let rows = statement
            .query_map([start_day], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        transaction.commit()?;
        let mut health = CaptureHealth::default();
        for (outcome, count) in rows {
            health.total_attempts = health.total_attempts.saturating_add(count);
            health.outcomes.insert(outcome, count);
        }
        Ok(health)
    }

    fn append_event(&self, event: AdoptionEvent) -> Result<()> {
        let _guard = self.lock_state()?;
        let mut connection = self.open_connection()?;
        let transaction = begin_write(&mut connection)?;
        prune_expired(&transaction)?;
        insert_event(&transaction, &event)?;
        transaction
            .commit()
            .context("failed to commit adoption metric")
    }

    fn append_event_once(&self, metric_id: String, event: AdoptionEvent) -> Result<bool> {
        let _guard = self.lock_state()?;
        let mut connection = self.open_connection()?;
        let transaction = begin_write(&mut connection)?;
        prune_expired(&transaction)?;
        if let Some(existing) = event_for_metric_id(&transaction, &metric_id)? {
            if metric_event_matches(&existing, &event) {
                transaction
                    .commit()
                    .context("failed to commit metric retry")?;
                return Ok(false);
            }
            return Err(anyhow::anyhow!(
                "adoption metric id already exists with different payload"
            ));
        }
        insert_event(&transaction, &event)?;
        transaction
            .commit()
            .context("failed to commit adoption metric")?;
        Ok(true)
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        self.lock
            .lock()
            .map_err(|_| anyhow::anyhow!("adoption metrics lock poisoned"))
    }

    fn read_ledger(&self) -> Result<AdoptionLedger> {
        let _guard = self.lock_state()?;
        let mut connection = self.open_connection()?;
        let transaction = begin_write(&mut connection)?;
        prune_expired(&transaction)?;
        let events = read_events(&transaction)?;
        transaction
            .commit()
            .context("failed to commit retention cleanup")?;
        Ok(ledger_from_events(&events))
    }

    #[cfg(test)]
    fn ensure_parent_dir(&self) -> Result<()> {
        self.home()?;
        Ok(())
    }

    fn open_connection(&self) -> Result<ManagedSqlite> {
        let mut connection = ManagedSqlite::open(
            self.home()?,
            DATABASE_FILE,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .with_context(|| format!("failed to open {}", self.path.display()))?;
        connection
            .busy_timeout(std::time::Duration::from_secs(10))
            .context("failed to configure adoption metrics SQLite busy timeout")?;
        initialize_schema(&connection)?;
        self.import_legacy_if_needed(&mut connection)?;
        Ok(connection)
    }

    /// Imports bounded pages of the retired ledger. Each page and its offset
    /// commit atomically; normal writes wait for the completion marker so they
    /// cannot race an as-yet unread historical metric ID.
    fn import_legacy_if_needed(&self, connection: &mut Connection) -> Result<()> {
        let transaction = begin_write(connection)?;
        let imported: Option<String> = transaction
            .query_row(
                "SELECT value FROM adoption_metric_metadata WHERE key = ?1",
                [MIGRATION_MARKER],
                |row| row.get(0),
            )
            .optional()?;
        if imported.is_some() {
            transaction.commit()?;
            return Ok(());
        }
        let cursor: Option<String> = transaction.query_row(
            "SELECT CASE WHEN length(CAST(value AS BLOB)) <= 4096 THEN value ELSE 'oversized migration cursor' END FROM adoption_metric_metadata WHERE key=?1",
            [MIGRATION_CURSOR], |row| row.get(0),
        ).optional()?;
        let file = match self.home()?.open_file(LEGACY_LEDGER_FILE, false) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && cursor.is_none() => None,
            Err(error) => return Err(error).context("legacy telemetry source unavailable during migration; preserve the source and retry"),
        };
        if let Some(mut file) = file {
            let signature = legacy_signature(&file)?;
            let mut cursor = match cursor {
                Some(value) => serde_json::from_str::<LegacyImportCursor>(&value)
                    .context("invalid legacy telemetry migration cursor")?,
                None => LegacyImportCursor {
                    signature: signature.clone(),
                    offset: 0,
                    lines: 0,
                },
            };
            if cursor.signature != signature || cursor.offset > file.metadata()?.len() {
                anyhow::bail!("legacy telemetry source changed during migration; restore the original source before retrying");
            }
            file.seek(SeekFrom::Start(cursor.offset))?;
            let initial_offset = cursor.offset;
            let mut reader = BufReader::new(file);
            let mut complete = false;
            for _ in 0..LEGACY_PAGE_RECORDS {
                if cursor.offset != initial_offset
                    && LEGACY_PAGE_BYTES.saturating_sub(cursor.offset - initial_offset)
                        < LEGACY_RECORD_BYTES + 1
                {
                    break;
                }
                let mut line = Vec::new();
                let read = Read::by_ref(&mut reader)
                    .take(LEGACY_RECORD_BYTES + 1)
                    .read_until(b'\n', &mut line)?;
                if read == 0 {
                    complete = true;
                    break;
                }
                if read as u64 > LEGACY_RECORD_BYTES {
                    anyhow::bail!("legacy telemetry line {} exceeds the 1 MiB migration record limit; source preserved", cursor.lines + 1);
                }
                cursor.offset = cursor
                    .offset
                    .checked_add(read as u64)
                    .context("legacy telemetry byte cursor overflow")?;
                cursor.lines = cursor
                    .lines
                    .checked_add(1)
                    .context("legacy telemetry line cursor overflow")?;
                let terminated = line.ends_with(b"\n");
                let text = std::str::from_utf8(&line)
                    .context("legacy telemetry contains invalid UTF-8")?;
                if text.trim().is_empty() {
                    continue;
                }
                let event: AdoptionEvent = match serde_json::from_str(text) {
                    Ok(event) => event,
                    Err(_) if !terminated => {
                        complete = true;
                        break;
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "failed to parse legacy adoption metrics {} line {}",
                                self.legacy_path.display(),
                                cursor.lines
                            )
                        })
                    }
                };
                if let Some(metric_id) = event_metric_id(&event) {
                    if let Some(existing) = event_for_metric_id(&transaction, metric_id)? {
                        if !metric_event_matches(&existing, &event) {
                            anyhow::bail!("legacy adoption metric id {metric_id:?} conflicts with existing SQLite event");
                        }
                        continue;
                    }
                }
                insert_event(&transaction, &event)?;
            }
            if legacy_signature(reader.get_ref())? != signature {
                anyhow::bail!("legacy telemetry source changed while importing; page rolled back");
            }
            if !complete && cursor.offset == reader.get_ref().metadata()?.len() {
                complete = true;
            }
            if !complete {
                transaction.execute("INSERT INTO adoption_metric_metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![MIGRATION_CURSOR, serde_json::to_string(&cursor)?])?;
                transaction
                    .commit()
                    .context("failed to commit legacy telemetry migration page")?;
                anyhow::bail!("legacy telemetry migration in progress at byte {}; retry to advance the next bounded page", cursor.offset);
            }
        }
        transaction.execute(
            "DELETE FROM adoption_metric_metadata WHERE key=?1",
            [MIGRATION_CURSOR],
        )?;
        prune_expired(&transaction)?;
        transaction.execute(
            "INSERT INTO adoption_metric_metadata(key, value) VALUES(?1, ?2)",
            params![MIGRATION_MARKER, "complete"],
        )?;
        transaction
            .commit()
            .context("failed to commit legacy adoption metrics import")
    }
}

fn initialize_schema(connection: &Connection) -> Result<()> {
    const SCHEMA: &str = "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;
        CREATE TABLE IF NOT EXISTS adoption_metric_events (
          sequence INTEGER PRIMARY KEY,
          timestamp_secs INTEGER NOT NULL,
          metric_id TEXT UNIQUE,
          event_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS adoption_metric_events_timestamp
          ON adoption_metric_events(timestamp_secs);
        CREATE TABLE IF NOT EXISTS adoption_metric_metadata (
          key TEXT PRIMARY KEY, value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS capture_outcome_daily (
          day_utc TEXT NOT NULL,
          outcome TEXT NOT NULL CHECK(outcome IN ('captured','partially_captured','rejected','daemon_unavailable','store_unavailable','queued','skipped')),
          total INTEGER NOT NULL CHECK(total >= 0),
          PRIMARY KEY(day_utc,outcome)
        );
        CREATE TABLE IF NOT EXISTS capture_outcome_accounted (
          sequence INTEGER PRIMARY KEY REFERENCES adoption_metric_events(sequence) ON DELETE CASCADE
        );";

    // The first two independent stores can race to set WAL mode. SQLite does
    // not apply the connection busy handler to that mode transition on every
    // platform, so retry only that bounded initialization window.
    let mut last_error = None;
    for _ in 0..100 {
        match connection.execute_batch(SCHEMA) {
            Ok(()) => return Ok(()),
            Err(error) if sqlite_lock_error(&error) => {
                last_error = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => {
                return Err(error).context("failed to initialize adoption metrics SQLite schema")
            }
        }
    }
    Err(anyhow::anyhow!(
        "failed to initialize adoption metrics SQLite schema after waiting for a concurrent store: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "unknown SQLite lock error".to_string())
    ))
}

fn sqlite_lock_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

fn begin_write(connection: &mut Connection) -> Result<Transaction<'_>> {
    connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .context("failed to begin adoption metrics SQLite write transaction")
}

fn insert_event(transaction: &Transaction<'_>, event: &AdoptionEvent) -> Result<()> {
    let metric_id = event_metric_id(event);
    transaction
        .execute(
            "INSERT INTO adoption_metric_events(timestamp_secs, metric_id, event_json)
             VALUES(?1, ?2, ?3)",
            params![
                event_timestamp(event) as i64,
                metric_id,
                serde_json::to_string(event)?
            ],
        )
        .context("failed to insert adoption metric event")?;
    if matches!(event, AdoptionEvent::CaptureOutcome { .. }) {
        account_capture_event(transaction, transaction.last_insert_rowid(), event)?;
    }
    Ok(())
}

fn account_capture_event(
    transaction: &Transaction<'_>,
    sequence: i64,
    event: &AdoptionEvent,
) -> Result<()> {
    let AdoptionEvent::CaptureOutcome {
        timestamp_secs,
        outcome,
        ..
    } = event
    else {
        return Ok(());
    };
    let inserted = transaction.execute(
        "INSERT OR IGNORE INTO capture_outcome_accounted(sequence) VALUES(?1)",
        [sequence],
    )?;
    if inserted == 1 {
        transaction.execute(
            "INSERT INTO capture_outcome_daily(day_utc,outcome,total) VALUES(?1,?2,1)
             ON CONFLICT(day_utc,outcome) DO UPDATE SET total=total+1",
            params![
                date_key_from_epoch_day(*timestamp_secs / SECS_PER_DAY),
                outcome.label()
            ],
        )?;
    }
    Ok(())
}

fn reconcile_capture_aggregate(transaction: &Transaction<'_>) -> Result<bool> {
    let complete: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM adoption_metric_metadata WHERE key=?1)",
        [CAPTURE_AGGREGATE_MARKER],
        |row| row.get(0),
    )?;
    if complete {
        return Ok(true);
    }
    let cursor: i64 = transaction.query_row(
        "SELECT COALESCE((SELECT CAST(value AS INTEGER) FROM adoption_metric_metadata WHERE key=?1),0)",
        [CAPTURE_AGGREGATE_CURSOR], |row| row.get(0),
    )?;
    let page_end: Option<i64> = transaction.query_row(
        "SELECT MAX(sequence) FROM (SELECT sequence FROM adoption_metric_events WHERE sequence>?1 ORDER BY sequence LIMIT ?2)",
        params![cursor, CAPTURE_AGGREGATE_PAGE_ROWS as i64], |row| row.get(0),
    )?;
    let Some(page_end) = page_end else {
        transaction.execute(
            "INSERT OR REPLACE INTO adoption_metric_metadata(key,value) VALUES(?1,'complete')",
            [CAPTURE_AGGREGATE_MARKER],
        )?;
        transaction.execute(
            "DELETE FROM adoption_metric_metadata WHERE key=?1",
            [CAPTURE_AGGREGATE_CURSOR],
        )?;
        return Ok(true);
    };
    let oversized: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM adoption_metric_events
         WHERE sequence>?1 AND sequence<=?2
           AND instr(substr(event_json,1,64),'\"kind\":\"capture_outcome\"')>0
           AND length(event_json)>?3)",
        params![cursor, page_end, CAPTURE_EVENT_JSON_BYTES as i64],
        |row| row.get(0),
    )?;
    if oversized {
        anyhow::bail!("stored capture telemetry row exceeds aggregation limit")
    }
    let candidates = {
        let mut statement = transaction.prepare(
            "SELECT sequence,event_json FROM adoption_metric_events
             WHERE sequence>?1 AND sequence<=?2
               AND instr(substr(event_json,1,64),'\"kind\":\"capture_outcome\"')>0
             ORDER BY sequence",
        )?;
        let rows = statement
            .query_map(params![cursor, page_end], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for (sequence, json) in candidates {
        let event: AdoptionEvent =
            serde_json::from_str(&json).context("stored capture telemetry row is invalid")?;
        if !matches!(event, AdoptionEvent::CaptureOutcome { .. }) {
            anyhow::bail!("capture aggregation candidate has the wrong event kind")
        }
        account_capture_event(transaction, sequence, &event)?;
    }
    let remaining: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM adoption_metric_events WHERE sequence>?1)",
        [page_end],
        |row| row.get(0),
    )?;
    if remaining {
        transaction.execute(
            "INSERT INTO adoption_metric_metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![CAPTURE_AGGREGATE_CURSOR,page_end.to_string()],
        )?;
        Ok(false)
    } else {
        transaction.execute(
            "INSERT OR REPLACE INTO adoption_metric_metadata(key,value) VALUES(?1,'complete')",
            [CAPTURE_AGGREGATE_MARKER],
        )?;
        transaction.execute(
            "DELETE FROM adoption_metric_metadata WHERE key=?1",
            [CAPTURE_AGGREGATE_CURSOR],
        )?;
        Ok(true)
    }
}

fn capture_health_start_day() -> String {
    date_key_from_epoch_day(current_epoch_day().saturating_sub(RETENTION_DAYS - 1))
}

fn prune_capture_days(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute(
        "DELETE FROM capture_outcome_daily WHERE day_utc<?1",
        [capture_health_start_day()],
    )?;
    Ok(())
}

fn event_for_metric_id(
    transaction: &Transaction<'_>,
    metric_id: &str,
) -> Result<Option<AdoptionEvent>> {
    transaction
        .query_row(
            "SELECT event_json FROM adoption_metric_events WHERE metric_id = ?1 AND timestamp_secs >= ?2",
            params![metric_id, now_secs().saturating_sub(RETENTION_DAYS * SECS_PER_DAY) as i64],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|json| {
            serde_json::from_str(&json).context("stored adoption metric event is not valid JSON")
        })
        .transpose()
}

fn read_events(transaction: &Transaction<'_>) -> Result<Vec<AdoptionEvent>> {
    let mut statement = transaction.prepare(
        "SELECT event_json FROM adoption_metric_events WHERE timestamp_secs >= ?1
         ORDER BY timestamp_secs ASC, sequence ASC",
    )?;
    let events = statement
        .query_map(
            [now_secs().saturating_sub(RETENTION_DAYS * SECS_PER_DAY) as i64],
            |row| row.get::<_, String>(0),
        )?
        .map(|row| {
            let json = row?;
            serde_json::from_str(&json).context("stored adoption metric event is not valid JSON")
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(events)
}

/// Bound reclamation latency on a delivery write even after a long idle period.
/// Expired rows are excluded from reads while successive calls drain the backlog.
fn prune_expired(transaction: &Transaction<'_>) -> Result<()> {
    let cutoff = now_secs().saturating_sub(RETENTION_DAYS * SECS_PER_DAY) as i64;
    transaction
        .execute(
            "DELETE FROM adoption_metric_events WHERE sequence IN (SELECT sequence FROM adoption_metric_events WHERE timestamp_secs < ?1 ORDER BY timestamp_secs,sequence LIMIT 1024)",
            [cutoff],
        )
        .context("failed to prune expired adoption metric events")?;
    Ok(())
}

/// Metadata is checked before and after each page. A retired source must stay
/// unchanged until migration completes; this is an integrity fence, not proof
/// against a writer deliberately restoring identical filesystem metadata.
fn legacy_signature(file: &fs::File) -> Result<String> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        anyhow::bail!("legacy telemetry source must be a regular file");
    }
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let identity = format!("{:?}", metadata.created()?);
    Ok(format!(
        "{identity}:{}:{:?}",
        metadata.len(),
        metadata.modified()?
    ))
}

fn stable_metric_id(metric_id: &str) -> Result<String> {
    let metric_id = metric_id.trim();
    if metric_id.is_empty() {
        return Err(anyhow::anyhow!("adoption metric id must not be empty"));
    }
    if metric_id.chars().any(char::is_control) {
        return Err(anyhow::anyhow!(
            "adoption metric id must not contain control characters"
        ));
    }
    Ok(metric_id.to_string())
}

fn event_metric_id(event: &AdoptionEvent) -> Option<&str> {
    match event {
        AdoptionEvent::MemoryRetrieval { metric_id, .. }
        | AdoptionEvent::MemoryUse { metric_id, .. }
        | AdoptionEvent::MemoryInjection { metric_id, .. }
        | AdoptionEvent::MemoryInjectionAction { metric_id, .. } => metric_id.as_deref(),
        _ => None,
    }
}

fn metric_event_matches(existing: &AdoptionEvent, candidate: &AdoptionEvent) -> bool {
    match (existing, candidate) {
        (
            AdoptionEvent::MemoryRetrieval {
                metric_id: Some(a),
                session_id: asession,
                client: aclient,
                channel: achannel,
                retrieval_id: arid,
                retrieved_count: acount,
                ..
            },
            AdoptionEvent::MemoryRetrieval {
                metric_id: Some(b),
                session_id: bsession,
                client: bclient,
                channel: bchannel,
                retrieval_id: brid,
                retrieved_count: bcount,
                ..
            },
        ) => {
            a == b
                && asession == bsession
                && aclient == bclient
                && achannel == bchannel
                && arid == brid
                && acount == bcount
        }
        (
            AdoptionEvent::MemoryUse {
                metric_id: Some(a),
                retrieval_id: arid,
                used_count: acount,
                ..
            },
            AdoptionEvent::MemoryUse {
                metric_id: Some(b),
                retrieval_id: brid,
                used_count: bcount,
                ..
            },
        ) => a == b && arid == brid && acount == bcount,
        (
            AdoptionEvent::MemoryInjection {
                metric_id: Some(a),
                session_id: asession,
                client: aclient,
                channel: achannel,
                injection_id: aiid,
                shown_count: acount,
                ..
            },
            AdoptionEvent::MemoryInjection {
                metric_id: Some(b),
                session_id: bsession,
                client: bclient,
                channel: bchannel,
                injection_id: biid,
                shown_count: bcount,
                ..
            },
        ) => {
            a == b
                && asession == bsession
                && aclient == bclient
                && achannel == bchannel
                && aiid == biid
                && acount == bcount
        }
        (
            AdoptionEvent::MemoryInjectionAction {
                metric_id: Some(a),
                injection_id: aiid,
                acted_count: acount,
                ..
            },
            AdoptionEvent::MemoryInjectionAction {
                metric_id: Some(b),
                injection_id: biid,
                acted_count: bcount,
                ..
            },
        ) => a == b && aiid == biid && acount == bcount,
        _ => false,
    }
}

pub(crate) fn source_from_arguments(
    args: &serde_json::Value,
    default_client: &str,
    default_channel: &str,
) -> ToolCallSource {
    ToolCallSource {
        client: args
            .get("_lattice_client")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(default_client)
            .to_string(),
        channel: args
            .get("_lattice_channel")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(default_channel)
            .to_string(),
    }
}

/// Extracts only file references explicitly present in a successful tool
/// result. Free-form query text is deliberately ignored: follow-through must
/// be tied to files Lattice returned, not to strings the caller happened to
/// send us.
pub(crate) fn suggested_files_from_tool_result(
    tool: &str,
    result: &serde_json::Value,
) -> Vec<String> {
    if !matches!(
        tool,
        "context" | "impact" | "get_context_capsule" | "impact_from_diff"
    ) {
        return Vec::new();
    }
    let mut files = BTreeSet::new();
    collect_response_files(result, &mut files);
    files.into_iter().take(MAX_SUGGESTED_FILES).collect()
}

pub(crate) fn render_metrics_for_workspace(
    workspace: &Path,
    days: usize,
    json: bool,
) -> Result<String> {
    let store = AdoptionMetricsStore::new(workspace);
    if json {
        return Ok(serde_json::to_string_pretty(&store.read_json()?)?);
    }
    store.render_table(days)
}

/// Loads the content-free capture aggregate for operational health checks.
/// The returned value never exposes per-delivery identity or capture content.
pub(crate) fn capture_health_for_workspace(workspace: &Path) -> Result<CaptureHealth> {
    let Some(home) = AdoptionMetricsStore::resolve_home(workspace, false)? else {
        return Ok(CaptureHealth::default());
    };
    let store = AdoptionMetricsStore::from_home(workspace, Ok(home));
    let home = store.home()?;
    if home.metadata(DATABASE_FILE)?.is_none() && home.metadata(LEGACY_LEDGER_FILE)?.is_none() {
        return Ok(CaptureHealth::default());
    }
    store.capture_health()
}

fn ledger_from_events(events: &[AdoptionEvent]) -> AdoptionLedger {
    let mut ledger = AdoptionLedger::default();
    let mut pending = Vec::new();
    // Health injections join edits through their own pending list. Sharing the
    // one above would let a health citation and a `context` call compete for
    // the same edit, so crediting one would silently rob the other.
    let mut health_pending: Vec<PendingAssistance> = Vec::new();
    let mut memory_retrievals = BTreeMap::new();
    let mut memory_injections = BTreeMap::new();
    for event in events {
        match event {
            AdoptionEvent::ToolCall {
                timestamp_secs,
                session_id,
                client,
                channel,
                tool,
                latency_ms,
                suggested_files,
            } => {
                let day = date_key_from_epoch_day(*timestamp_secs / SECS_PER_DAY);
                let counter = ledger
                    .days
                    .entry(day.clone())
                    .or_default()
                    .entry(client.clone())
                    .or_default()
                    .entry(channel.clone())
                    .or_default()
                    .entry(tool.clone())
                    .or_default();
                counter.calls += 1;
                counter.total_latency_ms += latency_ms;
                counter.max_latency_ms = counter.max_latency_ms.max(*latency_ms);
                if matches!(tool.as_str(), "context" | "impact") && !suggested_files.is_empty() {
                    pending.push(PendingAssistance {
                        timestamp_secs: *timestamp_secs,
                        session_id: session_id.clone(),
                        suggested_files: suggested_files.iter().cloned().collect(),
                        counter_key: CounterKey {
                            day,
                            client: client.clone(),
                            channel: channel.clone(),
                            tool: tool.clone(),
                        },
                        credited: false,
                    });
                }
            }
            AdoptionEvent::ObservedEdit {
                timestamp_secs,
                session_id,
                file,
            } => {
                // Health evidence is credited independently of the tool-call
                // follow-through above: one edit can confirm both.
                if let Some(cited) = health_pending.iter_mut().rev().find(|candidate| {
                    !candidate.credited
                        && candidate.session_id == *session_id
                        && *timestamp_secs >= candidate.timestamp_secs
                        && timestamp_secs.saturating_sub(candidate.timestamp_secs)
                            <= FOLLOW_THROUGH_WINDOW_SECS
                        && candidate.suggested_files.contains(file)
                }) {
                    cited.credited = true;
                    if let Some(counter) = ledger
                        .days
                        .get_mut(&cited.counter_key.day)
                        .and_then(|clients| clients.get_mut(&cited.counter_key.client))
                        .and_then(|channels| channels.get_mut(&cited.counter_key.channel))
                        .and_then(|tools| tools.get_mut(&cited.counter_key.tool))
                    {
                        counter.health_evidence_followed += 1;
                    }
                }
                let Some(candidate) = pending.iter_mut().rev().find(|candidate| {
                    !candidate.credited
                        && candidate.session_id == *session_id
                        && *timestamp_secs >= candidate.timestamp_secs
                        && timestamp_secs.saturating_sub(candidate.timestamp_secs)
                            <= FOLLOW_THROUGH_WINDOW_SECS
                        && candidate.suggested_files.contains(file)
                }) else {
                    continue;
                };
                candidate.credited = true;
                if let Some(counter) = ledger
                    .days
                    .get_mut(&candidate.counter_key.day)
                    .and_then(|clients| clients.get_mut(&candidate.counter_key.client))
                    .and_then(|channels| channels.get_mut(&candidate.counter_key.channel))
                    .and_then(|tools| tools.get_mut(&candidate.counter_key.tool))
                {
                    counter.follow_through_edits += 1;
                }
            }
            AdoptionEvent::HealthEvidenceInjection {
                timestamp_secs,
                session_id,
                client,
                channel,
                cited_files,
                ..
            } => {
                let counter_key = health_counter_key(*timestamp_secs, client, channel);
                let counter = counter_for_key(&mut ledger, &counter_key);
                counter.health_evidence_injections += 1;
                counter.health_evidence_cited_files += cited_files.len() as u64;
                health_pending.push(PendingAssistance {
                    timestamp_secs: *timestamp_secs,
                    session_id: session_id.clone(),
                    suggested_files: cited_files.clone(),
                    counter_key,
                    credited: false,
                });
            }
            AdoptionEvent::MemoryRetrieval {
                timestamp_secs,
                client,
                channel,
                retrieval_id,
                retrieved_count,
                ..
            } => {
                let counter_key = memory_counter_key(*timestamp_secs, client, channel);
                let counter = counter_for_key(&mut ledger, &counter_key);
                counter.memory_retrievals += 1;
                counter.memory_retrieved_items += retrieved_count;
                // A retrieval id must identify exactly one retrieval. Preserve
                // the first event so a duplicate append cannot redirect a
                // later use record to a different day or source.
                memory_retrievals
                    .entry(retrieval_id.clone())
                    .or_insert(PendingMemoryRetrieval {
                        counter_key,
                        retrieved_count: *retrieved_count,
                        used_count: 0,
                    });
            }
            AdoptionEvent::MemoryUse {
                retrieval_id,
                used_count,
                ..
            } => {
                let Some(retrieval) = memory_retrievals.get_mut(retrieval_id) else {
                    continue;
                };
                let remaining = retrieval
                    .retrieved_count
                    .saturating_sub(retrieval.used_count);
                let credited = (*used_count).min(remaining);
                if credited == 0 {
                    continue;
                }
                retrieval.used_count += credited;
                counter_for_key(&mut ledger, &retrieval.counter_key).memory_used_items += credited;
            }
            AdoptionEvent::MemoryInjection {
                timestamp_secs,
                client,
                channel,
                injection_id,
                shown_count,
                ..
            } => {
                let counter_key = memory_counter_key(*timestamp_secs, client, channel);
                let counter = counter_for_key(&mut ledger, &counter_key);
                counter.memory_injections += 1;
                counter.memory_injected_items += shown_count;
                memory_injections
                    .entry(injection_id.clone())
                    .or_insert(PendingMemoryInjection {
                        counter_key,
                        shown_count: *shown_count,
                        acted_count: 0,
                    });
            }
            AdoptionEvent::MemoryInjectionAction {
                injection_id,
                acted_count,
                ..
            } => {
                let Some(injection) = memory_injections.get_mut(injection_id) else {
                    continue;
                };
                let remaining = injection.shown_count.saturating_sub(injection.acted_count);
                let credited = (*acted_count).min(remaining);
                if credited == 0 {
                    continue;
                }
                injection.acted_count += credited;
                counter_for_key(&mut ledger, &injection.counter_key).memory_injection_actions +=
                    credited;
            }
            AdoptionEvent::CaptureOutcome {
                integration,
                schema_version,
                extractor_version,
                outcome,
                ..
            } => {
                let schema = capture_version_label(*schema_version, MAX_CAPTURE_SCHEMA_VERSION);
                let extractor =
                    capture_version_label(*extractor_version, MAX_CAPTURE_EXTRACTOR_VERSION);
                let outcome = outcome.label().to_string();
                ledger.capture.total_attempts += 1;
                *ledger.capture.outcomes.entry(outcome.clone()).or_default() += 1;
                *ledger
                    .capture
                    .by_integration
                    .entry(capture_integration(integration))
                    .or_default()
                    .entry(schema)
                    .or_default()
                    .entry(extractor)
                    .or_default()
                    .entry(outcome)
                    .or_default() += 1;
            }
        }
    }
    ledger
}

fn memory_counter_key(timestamp_secs: u64, client: &str, channel: &str) -> CounterKey {
    CounterKey {
        day: date_key_from_epoch_day(timestamp_secs / SECS_PER_DAY),
        client: client.to_string(),
        channel: channel.to_string(),
        tool: "memory".to_string(),
    }
}

/// Health evidence is filed under its own synthetic tool key, exactly as memory
/// events are, so it never distorts a real verb's call and latency counts.
fn health_counter_key(timestamp_secs: u64, client: &str, channel: &str) -> CounterKey {
    CounterKey {
        day: date_key_from_epoch_day(timestamp_secs / SECS_PER_DAY),
        client: client.to_string(),
        channel: channel.to_string(),
        tool: "health".to_string(),
    }
}

fn counter_for_key<'a>(
    ledger: &'a mut AdoptionLedger,
    key: &CounterKey,
) -> &'a mut AdoptionCounter {
    ledger
        .days
        .entry(key.day.clone())
        .or_default()
        .entry(key.client.clone())
        .or_default()
        .entry(key.channel.clone())
        .or_default()
        .entry(key.tool.clone())
        .or_default()
}

fn event_timestamp(event: &AdoptionEvent) -> u64 {
    match event {
        AdoptionEvent::ToolCall { timestamp_secs, .. }
        | AdoptionEvent::ObservedEdit { timestamp_secs, .. }
        | AdoptionEvent::MemoryRetrieval { timestamp_secs, .. }
        | AdoptionEvent::MemoryUse { timestamp_secs, .. }
        | AdoptionEvent::MemoryInjection { timestamp_secs, .. }
        | AdoptionEvent::MemoryInjectionAction { timestamp_secs, .. }
        | AdoptionEvent::HealthEvidenceInjection { timestamp_secs, .. }
        | AdoptionEvent::CaptureOutcome { timestamp_secs, .. } => *timestamp_secs,
    }
}

fn capture_integration(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "codex" => "codex".to_string(),
        "claude" | "claude-code" | "claude_code" => "claude_code".to_string(),
        "cursor" => "cursor".to_string(),
        "generic" => "generic".to_string(),
        _ => "other".to_string(),
    }
}

fn bounded_capture_version(version: u32, maximum: u32) -> u32 {
    if (1..=maximum).contains(&version) {
        version
    } else {
        0
    }
}

fn capture_version_label(version: u32, maximum: u32) -> String {
    let version = bounded_capture_version(version, maximum);
    if version == 0 {
        "other".to_string()
    } else {
        version.to_string()
    }
}

fn normalize_suggested_files(files: Vec<String>) -> Vec<String> {
    files
        .into_iter()
        .filter_map(|file| clean_file(&file))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_SUGGESTED_FILES)
        .collect()
}

fn collect_response_files(value: &serde_json::Value, files: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object {
                match (key.as_str(), child) {
                    (
                        "file" | "from_file" | "to_file" | "path" | "f",
                        serde_json::Value::String(file),
                    ) => {
                        if let Some(file) = clean_file(file) {
                            files.insert(file);
                        }
                    }
                    ("files", serde_json::Value::Array(values)) => {
                        for value in values {
                            if let Some(file) = value.as_str().and_then(clean_file) {
                                files.insert(file);
                            }
                        }
                    }
                    ("text", serde_json::Value::String(text)) => {
                        if let Ok(payload) = serde_json::from_str(text) {
                            collect_response_files(&payload, files);
                        }
                    }
                    _ => collect_response_files(child, files),
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_response_files(value, files);
            }
        }
        _ => {}
    }
}

fn clean_file(file: &str) -> Option<String> {
    let file = file.trim().replace('\\', "/");
    (!file.is_empty()
        && !file.starts_with('/')
        && !file
            .split('/')
            .any(|segment| segment == ".." || segment.is_empty()))
    .then_some(file)
}

fn render_table_from_ledger(ledger: &AdoptionLedger, days: usize) -> String {
    let mut rows = Vec::new();
    let min_day = current_epoch_day().saturating_sub(days.saturating_sub(1) as u64);
    for (day, clients) in &ledger.days {
        let day_number = day_number_from_key(day);
        if day_number.is_some_and(|value| value < min_day) {
            continue;
        }
        for (client, channels) in clients {
            for (channel, tools) in channels {
                for (tool, counter) in tools {
                    let avg = if counter.calls == 0 {
                        0
                    } else {
                        counter.total_latency_ms / counter.calls
                    };
                    let rate = if counter.calls == 0 {
                        0.0
                    } else {
                        counter.follow_through_edits as f64 / counter.calls as f64
                    };
                    rows.push(format!(
                        "{day} | {client} | {channel} | {tool} | {} | {avg} | {} | {:.0}% | {} | {} | {} | {}",
                        counter.calls,
                        counter.max_latency_ms,
                        rate * 100.0,
                        counter.memory_retrieved_items,
                        counter.memory_used_items,
                        counter.memory_injected_items,
                        counter.memory_injection_actions,
                    ));
                }
            }
        }
    }
    let mut out =
        String::from(
            "day | client | channel | tool | calls | avg_ms | max_ms | follow_through | memories_returned | memories_used | memories_shown | injection_actions\n",
        );
    out.push_str("--- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---:\n");
    if rows.is_empty() {
        out.push_str("_no adoption metrics recorded_\n");
    } else {
        for row in rows {
            out.push_str(&row);
            out.push('\n');
        }
    }
    out
}

fn clean_key(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed
            .chars()
            .map(|ch| if ch.is_control() { '_' } else { ch })
            .collect()
    }
}

fn clean_identifier(value: &str, fallback: &str) -> String {
    let key = clean_key(value, fallback);
    key.chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '\n' | '\r' => '_',
            _ => ch,
        })
        .collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn current_epoch_day() -> u64 {
    now_secs() / SECS_PER_DAY
}

fn day_number_from_key(day: &str) -> Option<u64> {
    if let Some(raw) = day.strip_prefix("day-") {
        return raw.parse().ok();
    }
    let mut parts = day.split('-');
    let year = parts.next()?.parse::<i32>().ok()?;
    let month = parts.next()?.parse::<u32>().ok()?;
    let day = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day)? as u64)
}

fn date_key_from_epoch_day(epoch_day: u64) -> String {
    let (year, month, day) = civil_from_days(epoch_day as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    let year = year as i64 - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i64;
    let day = day as i64;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    if doy < 0 || doy > 365 {
        return None;
    }
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn telemetry_linked_checkout_uses_repository_home_and_preserves_old_local_audit() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("repo");
        fs::create_dir(&root).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=Telemetry Fixture",
            "-c",
            "user.email=fixture@invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ]);
        let linked = temporary.path().join("linked");
        git(&[
            "worktree",
            "add",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ]);
        fs::create_dir(linked.join(".lattice")).unwrap();
        let historical = linked.join(".lattice").join(LEGACY_LEDGER_FILE);
        fs::write(&historical, b"unproven historical audit\n").unwrap();
        let primary = AdoptionMetricsStore::new(&root);
        let secondary = AdoptionMetricsStore::new(&linked);
        assert_eq!(primary.path, secondary.path);
        primary
            .record(call_record("primary", "context", &[]))
            .unwrap();
        secondary
            .record(call_record("linked", "recall", &[]))
            .unwrap();
        assert_eq!(database_events(&primary).len(), 2);
        assert_eq!(
            fs::read(&historical).unwrap(),
            b"unproven historical audit\n"
        );
        assert!(!linked.join(".lattice").join(DATABASE_FILE).exists());
    }

    #[test]
    fn telemetry_invalid_git_authority_does_not_fall_back_to_local_storage() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join(".git"), "gitdir: missing\n").unwrap();
        let store = AdoptionMetricsStore::new(temporary.path());
        assert!(store
            .record(call_record("session", "context", &[]))
            .is_err());
        assert!(capture_health_for_workspace(temporary.path()).is_err());
        assert!(!temporary.path().join(".lattice").exists());
    }

    #[test]
    fn telemetry_home_replacement_keeps_database_and_import_on_pinned_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let store = AdoptionMetricsStore::new(temporary.path());
        let home = temporary.path().join(".lattice");
        let old = temporary.path().join("pinned-original");
        fs::rename(&home, &old).unwrap();
        fs::create_dir(&home).unwrap();
        fs::write(home.join(LEGACY_LEDGER_FILE), "foreign invalid payload\n").unwrap();
        store
            .record(call_record("session", "context", &[]))
            .unwrap();
        assert_eq!(database_events(&store).len(), 1);
        assert!(old.join(DATABASE_FILE).exists());
        assert!(!home.join(DATABASE_FILE).exists());
        assert_eq!(
            fs::read(home.join(LEGACY_LEDGER_FILE)).unwrap(),
            b"foreign invalid payload\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn telemetry_rejects_symlink_homes_databases_and_legacy_sources() {
        for leaf in [".lattice", DATABASE_FILE, LEGACY_LEDGER_FILE] {
            let temporary = tempfile::tempdir().unwrap();
            let workspace = temporary.path().join("workspace");
            let outside = temporary.path().join("outside");
            fs::create_dir(&workspace).unwrap();
            fs::create_dir(&outside).unwrap();
            let marker = outside.join("preserve");
            fs::write(&marker, b"foreign data").unwrap();
            if leaf == ".lattice" {
                std::os::unix::fs::symlink(&outside, workspace.join(leaf)).unwrap();
            } else {
                fs::create_dir(workspace.join(".lattice")).unwrap();
                std::os::unix::fs::symlink(&marker, workspace.join(".lattice").join(leaf)).unwrap();
            }
            let store = AdoptionMetricsStore::new(&workspace);
            assert!(
                store
                    .record(call_record("session", "context", &[]))
                    .is_err(),
                "{leaf}"
            );
            assert_eq!(fs::read(&marker).unwrap(), b"foreign data");
            assert!(!outside.join(DATABASE_FILE).exists());
        }
    }

    #[test]
    fn watcher_edit_credits_only_same_session_and_suggested_file() {
        let events = vec![
            call(10, "session-a", "context", &["src/auth.rs"]),
            call(11, "session-b", "impact", &["src/auth.rs"]),
            edit(12, "session-b", "src/auth.rs"),
            edit(13, "session-a", "src/other.rs"),
            edit(14, "session-a", "src/auth.rs"),
        ];
        let ledger = ledger_from_events(&events);
        let tools = today_tools(&ledger);

        assert_eq!(tools["context"].follow_through_edits, 1);
        assert_eq!(tools["impact"].follow_through_edits, 1);
    }

    fn health_evidence(timestamp: u64, session: &str, tool: &str, files: &[&str]) -> AdoptionEvent {
        AdoptionEvent::HealthEvidenceInjection {
            timestamp_secs: timestamp,
            metric_id: None,
            session_id: session.to_string(),
            client: "codex".to_string(),
            channel: "mcp".to_string(),
            tool: tool.to_string(),
            cited_files: files.iter().map(ToString::to_string).collect(),
        }
    }

    /// Spec H4.5: whether injected health evidence was followed.
    #[test]
    fn health_evidence_is_credited_when_a_cited_file_is_edited() {
        let events = vec![
            health_evidence(10, "session-a", "impact", &["src/hot.rs", "src/calm.rs"]),
            edit(12, "session-a", "src/hot.rs"),
        ];
        let counter = today_tools(&ledger_from_events(&events))["health"].clone();

        assert_eq!(counter.health_evidence_injections, 1);
        assert_eq!(counter.health_evidence_cited_files, 2);
        assert_eq!(counter.health_evidence_followed, 1);
    }

    #[test]
    fn health_evidence_is_not_credited_for_an_unrelated_or_late_edit() {
        let events = vec![
            health_evidence(10, "session-a", "impact", &["src/hot.rs"]),
            // Right file, wrong session.
            edit(11, "session-b", "src/hot.rs"),
            // Right session, wrong file.
            edit(12, "session-a", "src/elsewhere.rs"),
            // Right session and file, outside the window.
            edit(
                10 + FOLLOW_THROUGH_WINDOW_SECS + 1,
                "session-a",
                "src/hot.rs",
            ),
        ];
        let counter = today_tools(&ledger_from_events(&events))["health"].clone();

        assert_eq!(counter.health_evidence_injections, 1);
        assert_eq!(counter.health_evidence_followed, 0);
    }

    /// Health evidence must not steal an edit from the verb that suggested the
    /// same file: one edit can honestly confirm both.
    #[test]
    fn health_evidence_credit_does_not_displace_tool_follow_through() {
        let events = vec![
            call(10, "session-a", "impact", &["src/hot.rs"]),
            health_evidence(10, "session-a", "impact", &["src/hot.rs"]),
            edit(12, "session-a", "src/hot.rs"),
        ];
        let ledger = ledger_from_events(&events);
        let tools = today_tools(&ledger);

        assert_eq!(tools["impact"].follow_through_edits, 1);
        assert_eq!(tools["health"].health_evidence_followed, 1);
        // The health row is synthetic and must not inflate real call counts.
        assert_eq!(tools["health"].calls, 0);
    }

    /// Replaying the same log twice must produce the same counters.
    #[test]
    fn health_evidence_replay_is_deterministic_and_credited_at_most_once() {
        let events = vec![
            health_evidence(10, "session-a", "impact", &["src/hot.rs"]),
            edit(11, "session-a", "src/hot.rs"),
            edit(12, "session-a", "src/hot.rs"),
        ];
        let first = today_tools(&ledger_from_events(&events))["health"].clone();
        let second = today_tools(&ledger_from_events(&events))["health"].clone();

        assert_eq!(
            first.health_evidence_followed, 1,
            "a single injection is credited once however many edits follow"
        );
        assert_eq!(
            first.health_evidence_followed,
            second.health_evidence_followed
        );
        assert_eq!(
            first.health_evidence_injections,
            second.health_evidence_injections
        );
    }

    #[test]
    fn observed_edit_does_not_credit_outside_bounded_session_window() {
        let events = vec![
            call(10, "session-a", "context", &["src/auth.rs"]),
            edit(
                10 + FOLLOW_THROUGH_WINDOW_SECS + 1,
                "session-a",
                "src/auth.rs",
            ),
        ];
        let ledger = ledger_from_events(&events);
        assert_eq!(today_tools(&ledger)["context"].follow_through_edits, 0);
    }

    #[test]
    fn expired_backlog_reclamation_is_bounded_and_hidden_from_reads() {
        let root = tempfile::tempdir().unwrap();
        let store = AdoptionMetricsStore::new(root.path());
        let mut connection = store.open_connection().unwrap();
        let tx = begin_write(&mut connection).unwrap();
        for index in 0..2500 {
            insert_event(
                &tx,
                &AdoptionEvent::ObservedEdit {
                    timestamp_secs: now_secs().saturating_sub((RETENTION_DAYS + 1) * SECS_PER_DAY),
                    session_id: format!("expired-{index}"),
                    file: "old.rs".into(),
                },
            )
            .unwrap();
        }
        prune_expired(&tx).unwrap();
        assert_eq!(
            tx.query_row("SELECT count(*) FROM adoption_metric_events", [], |r| r
                .get::<_, usize>(
                0
            ))
            .unwrap(),
            1476
        );
        assert!(read_events(&tx).unwrap().is_empty());
        prune_expired(&tx).unwrap();
        assert_eq!(
            tx.query_row("SELECT count(*) FROM adoption_metric_events", [], |r| r
                .get::<_, usize>(
                0
            ))
            .unwrap(),
            452
        );
        prune_expired(&tx).unwrap();
        assert_eq!(
            tx.query_row("SELECT count(*) FROM adoption_metric_events", [], |r| r
                .get::<_, usize>(
                0
            ))
            .unwrap(),
            0
        );
        tx.commit().unwrap();
    }

    #[test]
    fn append_only_records_and_compacts_entries_past_retention() {
        let root = unique_root("retention");
        let store = AdoptionMetricsStore::new(&root);
        let old = AdoptionEvent::ObservedEdit {
            timestamp_secs: now_secs().saturating_sub((RETENTION_DAYS + 1) * SECS_PER_DAY),
            session_id: "old".to_string(),
            file: "src/old.rs".to_string(),
        };
        store.ensure_parent_dir().expect("metrics parent");
        fs::write(
            &store.legacy_path,
            format!("{}\n", serde_json::to_string(&old).unwrap()),
        )
        .expect("seed old event");
        store
            .record(call_record("session-a", "context", &["src/auth.rs"]))
            .expect("append event");

        let events = database_events(&store);
        assert_eq!(events.len(), 1, "old record should be pruned");
        assert!(matches!(events[0], AdoptionEvent::ToolCall { .. }));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hidden_arguments_override_default_source() {
        let source = source_from_arguments(
            &serde_json::json!({
                "_lattice_client": "lattice-cli",
                "_lattice_channel": "cli"
            }),
            "mcp",
            "mcp",
        );
        assert_eq!(source.client, "lattice-cli");
        assert_eq!(source.channel, "cli");
    }

    #[test]
    fn result_file_extraction_ignores_caller_text_and_keeps_returned_files() {
        let files = suggested_files_from_tool_result(
            "context",
            &serde_json::json!({
                "content": [{
                    "text": "{\"pivots\":[{\"file\":\"src/auth.rs\"}],\"query\":\"do not credit src/untrusted.rs\"}"
                }]
            }),
        );

        assert_eq!(files, vec!["src/auth.rs".to_string()]);
    }

    #[test]
    fn memory_value_metrics_join_only_known_ids_and_cap_each_item_once() {
        let events = vec![
            memory_retrieval(10, "retrieval-a", 3),
            memory_use(11, "unknown-retrieval", 99),
            memory_use(12, "retrieval-a", 2),
            memory_use(13, "retrieval-a", 2),
            memory_injection(14, "injection-a", 2),
            memory_injection_action(15, "unknown-injection", 99),
            memory_injection_action(16, "injection-a", 1),
            memory_injection_action(17, "injection-a", 2),
        ];

        let ledger = ledger_from_events(&events);
        let memory = &day_tools(&ledger, "hook")["memory"];
        assert_eq!(memory.memory_retrievals, 1);
        assert_eq!(memory.memory_retrieved_items, 3);
        assert_eq!(memory.memory_used_items, 3);
        assert_eq!(memory.memory_injections, 1);
        assert_eq!(memory.memory_injected_items, 2);
        assert_eq!(memory.memory_injection_actions, 2);
    }

    #[test]
    fn memory_metric_events_obey_the_same_ninety_day_retention() {
        let root = unique_root("memory-retention");
        let store = AdoptionMetricsStore::new(&root);
        let old = memory_retrieval(
            now_secs().saturating_sub((RETENTION_DAYS + 1) * SECS_PER_DAY),
            "retrieval-old",
            1,
        );
        store.ensure_parent_dir().expect("metrics parent");
        fs::write(
            &store.legacy_path,
            format!("{}\n", serde_json::to_string(&old).unwrap()),
        )
        .expect("seed old event");
        store
            .record_memory_retrieval(MemoryRetrievalRecord {
                session_id: "session-a".to_string(),
                client: "codex".to_string(),
                channel: "hook".to_string(),
                retrieval_id: "retrieval-current".to_string(),
                retrieved_count: 1,
            })
            .expect("append memory retrieval");

        let events = database_events(&store);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AdoptionEvent::MemoryRetrieval { retrieval_id, .. } if retrieval_id == "retrieval-current"
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stable_metric_id_appends_once_even_when_recovery_metadata_changes() {
        let root = unique_root("metric-id-deduplication");
        let store = AdoptionMetricsStore::new(&root);
        let record = MemoryRetrievalRecord {
            session_id: "session-a".to_string(),
            client: "codex".to_string(),
            channel: "hook".to_string(),
            retrieval_id: "retrieval-a".to_string(),
            retrieved_count: 2,
        };

        assert!(store
            .record_memory_retrieval_once("retrieval:retrieval-a", record.clone())
            .expect("first append"));
        assert!(!store
            .record_memory_retrieval_once("retrieval:retrieval-a", record.clone())
            .expect("recovered retry"));
        assert!(
            store
                .record_memory_retrieval_once(
                    "retrieval:retrieval-a",
                    MemoryRetrievalRecord {
                        session_id: "session-after-restart".to_string(),
                        retrieved_count: 3,
                        ..record
                    },
                )
                .is_err(),
            "same id with a different payload must fail"
        );

        let events = database_events(&store);
        assert_eq!(events.len(), 1);
        assert_eq!(event_metric_id(&events[0]), Some("retrieval:retrieval-a"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stable_metric_id_deduplicates_across_independent_store_instances() {
        let root = unique_root("cross-store-metric-id-deduplication");
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                let store = AdoptionMetricsStore::new(&root);
                barrier.wait();
                store.record_memory_use_once(
                    "use:terminal-a",
                    MemoryUseRecord {
                        retrieval_id: "retrieval-a".to_string(),
                        used_count: 1,
                    },
                )
            }));
        }
        barrier.wait();
        let inserted = workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .expect("worker panicked")
                    .expect("metric append")
            })
            .filter(|inserted| *inserted)
            .count();

        assert_eq!(inserted, 1);
        let store = AdoptionMetricsStore::new(&root);
        assert_eq!(database_events(&store).len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn injection_metric_id_is_idempotent_and_rejects_conflicts() {
        let root = unique_root("injection-metric-id");
        let store = AdoptionMetricsStore::new(&root);
        let record = MemoryInjectionRecord {
            session_id: "session-a".to_string(),
            client: "codex".to_string(),
            channel: "hook".to_string(),
            injection_id: "injection-a".to_string(),
            shown_count: 2,
        };
        assert!(store
            .record_memory_injection_once("injection:injection-a", record.clone())
            .expect("first injection append"));
        assert!(!store
            .record_memory_injection_once("injection:injection-a", record.clone())
            .expect("replayed injection"));
        assert!(store
            .record_memory_injection_once(
                "injection:injection-a",
                MemoryInjectionRecord {
                    shown_count: 3,
                    ..record
                },
            )
            .is_err());
        assert_eq!(database_events(&store).len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn injection_action_metric_id_survives_compaction_and_replay() {
        let root = unique_root("injection-action-metric-id");
        let store = AdoptionMetricsStore::new(&root);
        let record = MemoryInjectionActionRecord {
            injection_id: "injection-a".to_string(),
            acted_count: 1,
        };
        store
            .record_memory_injection(MemoryInjectionRecord {
                session_id: "session-a".to_string(),
                client: "codex".to_string(),
                channel: "hook".to_string(),
                injection_id: "injection-a".to_string(),
                shown_count: 1,
            })
            .expect("injection for action join");
        assert!(store
            .record_memory_injection_action_once("action:injection-a", record.clone())
            .expect("first action append"));
        assert!(!store
            .record_memory_injection_action_once("action:injection-a", record)
            .expect("replayed action after compaction"));
        assert_eq!(database_events(&store).len(), 2);
        let ledger = store.read_json().expect("replay compacted ledger");
        assert_eq!(ledger["days"].as_object().unwrap().len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn capture_outcomes_are_content_free_bounded_and_replay_safe() {
        let root = unique_root("capture-outcomes");
        let store = AdoptionMetricsStore::new(&root);
        let captured = CaptureMetricRecord {
            integration: "codex".to_string(),
            schema_version: 1,
            extractor_version: 2,
            outcome: CaptureOutcome::Captured,
        };
        assert!(store
            .record_capture_outcome(captured.clone(), false)
            .expect("new capture outcome"));
        assert!(!store
            .record_capture_outcome(captured, true)
            .expect("replayed capture outcome"));
        assert!(store
            .record_capture_outcome(
                CaptureMetricRecord {
                    integration: "untrusted-integration-value".to_string(),
                    schema_version: MAX_CAPTURE_SCHEMA_VERSION + 1,
                    extractor_version: MAX_CAPTURE_EXTRACTOR_VERSION + 1,
                    outcome: CaptureOutcome::StoreUnavailable,
                },
                false,
            )
            .expect("failed capture outcome"));

        let json = store.read_json().expect("capture ledger");
        assert_eq!(json["capture"]["total_attempts"], 2);
        assert_eq!(json["capture"]["outcomes"]["captured"], 1);
        assert_eq!(json["capture"]["outcomes"]["store_unavailable"], 1);
        assert_eq!(
            json["capture"]["by_integration"]["other"]["other"]["other"]["store_unavailable"],
            1
        );
        assert!(!json.to_string().contains("untrusted-integration-value"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn capture_ledger_counts_each_supported_outcome() {
        let outcomes = [
            CaptureOutcome::Captured,
            CaptureOutcome::PartiallyCaptured,
            CaptureOutcome::Rejected,
            CaptureOutcome::DaemonUnavailable,
            CaptureOutcome::StoreUnavailable,
            CaptureOutcome::Queued,
            CaptureOutcome::Skipped,
        ];
        let events = outcomes
            .into_iter()
            .map(|outcome| capture(1, "codex", 1, 1, outcome))
            .collect::<Vec<_>>();

        let ledger = ledger_from_events(&events);

        assert_eq!(ledger.capture.total_attempts, 7);
        for outcome in outcomes {
            assert_eq!(ledger.capture.outcomes[outcome.label()], 1);
        }
    }

    #[test]
    fn capture_metric_events_obey_ninety_day_retention() {
        let root = unique_root("capture-retention");
        let store = AdoptionMetricsStore::new(&root);
        store.ensure_parent_dir().expect("metrics parent");
        let old = capture(
            now_secs().saturating_sub((RETENTION_DAYS + 1) * SECS_PER_DAY),
            "codex",
            1,
            1,
            CaptureOutcome::Rejected,
        );
        fs::write(
            &store.legacy_path,
            format!("{}\n", serde_json::to_string(&old).unwrap()),
        )
        .expect("seed old capture event");

        store
            .record_capture_outcome(
                CaptureMetricRecord {
                    integration: "codex".to_string(),
                    schema_version: 1,
                    extractor_version: 1,
                    outcome: CaptureOutcome::Captured,
                },
                false,
            )
            .expect("append current capture event");

        let events = database_events(&store);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            AdoptionEvent::CaptureOutcome {
                outcome: CaptureOutcome::Captured,
                ..
            }
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn capture_health_read_does_not_create_metrics_state() {
        let root = unique_root("capture-health-empty");

        assert_eq!(
            capture_health_for_workspace(&root).expect("empty capture health"),
            CaptureHealth::default()
        );
        assert!(!root.join(".lattice").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn capture_health_skips_large_unrelated_event_payloads() {
        let root = unique_root("capture-health-unrelated");
        let store = AdoptionMetricsStore::new(&root);
        store.ensure_parent_dir().unwrap();
        let connection = Connection::open(&store.path).unwrap();
        initialize_schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO adoption_metric_events(timestamp_secs,event_json) VALUES(?1,?2)",
                params![now_secs() as i64, "x".repeat(2 * CAPTURE_EVENT_JSON_BYTES)],
            )
            .unwrap();
        let event = capture(now_secs(), "codex", 1, 1, CaptureOutcome::Rejected);
        connection
            .execute(
                "INSERT INTO adoption_metric_events(timestamp_secs,event_json) VALUES(?1,?2)",
                params![
                    event_timestamp(&event) as i64,
                    serde_json::to_string(&event).unwrap()
                ],
            )
            .unwrap();
        drop(connection);

        let health = store.capture_health().unwrap();
        assert_eq!(health.total_attempts, 1);
        assert_eq!(health.outcomes["rejected"], 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn capture_health_migration_is_bounded_restartable_and_never_partial() {
        let root = unique_root("capture-health-paged");
        let store = AdoptionMetricsStore::new(&root);
        store.ensure_parent_dir().unwrap();
        let mut connection = Connection::open(&store.path).unwrap();
        initialize_schema(&connection).unwrap();
        let transaction = connection.transaction().unwrap();
        for _ in 0..CAPTURE_AGGREGATE_PAGE_ROWS {
            transaction
                .execute(
                    "INSERT INTO adoption_metric_events(timestamp_secs,event_json) VALUES(?1,'{}')",
                    [now_secs() as i64],
                )
                .unwrap();
        }
        let event = capture(now_secs(), "codex", 1, 1, CaptureOutcome::Captured);
        transaction
            .execute(
                "INSERT INTO adoption_metric_events(timestamp_secs,event_json) VALUES(?1,?2)",
                params![
                    event_timestamp(&event) as i64,
                    serde_json::to_string(&event).unwrap()
                ],
            )
            .unwrap();
        transaction.commit().unwrap();
        drop(connection);

        assert!(store
            .capture_health()
            .unwrap_err()
            .to_string()
            .contains("in progress"));
        let restarted = AdoptionMetricsStore::new(&root);
        let health = restarted.capture_health().unwrap();
        assert_eq!(health.total_attempts, 1);
        assert_eq!(health.outcomes["captured"], 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn capture_health_uses_ninety_utc_calendar_days() {
        let root = unique_root("capture-health-calendar-retention");
        let store = AdoptionMetricsStore::new(&root);
        let mut connection = store.open_connection().unwrap();
        let transaction = begin_write(&mut connection).unwrap();
        insert_event(
            &transaction,
            &capture(
                current_epoch_day().saturating_sub(RETENTION_DAYS) * SECS_PER_DAY,
                "codex",
                1,
                1,
                CaptureOutcome::Rejected,
            ),
        )
        .unwrap();
        insert_event(
            &transaction,
            &capture(
                current_epoch_day().saturating_sub(RETENTION_DAYS - 1) * SECS_PER_DAY,
                "codex",
                1,
                1,
                CaptureOutcome::Captured,
            ),
        )
        .unwrap();
        insert_event(
            &transaction,
            &capture(
                current_epoch_day() * SECS_PER_DAY,
                "codex",
                1,
                1,
                CaptureOutcome::Captured,
            ),
        )
        .unwrap();
        transaction.commit().unwrap();
        let health = store.capture_health().unwrap();
        assert_eq!(health.total_attempts, 2);
        assert_eq!(health.outcomes["captured"], 2);
        assert!(!health.outcomes.contains_key("rejected"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stable_metric_append_discards_a_torn_final_jsonl_record_before_deduping() {
        let root = unique_root("torn-metric-id-append");
        let store = AdoptionMetricsStore::new(&root);
        let durable = AdoptionEvent::ObservedEdit {
            timestamp_secs: now_secs(),
            session_id: "session-a".to_string(),
            file: "src/auth.rs".to_string(),
        };
        store.ensure_parent_dir().expect("metrics parent");
        fs::write(
            &store.legacy_path,
            format!(
                "{}\n{{\"kind\":\"memory_retrieval\",\"timestamp_secs\":",
                serde_json::to_string(&durable).expect("serialize durable event")
            ),
        )
        .expect("seed torn log");

        assert!(store
            .record_memory_retrieval_once(
                "retrieval:after-crash",
                MemoryRetrievalRecord {
                    session_id: "session-a".to_string(),
                    client: "codex".to_string(),
                    channel: "hook".to_string(),
                    retrieval_id: "retrieval-after-crash".to_string(),
                    retrieved_count: 1,
                },
            )
            .expect("recover torn log and append"));

        let events = database_events(&store);
        assert_eq!(events.len(), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_import_pages_resume_without_duplicate_unkeyed_events() {
        let root = unique_root("legacy-paged");
        let store = AdoptionMetricsStore::new(&root);
        store.ensure_parent_dir().unwrap();
        let record =
            serde_json::to_string(&call(now_secs(), "session-a", "context", &[])).unwrap() + "\n";
        fs::write(&store.legacy_path, record.repeat(LEGACY_PAGE_RECORDS + 7)).unwrap();
        assert!(store
            .open_connection()
            .unwrap_err()
            .to_string()
            .contains("in progress"));
        let database = Connection::open(&store.path).unwrap();
        assert_eq!(
            database
                .query_row("SELECT COUNT(*) FROM adoption_metric_events", [], |row| row
                    .get::<_, usize>(0))
                .unwrap(),
            LEGACY_PAGE_RECORDS
        );
        drop(database);
        let reopened = AdoptionMetricsStore::new(&root);
        drop(reopened.open_connection().unwrap());
        drop(reopened.open_connection().unwrap());
        assert_eq!(database_events(&reopened).len(), LEGACY_PAGE_RECORDS + 7);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_import_rejects_source_replacement_and_oversized_records() {
        let root = unique_root("legacy-page-source");
        let store = AdoptionMetricsStore::new(&root);
        store.ensure_parent_dir().unwrap();
        let record =
            serde_json::to_string(&call(now_secs(), "session-a", "context", &[])).unwrap() + "\n";
        fs::write(&store.legacy_path, record.repeat(LEGACY_PAGE_RECORDS + 1)).unwrap();
        assert!(store.open_connection().is_err());
        fs::write(&store.legacy_path, "changed\n").unwrap();
        assert!(store
            .open_connection()
            .unwrap_err()
            .to_string()
            .contains("source changed"));
        fs::remove_dir_all(root).unwrap();

        let root = unique_root("legacy-record-budget");
        let store = AdoptionMetricsStore::new(&root);
        store.ensure_parent_dir().unwrap();
        fs::write(
            &store.legacy_path,
            vec![b'x'; LEGACY_RECORD_BYTES as usize + 1],
        )
        .unwrap();
        assert!(store
            .open_connection()
            .unwrap_err()
            .to_string()
            .contains("record limit"));
        let database = Connection::open(&store.path).unwrap();
        assert_eq!(
            database
                .query_row("SELECT COUNT(*) FROM adoption_metric_events", [], |row| row
                    .get::<_, usize>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            database
                .query_row(
                    "SELECT COUNT(*) FROM adoption_metric_metadata WHERE key=?1",
                    [MIGRATION_MARKER],
                    |row| row.get::<_, usize>(0)
                )
                .unwrap(),
            0
        );
        drop(database);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_import_is_once_only_across_store_restarts() {
        let root = unique_root("legacy-import-restart");
        let first = AdoptionMetricsStore::new(&root);
        first.ensure_parent_dir().expect("metrics parent");
        let now = now_secs();
        fs::write(
            &first.legacy_path,
            format!(
                "{}\n",
                serde_json::to_string(&call(now, "session-a", "context", &[])).unwrap()
            ),
        )
        .expect("seed legacy ledger");
        assert_eq!(database_events(&first).len(), 1);

        // A later edit to the preserved legacy file must not be imported after
        // the transactional marker has committed.
        fs::write(
            &first.legacy_path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&call(now, "session-a", "context", &[])).unwrap(),
                serde_json::to_string(&call(now + 1, "session-a", "impact", &[])).unwrap()
            ),
        )
        .expect("modify preserved legacy ledger");
        let restarted = AdoptionMetricsStore::new(&root);
        assert_eq!(database_events(&restarted).len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn incomplete_legacy_import_rejects_metric_id_conflicts_without_marker() {
        let root = unique_root("legacy-import-conflict");
        let store = AdoptionMetricsStore::new(&root);
        store.ensure_parent_dir().expect("metrics parent");
        let legacy = AdoptionEvent::MemoryUse {
            timestamp_secs: now_secs(),
            metric_id: Some("use:conflict".to_string()),
            retrieval_id: "legacy-retrieval".to_string(),
            used_count: 1,
        };
        fs::write(
            &store.legacy_path,
            format!("{}\n", serde_json::to_string(&legacy).unwrap()),
        )
        .expect("seed legacy ledger");

        let connection = Connection::open(&store.path).expect("create partial database");
        connection
            .execute_batch(
                "CREATE TABLE adoption_metric_events (
                   sequence INTEGER PRIMARY KEY,
                   timestamp_secs INTEGER NOT NULL,
                   metric_id TEXT UNIQUE,
                   event_json TEXT NOT NULL
                 );
                 CREATE INDEX adoption_metric_events_timestamp
                   ON adoption_metric_events(timestamp_secs);
                 CREATE TABLE adoption_metric_metadata (
                   key TEXT PRIMARY KEY, value TEXT NOT NULL
                 );",
            )
            .expect("create schema");
        let conflicting = AdoptionEvent::MemoryUse {
            timestamp_secs: now_secs(),
            metric_id: Some("use:conflict".to_string()),
            retrieval_id: "different-retrieval".to_string(),
            used_count: 1,
        };
        connection
            .execute(
                "INSERT INTO adoption_metric_events(timestamp_secs, metric_id, event_json)
                 VALUES(?1, ?2, ?3)",
                params![
                    event_timestamp(&conflicting) as i64,
                    "use:conflict",
                    serde_json::to_string(&conflicting).unwrap()
                ],
            )
            .expect("seed conflicting partial import");
        drop(connection);

        assert!(store
            .record(call_record("session-a", "context", &[]))
            .is_err());
        let connection = Connection::open(&store.path).expect("inspect partial database");
        let marker: Option<String> = connection
            .query_row(
                "SELECT value FROM adoption_metric_metadata WHERE key = ?1",
                [MIGRATION_MARKER],
                |row| row.get(0),
            )
            .optional()
            .expect("read marker");
        assert!(marker.is_none(), "failed migration must remain restartable");
        let _ = fs::remove_dir_all(root);
    }

    fn call(timestamp: u64, session: &str, tool: &str, files: &[&str]) -> AdoptionEvent {
        AdoptionEvent::ToolCall {
            timestamp_secs: timestamp,
            session_id: session.to_string(),
            client: "codex".to_string(),
            channel: "mcp".to_string(),
            tool: tool.to_string(),
            latency_ms: 10,
            suggested_files: files.iter().map(ToString::to_string).collect(),
        }
    }

    fn edit(timestamp: u64, session: &str, file: &str) -> AdoptionEvent {
        AdoptionEvent::ObservedEdit {
            timestamp_secs: timestamp,
            session_id: session.to_string(),
            file: file.to_string(),
        }
    }

    fn memory_retrieval(timestamp: u64, retrieval_id: &str, retrieved_count: u64) -> AdoptionEvent {
        AdoptionEvent::MemoryRetrieval {
            timestamp_secs: timestamp,
            metric_id: None,
            session_id: "session-a".to_string(),
            client: "codex".to_string(),
            channel: "hook".to_string(),
            retrieval_id: retrieval_id.to_string(),
            retrieved_count,
        }
    }

    fn memory_use(timestamp: u64, retrieval_id: &str, used_count: u64) -> AdoptionEvent {
        AdoptionEvent::MemoryUse {
            timestamp_secs: timestamp,
            metric_id: None,
            retrieval_id: retrieval_id.to_string(),
            used_count,
        }
    }

    fn memory_injection(timestamp: u64, injection_id: &str, shown_count: u64) -> AdoptionEvent {
        AdoptionEvent::MemoryInjection {
            timestamp_secs: timestamp,
            metric_id: None,
            session_id: "session-a".to_string(),
            client: "codex".to_string(),
            channel: "hook".to_string(),
            injection_id: injection_id.to_string(),
            shown_count,
        }
    }

    fn memory_injection_action(
        timestamp: u64,
        injection_id: &str,
        acted_count: u64,
    ) -> AdoptionEvent {
        AdoptionEvent::MemoryInjectionAction {
            timestamp_secs: timestamp,
            metric_id: None,
            injection_id: injection_id.to_string(),
            acted_count,
        }
    }

    fn capture(
        timestamp: u64,
        integration: &str,
        schema_version: u32,
        extractor_version: u32,
        outcome: CaptureOutcome,
    ) -> AdoptionEvent {
        AdoptionEvent::CaptureOutcome {
            timestamp_secs: timestamp,
            integration: integration.to_string(),
            schema_version,
            extractor_version,
            outcome,
        }
    }

    fn call_record(session: &str, tool: &str, files: &[&str]) -> ToolCallRecord {
        ToolCallRecord {
            session_id: session.to_string(),
            client: "codex".to_string(),
            channel: "mcp".to_string(),
            tool: tool.to_string(),
            latency_ms: 10,
            suggested_files: files.iter().map(ToString::to_string).collect(),
        }
    }

    fn today_tools(ledger: &AdoptionLedger) -> &BTreeMap<String, AdoptionCounter> {
        day_tools(ledger, "mcp")
    }

    fn day_tools<'a>(
        ledger: &'a AdoptionLedger,
        channel: &str,
    ) -> &'a BTreeMap<String, AdoptionCounter> {
        ledger
            .days
            .get(&date_key_from_epoch_day(0))
            .expect("event day")
            .get("codex")
            .expect("client")
            .get(channel)
            .expect("channel")
    }

    fn unique_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "lattice-adoption-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).expect("create existing telemetry workspace");
        root
    }

    fn database_events(store: &AdoptionMetricsStore) -> Vec<AdoptionEvent> {
        let _guard = store.lock_state().expect("store lock");
        let mut connection = store.open_connection().expect("open database");
        let transaction = begin_write(&mut connection).expect("read transaction");
        let events = read_events(&transaction).expect("read events");
        transaction.commit().expect("commit read transaction");
        events
    }
}
