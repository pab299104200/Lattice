pub mod context_cache;
pub mod event_capture;
mod event_capture_support;
#[cfg(test)]
pub mod identity_payload;
pub mod mcp;
pub mod memory_v2;
pub mod metrics_surface;
pub mod protocol;
pub mod server;
pub mod session_metrics;
pub mod workflow_v2;
pub mod working_memory_tool;

#[cfg(test)]
mod event_capture_tests;
#[cfg(test)]
mod mcp_compat_tests;
#[cfg(test)]
mod mcp_schema_tests;
#[cfg(test)]
mod metrics_surface_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod working_memory_tool_tests;
