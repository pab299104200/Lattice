use thiserror::Error;

#[derive(Error, Debug)]
pub enum LatticeError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parse error in {file}: {message}")]
    Parse { file: String, message: String },

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Memory storage is busy: {0}")]
    MemoryStorageBusy(String),

    #[error("Memory storage access denied: {0}")]
    MemoryStorageAccessDenied(String),

    #[error("Memory storage is full: {0}")]
    MemoryStorageFull(String),

    #[error("Unsupported memory schema: {0}")]
    UnsupportedMemorySchema(String),

    #[error("Corrupt memory storage: {0}")]
    CorruptMemoryStorage(String),

    #[error("Persistent memory storage is unavailable: {0}")]
    MemoryStorageUnavailable(String),

    #[error("Corrupt derived storage at {path}: {message}")]
    CorruptStorage { path: String, message: String },

    #[error("Query error: {0}")]
    Query(String),
}
