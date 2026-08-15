use peryx_core::Ecosystem;
use peryx_identity::IndexAcl;
use peryx_policy::Policy;
use peryx_upstream::UpstreamClient;

use super::{Index, IndexKind};

fn index(kind: IndexKind) -> Index {
    Index {
        name: "index".to_owned(),
        route: "index".to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind,
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }
}

#[test]
fn test_proxy_client_selects_only_online_cached_indexes() {
    let online = index(IndexKind::Cached {
        client: UpstreamClient::new("https://example.invalid/").unwrap(),
        offline: false,
    });
    let offline = index(IndexKind::Cached {
        client: UpstreamClient::new("https://example.invalid/").unwrap(),
        offline: true,
    });
    let hosted = index(IndexKind::Hosted { volatile: false });

    assert!(online.proxy_client().is_some());
    assert!(offline.proxy_client().is_none());
    assert!(hosted.proxy_client().is_none());
}
