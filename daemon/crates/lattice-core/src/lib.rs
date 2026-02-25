//! Lattice core library — parsing, graph, query engine, memory.

pub mod error;
pub mod graph;
pub mod parser;
pub mod query;
pub mod storage;
pub mod symbols;
pub mod watcher;

pub use error::LatticeError;
