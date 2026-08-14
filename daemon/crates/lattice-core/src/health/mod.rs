//! Deterministic health engine: per-file facts and the versioned configuration
//! they are measured against.
//!
//! See `docs/plans/2026-08-13-health-engine.md` § "Phase H2 — Per-file health
//! facts at index/refresh time". Fact producers are pure functions over data the
//! index already has; persistence follows the generational store pattern.

pub mod complexity_facts;
pub mod config;
