//! Daemon adapters for repository-owned memory attribution.
//!
//! Durable retrieval/access state lives in the authoritative `MemoryStore`.
//! This module only defines wire projections, deterministic ids, and the
//! separately persisted operational-metrics adapter contract.

use lattice_core::events::{canonical_json_bytes, hash_canonical_payload_bytes};
use lattice_core::identity::{EventId, MemoryId};
use lattice_core::memory_graph::MemoryAccessId;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RetrievedMemory {
    pub(crate) memory_id: MemoryId,
    pub(crate) inclusion_reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccessDisposition {
    Used,
    NotUsed,
}

pub(crate) trait MemoryAttributionMetrics: Send + Sync {
    fn record_retrieval(
        &self,
        metric_id: &str,
        retrieval_id: &str,
        retrieved_count: u64,
    ) -> Result<(), String>;
    fn record_use(
        &self,
        metric_id: &str,
        retrieval_id: &str,
        used_count: u64,
    ) -> Result<(), String>;
}

#[derive(Serialize)]
struct AttributionIdInput<'a> {
    kind: &'a str,
    workspace_id: &'a str,
    retrieval_event_ulid: &'a str,
    memory_ulid: Option<&'a str>,
    terminal_event_ulid: Option<&'a str>,
}

pub(crate) fn derive_retrieval_id(event: &EventId) -> Result<String, serde_json::Error> {
    derive_attribution_id("retrieval", event, None, None)
}

pub(crate) fn derive_access_id(
    event: &EventId,
    memory: &MemoryId,
) -> Result<String, serde_json::Error> {
    derive_attribution_id("access", event, Some(memory.ulid.as_str()), None)
}

pub(crate) fn derive_metric_id(
    kind: &'static str,
    retrieval_id: &str,
    access_and_event: Option<(&MemoryAccessId, &EventId)>,
) -> Result<String, serde_json::Error> {
    let input = match access_and_event {
        Some((access_id, event)) => AttributionIdInput {
            kind,
            workspace_id: &event.workspace_id,
            retrieval_event_ulid: retrieval_id,
            memory_ulid: Some(access_id.as_str()),
            terminal_event_ulid: Some(&event.ulid),
        },
        None => AttributionIdInput {
            kind,
            workspace_id: "",
            retrieval_event_ulid: retrieval_id,
            memory_ulid: None,
            terminal_event_ulid: None,
        },
    };
    let bytes = canonical_json_bytes(&input)?;
    Ok(format!(
        "memory_metric:{}",
        hash_canonical_payload_bytes(&bytes).to_hex()
    ))
}

fn derive_attribution_id(
    kind: &'static str,
    event: &EventId,
    memory_ulid: Option<&str>,
    terminal_event_ulid: Option<&str>,
) -> Result<String, serde_json::Error> {
    let bytes = canonical_json_bytes(&AttributionIdInput {
        kind,
        workspace_id: &event.workspace_id,
        retrieval_event_ulid: &event.ulid,
        memory_ulid,
        terminal_event_ulid,
    })?;
    Ok(format!(
        "memory_{kind}:{}",
        hash_canonical_payload_bytes(&bytes).to_hex()
    ))
}
