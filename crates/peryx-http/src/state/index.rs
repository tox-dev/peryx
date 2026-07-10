//! Index identity and role: the resolved shape of one configured index.

use peryx_core::Ecosystem;
use peryx_policy::Policy;
use peryx_upstream::UpstreamClient;

/// One resolved index. `layers`/`upload` in a virtual index are indices into [`AppState::indexes`], so
/// resolution is a plain vector walk with no name lookups at request time.
///
/// [`AppState::indexes`]: super::AppState::indexes
#[derive(Debug)]
pub struct Index {
    pub name: String,
    pub route: String,
    pub ecosystem: Ecosystem,
    pub kind: IndexKind,
    pub policy: Policy,
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

