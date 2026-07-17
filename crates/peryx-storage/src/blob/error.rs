use std::error::Error;
use std::fmt;

/// A backend operation that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobOperation {
    Health,
    Open,
    Head,
    Write,
    Commit,
    Delete,
    Verify,
    List,
    Materialize,
}

impl fmt::Display for BlobOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Health => "health",
            Self::Open => "open",
            Self::Head => "head",
            Self::Write => "write",
            Self::Commit => "commit",
            Self::Delete => "delete",
            Self::Verify => "verify",
            Self::List => "list",
            Self::Materialize => "materialize",
        })
    }
}

/// Backend-neutral context attached without changing the semantic error kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobErrorContext {
    pub backend: &'static str,
    pub operation: BlobOperation,
    pub digest: Option<String>,
}

impl fmt::Display for BlobErrorContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} blob backend {}", self.backend, self.operation)?;
        if let Some(digest) = &self.digest {
            write!(formatter, " for {digest}")?;
        }
        Ok(())
    }
}

/// The stable semantic category of a blob failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobErrorKind {
    Io,
    NotFound,
    DigestMismatch,
    InvalidRange,
    LimitExceeded,
    Unsupported,
}

/// An error from a blob backend.
#[derive(Debug)]
pub struct BlobError {
    context: Option<BlobErrorContext>,
    detail: BlobErrorDetail,
}

#[derive(Debug)]
enum BlobErrorDetail {
    Io(std::io::Error),
    NotFound(String),
    DigestMismatch { expected: String, actual: String },
    InvalidRange { start: u64, end: u64, bytes: u64 },
    LimitExceeded { limit: u64, actual: u64 },
    Unsupported(&'static str),
}

impl BlobError {
    #[must_use]
    pub fn io(error: std::io::Error) -> Self {
        error.into()
    }

    #[must_use]
    pub const fn kind(&self) -> BlobErrorKind {
        match self.detail {
            BlobErrorDetail::Io(_) => BlobErrorKind::Io,
            BlobErrorDetail::NotFound(_) => BlobErrorKind::NotFound,
            BlobErrorDetail::DigestMismatch { .. } => BlobErrorKind::DigestMismatch,
            BlobErrorDetail::InvalidRange { .. } => BlobErrorKind::InvalidRange,
            BlobErrorDetail::LimitExceeded { .. } => BlobErrorKind::LimitExceeded,
            BlobErrorDetail::Unsupported(_) => BlobErrorKind::Unsupported,
        }
    }

    #[must_use]
    pub const fn context(&self) -> Option<&BlobErrorContext> {
        self.context.as_ref()
    }

    #[must_use]
    pub fn not_found(digest: &super::Digest) -> Self {
        Self {
            context: None,
            detail: BlobErrorDetail::NotFound(digest.as_str().to_owned()),
        }
    }

    #[must_use]
    pub fn digest_mismatch(expected: &super::Digest, actual: &super::Digest) -> Self {
        Self {
            context: None,
            detail: BlobErrorDetail::DigestMismatch {
                expected: expected.as_str().to_owned(),
                actual: actual.as_str().to_owned(),
            },
        }
    }

    #[must_use]
    pub const fn invalid_range(start: u64, end: u64, bytes: u64) -> Self {
        Self {
            context: None,
            detail: BlobErrorDetail::InvalidRange { start, end, bytes },
        }
    }

    #[must_use]
    pub const fn unsupported(capability: &'static str) -> Self {
        Self {
            context: None,
            detail: BlobErrorDetail::Unsupported(capability),
        }
    }

    #[must_use]
    pub const fn limit_exceeded(limit: u64, actual: u64) -> Self {
        Self {
            context: None,
            detail: BlobErrorDetail::LimitExceeded { limit, actual },
        }
    }

    #[must_use]
    pub fn with_context(
        mut self,
        backend: &'static str,
        operation: BlobOperation,
        digest: Option<&super::Digest>,
    ) -> Self {
        self.context = Some(BlobErrorContext {
            backend,
            operation,
            digest: digest.map(|value| value.as_str().to_owned()),
        });
        self
    }

    #[must_use]
    pub fn mismatch(&self) -> Option<(&str, &str)> {
        match &self.detail {
            BlobErrorDetail::DigestMismatch { expected, actual } => Some((expected, actual)),
            _ => None,
        }
    }

    #[must_use]
    pub const fn invalid_range_values(&self) -> Option<(u64, u64, u64)> {
        match self.detail {
            BlobErrorDetail::InvalidRange { start, end, bytes } => Some((start, end, bytes)),
            _ => None,
        }
    }
}

impl From<std::io::Error> for BlobError {
    fn from(error: std::io::Error) -> Self {
        Self {
            context: None,
            detail: BlobErrorDetail::Io(error),
        }
    }
}

impl fmt::Display for BlobError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(context) = &self.context {
            write!(formatter, "{context}: ")?;
        }
        match &self.detail {
            BlobErrorDetail::Io(_) => formatter.write_str("I/O error"),
            BlobErrorDetail::NotFound(digest) => write!(formatter, "blob {digest} not found"),
            BlobErrorDetail::DigestMismatch { expected, actual } => {
                write!(formatter, "digest mismatch: expected {expected}, got {actual}")
            }
            BlobErrorDetail::InvalidRange { start, end, bytes } => {
                write!(formatter, "range {start}..{end} exceeds {bytes} bytes")
            }
            BlobErrorDetail::LimitExceeded { limit, actual } => {
                write!(formatter, "blob size {actual} exceeds {limit} byte limit")
            }
            BlobErrorDetail::Unsupported(capability) => write!(formatter, "{capability} is unsupported"),
        }
    }
}

impl Error for BlobError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.detail {
            BlobErrorDetail::Io(error) => Some(error),
            _ => None,
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
