//! Neutral response header helpers and the artifact-head type range reads build on.

use reqwest::header::{HeaderMap, HeaderName};

/// The parts of an artifact `HEAD` response needed before range reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileHead {
    pub len: u64,
}

pub(super) fn header_str(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
}
