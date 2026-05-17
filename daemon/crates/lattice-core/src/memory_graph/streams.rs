//! Typed memory streams and default retrieval policies.
//!
//! The mapping table in this module is the Rust source of truth for the
//! `## Streams` section of
//! `docs/architecture/2026-05-16-memory-graph-schema.md`.

use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{AssertionType, FreshnessKind, FreshnessPolicy, MemoryClass, MemoryScope};

pub type RankingProfileRef = &'static str;

/// Memory streams from the ViLoMem-derived cognitive workspace design.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStream {
    CodeTopology,
    WorkflowEpisodes,
    FailurePatterns,
    SemanticRepoClaims,
    ArchitectureDecisions,
    UserTeamPreferences,
    DocsAndContractState,
}

impl MemoryStream {
    pub const VALUES: &'static [&'static str] = &[
        "code_topology",
        "workflow_episodes",
        "failure_patterns",
        "semantic_repo_claims",
        "architecture_decisions",
        "user_team_preferences",
        "docs_and_contract_state",
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            MemoryStream::CodeTopology => "code_topology",
            MemoryStream::WorkflowEpisodes => "workflow_episodes",
            MemoryStream::FailurePatterns => "failure_patterns",
            MemoryStream::SemanticRepoClaims => "semantic_repo_claims",
            MemoryStream::ArchitectureDecisions => "architecture_decisions",
            MemoryStream::UserTeamPreferences => "user_team_preferences",
            MemoryStream::DocsAndContractState => "docs_and_contract_state",
        }
    }
}

