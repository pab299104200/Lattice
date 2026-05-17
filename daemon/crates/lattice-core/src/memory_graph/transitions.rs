use tracing::info_span;

use super::{MemoryEvidence, MemoryStore, MemoryStoreError, VerificationStatus};
use crate::identity::MemoryId;

#[derive(Debug, thiserror::Error)]
pub enum MemoryTransitionError {
    #[error(
        "memory transition from `{from}` to `{to}` is not allowed",
        from = .from.as_str(),
        to = .to.as_str()
    )]
    DisallowedTransition {
        from: VerificationStatus,
        to: VerificationStatus,
    },
    #[error(transparent)]
    Store(#[from] MemoryStoreError),
}

pub fn transition_status(
    store: &MemoryStore,
    memory_id: MemoryId,
    to: VerificationStatus,
    reason: &str,
    evidence: Option<MemoryEvidence>,
) -> Result<super::MemoryRecord, MemoryTransitionError> {
    let prior = store.load_for_transition(&memory_id)?;
    ensure_allowed(prior.verification_status, to)?;
    let _span = info_span!(
        "memory_store.transition",
        from = ?prior.verification_status,
        to = ?to
    )
    .entered();

    let mut updated = prior.clone();
    updated.verification_status = to;
    if to == VerificationStatus::Superseded && updated.superseded_by.is_none() {
        updated.superseded_by = updated
            .supersession_links
            .first()
            .map(|link| link.memory_id.clone());
    }
    if to != VerificationStatus::Superseded {
        updated.superseded_by = prior.superseded_by.clone();
    }
    store
        .write_transition(&prior, updated, reason, evidence)
        .map_err(MemoryTransitionError::from)
}

fn ensure_allowed(
    from: VerificationStatus,
    to: VerificationStatus,
) -> Result<(), MemoryTransitionError> {
    let allowed = matches!(
        (from, to),
        (VerificationStatus::Unverified, VerificationStatus::InReview)
            | (VerificationStatus::Unverified, VerificationStatus::Verified)
            | (VerificationStatus::InReview, VerificationStatus::Verified)
            | (VerificationStatus::InReview, VerificationStatus::Stale)
            | (
                VerificationStatus::InReview,
                VerificationStatus::Contradicted
            )
            | (VerificationStatus::InReview, VerificationStatus::Expired)
            | (
                VerificationStatus::InReview,
                VerificationStatus::Invalidated
            )
            | (VerificationStatus::Verified, VerificationStatus::Stale)
            | (
                VerificationStatus::Verified,
                VerificationStatus::Contradicted
            )
            | (VerificationStatus::Verified, VerificationStatus::Superseded)
            | (VerificationStatus::Verified, VerificationStatus::Expired)
            | (
                VerificationStatus::Verified,
                VerificationStatus::Invalidated
            )
            | (VerificationStatus::Stale, VerificationStatus::Verified)
            | (VerificationStatus::Stale, VerificationStatus::Invalidated)
    );
    if allowed {
        Ok(())
    } else {
        Err(MemoryTransitionError::DisallowedTransition { from, to })
    }
}
