//! LLM proposal provenance for auditable consolidation outputs.
//!
//! Spec constraints from `## 6. Consolidation Engine` "LLM-driven consolidation":
//!
//! - each job records its model, prompt hash, and response hash as part of the proposal provenance so outputs are auditable and reproducible
//! - consolidation queue depth must be bounded; when the queue is full, new jobs are dropped with a log warning rather than stalling the daemon
//! - cost and latency budgets for LLM consolidation should be documented per job type before Phase 6 begins; jobs exceeding budget must fall back to deterministic approximations or skip with a stale flag

use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Utc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateTime<Tz> {
    pub unix_micros: u64,
    #[serde(skip)]
    timezone: std::marker::PhantomData<Tz>,
}

impl DateTime<Utc> {
    pub fn now() -> Self {
        let unix_micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros()
            .min(u64::MAX as u128) as u64;
        Self {
            unix_micros,
            timezone: std::marker::PhantomData,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmProvenance {
    pub model: String,
    pub prompt_sha256: [u8; 32],
    pub response_sha256: [u8; 32],
    pub prompt_token_count: u32,
    pub response_token_count: u32,
    pub latency_ms: u32,
    pub called_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProvenanceError {
    #[error("LLM latency {0}ms exceeds provenance storage range")]
    LatencyOutOfRange(u128),
}

impl LlmProvenance {
    pub fn record(
        driver_name: &str,
        prompt_bytes: &[u8],
        response_bytes: &[u8],
        prompt_tokens: u32,
        response_tokens: u32,
        latency: Duration,
    ) -> Result<Self, ProvenanceError> {
        let latency_ms = latency.as_millis();
        let latency_ms = u32::try_from(latency_ms)
            .map_err(|_| ProvenanceError::LatencyOutOfRange(latency.as_millis()))?;
        Ok(Self {
            model: driver_name.to_string(),
            prompt_sha256: sha256(prompt_bytes),
            response_sha256: sha256(response_bytes),
            prompt_token_count: prompt_tokens,
            response_token_count: response_tokens,
            latency_ms,
            called_at: DateTime::<Utc>::now(),
        })
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(&digest);
    hash
}
