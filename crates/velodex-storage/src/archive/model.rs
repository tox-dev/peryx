use serde::Serialize;

/// One entry of an archive listing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Member {
    pub path: String,
    pub size: u64,
    pub kind: MemberKind,
    pub previewable: bool,
}

/// The UI behavior available for an archive member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberKind {
    Archive,
    Text,
    Binary,
    Unknown,
}

impl MemberKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::Text => "text",
            Self::Binary => "binary",
            Self::Unknown => "unknown",
        }
    }
}

/// A bounded slice of one archive member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberChunk {
    pub bytes: Vec<u8>,
    pub size: u64,
    pub offset: u64,
    pub next_offset: Option<u64>,
}

/// An error while reading an archive.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("unsupported archive type; accepted formats are .whl, .zip, .egg, .tar, .tar.gz, and .tgz")]
    Unsupported,
    #[error("nested archive member {0:?} is not a supported archive")]
    UnsupportedNestedArchive(String),
    #[error("archive member not found")]
    MemberNotFound,
    #[error("archive member offset {offset} is beyond member size {size}")]
    InvalidRange { offset: u64, size: u64 },
    #[error("archive member path {0:?} is not a safe relative path")]
    UnsafeMember(String),
    #[error("archive nesting depth {depth} exceeds the configured limit of {limit}")]
    NestingTooDeep { depth: usize, limit: usize },
    #[error("nested archive member {member:?} is {size} bytes, above the configured limit of {limit} bytes")]
    NestedArchiveTooLarge { member: String, size: u64, limit: u64 },
    #[error("archive listing exceeds the configured limit of {0} file entries")]
    TooManyEntries(usize),
    #[error("archive member {0:?} is not a text member and cannot be previewed inline")]
    BinaryMember(String),
    /// The archive is malformed against a format's rules; the message carries the format-specific
    /// detail (an ecosystem's wheel or sdist validator constructs this).
    #[error("{0}")]
    Invalid(String),
    #[error("archive read failed: {0}")]
    Read(String),
}
