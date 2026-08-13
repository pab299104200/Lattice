//! Lattice core library — parsing, graph, query engine, memory, intelligence.

extern crate self as chrono;

pub mod consolidation;
pub mod diff;
pub mod embeddings;
pub mod error;
pub mod events;
pub mod git_intelligence;
pub mod graph;
pub mod identity;
pub mod indexer;
pub mod intelligence;
pub mod memory;
pub mod memory_graph;
pub mod metrics;
pub mod parser;
pub mod query;
pub mod retrieval_v1;
pub mod security;
pub mod storage;
pub mod symbols;
mod temporal;
pub mod verification;
pub mod watcher;
pub mod working_memory;
pub mod workspace;

#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod hardening;

pub use error::LatticeError;
pub use temporal::{DateTime, Utc};
