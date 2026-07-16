use std::error::Error;

use peryx_storage::blob::BlobError;
use peryx_storage::meta::MetaError;

/// A page validation, transfer, or local commit failure.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("primary request failed: {0}")]
    Primary(#[source] Box<dyn Error + Send + Sync>),
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error(transparent)]
    Blob(#[from] BlobError),
    #[error("replica state is invalid: {0}")]
    State(#[from] serde_json::Error),
    #[error("unsupported replication protocol version {actual}; expected {expected}")]
    UnsupportedVersion { actual: u16, expected: u16 },
    #[error("replication page has an empty source identity")]
    EmptySource,
    #[error("replication page starts after serial {actual}; requested {expected}")]
    WrongPageStart { expected: u64, actual: u64 },
    #[error("replication page expected a serial after {after}, found {actual}")]
    SerialGap { after: u64, actual: u64 },
    #[error("primary current serial {current} precedes page serial {page}")]
    PrimaryBehind { current: u64, page: u64 },
    #[error("primary is at serial {current} but returned no changes after {after}")]
    MissingChanges { after: u64, current: u64 },
    #[error("replication page has {actual} changes; requested at most {limit}")]
    PageTooLarge { limit: usize, actual: usize },
    #[error("replica follows source {expected:?}, received {actual:?}")]
    SourceChanged { expected: String, actual: String },
    #[error("local journal serial {journal} differs from replica cursor {cursor}")]
    LocalSerialMismatch { cursor: u64, journal: u64 },
    #[error("invalid sha256 digest {0:?}")]
    InvalidDigest(String),
    #[error("blob {digest} has conflicting sizes {first} and {second}")]
    ConflictingBlobSize { digest: String, first: u64, second: u64 },
    #[error("blob {digest} has {actual} bytes; expected {expected}")]
    BlobSizeMismatch { digest: String, expected: u64, actual: u64 },
    #[error("blob {0} already exists with bytes that fail digest verification")]
    CorruptBlob(String),
    #[error("metadata mutation targets reserved key {0:?}")]
    ReservedMetadataKey(String),
}

impl SyncError {
    pub(crate) fn primary(error: impl Error + Send + Sync + 'static) -> Self {
        Self::Primary(Box::new(error))
    }
}
