//! Daemon-owned record of which Lattice workflow steps actually ran.
//!
//! The plan gate needs a fact no host supplies: that a `prepare_change` was
//! served for this checkout. Neither Claude Code nor Codex passes its session
//! identifier to an MCP server or to a shell command, so an MCP or CLI call
//! cannot be tied to one host session. The daemon therefore records the fact
//! itself, at the point where it serves the call, scoped to the checkout it
//! resolved for that connection. Hook sessions, which are authenticated by
//! their binding capability, then read it.
//!
//! Only a tool category, a checkout identity and a time are stored. No query,
//! argument, result or path ever reaches this database.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

use crate::hook_enforcement::FollowupGaps;

const SCHEMA_VERSION: i64 = 1;
const FACT_RETENTION_MS: i64 = 24 * 60 * 60 * 1_000;
const SESSION_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const MAX_FACTS_PER_CHECKOUT_TOOL: i64 = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkflowStep {
    PrepareChange,
    Remember,
    StaleDocs,
}

impl WorkflowStep {
    fn code(self) -> &'static str {
        match self {
            Self::PrepareChange => "prepare_change",
            Self::Remember => "remember",
            Self::StaleDocs => "stale_docs",
        }
    }

    /// Map a served public tool call to the workflow step it satisfies.
    /// `status_scope` is the `scope` argument of a `status` call.
    pub(crate) fn from_tool_call(tool: &str, status_scope: Option<&str>) -> Option<Self> {
        match (tool, status_scope) {
            ("prepare_change", _) => Some(Self::PrepareChange),
            ("remember", _) => Some(Self::Remember),
            ("status", Some("docs")) => Some(Self::StaleDocs),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SessionWorkflow {
    /// When this host session last began with a fresh model context.
    pub(crate) not_before_ms: i64,
    pub(crate) last_covered_edit_ms: Option<i64>,
    pub(crate) product_edits: u64,
    pub(crate) followup_reminded: bool,
}

pub(crate) struct HookWorkflowState {
    connection: Mutex<Connection>,
}

impl HookWorkflowState {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)
            .with_context(|| format!("open hook workflow state `{}`", path.display()))?;
        Self::initialize(connection)
    }

    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory()?)
    }

    fn initialize(connection: Connection) -> Result<Self> {
        connection.busy_timeout(std::time::Duration::from_millis(500))?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            anyhow::bail!(
                "hook workflow state schema {version} is newer than this daemon supports \
                 ({SCHEMA_VERSION}); upgrade the daemon"
            );
        }
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS workflow_facts (
                 checkout_id TEXT NOT NULL,
                 step TEXT NOT NULL,
                 recorded_at_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS workflow_facts_lookup
                 ON workflow_facts (checkout_id, step, recorded_at_ms);
             CREATE TABLE IF NOT EXISTS session_workflow (
                 session_id BLOB PRIMARY KEY,
                 not_before_ms INTEGER NOT NULL,
                 last_covered_edit_ms INTEGER,
                 product_edits INTEGER NOT NULL DEFAULT 0,
                 followup_reminded INTEGER NOT NULL DEFAULT 0,
                 updated_at_ms INTEGER NOT NULL
             );",
        )?;
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("hook workflow state lock is poisoned"))
    }

    pub(crate) fn record_step(
        &self,
        checkout_id: &str,
        step: WorkflowStep,
        now_ms: i64,
    ) -> Result<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO workflow_facts (checkout_id, step, recorded_at_ms) VALUES (?1, ?2, ?3)",
            params![checkout_id, step.code(), now_ms],
        )?;
        transaction.execute(
            "DELETE FROM workflow_facts WHERE recorded_at_ms < ?1",
            params![now_ms.saturating_sub(FACT_RETENTION_MS)],
        )?;
        // Only the newest fact is ever read; keep a short tail for diagnosis.
        transaction.execute(
            "DELETE FROM workflow_facts
             WHERE checkout_id = ?1 AND step = ?2 AND rowid NOT IN (
                 SELECT rowid FROM workflow_facts
                 WHERE checkout_id = ?1 AND step = ?2
                 ORDER BY recorded_at_ms DESC, rowid DESC LIMIT ?3)",
            params![checkout_id, step.code(), MAX_FACTS_PER_CHECKOUT_TOOL],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn latest_step(
        &self,
        checkout_id: &str,
        step: WorkflowStep,
        not_before_ms: i64,
    ) -> Result<Option<i64>> {
        Ok(self
            .lock()?
            .query_row(
                "SELECT MAX(recorded_at_ms) FROM workflow_facts
                 WHERE checkout_id = ?1 AND step = ?2 AND recorded_at_ms >= ?3",
                params![checkout_id, step.code(), not_before_ms],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Load a session, creating it on first sight. A session first seen now
    /// cannot rely on a plan made before it existed.
    pub(crate) fn session(&self, session_id: &[u8], now_ms: i64) -> Result<SessionWorkflow> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT OR IGNORE INTO session_workflow (session_id, not_before_ms, updated_at_ms)
             VALUES (?1, ?2, ?2)",
            params![session_id, now_ms],
        )?;
        connection.execute(
            "DELETE FROM session_workflow WHERE updated_at_ms < ?1",
            params![now_ms.saturating_sub(SESSION_RETENTION_MS)],
        )?;
        Ok(connection.query_row(
            "SELECT not_before_ms, last_covered_edit_ms, product_edits, followup_reminded
             FROM session_workflow WHERE session_id = ?1",
            params![session_id],
            |row| {
                Ok(SessionWorkflow {
                    not_before_ms: row.get(0)?,
                    last_covered_edit_ms: row.get(1)?,
                    product_edits: row.get::<_, i64>(2)?.max(0) as u64,
                    followup_reminded: row.get::<_, i64>(3)? != 0,
                })
            },
        )?)
    }

    /// The model's context was replaced (`startup`, `clear` or `compact`):
    /// earlier plans, and the earlier follow-up reminder, no longer apply.
    pub(crate) fn mark_context_reset(&self, session_id: &[u8], now_ms: i64) -> Result<()> {
        self.session(session_id, now_ms)?;
        self.lock()?.execute(
            "UPDATE session_workflow
             SET not_before_ms = ?2, last_covered_edit_ms = NULL, product_edits = 0,
                 followup_reminded = 0, updated_at_ms = ?2
             WHERE session_id = ?1",
            params![session_id, now_ms],
        )?;
        Ok(())
    }

    /// Count product edits. `covered` slides the plan's idle window and is
    /// true only when the edit was made under a current plan.
    pub(crate) fn record_product_edits(
        &self,
        session_id: &[u8],
        count: u64,
        covered: bool,
        now_ms: i64,
    ) -> Result<()> {
        self.session(session_id, now_ms)?;
        let count = i64::try_from(count).unwrap_or(i64::MAX);
        let connection = self.lock()?;
        if covered {
            connection.execute(
                "UPDATE session_workflow
                 SET product_edits = product_edits + ?2, last_covered_edit_ms = ?3,
                     updated_at_ms = ?3
                 WHERE session_id = ?1",
                params![session_id, count, now_ms],
            )?;
        } else {
            connection.execute(
                "UPDATE session_workflow
                 SET product_edits = product_edits + ?2, updated_at_ms = ?3
                 WHERE session_id = ?1",
                params![session_id, count, now_ms],
            )?;
        }
        Ok(())
    }

    /// Which end-of-work steps are missing for a session that edited product
    /// files. Returns no gaps when nothing was edited.
    pub(crate) fn followup_gaps(
        &self,
        checkout_id: &str,
        session: &SessionWorkflow,
    ) -> Result<FollowupGaps> {
        if session.product_edits == 0 {
            return Ok(FollowupGaps::default());
        }
        Ok(FollowupGaps {
            stale_docs: self
                .latest_step(checkout_id, WorkflowStep::StaleDocs, session.not_before_ms)?
                .is_none(),
            remember: self
                .latest_step(checkout_id, WorkflowStep::Remember, session.not_before_ms)?
                .is_none(),
        })
    }

    /// Atomically claim the single follow-up reminder for this session.
    pub(crate) fn claim_followup_reminder(&self, session_id: &[u8], now_ms: i64) -> Result<bool> {
        let changed = self.lock()?.execute(
            "UPDATE session_workflow SET followup_reminded = 1, updated_at_ms = ?2
             WHERE session_id = ?1 AND followup_reminded = 0",
            params![session_id, now_ms],
        )?;
        Ok(changed == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHECKOUT: &str = "/repository/checkout";
    const SESSION: &[u8] = b"internal-session-a";

    #[test]
    fn maps_only_the_three_workflow_steps() {
        assert_eq!(
            WorkflowStep::from_tool_call("prepare_change", None),
            Some(WorkflowStep::PrepareChange)
        );
        assert_eq!(
            WorkflowStep::from_tool_call("remember", None),
            Some(WorkflowStep::Remember)
        );
        assert_eq!(
            WorkflowStep::from_tool_call("status", Some("docs")),
            Some(WorkflowStep::StaleDocs)
        );
        for (tool, scope) in [
            ("status", None),
            ("status", Some("health")),
            ("context", None),
            ("impact", None),
            ("recall", None),
        ] {
            assert_eq!(WorkflowStep::from_tool_call(tool, scope), None);
        }
    }

    #[test]
    fn facts_are_scoped_to_checkout_step_and_time() {
        let state = HookWorkflowState::open_in_memory().unwrap();
        state
            .record_step(CHECKOUT, WorkflowStep::PrepareChange, 1_000)
            .unwrap();
        state
            .record_step(CHECKOUT, WorkflowStep::PrepareChange, 2_000)
            .unwrap();
        assert_eq!(
            state
                .latest_step(CHECKOUT, WorkflowStep::PrepareChange, 0)
                .unwrap(),
            Some(2_000)
        );
        assert_eq!(
            state
                .latest_step(CHECKOUT, WorkflowStep::PrepareChange, 2_001)
                .unwrap(),
            None
        );
        assert_eq!(
            state
                .latest_step("/other/checkout", WorkflowStep::PrepareChange, 0)
                .unwrap(),
            None
        );
        assert_eq!(
            state
                .latest_step(CHECKOUT, WorkflowStep::Remember, 0)
                .unwrap(),
            None
        );
    }

    #[test]
    fn fact_storage_is_bounded_by_age_and_count() {
        let state = HookWorkflowState::open_in_memory().unwrap();
        for index in 0..(MAX_FACTS_PER_CHECKOUT_TOOL + 40) {
            state
                .record_step(CHECKOUT, WorkflowStep::PrepareChange, 1_000 + index)
                .unwrap();
        }
        let count: i64 = state
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM workflow_facts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, MAX_FACTS_PER_CHECKOUT_TOOL);
        state
            .record_step(
                CHECKOUT,
                WorkflowStep::Remember,
                1_000 + FACT_RETENTION_MS + 10_000,
            )
            .unwrap();
        let count: i64 = state
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM workflow_facts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn session_first_sight_and_context_reset_move_the_plan_floor() {
        let state = HookWorkflowState::open_in_memory().unwrap();
        let first = state.session(SESSION, 10_000).unwrap();
        assert_eq!(first.not_before_ms, 10_000);
        assert_eq!(
            state.session(SESSION, 99_000).unwrap().not_before_ms,
            10_000
        );

        state
            .record_product_edits(SESSION, 2, true, 20_000)
            .unwrap();
        state
            .record_product_edits(SESSION, 3, false, 25_000)
            .unwrap();
        let edited = state.session(SESSION, 26_000).unwrap();
        assert_eq!(edited.product_edits, 5);
        assert_eq!(edited.last_covered_edit_ms, Some(20_000));

        assert!(state.claim_followup_reminder(SESSION, 27_000).unwrap());
        state.mark_context_reset(SESSION, 30_000).unwrap();
        let reset = state.session(SESSION, 31_000).unwrap();
        assert_eq!(
            reset,
            SessionWorkflow {
                not_before_ms: 30_000,
                last_covered_edit_ms: None,
                product_edits: 0,
                followup_reminded: false,
            }
        );
    }

    #[test]
    fn followup_gaps_need_an_edit_and_clear_per_step() {
        let state = HookWorkflowState::open_in_memory().unwrap();
        let untouched = state.session(SESSION, 1_000).unwrap();
        assert!(!state.followup_gaps(CHECKOUT, &untouched).unwrap().any());

        state.record_product_edits(SESSION, 1, true, 2_000).unwrap();
        let edited = state.session(SESSION, 2_000).unwrap();
        assert_eq!(
            state.followup_gaps(CHECKOUT, &edited).unwrap(),
            FollowupGaps {
                stale_docs: true,
                remember: true
            }
        );
        // A step recorded before the session's floor does not count.
        state
            .record_step(CHECKOUT, WorkflowStep::Remember, 500)
            .unwrap();
        assert!(state.followup_gaps(CHECKOUT, &edited).unwrap().remember);
        state
            .record_step(CHECKOUT, WorkflowStep::Remember, 3_000)
            .unwrap();
        assert_eq!(
            state.followup_gaps(CHECKOUT, &edited).unwrap(),
            FollowupGaps {
                stale_docs: true,
                remember: false
            }
        );
        state
            .record_step(CHECKOUT, WorkflowStep::StaleDocs, 3_500)
            .unwrap();
        assert!(!state.followup_gaps(CHECKOUT, &edited).unwrap().any());
    }

    #[test]
    fn followup_reminder_is_claimed_exactly_once() {
        let state = HookWorkflowState::open_in_memory().unwrap();
        state.session(SESSION, 1_000).unwrap();
        assert!(state.claim_followup_reminder(SESSION, 2_000).unwrap());
        assert!(!state.claim_followup_reminder(SESSION, 3_000).unwrap());
        assert!(!state.claim_followup_reminder(b"never-seen", 3_000).unwrap());
    }

    #[test]
    fn a_newer_schema_is_refused_without_mutation() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let error = HookWorkflowState::initialize(connection)
            .err()
            .expect("newer schema must be refused")
            .to_string();
        assert!(error.contains("upgrade the daemon"));
    }
}
