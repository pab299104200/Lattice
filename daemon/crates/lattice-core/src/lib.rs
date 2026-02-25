//! Lattice core library — parsing, graph, query engine, memory.

pub mod embeddings;
pub mod error;
pub mod graph;
pub mod indexer;
pub mod memory;
pub mod parser;
pub mod query;
pub mod storage;
pub mod symbols;
pub mod watcher;

pub use error::LatticeError;
