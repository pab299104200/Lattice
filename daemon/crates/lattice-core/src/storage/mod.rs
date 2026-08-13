pub mod git_intelligence_store;
pub mod graph_store;
pub mod parsed_file_cache;
pub mod schema;
pub mod usearch_index;
pub mod vector_index;
pub mod vector_store;

#[cfg(test)]
mod tests;

pub use git_intelligence_store::{
    GitIntelligenceLoad, GitIntelligenceRecovery, GitIntelligenceStore,
    StoredGitIntelligenceSnapshot,
};
pub use graph_store::{
    FileIndexEntry, GraphStore, GraphStoreRecovery, IndexSnapshot, IndexSnapshotLoad,
    ModuleDigestCache, FILE_INDEX_PARSER_VERSION, FILE_INDEX_SCHEMA_VERSION,
};
pub use parsed_file_cache::{
    content_sha256, ParsedCacheLookup, ParsedFileCache, PARSED_CACHE_CONFIG_VERSION,
    PARSED_CACHE_PARSER_VERSION, PARSED_CACHE_SCHEMA_VERSION,
};
pub use usearch_index::UsearchVectorIndex;
pub use vector_index::{SharedVectorIndex, VectorIndex, VectorScope, VectorSearchResult};
pub use vector_store::VectorStore;
