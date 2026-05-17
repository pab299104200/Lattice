//! Cross-layer contract tests for the cognitive workspace substrates.
//!
//! These tests live above any individual substrate so they can exercise the
//! end-to-end round-trip across the three first-class substrates the spec
//! names — workspace identity, event log, and memory graph — and prove they
//! compose. Each individual layer is covered by its own per-substrate review
//! gate (R10 for identity, R18 for events, R25 for memory). The R26 contract
//! gate uses the tests in this module to certify the cross-layer contracts
//! enumerated in `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Design Thesis` (three substrates sharing identity, storage, and
//! ranking primitives).

#[cfg(test)]
pub mod identity_event_memory;
#[cfg(test)]
mod identity_event_memory_support;
#[cfg(test)]
pub mod memory_verification_retrieval;
#[cfg(test)]
mod memory_verification_retrieval_support;
