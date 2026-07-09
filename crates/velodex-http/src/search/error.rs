//! The error type spanning tantivy, storage, and ecosystem-indexer failures.

use velodex_storage::meta::MetaScanError;

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error(transparent)]
    Tantivy(#[from] tantivy::TantivyError),
    #[error(transparent)]
    Directory(#[from] tantivy::directory::error::OpenDirectoryError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Meta(#[from] velodex_storage::meta::MetaError),
    #[error(transparent)]
    Blob(#[from] velodex_storage::blob::BlobError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The ecosystem indexer failed to derive a document from a stored record.
    #[error("indexing failed: {0}")]
    Indexer(String),
    #[error("invalid package source type {0:?}")]
    InvalidSource(String),
}

impl From<MetaScanError<Self>> for SearchError {
    fn from(err: MetaScanError<Self>) -> Self {
        match err {
            MetaScanError::Store(err) => Self::Meta(err),
            MetaScanError::Visit(err) => err,
        }
    }
}
