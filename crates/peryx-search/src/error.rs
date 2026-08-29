use peryx_storage::meta::MetaScanError;

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error(transparent)]
    Tantivy(#[from] tantivy::TantivyError),
    #[error(transparent)]
    Directory(#[from] tantivy::directory::error::OpenDirectoryError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Meta(#[from] peryx_storage::meta::MetaError),
    #[error(transparent)]
    Blob(#[from] peryx_storage::blob::BlobError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("indexing failed: {0}")]
    Indexer(String),
    #[error("invalid resource source type {0:?}")]
    InvalidSource(String),
    #[error("invalid availability filter {0:?}")]
    InvalidAvailability(String),
    #[error("search page {page} with size {page_size} exceeds the {max}-result window")]
    ResultWindowTooLarge { page: usize, page_size: usize, max: usize },
    #[error("invalid indexed ecosystem {0:?}")]
    InvalidEcosystem(String),
}

impl SearchError {
    /// Keeps Tantivy's error taxonomy out of protocol adapters.
    #[must_use]
    pub const fn is_bad_request(&self) -> bool {
        matches!(
            self,
            Self::InvalidSource(_)
                | Self::InvalidAvailability(_)
                | Self::ResultWindowTooLarge { .. }
                | Self::Tantivy(tantivy::TantivyError::InvalidArgument(_))
        )
    }
}

impl From<MetaScanError<Self>> for SearchError {
    fn from(err: MetaScanError<Self>) -> Self {
        match err {
            MetaScanError::Store(err) => Self::Meta(err),
            MetaScanError::Visit(err) => err,
        }
    }
}
