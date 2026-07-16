use std::error::Error;
use std::fmt;

/// A backend operation that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobOperation {
    Put,
    Get,
    Head,
    Range,
    Delete,
    Verify,
}

impl fmt::Display for BlobOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Put => "put",
            Self::Get => "get",
            Self::Head => "head",
            Self::Range => "range",
            Self::Delete => "delete",
            Self::Verify => "verify",
        })
    }
}

/// An error from the blob store.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("blob store io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("blob {0} not found")]
    NotFound(String),
    #[error("digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },
    #[error("blob {digest} cannot serve range {start}..{end} from {bytes} bytes")]
    InvalidRange {
        digest: String,
        start: u64,
        end: u64,
        bytes: u64,
    },
    #[error("{backend} blob backend {operation} failed for {digest}: {source}")]
    Backend {
        backend: &'static str,
        operation: BlobOperation,
        digest: String,
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
}

impl BlobError {
    pub(crate) fn backend(
        backend: &'static str,
        operation: BlobOperation,
        digest: &super::Digest,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self::Backend {
            backend,
            operation,
            digest: digest.as_str().to_owned(),
            source: Box::new(source),
        }
    }
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
