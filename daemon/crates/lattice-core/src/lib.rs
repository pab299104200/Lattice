//! Lattice core library — parsing, graph, query engine, memory, intelligence.

pub mod diff;
pub mod embeddings;
pub mod error;
pub mod graph;
pub mod indexer;
pub mod intelligence;
pub mod memory;
pub mod parser;
pub mod query;
pub mod security;
pub mod storage;
pub mod symbols;
pub mod watcher;
pub mod workspace;

pub use error::LatticeError;
