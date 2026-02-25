mod rpc;

use std::sync::Arc;
use tokio::sync::Mutex;
use anyhow::Result;
use tracing_subscriber::EnvFilter;

use lattice_core::graph::CodeGraph;
use lattice_core::query::QueryEngine;
use rpc::mcp::McpHandler;
use rpc::server::StdioServer;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr) // stderr for logs, stdout for JSON-RPC
        .init();

    tracing::info!("Lattice daemon starting...");

    let graph = CodeGraph::new();
    let engine = QueryEngine::new(graph, None);
    let engine = Arc::new(Mutex::new(engine));
    let handler = Arc::new(McpHandler::new(engine));
    let server = StdioServer::new(handler);
    server.run().await?;

    Ok(())
}
