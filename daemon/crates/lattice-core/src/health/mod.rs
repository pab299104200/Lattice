//! Deterministic health-engine fact producers (Phase H2 of
//! `docs/plans/2026-08-13-health-engine.md`).
//!
//! Every submodule here is a pure function over data an already-published
//! index or intelligence snapshot has: no process, filesystem, database, or
//! clock dependency of its own. Facts persist per generation; scores (H3) are
//! computed at read time from facts plus a versioned weight table and are
//! never persisted themselves. See the plan's design decisions 2-4 for the
//! facts-first, evidence-carrying, "unknown is never zero" rules every
//! submodule here must follow.
//!
//! This module is intentionally additive and minimal: sibling H2.x fact
//! producers (graph facts, complexity facts, dead-symbol facts, test-linkage
//! facts) land as their own submodules here without touching this file's
//! existing declarations.

pub mod churn_facts;
pub mod complexity_facts;
pub mod config;
pub mod graph_facts;
pub mod test_proximity_facts;
