use std::fmt;

#[derive(Debug)]
pub enum StorageError {
    /// Generic database/storage backend error
    Backend(String),
    /// Serialization/deserialization error
    Serialization(String),
    /// Invalid or corrupted data
    InvalidData(String),
    /// I/O error
    Io(std::io::Error),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            StorageError::Backend(e) => write!(f, "Storage backend error: {}", e),
            StorageError::Serialization(msg) => write!(f, "Serialization error: {}", msg),
            StorageError::InvalidData(msg) => write!(f, "Invalid data: {}", msg),
            StorageError::Io(e) => write!(f, "I/O error: {}", e),
        }
    }
}

impl std::error::Error for StorageError {}
impl From<rocksdb::Error> for StorageError {
    fn from(err: rocksdb::Error) -> Self {
        StorageError::Backend(err.to_string())
    }
}

impl From<std::io::Error> for StorageError {
    fn from(err: std::io::Error) -> Self {
        StorageError::Io(err)
    }
}

pub type Result<T> = std::result::Result<T, StorageError>;
