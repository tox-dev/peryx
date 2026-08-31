use std::sync::Arc;

use peryx_driver::AppState;
use peryx_driver::rate_limit::RateLimitConfig;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyConfig};
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;
use peryx_upstream::{NamedUpstream, UpstreamClient, UpstreamRouter};

pub struct StateFixture {
    pub dir: tempfile::TempDir,
    pub state: Arc<AppState>,
}

pub fn state(indexes: Vec<Index>) -> StateFixture {
    limited_state(indexes, None)
}

pub fn limited_state(indexes: Vec<Index>, upstream_concurrency: Option<usize>) -> StateFixture {
    let dir = tempfile::tempdir().unwrap();
    let routes = indexes
        .iter()
        .filter_map(|index| match &index.kind {
            IndexKind::Cached { client, .. } => Some((index.name.clone(), client.clone())),
            IndexKind::Hosted { .. } | IndexKind::Virtual { .. } => None,
        })
        .collect::<Vec<_>>();
    let limits = upstream_concurrency
        .map(|limit| {
            indexes
                .iter()
                .map(|index| (index.name.clone(), limit))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut state = AppState::with_rate_limits(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(dir.path().join("blobs")),
        60,
        indexes,
        RateLimitConfig::default(),
        limits,
    );
    let upstream_routes = &mut Arc::get_mut(&mut state.serving).unwrap().upstream_routes;
    for (name, client) in routes {
        upstream_routes.insert(
            name,
            UpstreamRouter::new(vec![NamedUpstream::new("primary", client)]).unwrap(),
        );
    }
    crate::tests::install(&mut state);
    StateFixture {
        dir,
        state: Arc::new(state),
    }
}

pub fn cached_index(base: &str, offline: bool) -> Index {
    Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Cached {
            client: UpstreamClient::new(base).unwrap(),
            offline,
        },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }
}

pub fn hosted_index(name: &str) -> Index {
    Index {
        name: name.to_owned(),
        route: name.to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }
}

pub fn virtual_index(name: &str, layers: Vec<usize>, policy: Policy) -> Index {
    Index {
        name: name.to_owned(),
        route: name.to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Virtual {
            layers,
            write_target: None,
        },
        policy,
        acl: IndexAcl::default(),
    }
}

pub fn policy(configure: impl FnOnce(&mut PolicyConfig, &mut crate::policy::PypiPolicyConfig)) -> Policy {
    let mut neutral = PolicyConfig::default();
    let mut pypi = crate::policy::PypiPolicyConfig::default();
    configure(&mut neutral, &mut pypi);
    Policy::compile(&neutral, crate::normalize_name)
        .with_capabilities(crate::policy::compile_capabilities(&pypi).unwrap())
}
