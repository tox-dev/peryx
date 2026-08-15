use peryx_core::Ecosystem;
use peryx_identity::IndexAcl;
use peryx_policy::Policy;
use peryx_upstream::UpstreamClient;

/// Virtual references store positions to avoid request-time name lookups.
#[derive(Debug)]
pub struct Index {
    pub name: String,
    pub route: String,
    pub ecosystem: Ecosystem,
    pub kind: IndexKind,
    pub policy: Policy,
    pub acl: IndexAcl,
}

impl Index {
    /// `None` means the repository cannot read through.
    #[must_use]
    pub const fn proxy_client(&self) -> Option<&UpstreamClient> {
        match &self.kind {
            IndexKind::Cached { client, offline: false } => Some(client),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum IndexKind {
    Cached {
        client: UpstreamClient,
        offline: bool,
    },
    Hosted {
        volatile: bool,
    },
    Virtual {
        layers: Vec<usize>,
        write_target: Option<usize>,
    },
}

#[cfg(test)]
#[path = "../tests/unit/index/tests.rs"]
mod tests;
