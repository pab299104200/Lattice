//! Unified identity model for cognitive workspace substrates.
//!
//! This module implements the stable ids required by
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 1: Unified Identity Model`. It also preserves the
//! stable follow-up handle contract described in
//! `docs/architecture/2026-04-11-stable-follow-up-handles.md`
//! `## Contract` by providing a migration path from the legacy graph
//! `SymbolId`.

pub mod ambiguity;
pub mod encoding;
pub mod kinds;
pub mod resolver;
pub mod serialization;

#[cfg(test)]
mod budget_tests;
#[cfg(test)]
mod resolver_tests;
#[cfg(test)]
mod tests;

pub use ambiguity::{AmbiguityReport, ResolveOutcome};
pub use encoding::{decode_identity, encode_identity, IdentityDecodeError};
pub use kinds::{
    ContextHandleId, DocId, EventId, FileId, Identity, IdentityKind, MemoryId, OperatorId,
    SectionId, SymbolId,
};
pub use resolver::{IdentityResolver, ResolveError, WorkspaceId};
pub use serialization::{
    serialize_outcome, IdentityAmbiguityPayload, IdentityPayload, IdentityPayloadOrAmbiguity,
};
