//! Generational persistence for complexity health facts.
//!
//! Follows the generational fact-store pattern required by
//! `docs/plans/2026-08-13-health-engine.md` § "Phase H2 — Per-file health facts
//! at index/refresh time": facts are written into a numbered generation, keyed
//! by a stable `(generation, file[, byte_offset])`, and become visible to
//! readers only when the generation is published by an atomic pointer swap.
//! Readers therefore never observe a half-written generation, and a failed
//! publish leaves the previous generation intact.
//!
//! Completeness is explicit: every generation records whether it was published
//! and how many files were available, degraded, or unavailable, so a consumer
//! can tell "no facts" from "facts say zero" (spec design decision 4).
//!
//! Paths are validated as canonical workspace-relative paths on write, so a
//! generation cannot mix `src/a.rs` with `./src/a.rs` or an absolute path and
//! silently double-count a file.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::error::LatticeError;
use crate::health::complexity_facts::{
    ComplexityUnavailableReason, FactAvailability, FileComplexityFacts, FileComplexityRollup,
    SymbolComplexityFacts,
};
use crate::symbols::Language;

/// Schema for the complexity fact tables.
const CREATE_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS health_complexity_generations (
    generation INTEGER PRIMARY KEY,
    created_at_ms INTEGER NOT NULL,
    config_version INTEGER NOT NULL,
    published INTEGER NOT NULL DEFAULT 0,
    files_total INTEGER NOT NULL DEFAULT 0,
    files_available INTEGER NOT NULL DEFAULT 0,
    files_degraded INTEGER NOT NULL DEFAULT 0,
    files_unavailable INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS health_complexity_file_facts (
    generation INTEGER NOT NULL,
    file TEXT NOT NULL,
    language TEXT NOT NULL,
    availability TEXT NOT NULL,
    unavailable_reason TEXT,
    exemption_reason TEXT,
    config_version INTEGER NOT NULL,
    function_count INTEGER,
    functions_with_unknown_param_count INTEGER,
    max_cyclomatic_complexity INTEGER,
    p90_cyclomatic_complexity INTEGER,
    max_function_length INTEGER,
    p90_function_length INTEGER,
    max_nesting_depth INTEGER,
    functions_over_complexity_threshold INTEGER,
    functions_over_length_threshold INTEGER,
    functions_over_nesting_threshold INTEGER,
    functions_over_param_threshold INTEGER,
    over_threshold_share_per_mille INTEGER,
    mean_cyclomatic_complexity_per_mille INTEGER,
    percentile_per_mille INTEGER,
    PRIMARY KEY (generation, file)
);

CREATE TABLE IF NOT EXISTS health_complexity_symbol_facts (
    generation INTEGER NOT NULL,
    file TEXT NOT NULL,
    byte_offset INTEGER NOT NULL,
    symbol TEXT NOT NULL,
    line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    function_length INTEGER NOT NULL,
    branch_count INTEGER NOT NULL,
    cyclomatic_complexity INTEGER NOT NULL,
    max_nesting_depth INTEGER NOT NULL,
    param_count INTEGER,
    availability TEXT NOT NULL,
    PRIMARY KEY (generation, file, byte_offset)
);

CREATE TABLE IF NOT EXISTS health_complexity_active_generation (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    generation INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_health_complexity_file_facts_generation
    ON health_complexity_file_facts(generation);
CREATE INDEX IF NOT EXISTS idx_health_complexity_symbol_facts_file
    ON health_complexity_symbol_facts(generation, file);
"#;

/// Completeness metadata for one fact generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationStatus {
    pub generation: u64,
    /// Wall-clock creation time in epoch milliseconds. Metadata only: it is not
    /// a fact and never feeds a score.
    pub created_at_ms: i64,
    /// Health config version the facts were produced under.
    pub config_version: u32,
    /// Whether the generation has been published (and is therefore readable).
    pub published: bool,
    pub files_total: u32,
    pub files_available: u32,
    pub files_degraded: u32,
    pub files_unavailable: u32,
}

impl GenerationStatus {
    /// Whether every file in the generation produced usable facts.
    pub fn is_complete(&self) -> bool {
        self.published && self.files_unavailable == 0 && self.files_degraded == 0
    }
}

/// Generational store for per-file and per-symbol complexity facts.
pub struct HealthComplexityFactsStore {
    conn: Connection,
    path: Option<PathBuf>,
}

impl HealthComplexityFactsStore {
    /// Open a file-backed store with WAL enabled.
    pub fn open(path: &Path) -> Result<Self, LatticeError> {
        let conn = Connection::open(path).map_err(|e| {
            LatticeError::Storage(format!("Failed to open health fact store: {}", e))
        })?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| LatticeError::Storage(format!("Failed to set WAL mode: {}", e)))?;
        let store = Self {
            conn,
            path: Some(path.to_path_buf()),
        };
        store.initialize()?;
        Ok(store)
    }

    /// Open an in-memory store (for tests).
    pub fn open_in_memory() -> Result<Self, LatticeError> {
        let conn = Connection::open_in_memory().map_err(|e| {
            LatticeError::Storage(format!("Failed to open in-memory health fact store: {}", e))
        })?;
        let store = Self { conn, path: None };
        store.initialize()?;
        Ok(store)
    }

    /// Path of the backing database, when file-backed.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn initialize(&self) -> Result<(), LatticeError> {
        self.conn.execute_batch(CREATE_TABLES).map_err(|e| {
            LatticeError::Storage(format!("Failed to initialize health fact schema: {}", e))
        })
    }

    /// Open a new, unpublished generation and return its number.
    ///
    /// Generations are monotonic: the new number is one past the highest ever
    /// allocated, so a pruned generation number is never reused.
    pub fn begin_generation(&self, config_version: u32) -> Result<u64, LatticeError> {
        let highest: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(generation) FROM health_complexity_generations",
                [],
                |row| row.get(0),
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to read generations: {}", e)))?;

        let generation = highest.unwrap_or(0) + 1;
        self.conn
            .execute(
                "INSERT INTO health_complexity_generations (generation, created_at_ms, config_version, published) \
                 VALUES (?1, ?2, ?3, 0)",
                params![generation, now_ms(), config_version as i64],
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to open generation: {}", e)))?;
        Ok(generation as u64)
    }

    /// Write one file's facts into a generation, replacing anything previously
    /// written for that file in that generation.
    ///
    /// Writing into the published generation is how an incremental re-index
    /// refreshes a single file: only that file's rows change.
    pub fn write_file_facts(
        &self,
        generation: u64,
        facts: &FileComplexityFacts,
    ) -> Result<(), LatticeError> {
        validate_canonical_path(&facts.file)?;
        if self.generation_status(generation)?.is_none() {
            return Err(LatticeError::Storage(format!(
                "Generation {} does not exist",
                generation
            )));
        }

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| LatticeError::Storage(format!("Failed to begin transaction: {}", e)))?;

        tx.execute(
            "DELETE FROM health_complexity_symbol_facts WHERE generation = ?1 AND file = ?2",
            params![generation as i64, &facts.file],
        )
        .map_err(|e| LatticeError::Storage(format!("Failed to clear symbol facts: {}", e)))?;

        let rollup = facts.rollup.as_ref();
        tx.execute(
            "INSERT OR REPLACE INTO health_complexity_file_facts ( \
                generation, file, language, availability, unavailable_reason, exemption_reason, \
                config_version, function_count, functions_with_unknown_param_count, \
                max_cyclomatic_complexity, p90_cyclomatic_complexity, max_function_length, \
                p90_function_length, max_nesting_depth, functions_over_complexity_threshold, \
                functions_over_length_threshold, functions_over_nesting_threshold, \
                functions_over_param_threshold, over_threshold_share_per_mille, \
                mean_cyclomatic_complexity_per_mille, percentile_per_mille) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
            params![
                generation as i64,
                &facts.file,
                format!("{:?}", facts.language),
                facts.availability.as_str(),
                facts.unavailable_reason.map(|reason| reason.as_str()),
                facts.exemption_reason.as_deref(),
                facts.config_version as i64,
                rollup.map(|r| r.function_count as i64),
                rollup.map(|r| r.functions_with_unknown_param_count as i64),
                rollup.and_then(|r| r.max_cyclomatic_complexity.map(i64::from)),
                rollup.and_then(|r| r.p90_cyclomatic_complexity.map(i64::from)),
                rollup.and_then(|r| r.max_function_length.map(i64::from)),
                rollup.and_then(|r| r.p90_function_length.map(i64::from)),
                rollup.and_then(|r| r.max_nesting_depth.map(i64::from)),
                rollup.map(|r| r.functions_over_complexity_threshold as i64),
                rollup.map(|r| r.functions_over_length_threshold as i64),
                rollup.map(|r| r.functions_over_nesting_threshold as i64),
                rollup.map(|r| r.functions_over_param_threshold as i64),
                rollup.map(|r| r.over_threshold_share_per_mille as i64),
                rollup.map(|r| r.mean_cyclomatic_complexity_per_mille as i64),
                rollup.map(|r| r.percentile_per_mille as i64),
            ],
        )
        .map_err(|e| LatticeError::Storage(format!("Failed to write file facts: {}", e)))?;

        {
            let mut insert = tx
                .prepare(
                    "INSERT OR REPLACE INTO health_complexity_symbol_facts ( \
                        generation, file, byte_offset, symbol, line, end_line, function_length, \
                        branch_count, cyclomatic_complexity, max_nesting_depth, param_count, availability) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                )
                .map_err(|e| {
                    LatticeError::Storage(format!("Failed to prepare symbol insert: {}", e))
                })?;

            for symbol in &facts.symbols {
                insert
                    .execute(params![
                        generation as i64,
                        &symbol.file,
                        symbol.byte_offset as i64,
                        &symbol.symbol,
                        symbol.line as i64,
                        symbol.end_line as i64,
                        symbol.function_length as i64,
                        symbol.branch_count as i64,
                        symbol.cyclomatic_complexity as i64,
                        symbol.max_nesting_depth as i64,
                        symbol.param_count.map(i64::from),
                        symbol.availability.as_str(),
                    ])
                    .map_err(|e| {
                        LatticeError::Storage(format!("Failed to write symbol facts: {}", e))
                    })?;
            }
        }

        tx.commit()
            .map_err(|e| LatticeError::Storage(format!("Failed to commit file facts: {}", e)))
    }

    /// Publish a generation: recount completeness and swap the active pointer,
    /// both inside one transaction so readers move between generations atomically.
    pub fn publish_generation(&self, generation: u64) -> Result<GenerationStatus, LatticeError> {
        if self.generation_status(generation)?.is_none() {
            return Err(LatticeError::Storage(format!(
                "Generation {} does not exist",
                generation
            )));
        }

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| LatticeError::Storage(format!("Failed to begin transaction: {}", e)))?;

        let count_by = |availability: &str| -> Result<i64, LatticeError> {
            tx.query_row(
                "SELECT COUNT(*) FROM health_complexity_file_facts WHERE generation = ?1 AND availability = ?2",
                params![generation as i64, availability],
                |row| row.get(0),
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to count facts: {}", e)))
        };

        let available = count_by(FactAvailability::Available.as_str())?;
        let degraded = count_by(FactAvailability::Degraded.as_str())?;
        let unavailable = count_by(FactAvailability::Unavailable.as_str())?;
        let total = available + degraded + unavailable;

        tx.execute(
            "UPDATE health_complexity_generations \
             SET published = 1, files_total = ?2, files_available = ?3, files_degraded = ?4, files_unavailable = ?5 \
             WHERE generation = ?1",
            params![generation as i64, total, available, degraded, unavailable],
        )
        .map_err(|e| LatticeError::Storage(format!("Failed to mark generation published: {}", e)))?;

        tx.execute(
            "INSERT INTO health_complexity_active_generation (id, generation) VALUES (1, ?1) \
             ON CONFLICT(id) DO UPDATE SET generation = excluded.generation",
            params![generation as i64],
        )
        .map_err(|e| LatticeError::Storage(format!("Failed to swap active generation: {}", e)))?;

        tx.commit()
            .map_err(|e| LatticeError::Storage(format!("Failed to commit publish: {}", e)))?;

        self.generation_status(generation)?.ok_or_else(|| {
            LatticeError::Storage(format!("Generation {} vanished during publish", generation))
        })
    }

    /// The published generation readers see, if any generation was ever published.
    pub fn active_generation(&self) -> Result<Option<u64>, LatticeError> {
        self.conn
            .query_row(
                "SELECT generation FROM health_complexity_active_generation WHERE id = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map(|value| value.map(|generation| generation as u64))
            .map_err(|e| LatticeError::Storage(format!("Failed to read active generation: {}", e)))
    }

    /// Completeness metadata for a generation.
    pub fn generation_status(
        &self,
        generation: u64,
    ) -> Result<Option<GenerationStatus>, LatticeError> {
        self.conn
            .query_row(
                "SELECT generation, created_at_ms, config_version, published, files_total, \
                        files_available, files_degraded, files_unavailable \
                 FROM health_complexity_generations WHERE generation = ?1",
                params![generation as i64],
                |row| {
                    Ok(GenerationStatus {
                        generation: row.get::<_, i64>(0)? as u64,
                        created_at_ms: row.get(1)?,
                        config_version: row.get::<_, i64>(2)? as u32,
                        published: row.get::<_, i64>(3)? != 0,
                        files_total: row.get::<_, i64>(4)? as u32,
                        files_available: row.get::<_, i64>(5)? as u32,
                        files_degraded: row.get::<_, i64>(6)? as u32,
                        files_unavailable: row.get::<_, i64>(7)? as u32,
                    })
                },
            )
            .optional()
            .map_err(|e| LatticeError::Storage(format!("Failed to read generation status: {}", e)))
    }

    /// Status of the active generation, if one is published.
    pub fn active_generation_status(&self) -> Result<Option<GenerationStatus>, LatticeError> {
        match self.active_generation()? {
            Some(generation) => self.generation_status(generation),
            None => Ok(None),
        }
    }

    /// Facts for one file from the active generation.
    ///
    /// `Ok(None)` means "no facts recorded for this file", which a consumer must
    /// present as unknown — never as zero complexity.
    pub fn file_facts(&self, file: &str) -> Result<Option<FileComplexityFacts>, LatticeError> {
        match self.active_generation()? {
            Some(generation) => self.file_facts_in(generation, file),
            None => Ok(None),
        }
    }

    /// Facts for one file from an explicit generation.
    pub fn file_facts_in(
        &self,
        generation: u64,
        file: &str,
    ) -> Result<Option<FileComplexityFacts>, LatticeError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT file, language, availability, unavailable_reason, exemption_reason, \
                        config_version, function_count, functions_with_unknown_param_count, \
                        max_cyclomatic_complexity, p90_cyclomatic_complexity, max_function_length, \
                        p90_function_length, max_nesting_depth, functions_over_complexity_threshold, \
                        functions_over_length_threshold, functions_over_nesting_threshold, \
                        functions_over_param_threshold, over_threshold_share_per_mille, \
                        mean_cyclomatic_complexity_per_mille, percentile_per_mille \
                 FROM health_complexity_file_facts WHERE generation = ?1 AND file = ?2",
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to prepare file read: {}", e)))?;

        let facts = statement
            .query_row(params![generation as i64, file], read_file_facts)
            .optional()
            .map_err(|e| LatticeError::Storage(format!("Failed to read file facts: {}", e)))?;

        let Some(mut facts) = facts else {
            return Ok(None);
        };
        facts.symbols = self.symbol_facts_in(generation, file)?;
        Ok(Some(facts))
    }

    /// Per-symbol facts for one file in a generation, ordered by byte offset.
    pub fn symbol_facts_in(
        &self,
        generation: u64,
        file: &str,
    ) -> Result<Vec<SymbolComplexityFacts>, LatticeError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT file, symbol, byte_offset, line, end_line, function_length, branch_count, \
                        cyclomatic_complexity, max_nesting_depth, param_count, availability \
                 FROM health_complexity_symbol_facts WHERE generation = ?1 AND file = ?2 \
                 ORDER BY byte_offset",
            )
            .map_err(|e| LatticeError::Storage(format!("Failed to prepare symbol read: {}", e)))?;

        let rows = statement
            .query_map(params![generation as i64, file], read_symbol_facts)
            .map_err(|e| LatticeError::Storage(format!("Failed to read symbol facts: {}", e)))?;

        let mut symbols = Vec::new();
        for row in rows {
            symbols.push(row.map_err(|e| {
                LatticeError::Storage(format!("Failed to decode symbol facts: {}", e))
            })?);
        }
        Ok(symbols)
    }

    /// Every file's facts in the active generation, ordered by path.
    pub fn active_file_facts(&self) -> Result<Vec<FileComplexityFacts>, LatticeError> {
        let Some(generation) = self.active_generation()? else {
            return Ok(Vec::new());
        };

        let files = {
            let mut statement = self
                .conn
                .prepare(
                    "SELECT file FROM health_complexity_file_facts WHERE generation = ?1 ORDER BY file",
                )
                .map_err(|e| LatticeError::Storage(format!("Failed to prepare file list: {}", e)))?;
            let rows = statement
                .query_map(params![generation as i64], |row| row.get::<_, String>(0))
                .map_err(|e| LatticeError::Storage(format!("Failed to list files: {}", e)))?;
            let mut files = Vec::new();
            for row in rows {
                files.push(
                    row.map_err(|e| LatticeError::Storage(format!("Failed to read file: {}", e)))?,
                );
            }
            files
        };

        let mut all = Vec::with_capacity(files.len());
        for file in files {
            if let Some(facts) = self.file_facts_in(generation, &file)? {
                all.push(facts);
            }
        }
        Ok(all)
    }

    /// Drop every generation except the active one and the `keep` most recent,
    /// so history does not grow without bound. The active generation is never
    /// pruned, whatever `keep` says.
    pub fn prune_generations(&self, keep: usize) -> Result<usize, LatticeError> {
        let active = self.active_generation()?;

        let generations = {
            let mut statement = self
                .conn
                .prepare(
                    "SELECT generation FROM health_complexity_generations ORDER BY generation DESC",
                )
                .map_err(|e| {
                    LatticeError::Storage(format!("Failed to prepare generation list: {}", e))
                })?;
            let rows = statement
                .query_map([], |row| row.get::<_, i64>(0))
                .map_err(|e| LatticeError::Storage(format!("Failed to list generations: {}", e)))?;
            let mut generations = Vec::new();
            for row in rows {
                generations.push(row.map_err(|e| {
                    LatticeError::Storage(format!("Failed to read generation: {}", e))
                })?);
            }
            generations
        };

        let doomed: Vec<i64> = generations
            .into_iter()
            .skip(keep)
            .filter(|generation| active != Some(*generation as u64))
            .collect();

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| LatticeError::Storage(format!("Failed to begin transaction: {}", e)))?;
        for generation in &doomed {
            for table in [
                "health_complexity_symbol_facts",
                "health_complexity_file_facts",
                "health_complexity_generations",
            ] {
                tx.execute(
                    &format!("DELETE FROM {} WHERE generation = ?1", table),
                    params![generation],
                )
                .map_err(|e| LatticeError::Storage(format!("Failed to prune generation: {}", e)))?;
            }
        }
        tx.commit()
            .map_err(|e| LatticeError::Storage(format!("Failed to commit prune: {}", e)))?;

        Ok(doomed.len())
    }
}

