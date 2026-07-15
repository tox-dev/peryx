//! Human-facing descriptions of configured indexes, shared by `/+status` and the web UI.

use peryx_identity::Action;
use peryx_index::{Index, IndexKind, shadow_order};

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
    let (layers, precedence, uploads, volatile_deletes, upload_to) = match &index.kind {
        IndexKind::Cached { .. } => (Vec::new(), Vec::new(), false, false, None),
        IndexKind::Hosted { .. } => (
            Vec::new(),
            Vec::new(),
            writable(index),
            writable(index) && volatile(index),
            None,
        ),
        IndexKind::Virtual { layers, upload } => {
            let names = layers.iter().map(|&pos| indexes[pos].name.clone()).collect();
            let precedence = shadow_order(indexes, layers)
                .into_iter()
                .map(|pos| MemberDescription {
                    name: indexes[pos].name.clone(),
                    role: kind_str(&indexes[pos].kind),
                })
                .collect();
            let target = upload.map(|pos| &indexes[pos]);
            let uploads = target.is_some_and(writable);
            let volatile_deletes = target.is_some_and(|index| writable(index) && volatile(index));
            let upload_to = target.map(|index| index.name.clone());
            (names, precedence, uploads, volatile_deletes, upload_to)
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
        kind: kind_str(&index.kind),
        layers,
        precedence,
        uploads,
        volatile_deletes,
        upload_to,
        upstream,
        hosted,
    }
}

/// The stable role name of an index kind, shared by the top-level `kind` and each virtual member's
/// `role`, so the two never drift.
const fn kind_str(kind: &IndexKind) -> &'static str {
    match kind {
        IndexKind::Cached { .. } => "cached",
        IndexKind::Hosted { .. } => "hosted",
        IndexKind::Virtual { .. } => "virtual",
    }
}

/// Whether the index has a credential that may upload: what a status surface means by "uploads are
/// enabled", the `upload_token`-is-set question widened to an ACL that may hold several tokens.
fn writable(index: &Index) -> bool {
    index.acl.grants_to_anyone(Action::Write)
}

/// Whether the index is a hosted store that permits delete and overwrite.
const fn volatile(index: &Index) -> bool {
    matches!(index.kind, IndexKind::Hosted { volatile: true })
}

/// A configured index as presented to humans: on the dashboard, in `/+status`, and in discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDescription {
    pub name: String,
    pub route: String,
    pub ecosystem: &'static str,
    pub kind: &'static str,
    /// A virtual index's members named in the operator's configured order; empty otherwise.
    pub layers: Vec<String>,
    /// A virtual index's members in the order requests actually merge them — cached members forced
    /// last whatever the configured `layers` order, so an earlier entry shadows a later one. Each
    /// carries its role, distinguishing a local hosted source from a proxied upstream. Empty for a
    /// non-virtual index.
    pub precedence: Vec<MemberDescription>,
    pub uploads: bool,
    pub volatile_deletes: bool,
    /// For a virtual index: the layer uploads land in, whether or not a token currently enables them.
    pub upload_to: Option<String>,
    pub upstream: Option<UpstreamDescription>,
    pub hosted: Option<HostedDescription>,
}

/// One member of a virtual index as a status surface presents it: its name and role, positioned by
/// [`IndexDescription::precedence`] so its rank shows which member shadows which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberDescription {
    pub name: String,
    pub role: &'static str,
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

#[cfg(test)]
mod tests {
    use super::{MemberDescription, describe_index};
    use peryx_core::Ecosystem;
    use peryx_identity::IndexAcl;
    use peryx_index::{Index, IndexKind};
    use peryx_policy::Policy;
    use peryx_upstream::UpstreamClient;

    fn index(name: &str, kind: IndexKind, acl: IndexAcl) -> Index {
        Index {
            name: name.to_owned(),
            route: name.to_owned(),
            ecosystem: Ecosystem::Pypi,
            kind,
            policy: Policy::default(),
            acl,
        }
    }

    fn cached() -> IndexKind {
        IndexKind::Cached {
            client: UpstreamClient::new("http://example.invalid/simple/").unwrap(),
            offline: false,
        }
    }

    fn member(name: &str, role: &'static str) -> MemberDescription {
        MemberDescription {
            name: name.to_owned(),
            role,
        }
    }

    #[test]
    fn test_cached_index_names_its_role_and_lists_no_members() {
        let indexes = vec![index("pypi", cached(), IndexAcl::default())];
        let described = describe_index(&indexes, 0);
        assert_eq!(described.kind, "cached");
        assert!(described.layers.is_empty());
        assert!(described.precedence.is_empty());
    }

    #[test]
    fn test_hosted_index_reports_volatile_deletes_when_writable_and_volatile() {
        let indexes = vec![index(
            "store",
            IndexKind::Hosted { volatile: true },
            IndexAcl::upload_token("s"),
        )];
        let described = describe_index(&indexes, 0);
        assert_eq!(described.kind, "hosted");
        assert!(described.volatile_deletes);
        assert!(described.precedence.is_empty());
    }

    #[test]
    fn test_virtual_precedence_forces_cached_members_last_and_tags_roles() {
        let indexes = vec![
            index("pypi", cached(), IndexAcl::default()),
            index("local", IndexKind::Hosted { volatile: false }, IndexAcl::default()),
            index(
                "mix",
                IndexKind::Virtual {
                    layers: vec![0, 1],
                    upload: None,
                },
                IndexAcl::default(),
            ),
        ];
        let described = describe_index(&indexes, 2);
        assert_eq!(described.layers, vec!["pypi".to_owned(), "local".to_owned()]);
        assert_eq!(
            described.precedence,
            vec![member("local", "hosted"), member("pypi", "cached")]
        );
    }

    #[test]
    fn test_virtual_upload_target_drives_uploads_and_volatile_deletes() {
        let indexes = vec![
            index(
                "store",
                IndexKind::Hosted { volatile: true },
                IndexAcl::upload_token("s"),
            ),
            index(
                "v",
                IndexKind::Virtual {
                    layers: vec![0],
                    upload: Some(0),
                },
                IndexAcl::default(),
            ),
        ];
        let described = describe_index(&indexes, 1);
        assert!(described.uploads);
        assert!(described.volatile_deletes);
        assert_eq!(described.upload_to.as_deref(), Some("store"));
        assert_eq!(described.precedence, vec![member("store", "hosted")]);
    }
}
