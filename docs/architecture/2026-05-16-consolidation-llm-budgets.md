# Consolidation LLM Budgets

## Scope

This note defines the default budget, bounded queue, failure, and provenance contract for Phase 6 LLM-driven consolidation jobs: episode summary generation, procedure extraction, contradiction detection, and failure-pattern extraction. `BudgetCatalog` in `daemon/crates/lattice-core/src/consolidation/llm/budget.rs` is the canonical source of truth; this document mirrors those defaults for operators.

## Per-job budgets

| job_kind | max_prompt_tokens | max_response_tokens | max_latency_ms | max_cost_micro_usd | fallback_strategy |
|---|---:|---:|---:|---:|---|
| episode_summary | 6000 | 1200 | 8000 | 250 | deterministic episode template |
| procedure_extraction | 10000 | 1800 | 12000 | 450 | skip and mark affected memory stale |
| contradiction_detection | 3000 | 800 | 5000 | 125 | deterministic supersession candidate |
| failure_pattern_extraction | 7000 | 1400 | 9000 | 300 | skip and mark affected memory stale |

## Bounded queue depth

The consolidation queue is globally bounded by `ConsolidationConfig::max_queue_depth`. LLM submission can additionally use a per-kind slice quota through `BoundedJobQueue::with_per_kind_max_depth`. When a slice is full, the daemon drops the incoming job, persists it as `dropped`, emits a `tracing::warn!` with `workspace_id`, `kind`, `current_depth`, and `max_depth`, and writes a `ConsolidationFailed` event with `error_kind = "queue_full"`.

## Failure handling

Malformed responses, driver failures, budget overruns, and queue-full drops leave prior memory state unchanged. The failure path emits `ConsolidationFailed` before returning, with the job id, job kind, mode, model name, error kind, and actionable error message. Budget overruns either use the documented deterministic fallback strategy or skip with a stale flag rather than accepting an over-budget LLM response.

## Provenance record

Every successful LLM proposal stores `provenance_json` on `consolidation_proposals`. The record contains `model`, `prompt_sha256`, `response_sha256`, `prompt_token_count`, `response_token_count`, `latency_ms`, and `called_at`. Hashes use SHA-256 over the exact prompt and response bytes, which lets operators verify that a proposal came from the recorded prompt and response without storing either blob in the proposal row.

## Operator overrides

Operators override budgets by constructing `ConsolidationConfig` with `llm_budget_catalog: Some(BudgetCatalog { ... })`; otherwise `BudgetCatalog::default()` supplies the pinned values above. Queue depth is controlled by `max_queue_depth`, and LLM per-kind slice depth is configured on `BoundedJobQueue` where the LLM submission surface owns the event writer required for queue-full audit events.

## Spec citation

This contract implements [§6. Consolidation Engine](../plans/2026-05-16-cognitive-workspace-fork-plan.md#6-consolidation-engine), specifically the "LLM-driven consolidation" bullets requiring model/prompt/response provenance, bounded queue drops with warnings, and documented cost and latency budgets with deterministic fallback or stale marking.
