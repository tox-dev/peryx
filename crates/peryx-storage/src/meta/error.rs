use std::error::Error;
use std::fmt;

/// An error from the metadata store.
#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error(transparent)]
    Database(#[from] redb::DatabaseError),
    #[error(transparent)]
    Transaction(#[from] redb::TransactionError),
    #[error(transparent)]
    Table(#[from] redb::TableError),
    #[error(transparent)]
    Storage(#[from] redb::StorageError),
    #[error(transparent)]
    Commit(#[from] redb::CommitError),
    #[error(transparent)]
    Decode(#[from] serde_json::Error),
    #[error("replica serial conflict: expected {expected}, found {actual}")]
    ReplicaSerialConflict { expected: u64, actual: u64 },
}

/// A rejected writer-identity claim or promotion.
#[derive(Debug, thiserror::Error)]
pub enum WriterIdentityError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("writer identity cannot be empty")]
    Empty,
    #[error("metadata store is claimed by writer {active:?}; refusing {requested:?}")]
    Claimed { active: String, requested: String },
    #[error("metadata store writer is {active:?}; expected {expected:?}")]
    Changed { active: Option<String>, expected: String },
}

/// A metadata scan error: either the store failed or the visitor rejected one row.
#[derive(Debug)]
pub enum MetaScanError<E> {
    Store(MetaError),
    Visit(E),
}

impl<E> From<MetaError> for MetaScanError<E> {
    fn from(err: MetaError) -> Self {
        Self::Store(err)
    }
}

impl<E: fmt::Display> fmt::Display for MetaScanError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(err) => err.fmt(formatter),
            Self::Visit(err) => err.fmt(formatter),
        }
    }
}

impl<E: Error + 'static> Error for MetaScanError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            Self::Visit(err) => Some(err),
        }
    }
}
