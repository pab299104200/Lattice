use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::events::StableRef;
use crate::identity::{EventId, FileId, MemoryId, SectionId, SymbolId};

/// Primary memory classes from the cognitive workspace Memory Graph spec:
/// `Observation`, `Decision`, `Constraint`, `Pattern`, `AntiPattern`,
/// `WorkflowOutcome`, `FailurePattern`, `Procedure`, `Preference`,
/// `ArchitectureInvariant`, `DocsContract`, `OpenQuestion`, `CounterMemory`.
///
/// `CounterMemory` -- a memory record that explicitly asserts a prior memory is
/// wrong or no longer applicable. Distinct from a `contradicts` link: a link
/// connects two existing memories; a CounterMemory is a first-class record
/// authored by an agent or user that carries its own evidence, scope, and
/// verification status and can itself be superseded or invalidated.
///
/// Each memory record requires: stable id, content, memory class, assertion
/// type, scope: session, branch, repo, user, organization, verification status,
/// confidence and confidence reason, freshness policy, validity conditions,
/// invalidation triggers, provenance events, evidence references, linked files,
/// linked symbols, linked docs, linked tests, linked memories, contradiction
/// links, supersession links, access history, usefulness scores, last verified
/// state.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MemoryClass {
    Observation,
    Decision,
    Constraint,
    Pattern,
    AntiPattern,
    WorkflowOutcome,
    FailurePattern,
    Procedure,
    Preference,
    ArchitectureInvariant,
    DocsContract,
    OpenQuestion,
    CounterMemory,
}

impl MemoryClass {
    pub const VALUES: &'static [&'static str] = &[
        "observation",
        "decision",
        "constraint",
        "pattern",
        "anti_pattern",
        "workflow_outcome",
        "failure_pattern",
        "procedure",
        "preference",
        "architecture_invariant",
        "docs_contract",
        "open_question",
        "counter_memory",
    ];

    /// Return the canonical snake_case wire name used in storage and MCP payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryClass::Observation => "observation",
            MemoryClass::Decision => "decision",
            MemoryClass::Constraint => "constraint",
            MemoryClass::Pattern => "pattern",
            MemoryClass::AntiPattern => "anti_pattern",
            MemoryClass::WorkflowOutcome => "workflow_outcome",
            MemoryClass::FailurePattern => "failure_pattern",
            MemoryClass::Procedure => "procedure",
            MemoryClass::Preference => "preference",
            MemoryClass::ArchitectureInvariant => "architecture_invariant",
            MemoryClass::DocsContract => "docs_contract",
            MemoryClass::OpenQuestion => "open_question",
            MemoryClass::CounterMemory => "counter_memory",
        }
    }
}

impl FromStr for MemoryClass {
    type Err = MemoryGraphParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "observation" => Ok(MemoryClass::Observation),
            "decision" => Ok(MemoryClass::Decision),
            "constraint" => Ok(MemoryClass::Constraint),
            "pattern" => Ok(MemoryClass::Pattern),
            "anti_pattern" => Ok(MemoryClass::AntiPattern),
            "workflow_outcome" => Ok(MemoryClass::WorkflowOutcome),
            "failure_pattern" => Ok(MemoryClass::FailurePattern),
            "procedure" => Ok(MemoryClass::Procedure),
            "preference" => Ok(MemoryClass::Preference),
            "architecture_invariant" => Ok(MemoryClass::ArchitectureInvariant),
            "docs_contract" => Ok(MemoryClass::DocsContract),
            "open_question" => Ok(MemoryClass::OpenQuestion),
            "counter_memory" => Ok(MemoryClass::CounterMemory),
            other => Err(MemoryGraphParseError::UnknownMemoryClass(other.to_string())),
        }
    }
}

/// Truth-status asserted by a memory record, separate from the structural class.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AssertionType {
    Observation,
    Decision,
    Constraint,
    Hypothesis,
    Procedure,
    Outcome,
    Preference,
    Question,
    Counter,
}

