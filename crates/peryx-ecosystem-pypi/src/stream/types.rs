//! The per-request inputs and outputs of the streaming page transform.

use std::collections::{HashMap, HashSet};

use peryx_policy::Policy;
use url::Url;

use crate::{File, Yanked};

/// Per-request configuration: how to rewrite and merge one page.
#[derive(Debug, Default, Clone)]
pub struct PageContext {
    /// The route file URLs point back at, for example `root/pypi`.
    pub route: String,
    /// The upstream page's response URL, present on a live fetch so relative file URLs resolve. A
    /// warm re-transform reads an already-canonicalized body, so it leaves this `None`.
    pub base: Option<Url>,
    /// The normalized project name this page serves.
    pub project: String,
    /// The compiled policy for the route being served.
    pub policy: Policy,
    /// Locally uploaded files, emitted ahead of the upstream ones (their URLs are already local).
    pub local_files: Vec<File>,
    /// Locally known versions, merged into the upstream version list.
    pub local_versions: Vec<String>,
    /// Filenames to drop: shadowed by a local file or hidden by an override.
    pub skip: HashSet<String>,
    /// Filenames forced to the yanked state by an override.
    pub yanked: HashMap<String, Yanked>,
    /// Generated metadata already cached by artifact sha256.
    pub known_metadata: HashMap<String, String>,
}

/// A file's upstream source recorded while transforming, persisted later in one batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub filename: String,
    pub sha256: String,
    pub url: String,
    pub size: Option<u64>,
    /// `(sibling url, metadata sha256)` when the file advertises PEP 658 metadata.
    pub metadata: Option<(String, String)>,
}

/// Everything the transformer learned about the page, enough to persist it without a re-parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageSummary {
    pub registrations: Vec<Registration>,
    /// The page's top-level display name, when it carried one.
    pub name: Option<String>,
    pub project_status: Option<String>,
    pub project_status_reason: Option<String>,
}

/// A malformed upstream page.
#[derive(Debug, thiserror::Error)]
pub enum TransformError {
    #[error("upstream page is not valid JSON: {0}")]
    Parse(#[from] serde_json::Error),
    #[error(transparent)]
    Simple(#[from] crate::SimpleError),
    #[error("upstream page ended mid-token")]
    Truncated,
    #[error("upstream page carries data after the document root")]
    Trailing,
}
