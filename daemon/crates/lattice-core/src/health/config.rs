//! Versioned health configuration: the single source of truth for every
//! threshold, cutoff, and percentile the health engine uses.
//!
//! Thresholds are versioned rather than scattered as per-language constants so
//! that facts (H2), scoring (H3), and the backtest harness (H1) all read the
//! same table and every persisted or rendered payload can echo the
//! `config_version` it was produced under. Recalibration means publishing a new
//! version of this table, never editing a constant in a producer.
//!
//! Arithmetic discipline (spec "Design decisions" 3): integers only, per-mille
//! where a fraction is required, so output is byte-identical across platforms.

use crate::symbols::Language;

/// Version of the health configuration table. Bump whenever any value below
/// changes; consumers persist and echo it alongside the facts they produce.
pub const HEALTH_CONFIG_VERSION: u32 = 1;

/// Thresholds over a single function's complexity facts.
///
/// A function is "over threshold" when its value is *strictly greater* than the
/// threshold, so a threshold of 10 admits a complexity of exactly 10.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComplexityThresholds {
    /// Cyclomatic complexity above which a function is flagged.
    pub high_cyclomatic_complexity: u32,
    /// Function length in lines above which a function is flagged.
    pub high_function_length: u32,
    /// Structural nesting depth above which a function is flagged.
    pub high_nesting_depth: u32,
    /// Declared parameter count above which a function is flagged.
    pub high_param_count: u32,
}

/// The complete health configuration for one version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthConfig {
    /// Value of [`HEALTH_CONFIG_VERSION`] this table represents.
    pub version: u32,
    /// Complexity thresholds shared by every supported language (see
    /// [`HealthConfig::complexity_for`]).
    pub complexity: ComplexityThresholds,
    /// Percentile reported in per-file rollups, in per-mille (900 = p90).
    pub rollup_percentile_per_mille: u32,
}

impl HealthConfig {
    /// Complexity thresholds for a language.
    ///
    /// Version 1 applies one uniform table to every supported language: the
    /// backtest harness (H1) has not yet produced evidence that per-language
    /// cutoffs predict defects better than shared ones, and inventing different
    /// numbers per grammar without that evidence would be unexplainable. When
    /// H1 reports per-language calibration, this accessor is where it lands.
    pub fn complexity_for(&self, _language: Language) -> &ComplexityThresholds {
        &self.complexity
    }
}

/// Version 1 of the health configuration table.
///
/// Initial values are the widely used industry defaults (McCabe's original
/// complexity-10 guidance, a 60-line function budget, four levels of nesting,
/// five parameters). They are explicitly *uncalibrated*: the H1 backtest report
/// is the only authority permitted to change them, and until it exists no
/// consumer may describe a threshold breach in predictive language.
pub static HEALTH_CONFIG_V1: HealthConfig = HealthConfig {
    version: HEALTH_CONFIG_VERSION,
    complexity: ComplexityThresholds {
        high_cyclomatic_complexity: 10,
        high_function_length: 60,
        high_nesting_depth: 4,
        high_param_count: 5,
    },
    rollup_percentile_per_mille: 900,
};

/// The configuration table producers and scorers must use.
pub fn active_config() -> &'static HealthConfig {
    &HEALTH_CONFIG_V1
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