impl AssertionType {
    pub const VALUES: &'static [&'static str] = &[
        "observation",
        "decision",
        "constraint",
        "hypothesis",
        "procedure",
        "outcome",
        "preference",
        "question",
        "counter",
    ];

    /// Return the canonical snake_case wire name used in storage and MCP payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            AssertionType::Observation => "observation",
            AssertionType::Decision => "decision",
            AssertionType::Constraint => "constraint",
            AssertionType::Hypothesis => "hypothesis",
            AssertionType::Procedure => "procedure",
            AssertionType::Outcome => "outcome",
            AssertionType::Preference => "preference",
            AssertionType::Question => "question",
            AssertionType::Counter => "counter",
        }
    }
}

impl FromStr for AssertionType {
    type Err = MemoryGraphParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "observation" => Ok(AssertionType::Observation),
            "decision" => Ok(AssertionType::Decision),
            "constraint" => Ok(AssertionType::Constraint),
            "hypothesis" => Ok(AssertionType::Hypothesis),
            "procedure" => Ok(AssertionType::Procedure),
            "outcome" => Ok(AssertionType::Outcome),
            "preference" => Ok(AssertionType::Preference),
            "question" => Ok(AssertionType::Question),
            "counter" => Ok(AssertionType::Counter),
            other => Err(MemoryGraphParseError::UnknownAssertionType(
                other.to_string(),
            )),
        }
    }
}

/// Scope boundary for memory visibility and retrieval.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Session,
    Branch,
    Repo,
    User,
    Organization,
}

impl MemoryScope {
    pub const VALUES: &'static [&'static str] =
        &["session", "branch", "repo", "user", "organization"];

    /// Return the canonical snake_case wire name used in storage and MCP payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryScope::Session => "session",
            MemoryScope::Branch => "branch",
            MemoryScope::Repo => "repo",
            MemoryScope::User => "user",
            MemoryScope::Organization => "organization",
        }
    }
}

impl FromStr for MemoryScope {
    type Err = MemoryGraphParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "session" => Ok(MemoryScope::Session),
            "branch" => Ok(MemoryScope::Branch),
            "repo" => Ok(MemoryScope::Repo),
            "user" => Ok(MemoryScope::User),
            "organization" => Ok(MemoryScope::Organization),
            other => Err(MemoryGraphParseError::UnknownMemoryScope(other.to_string())),
        }
    }
}

/// Verification state produced by the memory verification engine.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Unverified,
    InReview,
    Verified,
    Stale,
    Contradicted,
    Superseded,
    Expired,
    Invalidated,
}

impl VerificationStatus {
    pub const VALUES: &'static [&'static str] = &[
        "unverified",
        "in_review",
        "verified",
        "stale",
        "contradicted",
        "superseded",
        "expired",
        "invalidated",
    ];

    /// Return the canonical snake_case wire name used in storage and MCP payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            VerificationStatus::Unverified => "unverified",
            VerificationStatus::InReview => "in_review",
            VerificationStatus::Verified => "verified",
            VerificationStatus::Stale => "stale",
            VerificationStatus::Contradicted => "contradicted",
            VerificationStatus::Superseded => "superseded",
            VerificationStatus::Expired => "expired",
            VerificationStatus::Invalidated => "invalidated",
        }
    }
}