impl FromStr for MemoryStream {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "code_topology" => Ok(MemoryStream::CodeTopology),
            "workflow_episodes" => Ok(MemoryStream::WorkflowEpisodes),
            "failure_patterns" => Ok(MemoryStream::FailurePatterns),
            "semantic_repo_claims" => Ok(MemoryStream::SemanticRepoClaims),
            "architecture_decisions" => Ok(MemoryStream::ArchitectureDecisions),
            "user_team_preferences" => Ok(MemoryStream::UserTeamPreferences),
            "docs_and_contract_state" => Ok(MemoryStream::DocsAndContractState),
            _ => Err("unknown memory stream"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamPolicy {
    pub stream: MemoryStream,
    pub freshness_default: FreshnessPolicy,
    pub consolidation_window: Duration,
    pub default_scope: MemoryScope,
    pub ranking_profile: RankingProfileRef,
}

/// Deterministic stream classifier.
///
/// The class is authoritative. For the ambiguous "claim or topology" classes
/// (`Observation`, `Constraint`, `Pattern`, `AntiPattern`), the assertion type
/// is the tiebreaker:
///
/// - `observation` assertions are treated as code-topology facts.
/// - `constraint`, `hypothesis`, `decision`, and `counter` assertions become
///   semantic repo claims.
/// - workflow assertions (`procedure`, `outcome`, `question`) stay in the
///   workflow stream even if the class is one of the ambiguous claim classes.
///
/// `CounterMemory` normally inherits the stream of the memory it counters. This
/// function has only the class and assertion type, so the counter target is not
/// available here. In that underspecified case, the fallback stream is
/// `SemanticRepoClaims` and a warning is emitted for operator visibility.
pub fn classify_stream(class: MemoryClass, assertion_type: AssertionType) -> MemoryStream {
    match class {
        MemoryClass::WorkflowOutcome | MemoryClass::Procedure | MemoryClass::OpenQuestion => {
            MemoryStream::WorkflowEpisodes
        }
        MemoryClass::FailurePattern => MemoryStream::FailurePatterns,
        MemoryClass::Decision | MemoryClass::ArchitectureInvariant => {
            MemoryStream::ArchitectureDecisions
        }
        MemoryClass::Preference => MemoryStream::UserTeamPreferences,
        MemoryClass::DocsContract => MemoryStream::DocsAndContractState,
        MemoryClass::CounterMemory => {
            warn!(
                class = %class.as_str(),
                assertion_type = %assertion_type.as_str(),
                "CounterMemory stream fallback used because the counter target stream is unknown"
            );
            MemoryStream::SemanticRepoClaims
        }
        MemoryClass::Observation
        | MemoryClass::Constraint
        | MemoryClass::Pattern
        | MemoryClass::AntiPattern => classify_claim_or_topology(assertion_type),
    }
}

pub fn default_policy(stream: MemoryStream) -> StreamPolicy {
    match stream {
        MemoryStream::CodeTopology => StreamPolicy {
            stream,
            freshness_default: scoped_freshness(FreshnessKind::BranchScoped, 6, 12),
            consolidation_window: Duration::from_secs(7 * 24 * 60 * 60),
            default_scope: MemoryScope::Branch,
            ranking_profile: "code_topology_v1",
        },
        MemoryStream::WorkflowEpisodes => StreamPolicy {
            stream,
            freshness_default: scoped_freshness(FreshnessKind::SessionScoped, 1, 4),
            consolidation_window: Duration::from_secs(3 * 24 * 60 * 60),
            default_scope: MemoryScope::Session,
            ranking_profile: "workflow_episodes_v1",
        },
        MemoryStream::FailurePatterns => StreamPolicy {
            stream,
            freshness_default: event_freshness(30, 7),
            consolidation_window: Duration::from_secs(14 * 24 * 60 * 60),
            default_scope: MemoryScope::Repo,
            ranking_profile: "failure_patterns_v1",
        },
        MemoryStream::SemanticRepoClaims => StreamPolicy {
            stream,
            freshness_default: scoped_freshness(FreshnessKind::RepoScoped, 14, 7),
            consolidation_window: Duration::from_secs(21 * 24 * 60 * 60),
            default_scope: MemoryScope::Repo,
            ranking_profile: "semantic_repo_claims_v1",
        },
        MemoryStream::ArchitectureDecisions => StreamPolicy {
            stream,
            freshness_default: scoped_freshness(FreshnessKind::RepoScoped, 90, 30),
            consolidation_window: Duration::from_secs(90 * 24 * 60 * 60),
            default_scope: MemoryScope::Repo,
            ranking_profile: "architecture_decisions_v1",
        },
        MemoryStream::UserTeamPreferences => StreamPolicy {
            stream,
            freshness_default: time_bound_freshness(30, 14),
            consolidation_window: Duration::from_secs(30 * 24 * 60 * 60),
            default_scope: MemoryScope::User,
            ranking_profile: "user_team_preferences_v1",
        },
        MemoryStream::DocsAndContractState => StreamPolicy {
            stream,
            freshness_default: scoped_freshness(FreshnessKind::RepoScoped, 14, 7),
            consolidation_window: Duration::from_secs(14 * 24 * 60 * 60),
            default_scope: MemoryScope::Repo,
            ranking_profile: "docs_and_contract_state_v1",
        },
    }
}

fn classify_claim_or_topology(assertion_type: AssertionType) -> MemoryStream {
    match assertion_type {
        AssertionType::Observation => MemoryStream::CodeTopology,
        AssertionType::Procedure | AssertionType::Outcome | AssertionType::Question => {
            MemoryStream::WorkflowEpisodes
        }
        AssertionType::Constraint
        | AssertionType::Hypothesis
        | AssertionType::Decision
        | AssertionType::Preference
        | AssertionType::Counter => MemoryStream::SemanticRepoClaims,
    }
}

fn scoped_freshness(kind: FreshnessKind, ttl_days: u64, recheck_days: u64) -> FreshnessPolicy {
    FreshnessPolicy {
        kind,
        ttl: Some(Duration::from_secs(ttl_days * 24 * 60 * 60)),
        recheck_interval: Some(Duration::from_secs(recheck_days * 24 * 60 * 60)),
    }
}

fn time_bound_freshness(ttl_days: u64, recheck_days: u64) -> FreshnessPolicy {
    scoped_freshness(FreshnessKind::TimeBound, ttl_days, recheck_days)
}

fn event_freshness(ttl_days: u64, recheck_days: u64) -> FreshnessPolicy {
    scoped_freshness(FreshnessKind::EventTriggered, ttl_days, recheck_days)
}