/// Reject anything that is not a canonical workspace-relative path, so one file
/// cannot appear under two spellings in the same generation.
pub fn validate_canonical_path(file: &str) -> Result<(), LatticeError> {
    let invalid = |reason: &str| {
        Err(LatticeError::Storage(format!(
            "Non-canonical fact path {:?}: {}",
            file, reason
        )))
    };

    if file.is_empty() {
        return invalid("path is empty");
    }
    if file.starts_with('/') || file.contains(':') {
        return invalid("path must be workspace-relative");
    }
    if file.contains('\\') {
        return invalid("path must use forward slashes");
    }
    if file.starts_with("./") || file.contains("/./") {
        return invalid("path must not contain '.' segments");
    }
    if file == ".." || file.starts_with("../") || file.contains("/../") || file.ends_with("/..") {
        return invalid("path must not escape the workspace");
    }
    if file.contains("//") {
        return invalid("path must not contain empty segments");
    }
    if file.ends_with('/') {
        return invalid("path must not be a directory");
    }
    Ok(())
}

/// Decode a stored file-facts row; the rollup is absent exactly when the file
/// produced no facts.
fn read_file_facts(row: &Row) -> rusqlite::Result<FileComplexityFacts> {
    let availability: String = row.get(2)?;
    let unavailable_reason: Option<String> = row.get(3)?;
    let function_count: Option<i64> = row.get(6)?;

    let rollup = match function_count {
        Some(count) => Some(FileComplexityRollup {
            function_count: count as u32,
            functions_with_unknown_param_count: row.get::<_, Option<i64>>(7)?.unwrap_or(0) as u32,
            max_cyclomatic_complexity: row.get::<_, Option<i64>>(8)?.map(|v| v as u32),
            p90_cyclomatic_complexity: row.get::<_, Option<i64>>(9)?.map(|v| v as u32),
            max_function_length: row.get::<_, Option<i64>>(10)?.map(|v| v as u32),
            p90_function_length: row.get::<_, Option<i64>>(11)?.map(|v| v as u32),
            max_nesting_depth: row.get::<_, Option<i64>>(12)?.map(|v| v as u32),
            functions_over_complexity_threshold: row.get::<_, Option<i64>>(13)?.unwrap_or(0) as u32,
            functions_over_length_threshold: row.get::<_, Option<i64>>(14)?.unwrap_or(0) as u32,
            functions_over_nesting_threshold: row.get::<_, Option<i64>>(15)?.unwrap_or(0) as u32,
            functions_over_param_threshold: row.get::<_, Option<i64>>(16)?.unwrap_or(0) as u32,
            over_threshold_share_per_mille: row.get::<_, Option<i64>>(17)?.unwrap_or(0) as u32,
            mean_cyclomatic_complexity_per_mille: row.get::<_, Option<i64>>(18)?.unwrap_or(0)
                as u32,
            percentile_per_mille: row.get::<_, Option<i64>>(19)?.unwrap_or(0) as u32,
        }),
        None => None,
    };

    Ok(FileComplexityFacts {
        file: row.get(0)?,
        language: parse_language(&row.get::<_, String>(1)?),
        availability: FactAvailability::from_code(&availability)
            .unwrap_or(FactAvailability::Unavailable),
        unavailable_reason: unavailable_reason
            .as_deref()
            .and_then(ComplexityUnavailableReason::from_code),
        exemption_reason: row.get(4)?,
        config_version: row.get::<_, i64>(5)? as u32,
        symbols: Vec::new(),
        rollup,
    })
}

