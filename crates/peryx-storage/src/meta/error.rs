use std::error::Error;
use std::fmt;

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
    #[error("driver precondition failed: {0}")]
    DriverPrecondition(String),
    #[error("driver record {key:?} is not UTF-8")]
    DriverRecordUtf8 {
        key: String,
        #[source]
        source: std::string::FromUtf8Error,
    },
    #[error("driver record {key:?} has invalid integer field {field:?}")]
    DriverRecordInteger {
        key: String,
        field: &'static str,
        #[source]
        source: std::num::ParseIntError,
    },
    #[error("driver record {key:?} is missing field {field:?}")]
    DriverRecordMissing { key: String, field: &'static str },
    #[error("driver record {key:?} does not decode")]
    DriverRecordMalformed {
        key: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("driver record {key:?} carries unknown field {field:?} and needs a newer peryx")]
    DriverRecordSchema { key: String, field: String },
    #[error("external role grant key {key:?} is malformed")]
    MalformedExternalGrantKey { key: String },
    #[error("user name {canonical_name:?} identifies multiple accounts {user_ids:?}")]
    UserNameCollision {
        canonical_name: String,
        user_ids: Vec<String>,
    },
    #[error("server user {id} has an invalid display name")]
    UserNameMigration {
        id: String,
        #[source]
        source: peryx_identity::UserNameError,
    },
    #[error("blob {digest} is being reclaimed; publish the reference again once its deletion finishes")]
    BlobReclaiming { digest: String },
}

impl MetaError {
    /// Detects redb's exclusive file-lock conflict between readers and writers.
    #[must_use]
    pub const fn is_database_already_open(&self) -> bool {
        matches!(self, Self::Database(redb::DatabaseError::DatabaseAlreadyOpen))
    }
}

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
