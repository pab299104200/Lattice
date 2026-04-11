use crate::error::LatticeError;
use std::sync::Arc;

pub type VectorSearchResult = (String, String, usize, f32);
pub type SharedVectorIndex = Arc<dyn VectorIndex>;

pub trait VectorIndex: Send + Sync {
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

    fn delete_by_file(&self, file: &str) -> Result<(), LatticeError>;

    fn clear_all(&self) -> Result<(), LatticeError>;

    fn search(&self, query: &[f32], top_k: usize) -> Result<Vec<VectorSearchResult>, LatticeError>;

    fn flush(&self) -> Result<(), LatticeError> {
        Ok(())
    }

    fn implementation_name(&self) -> &'static str;
}
