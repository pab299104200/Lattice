//! Verification engine core for durable memory evidence.
//!
//! This module implements the verifier contract from
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 8. Verification Engine`,
//! `## Phase 7: Verification And Freshness`, and
//! `## Storage Design`.
//!
//! Verification checks:
//!
//! - linked files still exist
//! - linked symbols still exist
//! - cited docs still exist
//! - linked tests still exist
//! - evidence text still matches when exact spans were captured
//! - implementation still matches memory claim where deterministic checks are possible
//! - contradicted/superseded states remain coherent
//! - branch-scoped memory is not leaking into unrelated branches
//! - time-bound memory has expired
//!
//! Verification outputs:
//!
//! - `verified`
//! - `unverified`
//! - `in_review`
//! - `stale`
//! - `contradicted`
//! - `superseded`
//! - `expired`
//! - `invalidated`

use std::fmt;
use std::str::FromStr;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::error::LatticeError;

pub mod existence;
pub mod expiry;
pub mod incremental;
pub mod scope_enforcement;
pub mod spans;
pub mod surfacing;

#[cfg(test)]
mod existence_tests;
#[cfg(test)]
mod incremental_tests;
#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod scope_leak_tests;
#[cfg(test)]
mod spans_tests;
#[cfg(test)]
mod surfacing_tests;

pub use existence::{
    VerificationError, VerificationOutcome, VerifierBudget, VerifierCore, VerifierReport,
};
pub use expiry::{ExpiryError, ExpiryReport, ExpiryScanner};
pub use incremental::{
    IncrementalError, IncrementalReport, IncrementalVerifier, VerificationObserver,
};
pub use scope_enforcement::{
    allows, MemoryScopeFilteredEvent, OrganizationId, ScopeEnforcement, ScopeFilter,
    ScopeFilterError,
};
pub use spans::{
    SpanMismatch, SpanMismatchReason, SpanReader, SpanValidationError, SpanValidator,
    WorkspaceFileReader,
};
pub use surfacing::{
    classify_status, BundleProducer, BundleSection, LabeledMemory, SurfacedBundle, SurfacingError,
    SurfacingPipeline,
};

const VERIFICATION_SCHEMA_SQL: &str = include_str!("schema.sql");

pub fn initialize_schema(conn: &Connection) -> Result<(), LatticeError> {
    conn.execute_batch(VERIFICATION_SCHEMA_SQL)
        .map_err(|error| {
            LatticeError::Storage(format!("Failed to initialize verification schema: {error}"))
        })?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Verified,
    Unverified,
    InReview,
    Stale,
    Contradicted,
    Superseded,
    Expired,
    Invalidated,
}

impl VerificationStatus {
    pub const VALUES: &'static [&'static str] = &[
        "verified",
        "unverified",
        "in_review",
        "stale",
        "contradicted",
        "superseded",
        "expired",
        "invalidated",
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Unverified => "unverified",
            Self::InReview => "in_review",
            Self::Stale => "stale",
            Self::Contradicted => "contradicted",
            Self::Superseded => "superseded",
            Self::Expired => "expired",
            Self::Invalidated => "invalidated",
        }
    }
}

impl fmt::Display for VerificationStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for VerificationStatus {
    type Err = LatticeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "verified" => Ok(Self::Verified),
            "unverified" => Ok(Self::Unverified),
            "in_review" => Ok(Self::InReview),
            "stale" => Ok(Self::Stale),
            "contradicted" => Ok(Self::Contradicted),
            "superseded" => Ok(Self::Superseded),
            "expired" => Ok(Self::Expired),
            "invalidated" => Ok(Self::Invalidated),
            other => Err(LatticeError::Storage(format!(
                "Unknown verification status '{other}'"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationVerdict {
    pub status: VerificationStatus,
    pub reason: String,
}

impl VerificationVerdict {
    pub fn new(status: VerificationStatus, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
        }
    }
}
