use std::error::Error;
use std::fmt;

/// An error from the blob store.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("blob store io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("blob {0} not found")]
    NotFound(String),
    #[error("digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },
}

/// A blob scan error: either the store failed or the visitor rejected one row.
#[derive(Debug)]
pub enum BlobScanError<E> {
    Store(BlobError),
    Visit(E),
}

impl<E> From<BlobError> for BlobScanError<E> {
    fn from(err: BlobError) -> Self {
        Self::Store(err)
    }
}

impl<E: fmt::Display> fmt::Display for BlobScanError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(err) => err.fmt(formatter),
            Self::Visit(err) => err.fmt(formatter),
        }
    }
}

impl<E: Error + 'static> Error for BlobScanError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            Self::Visit(err) => Some(err),
        }
    }
}