impl fmt::Display for VerificationStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for VerificationStatus {
    type Err = MemoryGraphParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "unverified" => Ok(VerificationStatus::Unverified),
            "in_review" => Ok(VerificationStatus::InReview),
            "verified" => Ok(VerificationStatus::Verified),
            "stale" => Ok(VerificationStatus::Stale),
            "contradicted" => Ok(VerificationStatus::Contradicted),
            "superseded" => Ok(VerificationStatus::Superseded),
            "expired" => Ok(VerificationStatus::Expired),
            "invalidated" => Ok(VerificationStatus::Invalidated),
            other => Err(MemoryGraphParseError::UnknownVerificationStatus(
                other.to_string(),
            )),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessKind {
    SessionScoped,
    BranchScoped,
    RepoScoped,
    TimeBound,
    EventTriggered,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FreshnessPolicy {
    pub kind: FreshnessKind,
    pub ttl: Option<Duration>,
    pub recheck_interval: Option<Duration>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ValidityCondition {
    pub description: String,
    pub predicate: ValidityPredicate,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ValidityPredicate {
    FileExists { file_id: FileId },
    SymbolExists { symbol_id: SymbolId },
    DocHeadingExists { section_id: SectionId },
    TestPasses { test_id: TestId },
    TimeBefore { unix_timestamp: i64 },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct InvalidationTrigger {
    pub kind: TriggerKind,
    pub target: StableRef,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    FileChanged,
    SymbolChanged,
    DocSectionChanged,
    TestFailed,
    TimeExpired,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct EvidenceReference {
    pub target: StableRef,
    pub event_id: Option<EventId>,
    pub summary: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct MemoryLinkReference {
    pub memory_id: MemoryId,
    pub reason: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct MemoryAccessRecord {
    pub accessed_at: i64,
    pub accessor: String,
    pub purpose: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TestId {
    pub workspace_id: String,
    pub test_path: String,
    pub test_name: String,
}

/// Canonical memory row mirrored by `memory_graph/schema.sql`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MemoryRecord {
    pub memory_id: MemoryId,
    pub content: String,
    pub class: MemoryClass,
    pub assertion_type: AssertionType,
    pub scope: MemoryScope,
    pub scope_session_id: Option<String>,
    pub scope_branch: Option<String>,
    pub scope_workspace_id: Option<String>,
    pub scope_user_id: Option<String>,
    pub scope_org_id: Option<String>,
    pub verification_status: VerificationStatus,
    pub confidence: f64,
    pub confidence_reason: String,
    pub freshness_policy: FreshnessPolicy,
    pub validity_conditions: Vec<ValidityCondition>,
    pub invalidation_triggers: Vec<InvalidationTrigger>,
    pub provenance_event_ids: Vec<EventId>,
    pub evidence_references: Vec<EvidenceReference>,
    pub linked_files: Vec<FileId>,
    pub linked_symbols: Vec<SymbolId>,
    pub linked_docs: Vec<SectionId>,
    pub linked_tests: Vec<TestId>,
    pub linked_memories: Vec<MemoryLinkReference>,
    pub contradiction_links: Vec<MemoryLinkReference>,
    pub supersession_links: Vec<MemoryLinkReference>,
    pub access_history: Vec<MemoryAccessRecord>,
    pub last_verified_event_id: Option<i64>,
    pub last_verified_state: Option<serde_json::Value>,
    pub usefulness_score: f64,
    pub usefulness_score_updated_at: i64,
    pub created_at: i64,
    pub created_by: String,
    pub updated_at: i64,
    pub updated_by: String,
    pub superseded_by: Option<MemoryId>,
    pub schema_version: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryGraphParseError {
    UnknownMemoryClass(String),
    UnknownAssertionType(String),
    UnknownMemoryScope(String),
    UnknownVerificationStatus(String),
    InvalidJsonColumn {
        column: &'static str,
        reason: String,
    },
}

impl fmt::Display for MemoryGraphParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryGraphParseError::UnknownMemoryClass(value) => {
                write!(formatter, "unknown memory class `{value}`")
            }
            MemoryGraphParseError::UnknownAssertionType(value) => {
                write!(formatter, "unknown assertion type `{value}`")
            }
            MemoryGraphParseError::UnknownMemoryScope(value) => {
                write!(formatter, "unknown memory scope `{value}`")
            }
            MemoryGraphParseError::UnknownVerificationStatus(value) => {
                write!(formatter, "unknown verification status `{value}`")
            }
            MemoryGraphParseError::InvalidJsonColumn { column, reason } => {
                write!(
                    formatter,
                    "invalid JSON in memory column `{column}`: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for MemoryGraphParseError {}

pub fn decode_json_column<T>(
    column: &'static str,
    encoded: &str,
) -> Result<T, MemoryGraphParseError>
where
    T: DeserializeOwned,
{
    serde_json::from_str(encoded).map_err(|error| MemoryGraphParseError::InvalidJsonColumn {
        column,
        reason: error.to_string(),
    })
}