/// Decode a stored symbol-facts row.
fn read_symbol_facts(row: &Row) -> rusqlite::Result<SymbolComplexityFacts> {
    let availability: String = row.get(10)?;
    Ok(SymbolComplexityFacts {
        file: row.get(0)?,
        symbol: row.get(1)?,
        byte_offset: row.get::<_, i64>(2)? as usize,
        line: row.get::<_, i64>(3)? as usize,
        end_line: row.get::<_, i64>(4)? as usize,
        function_length: row.get::<_, i64>(5)? as u32,
        branch_count: row.get::<_, i64>(6)? as u32,
        cyclomatic_complexity: row.get::<_, i64>(7)? as u32,
        max_nesting_depth: row.get::<_, i64>(8)? as u32,
        param_count: row.get::<_, Option<i64>>(9)?.map(|v| v as u32),
        availability: FactAvailability::from_code(&availability)
            .unwrap_or(FactAvailability::Unavailable),
    })
}

/// Decode the stored language code, matching how `graph_store` writes languages.
fn parse_language(value: &str) -> Language {
    match value {
        "TypeScript" => Language::TypeScript,
        "JavaScript" => Language::JavaScript,
        "Python" => Language::Python,
        "Rust" => Language::Rust,
        "Go" => Language::Go,
        "Java" => Language::Java,
        "Markdown" => Language::Markdown,
        _ => Language::Unknown,
    }
}

/// Epoch milliseconds, clamped at zero before the epoch.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "health_complexity_facts_store_tests.rs"]
mod tests;
