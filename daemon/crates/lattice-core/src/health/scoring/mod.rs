//! Scores as explainable fact bundles (Phase H3 of
//! `docs/plans/2026-08-13-health-engine.md`).
//!
//! Two axes are scored, `defect_risk` and `maintainability`, from the facts the
//! H2 producers publish. Nothing here is persisted: a score is derived at read
//! time from `(facts generation, weights version)` and both versions ride in
//! every bundle, so any response can be reproduced exactly and a recalibration
//! changes weights rather than stored data (spec design decision 2).
//!
//! # The shape of an answer
//!
//! [`AxisScore`] is an ordered bundle, never a bare number (spec design
//! decision 3): a per-mille score, a calibrated [`Band`], the contributing
//! facts with their raw values, ranks, weights and source ranges, the inputs
//! that were missing, and the versions it was computed under. A consumer can
//! render "high defect risk: fan-out 12, 8 bug-fix commits in window, cycle
//! member, no edge-linked tests" from the bundle alone.
//!
//! # What the evidence supports, and what it does not
//!
//! The `defect_risk` weights are the measured discrimination published in
//! `docs/reports/health-backtest/2026-08-14.md`, and the band cutoffs come from
//! that report's calibration-by-decile table. Three limits from its § "What H3
//! may and may not conclude" bind everything in this module:
//!
//! * The ground truth is a subject-line heuristic, corroborated by an
//!   independent signal but leaving a measured **15.9% recall gap**. Every
//!   figure inherits that classifier's error.
//! * 30 cut points across 5 repositories is a **small sample**; differences
//!   smaller than the spread across cut points are not evidence.
//! * The measurement is **correlational**. A score ranks files by evidence
//!   associated with later fix-shaped commits. It is not a prediction that any
//!   particular file will fail, and no text rendered from these bundles may say
//!   or imply that it is.
//!
//! Two of the facts here — `untested_change` from H2.4 and
//! `dead_exported_symbols` from H2.3 — are **evidence-linked but not
//! backtested**: the harness never scored them, because they are fact families
//! it did not have. They carry a deliberately small provisional weight
//! ([`weights::PROVISIONAL_WEIGHT_PER_MILLE`]) and are marked
//! `backtested: false` in every bundle, so a consumer can tell measured
//! evidence from provisional evidence.
//!
//! The `maintainability` axis is **not backtested at all**. The report measured
//! whether facts rank files that later receive a fix-shaped commit, which is
//! the `defect_risk` question; it says nothing about how costly a file is to
//! change. Its weights are a documented editorial judgment recorded at
//! [`weights::MAINTAINABILITY_WEIGHTS`], and no consumer may cite the backtest
//! as their justification.
//!
//! # Layout
//!
//! * [`facts`] — the fact vocabulary and the per-file bundle a score reads.
//! * [`weights`] — the versioned weight table and where every number came from.
//! * [`bands`] — band cutoffs and the calibration evidence that placed them.
//! * [`engine`] — the pure `(facts, weights) -> AxisScore` function.
//! * [`index`] — joining published snapshots into ranked, scoreable facts.

pub mod bands;
pub mod engine;
pub mod facts;
pub mod index;
pub mod weights;

pub use bands::{Band, BandRange, ALL_BANDS};
pub use engine::{score_axis, AxisScore, FactContribution};
pub use facts::{
    FactAvailability, FactFamily, FactKind, FactSourceRange, FactValue, FactWindow, FileFacts,
    RiskDirection, ALL_FACT_KINDS,
};
pub use index::{FactPopulations, HealthFactIndex, HealthFactIndexBuilder};
pub use weights::{
    active_weights, Axis, WeightTable, ALL_AXES, HEALTH_WEIGHTS_VERSION,
    PROVISIONAL_WEIGHT_PER_MILLE,
};

#[cfg(test)]
#[path = "golden_tests.rs"]
mod golden_tests;

#[cfg(test)]
#[path = "property_tests.rs"]
mod property_tests;

#[cfg(test)]
#[path = "availability_tests.rs"]
mod availability_tests;

#[cfg(test)]
pub(crate) mod test_support;
