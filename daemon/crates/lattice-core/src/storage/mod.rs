pub mod cache_publication;
pub mod object_accounting;
pub use cache_publication::CachePublicationGuard;
pub mod commit_manifest;
pub mod content_objects;
pub mod git_intelligence_store;
pub mod graph_store;
pub mod health_complexity_facts_store;
pub mod health_dead_symbol_facts_store;
pub mod health_graph_facts_store;
pub mod health_test_proximity_facts_store;
pub mod history_object_cache;
pub mod lifecycle;
pub mod managed_fs;
pub mod managed_sqlite;
pub mod operator;
pub mod parsed_file_cache;
pub mod relocation_registry;
pub mod schema;
pub mod usearch_index;
pub mod vector_index;
pub mod vector_store;

#[cfg(test)]
mod tests;

pub use content_objects::{ContentObjectStore, ObjectGcReport};
pub use git_intelligence_store::{
    GitIntelligenceLoad, GitIntelligenceRecovery, GitIntelligenceStore,
    StoredGitIntelligenceSnapshot,
};
pub use graph_store::{
    FileIndexEntry, GraphStore, GraphStoreRecovery, IndexSnapshot, IndexSnapshotLoad,
    ModuleDigestCache, FILE_INDEX_PARSER_VERSION, FILE_INDEX_SCHEMA_VERSION,
};
pub use health_complexity_facts_store::{
    validate_canonical_path, GenerationStatus, HealthComplexityFactsStore,
};
pub use health_dead_symbol_facts_store::{
    DeadSymbolFactsLoad, DeadSymbolFactsRecovery, HealthDeadSymbolFactsStore, StoredDeadSymbolFacts,
};
pub use health_graph_facts_store::{
    GraphFactsLoad, GraphFactsRecovery, HealthGraphFactsStore, StoredGraphFacts,
};
pub use health_test_proximity_facts_store::{
    HealthTestProximityFactsStore, StoredTestProximityFacts, TestProximityFactsLoad,
    TestProximityFactsRecovery,
};
pub use history_object_cache::{HistoryCacheLookup, HistoryObjectCache};
pub use lifecycle::{
    CachePolicy, CheckoutLease, GcCandidate, GcReport, StorageInventory, StorageRegistry,
};
pub use managed_fs::{ManagedDirCursor, ManagedDirPage, ManagedEntry, ManagedIdentity, SecureDir};
pub use operator::{
    AccountingLimits, ByteAccounting, CacheMaintenanceOutcome, CacheMaintenancePlan,
    HistoricalRetirementEntry, HistoricalRetirementOutcome, HistoricalRetirementPlan,
    KnowledgeBackupManifest, KnowledgeBackupRequest, KnowledgeRestoreOutcome,
    KnowledgeRestoreRequest, StorageClasses, StorageOperator, StorageStatus, MAX_PLAN_CANDIDATES,
};
pub use parsed_file_cache::{
    content_sha256, ParsedCacheLookup, ParsedFileCache, PARSED_CACHE_CONFIG_VERSION,
    PARSED_CACHE_PARSER_VERSION, PARSED_CACHE_SCHEMA_VERSION,
};
pub use relocation_registry::{
    relocate_repository_home, resolve_recorded_relocation, RecordedRelocation,
    RepositoryRelocationOutcome, RepositoryRelocationRequest,
};
pub use usearch_index::UsearchVectorIndex;
pub use vector_index::{SharedVectorIndex, VectorIndex, VectorScope, VectorSearchResult};
pub use vector_store::VectorStore;
