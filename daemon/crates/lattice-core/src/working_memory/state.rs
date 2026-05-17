//! Working memory state model.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 5. Working Memory` says working memory contains:
//!
//! - task statement
//! - interpreted intent
//! - active files and symbols
//! - active hypotheses
//! - active failures
//! - current plan
//! - selected memories
//! - excluded memories and reasons
//! - budget decisions
//! - unresolved questions
//! - verification status

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::LatticeError;
use crate::identity::{FileId, Identity, SymbolId};
use crate::memory::MemoryVerificationStatus;
use crate::retrieval_v1::{classify_intent, schema::BundleResult, IntentClassification};

pub const WORKING_MEMORY_STATE_VERSION: i64 = 1;
const DEFAULT_SESSION_ID: &str = "unspecified-session";

pub type CheckpointId = i64;
pub type FileIdentity = FileId;
pub type SymbolIdentity = SymbolId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkingMemoryState {
    pub task_statement: String,
    pub interpreted_intent: IntentClassification,
    pub active_files: BTreeSet<FileIdentity>,
    pub active_symbols: BTreeSet<SymbolIdentity>,
    pub active_hypotheses: Vec<Hypothesis>,
    pub active_failures: Vec<FailureRecord>,
    pub current_plan: Option<PlanRef>,
    pub selected_memories: Vec<BundleResult>,
    pub excluded_memories: Vec<ExcludedMemory>,
    pub budget_decisions: BudgetDecisions,
    pub unresolved_questions: Vec<String>,
    pub verification_status: WorkingMemoryVerification,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hypothesis {
    pub text: String,
    pub evidence_refs: Vec<String>,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRecord {
    pub kind: String,
    pub message: String,
    pub observed_at: u64,
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRef {
    pub plan_id: String,
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExcludedMemory {
    pub result: BundleResult,
    pub exclusion_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetDecisions {
    pub token_cap: usize,
    pub dropped_count: usize,
    pub truncated_results: usize,
    pub pinned_identities: BTreeSet<Identity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingMemoryVerification {
    pub last_verified_at: Option<u64>,
    pub status: MemoryVerificationStatus,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointScope {
    pub workspace_id: String,
    pub session_id: String,
    pub task_id: String,
}

impl WorkingMemoryState {
    pub fn new(task_statement: impl Into<String>) -> Self {
        let task_statement = task_statement.into();
        let interpreted_intent = classify_intent(&task_statement);
        Self {
            task_statement,
            interpreted_intent,
            active_files: BTreeSet::new(),
            active_symbols: BTreeSet::new(),
            active_hypotheses: Vec::new(),
            active_failures: Vec::new(),
            current_plan: None,
            selected_memories: Vec::new(),
            excluded_memories: Vec::new(),
            budget_decisions: BudgetDecisions::default(),
            unresolved_questions: Vec::new(),
            verification_status: WorkingMemoryVerification::default(),
        }
    }
}

impl Default for WorkingMemoryState {
    fn default() -> Self {
        Self::new("")
    }
}

impl Default for BudgetDecisions {
    fn default() -> Self {
        Self {
            token_cap: 0,
            dropped_count: 0,
            truncated_results: 0,
            pinned_identities: BTreeSet::new(),
        }
    }
}

impl Default for WorkingMemoryVerification {
    fn default() -> Self {
        Self {
            last_verified_at: None,
            status: MemoryVerificationStatus::Unverified,
            notes: Vec::new(),
        }
    }
}

impl CheckpointScope {
    pub fn new(
        workspace_id: impl Into<String>,
        session_id: impl Into<String>,
        task_id: impl Into<String>,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            session_id: session_id.into(),
            task_id: task_id.into(),
        }
    }

    pub fn from_state(state: &WorkingMemoryState) -> Self {
        Self {
            workspace_id: infer_workspace_id(state),
            session_id: DEFAULT_SESSION_ID.to_string(),
            task_id: task_hash(&state.task_statement),
        }
    }
}

pub fn initialize_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(include_str!("schema.sql"))
}

pub fn save_checkpoint(
    state: &WorkingMemoryState,
    name: &str,
    conn: &Connection,
) -> Result<CheckpointId, LatticeError> {
    let scope = CheckpointScope::from_state(state);
    save_checkpoint_for_scope(state, name, &scope, conn)
}

pub fn save_checkpoint_for_scope(
    state: &WorkingMemoryState,
    name: &str,
    scope: &CheckpointScope,
    conn: &Connection,
) -> Result<CheckpointId, LatticeError> {
    initialize_schema(conn).map_err(schema_error)?;
    let state_json = canonical_state_json(state)?;
    let state_hash = hash_text(&state_json);
    conn.execute(
        "INSERT INTO working_memory_checkpoints
            (workspace_id, session_id, task_id, checkpoint_name, created_at,
             state_version, state_json, state_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            scope.workspace_id,
            scope.session_id,
            scope.task_id,
            name,
            now_epoch_secs()?,
            WORKING_MEMORY_STATE_VERSION,
            state_json,
            state_hash,
        ],
    )
    .map_err(|e| {
        LatticeError::Storage(format!("Failed to save working memory checkpoint: {}", e))
    })?;
    Ok(conn.last_insert_rowid())
}

pub fn load_checkpoint(
    id: CheckpointId,
    conn: &Connection,
) -> Result<WorkingMemoryState, LatticeError> {
    initialize_schema(conn).map_err(schema_error)?;
    let row = conn
        .query_row(
            "SELECT state_version, state_json, state_hash
             FROM working_memory_checkpoints
             WHERE checkpoint_id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|e| {
            LatticeError::Storage(format!("Failed to load working memory checkpoint: {}", e))
        })?;
    let (state_version, state_json, state_hash) = row.ok_or_else(|| {
        LatticeError::Storage(format!("Working memory checkpoint {} was not found", id))
    })?;

    reject_unknown_state_version(state_version)?;
    verify_state_hash(&state_json, &state_hash)?;
    serde_json::from_str(&state_json).map_err(|e| {
        LatticeError::Storage(format!(
            "Failed to deserialize working memory checkpoint {}: {}",
            id, e
        ))
    })
}

pub fn load_latest_checkpoint_for_scope(
    scope: &CheckpointScope,
    conn: &Connection,
) -> Result<Option<(CheckpointId, WorkingMemoryState)>, LatticeError> {
    initialize_schema(conn).map_err(schema_error)?;
    let checkpoint_id = conn
        .query_row(
            "SELECT checkpoint_id
             FROM working_memory_checkpoints
             WHERE workspace_id = ?1 AND session_id = ?2 AND task_id = ?3
             ORDER BY created_at DESC, checkpoint_id DESC
             LIMIT 1",
            params![scope.workspace_id, scope.session_id, scope.task_id],
            |row| row.get::<_, CheckpointId>(0),
        )
        .optional()
        .map_err(|e| {
            LatticeError::Storage(format!(
                "Failed to load latest working memory checkpoint for task {}: {}",
                scope.task_id, e
            ))
        })?;
    checkpoint_id
        .map(|id| load_checkpoint(id, conn).map(|state| (id, state)))
        .transpose()
}

pub fn canonical_state_json(state: &WorkingMemoryState) -> Result<String, LatticeError> {
    serde_json::to_string(state).map_err(|e| {
        LatticeError::Storage(format!("Failed to serialize working memory state: {}", e))
    })
}

pub fn state_hash(state: &WorkingMemoryState) -> Result<String, LatticeError> {
    canonical_state_json(state).map(|json| hash_text(&json))
}

fn reject_unknown_state_version(state_version: i64) -> Result<(), LatticeError> {
    if state_version == WORKING_MEMORY_STATE_VERSION {
        return Ok(());
    }
    Err(LatticeError::Storage(format!(
        "Unknown working memory state_version {}; supported version is {}",
        state_version, WORKING_MEMORY_STATE_VERSION
    )))
}

fn verify_state_hash(state_json: &str, expected_hash: &str) -> Result<(), LatticeError> {
    let actual_hash = hash_text(state_json);
    if actual_hash == expected_hash {
        return Ok(());
    }
    Err(LatticeError::Storage(format!(
        "Working memory checkpoint hash mismatch: expected {}, got {}",
        expected_hash, actual_hash
    )))
}

fn infer_workspace_id(state: &WorkingMemoryState) -> String {
    if let Some(file) = state.active_files.iter().next() {
        return file.workspace_id.clone();
    }
    if let Some(symbol) = state.active_symbols.iter().next() {
        return symbol.file.workspace_id.clone();
    }
    "unspecified-workspace".to_string()
}

fn task_hash(task_statement: &str) -> String {
    format!("task-{}", hash_text(task_statement))
}

fn hash_text(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{:02x}", byte)).collect()
}

fn now_epoch_secs() -> Result<u64, LatticeError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|e| LatticeError::Storage(format!("System clock is before UNIX_EPOCH: {}", e)))
}

fn schema_error(error: rusqlite::Error) -> LatticeError {
    LatticeError::Storage(format!(
        "Failed to initialize working memory checkpoint schema: {}",
        error
    ))
}
