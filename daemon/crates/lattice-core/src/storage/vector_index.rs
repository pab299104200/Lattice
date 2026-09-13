use crate::error::LatticeError;
use std::sync::Arc;

pub type VectorSearchResult = (String, String, usize, f32);
pub type SharedVectorIndex = Arc<dyn VectorIndex>;
pub trait VectorPublicationLease {}
struct NoopVectorPublicationLease;
impl VectorPublicationLease for NoopVectorPublicationLease {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorScope {
    Symbol,
    FileSummary,
    All,
}

pub trait VectorIndex: Send + Sync {
    fn begin_publication(&self) -> Result<Box<dyn VectorPublicationLease + '_>, LatticeError> {
        Ok(Box::new(NoopVectorPublicationLease))
    }
    /// Bind persisted vectors to the complete embedding identity. Backends
    /// without identity metadata conservatively discard their accelerator.
    fn bind_embedding_identity(&self, _identity: &str) -> Result<(), LatticeError> {
        self.clear_all()
    }

    fn initialize(&self, dimension: usize) -> Result<(), LatticeError>;

    fn warm(&self) -> Result<(), LatticeError> {
        Ok(())
    }

    fn upsert_vector(
        &self,
        file: &str,
        name: &str,
        byte_offset: usize,
        vector: &[f32],
    ) -> Result<(), LatticeError>;

    fn upsert_vector_in_scope(
        &self,
        file: &str,
        name: &str,
        byte_offset: usize,
        scope: VectorScope,
        vector: &[f32],
    ) -> Result<(), LatticeError> {
        let _ = scope;
        self.upsert_vector(file, name, byte_offset, vector)
    }

    fn delete_by_file(&self, file: &str) -> Result<(), LatticeError>;

    fn clear_all(&self) -> Result<(), LatticeError>;

    fn search(&self, query: &[f32], top_k: usize) -> Result<Vec<VectorSearchResult>, LatticeError>;

    fn search_in_scope(
        &self,
        query: &[f32],
        top_k: usize,
        scope: VectorScope,
    ) -> Result<Vec<VectorSearchResult>, LatticeError> {
        let _ = scope;
        self.search(query, top_k)
    }

    fn flush(&self) -> Result<(), LatticeError> {
        Ok(())
    }

    fn implementation_name(&self) -> &'static str;
}
