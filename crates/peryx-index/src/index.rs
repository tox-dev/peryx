//! Index identity: the resolved shape of one configured index.

use peryx_core::Ecosystem;
use peryx_policy::Policy;
use peryx_upstream::UpstreamClient;

/// One resolved index. `layers`/`upload` in a virtual index are positions in the process's index
/// vector, so resolution is a plain vector walk with no name lookups at request time.
#[derive(Debug)]
pub struct Index {
    pub name: String,
    pub route: String,
    pub ecosystem: Ecosystem,
    pub kind: IndexKind,
    pub policy: Policy,
}

impl Index {
    /// The upstream client of a cached index that is online. `None` for a hosted or virtual index, and
    /// for a cached index an operator took offline: both have nothing to read through to.
    #[must_use]
    pub const fn proxy_client(&self) -> Option<&UpstreamClient> {
        match &self.kind {
            IndexKind::Cached { client, offline: false } => Some(client),
            _ => None,
        }
    }
}

/// The runtime shape of an index by role: a cached index owns its upstream client, a hosted store its
/// upload policy, a virtual index the resolved positions of its members and upload target.
#[derive(Debug)]
pub enum IndexKind {
    Cached {
        client: UpstreamClient,
        offline: bool,
    },
    Hosted {
        upload_token: Option<String>,
        volatile: bool,
    },
    Virtual {
        layers: Vec<usize>,
        upload: Option<usize>,
    },
}
