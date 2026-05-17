//! Memory surfacing discipline for workflow bundles.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Non-Negotiable Product Properties` requires:
//! "Every stale or contradicted memory is surfaced as such, not hidden behind recency."
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## 8. Verification Engine` defines the verification outputs:
//! `verified`, `unverified`, `in_review`, `stale`, `contradicted`,
//! `superseded`, `expired`, and `invalidated`.
//!
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Risks — Stale Memory Leakage` names the control:
//! "freshness indexes, graph-change-triggered verification, stale labels
//! in all memory surfaces, and tests that stale memory cannot rank as trusted."

use serde::{Deserialize, Serialize};
use tracing::{debug, error};

use crate::identity::{Identity, MemoryId};
use crate::memory::{Memory, MemoryScope, MemoryVerificationStatus};
use crate::retrieval_v1::{RankedCandidate, ScoringContext};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleSection {
    Trusted,
    Stale,
    Contradicted,
    Superseded,
    Expired,
    Invalidated,
    InReview,
    Unverified,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabeledMemory {
    pub memory_id: MemoryId,
    pub section: BundleSection,
    pub label_reason: String,
    pub content: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfacedBundle {
    pub trusted: Vec<LabeledMemory>,
    pub advisory: Vec<LabeledMemory>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfacingError {
    RankerIntegrationNotInitialized { memory_id: String },
}

pub trait BundleProducer {
    fn surface(memories: Vec<Memory>) -> SurfacedBundle;
}

pub struct SurfacingPipeline;

impl BundleProducer for SurfacingPipeline {
    fn surface(memories: Vec<Memory>) -> SurfacedBundle {
        Self::partition(memories)
    }
}

impl SurfacingPipeline {
    pub fn partition(memories: Vec<Memory>) -> SurfacedBundle {
        Self::partition_for_caller(memories, module_path!())
    }

    pub fn partition_for_caller(
        memories: Vec<Memory>,
        caller_module: &'static str,
    ) -> SurfacedBundle {
        let mut trusted = Vec::new();
        let mut advisory = Vec::new();

        for memory in memories {
            let labeled = Self::label(memory);
            if labeled.section == BundleSection::Trusted {
                trusted.push(labeled);
            } else {
                advisory.push(labeled);
            }
        }

        Self::enforce_trusted_invariant(&mut trusted, caller_module);
        debug!(
            caller_module,
            trusted_count = trusted.len(),
            advisory_count = advisory.len(),
            "surfaced memory bundle partitioned"
        );
        SurfacedBundle { trusted, advisory }
    }

    pub fn classify(memory: &Memory) -> BundleSection {
        classify_status(&memory.verification_status)
    }

    pub fn format_reason(memory: &Memory, section: BundleSection) -> String {
        let status = memory.verification_status.as_str();
        let evidence = evidence_reason(memory);
        match section {
            BundleSection::Trusted => format!("Verified memory; {evidence}"),
            BundleSection::Stale => format!("Stale memory ({status}); {evidence}"),
            BundleSection::Contradicted => format!("Contradicted memory ({status}); {evidence}"),
            BundleSection::Superseded => format!("Superseded memory ({status}); {evidence}"),
            BundleSection::Expired => format!("Expired memory ({status}); {evidence}"),
            BundleSection::Invalidated => format!("Invalidated memory ({status}); {evidence}"),
            BundleSection::InReview => format!("Memory is still in review ({status}); {evidence}"),
            BundleSection::Unverified => format!("Unverified memory ({status}); {evidence}"),
        }
    }

    pub fn trusted_ranked_top_n(
        ranked: Vec<RankedCandidate>,
        ctx: &ScoringContext,
        top_n: usize,
    ) -> Vec<RankedCandidate> {
        ranked
            .into_iter()
            .filter(|candidate| ranked_candidate_is_trusted_memory(candidate, ctx))
            .take(top_n)
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn enforce_trusted_invariant_for_test(
        trusted: &mut Vec<LabeledMemory>,
        caller_module: &'static str,
    ) {
        Self::enforce_trusted_invariant(trusted, caller_module);
    }

    fn label(memory: Memory) -> LabeledMemory {
        let section = Self::classify(&memory);
        let label_reason = Self::format_reason(&memory, section);
        LabeledMemory {
            memory_id: memory_id(&memory),
            section,
            label_reason,
            content: memory.content,
        }
    }

    fn enforce_trusted_invariant(trusted: &mut Vec<LabeledMemory>, caller_module: &'static str) {
        #[cfg(debug_assertions)]
        {
            if let Some(leaked) = trusted
                .iter()
                .find(|memory| memory.section != BundleSection::Trusted)
            {
                panic!(
                    "stale_in_trusted_bundle memory_id={} status={:?} caller_module={}",
                    leaked.memory_id, leaked.section, caller_module
                );
            }
        }

        #[cfg(not(debug_assertions))]
        {
            trusted.retain(|memory| {
                let is_trusted = memory.section == BundleSection::Trusted;
                if !is_trusted {
                    error!(
                        memory_id = %memory.memory_id,
                        status = ?memory.section,
                        caller_module,
                        "stale_in_trusted_bundle"
                    );
                }
                is_trusted
            });
        }
    }
}

pub fn classify_status(status: &MemoryVerificationStatus) -> BundleSection {
    match status {
        MemoryVerificationStatus::Verified => BundleSection::Trusted,
        MemoryVerificationStatus::Unverified => BundleSection::Unverified,
        MemoryVerificationStatus::InReview => BundleSection::InReview,
        MemoryVerificationStatus::Stale => BundleSection::Stale,
        MemoryVerificationStatus::Contradicted => BundleSection::Contradicted,
        MemoryVerificationStatus::Superseded => BundleSection::Superseded,
        MemoryVerificationStatus::Expired => BundleSection::Expired,
        MemoryVerificationStatus::Invalidated => BundleSection::Invalidated,
    }
}

fn ranked_candidate_is_trusted_memory(candidate: &RankedCandidate, ctx: &ScoringContext) -> bool {
    let Identity::Memory(memory_id) = &candidate.candidate.identity else {
        return true;
    };
    ctx.memory_metadata
        .get(&candidate.candidate.identity.to_string())
        .map(|metadata| metadata.verification_status == MemoryVerificationStatus::Verified)
        .unwrap_or_else(|| {
            error!(
                memory_id = %memory_id,
                "ranker_integration_not_initialized"
            );
            false
        })
}

fn memory_id(memory: &Memory) -> MemoryId {
    MemoryId {
        workspace_id: memory
            .workspace_id
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        ulid: memory.id.clone(),
    }
}

fn evidence_reason(memory: &Memory) -> String {
    let stale_reason = memory
        .stale_reason
        .as_deref()
        .unwrap_or("no verifier reason recorded");
    let location = memory
        .linked_files
        .first()
        .map(|file| format!("file `{file}`"))
        .unwrap_or_else(|| "no linked file".to_string());
    let scope = format_scope(&memory.scope, memory.branch.as_deref());
    format!("{stale_reason}; {location}; {scope}")
}

fn format_scope(scope: &MemoryScope, branch: Option<&str>) -> String {
    match (scope, branch) {
        (MemoryScope::Branch, Some(branch)) => format!("scope branch `{branch}`"),
        (MemoryScope::Branch, None) => "scope branch".to_string(),
        (MemoryScope::Session, _) => "scope session".to_string(),
        (MemoryScope::Repo, _) => "scope repo".to_string(),
        (MemoryScope::Organization, _) => "scope organization".to_string(),
    }
}
