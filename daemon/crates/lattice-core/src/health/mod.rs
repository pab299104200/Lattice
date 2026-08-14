//! Deterministic code-health facts.
//!
//! Health *facts* are pure, integer-arithmetic aggregates over data the index
//! already holds. They are persisted per generation; *scores* are derived from
//! facts at read time and are never stored. Every fact family reports its own
//! completeness so that a missing input is presented as unknown rather than as
//! a fabricated zero.
//!
//! See `docs/plans/2026-08-13-health-engine.md`, section "Phase H2 — Per-file
//! health facts at index/refresh time".

pub mod graph_facts;
