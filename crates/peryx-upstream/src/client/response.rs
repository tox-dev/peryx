use reqwest::header::{HeaderMap, HeaderName};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileHead {
    pub len: u64,
}

pub(super) fn header_str(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
}
