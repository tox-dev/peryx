//! Index identity and role: the resolved shape of one configured index.

use serde::Serialize;
use velodex_format::Ecosystem;
use velodex_policy::Policy;
use velodex_upstream::UpstreamClient;

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

/// An index's role, without the payload [`IndexKind`] carries. Metric families scope themselves to
/// the roles that emit them, and the render layer gates counters by matching this against an index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Cached,
    Hosted,
    Virtual,
}

impl Role {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cached => "cached",
            Self::Hosted => "hosted",
            Self::Virtual => "virtual",
        }
    }
}
