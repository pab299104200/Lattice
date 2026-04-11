use anyhow::Result;
use lattice_core::embeddings::EmbeddingEngine;
use lattice_core::graph::CodeGraph;
use lattice_core::storage::VectorIndex;
use std::collections::HashSet;

pub(crate) fn sync_full_graph_embeddings(
    graph: &CodeGraph,
    embedding_engine: &EmbeddingEngine,
    vector_index: &dyn VectorIndex,
) -> Result<usize> {
    vector_index.clear_all()?;
    let embedded = upsert_graph_nodes(
        graph.all_nodes().into_iter(),
        embedding_engine,
        vector_index,
    )?;
    vector_index.flush()?;
    Ok(embedded)
}

pub(crate) fn sync_changed_files_embeddings(
    graph: &CodeGraph,
    changed_files: &[String],
    embedding_engine: &EmbeddingEngine,
    vector_index: &dyn VectorIndex,
) -> Result<usize> {
    if changed_files.is_empty() {
        return Ok(0);
    }

    let changed: HashSet<&str> = changed_files.iter().map(|file| file.as_str()).collect();
    for file in &changed {
        vector_index.delete_by_file(file)?;
    }

    let embedded = upsert_graph_nodes(
        graph
            .all_nodes()
            .into_iter()
            .filter(|node| changed.contains(node.file.as_str())),
        embedding_engine,
        vector_index,
    )?;
    vector_index.flush()?;
    Ok(embedded)
}

fn upsert_graph_nodes<'a>(
    nodes: impl IntoIterator<Item = &'a lattice_core::graph::model::GraphNode>,
    embedding_engine: &EmbeddingEngine,
    vector_index: &dyn VectorIndex,
) -> Result<usize> {
    let mut embedded = 0usize;
    for node in nodes {
        let text = format!("{} {}", node.name, node.signature);
        match embedding_engine.embed(&text) {
            Ok(vector) => {
                vector_index.upsert_vector(&node.file, &node.name, node.id.byte_offset, &vector)?;
                embedded += 1;
            }
            Err(err) => {
                tracing::warn!(
                    "Failed to embed '{}' in '{}': {}",
                    node.name,
                    node.file,
                    err
                );
            }
        }
    }
    Ok(embedded)
}
