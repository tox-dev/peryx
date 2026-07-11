//! Human-facing descriptions of configured indexes, shared by `/+status` and the web UI.

use peryx_identity::Action;
use peryx_index::{Index, IndexKind};

/// Describe every runtime index without touching storage or upstream state.
#[must_use]
pub fn describe_indexes(indexes: &[Index]) -> Vec<IndexDescription> {
    (0..indexes.len())
        .map(|position| describe_index(indexes, position))
        .collect()
}

#[must_use]
pub fn describe_index(indexes: &[Index], position: usize) -> IndexDescription {
    let index = &indexes[position];
    let (kind, layers, uploads, volatile_deletes, upload_to) = match &index.kind {
        IndexKind::Cached { .. } => ("cached", Vec::new(), false, false, None),
        IndexKind::Hosted { volatile } => (
            "hosted",
            Vec::new(),
            writable(index),
            writable(index) && *volatile,
            None,
        ),
        IndexKind::Virtual { layers, upload } => {
            let names = layers.iter().map(|&pos| indexes[pos].name.clone()).collect();
            let target = upload.map(|pos| &indexes[pos]);
            let uploads = target.is_some_and(writable);
            let volatile_deletes = target
                .is_some_and(|index| writable(index) && matches!(index.kind, IndexKind::Hosted { volatile: true }));
            let upload_to = target.map(|index| index.name.clone());
            ("virtual", names, uploads, volatile_deletes, upload_to)
        }
    };
    let (upstream, hosted) = match &index.kind {
        IndexKind::Cached { client, offline } => (
            Some(UpstreamDescription {
                url: client.redacted_base_url(),
                auth: client.auth_status().as_str(),
                offline: *offline,
            }),
            None,
        ),
        IndexKind::Hosted { volatile } => (
            None,
            Some(HostedDescription {
                volatile: *volatile,
                upload_token: SecretDescription::new(writable(index)),
            }),
        ),
        IndexKind::Virtual { .. } => (None, None),
    };
    IndexDescription {
        name: index.name.clone(),
        route: index.route.clone(),
        ecosystem: index.ecosystem.as_str(),
        kind,
        layers,
        uploads,
        volatile_deletes,
        upload_to,
        upstream,
        hosted,
    }
}

/// Whether the index has a credential that may upload: what a status surface means by "uploads are
/// enabled", the `upload_token`-is-set question widened to an ACL that may hold several tokens.
fn writable(index: &Index) -> bool {
    index.acl.grants_to_anyone(Action::Write)
}

/// A configured index as presented to humans: on the dashboard, in `/+status`, and in discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDescription {
    pub name: String,
    pub route: String,
    pub ecosystem: &'static str,
    pub kind: &'static str,
    pub layers: Vec<String>,
    pub uploads: bool,
    pub volatile_deletes: bool,
    /// For a virtual index: the layer uploads land in, whether or not a token currently enables them.
    pub upload_to: Option<String>,
    pub upstream: Option<UpstreamDescription>,
    pub hosted: Option<HostedDescription>,
}

/// A cached index's upstream status, with credential material excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamDescription {
    pub url: String,
    pub auth: &'static str,
    pub offline: bool,
}

/// A hosted store's status, with upload-token values excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedDescription {
    pub volatile: bool,
    pub upload_token: SecretDescription,
}

/// Redacted secret metadata for status surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecretDescription {
    pub configured: bool,
    pub redacted: Option<&'static str>,
}

impl SecretDescription {
    #[must_use]
    pub fn new(configured: bool) -> Self {
        Self {
            configured,
            redacted: configured.then_some("<redacted>"),
        }
    }
}
