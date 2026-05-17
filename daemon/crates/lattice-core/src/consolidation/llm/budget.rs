//! LLM consolidation budgets and deterministic fallback routing.
//!
//! Spec constraints from `## 6. Consolidation Engine` "LLM-driven consolidation":
//!
//! - each job records its model, prompt hash, and response hash as part of the proposal provenance so outputs are auditable and reproducible
//! - consolidation queue depth must be bounded; when the queue is full, new jobs are dropped with a log warning rather than stalling the daemon
//! - cost and latency budgets for LLM consolidation should be documented per job type before Phase 6 begins; jobs exceeding budget must fall back to deterministic approximations or skip with a stale flag

use serde::{Deserialize, Serialize};

use super::ConsolidationJobKind;
use crate::consolidation::ConsolidationConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmBudget {
    pub max_prompt_tokens: u32,
    pub max_response_tokens: u32,
    pub max_latency_ms: u32,
    pub max_cost_micro_usd: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeterministicFallback {
    EpisodeTemplate,
    NoOp,
    ContradictionSupersessionCandidate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BudgetOutcome {
    Within,
    ExceededFallback(DeterministicFallback),
    ExceededSkipWithStale,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetCatalog {
    pub episode_summary: LlmBudget,
    pub procedure_extraction: LlmBudget,
    pub contradiction_detection: LlmBudget,
    pub failure_pattern_extraction: LlmBudget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetUsage {
    pub prompt_tokens: u32,
    pub response_tokens: u32,
    pub latency_ms: u32,
    pub cost_micro_usd: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum BudgetError {
    #[error("LLM budget exceeded for {kind}: {dimension} actual={actual} budget={budget}")]
    Exceeded {
        kind: &'static str,
        dimension: &'static str,
        actual: u32,
        budget: u32,
        outcome: BudgetOutcome,
    },
}

impl BudgetCatalog {
    pub fn from_config(config: &ConsolidationConfig) -> Self {
        config
            .llm_budget_catalog
            .clone()
            .unwrap_or_else(Self::default)
    }

    pub fn for_kind(&self, kind: ConsolidationJobKind) -> LlmBudget {
        match kind {
            ConsolidationJobKind::EpisodeSummary => self.episode_summary,
            ConsolidationJobKind::ProcedureExtraction => self.procedure_extraction,
            ConsolidationJobKind::ContradictionDetection => self.contradiction_detection,
            ConsolidationJobKind::FailurePatternExtraction => self.failure_pattern_extraction,
        }
    }

    pub fn evaluate(&self, kind: ConsolidationJobKind, usage: BudgetUsage) -> BudgetOutcome {
        let budget = self.for_kind(kind);
        if usage.prompt_tokens <= budget.max_prompt_tokens
            && usage.response_tokens <= budget.max_response_tokens
            && usage.latency_ms <= budget.max_latency_ms
            && usage.cost_micro_usd <= budget.max_cost_micro_usd
        {
            return BudgetOutcome::Within;
        }
        exceeded_outcome(kind)
    }

    pub fn require_within(
        &self,
        kind: ConsolidationJobKind,
        usage: BudgetUsage,
    ) -> Result<(), BudgetError> {
        let budget = self.for_kind(kind);
        check_limit(
            kind,
            "prompt_tokens",
            usage.prompt_tokens,
            budget.max_prompt_tokens,
        )?;
        check_limit(
            kind,
            "response_tokens",
            usage.response_tokens,
            budget.max_response_tokens,
        )?;
        check_limit(kind, "latency_ms", usage.latency_ms, budget.max_latency_ms)?;
        check_limit(
            kind,
            "cost_micro_usd",
            usage.cost_micro_usd,
            budget.max_cost_micro_usd,
        )
    }
}

impl Default for BudgetCatalog {
    fn default() -> Self {
        Self {
            episode_summary: LlmBudget {
                max_prompt_tokens: 6_000,
                max_response_tokens: 1_200,
                max_latency_ms: 8_000,
                max_cost_micro_usd: 250,
            },
            procedure_extraction: LlmBudget {
                max_prompt_tokens: 10_000,
                max_response_tokens: 1_800,
                max_latency_ms: 12_000,
                max_cost_micro_usd: 450,
            },
            contradiction_detection: LlmBudget {
                max_prompt_tokens: 3_000,
                max_response_tokens: 800,
                max_latency_ms: 5_000,
                max_cost_micro_usd: 125,
            },
            failure_pattern_extraction: LlmBudget {
                max_prompt_tokens: 7_000,
                max_response_tokens: 1_400,
                max_latency_ms: 9_000,
                max_cost_micro_usd: 300,
            },
        }
    }
}

fn check_limit(
    kind: ConsolidationJobKind,
    dimension: &'static str,
    actual: u32,
    budget: u32,
) -> Result<(), BudgetError> {
    if actual <= budget {
        return Ok(());
    }
    Err(BudgetError::Exceeded {
        kind: kind.as_str(),
        dimension,
        actual,
        budget,
        outcome: exceeded_outcome(kind),
    })
}

fn exceeded_outcome(kind: ConsolidationJobKind) -> BudgetOutcome {
    match kind {
        ConsolidationJobKind::EpisodeSummary => {
            BudgetOutcome::ExceededFallback(DeterministicFallback::EpisodeTemplate)
        }
        ConsolidationJobKind::ProcedureExtraction => BudgetOutcome::ExceededSkipWithStale,
        ConsolidationJobKind::ContradictionDetection => BudgetOutcome::ExceededFallback(
            DeterministicFallback::ContradictionSupersessionCandidate,
        ),
        ConsolidationJobKind::FailurePatternExtraction => BudgetOutcome::ExceededSkipWithStale,
    }
}
