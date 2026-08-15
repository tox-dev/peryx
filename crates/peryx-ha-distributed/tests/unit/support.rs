use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use peryx_core::{Ecosystem, TopologyMember, TopologyMode};
use peryx_driver::serving::{AbsoluteProtocolDriver, IndexCredentialDriver};
use peryx_driver::state::{AppState, Index};
use peryx_identity::{Action, Denial, authorize_all, parse_basic};
use peryx_storage::meta::MetaStore;
use peryx_test_support::EcosystemDriverFixture;

use crate::{DcDurabilityMetrics, DistributedAnalyticsCompleteness, DistributedBlobDurability};

pub mod http_contract;

pub const EXTERNAL_USER: &str = "external";
pub const EXAMPLE_CREDENTIALS: ExampleCredentials = ExampleCredentials;
static EXAMPLE_DRIVER: EcosystemDriverFixture = EcosystemDriverFixture::new(
    Ecosystem::new("example"),
    peryx_driver::rate_limit::RouteClass::Artifact,
);

pub struct ExampleCredentials;
impl IndexCredentialDriver for ExampleCredentials {
    fn recognizes(&self, authorization: &str) -> bool {
        parse_basic(authorization).is_some_and(|credentials| credentials.user == EXTERNAL_USER)
    }

    fn authorize(&self, index: &Index, authorization: Option<&str>, action: Action, now: i64) -> Result<(), Denial> {
        if !authorization.is_some_and(|value| self.recognizes(value)) {
            return Err(Denial::Unauthenticated);
        }
        authorize_all(&index.acl.identify(authorization, now).principal, &index.acl, action)
    }
}

pub fn register_example_driver(state: &mut AppState) {
    EXAMPLE_DRIVER.clone().register(state);
    state.register_capabilities(|registry| {
        registry.register_index_credentials(Ecosystem::new("example"), Arc::new(EXAMPLE_CREDENTIALS));
    });
}

pub struct TestServer {
    pub url: String,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl TestServer {
    pub async fn start(router: Router) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        Self {
            url: format!("http://{address}/"),
            task: Some(tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
                listener, router,
            )))),
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub fn distributed_meta(path: impl AsRef<Path>) -> MetaStore {
    let meta = MetaStore::open(path).unwrap();
    meta.initialize_distributed_state().unwrap();
    meta
}

pub fn install_distributed_services(state: &mut AppState) {
    install_distributed_services_with_members(state, Vec::new());
}

pub fn install_distributed_services_with_members(state: &mut AppState, members: Vec<TopologyMember>) {
    let topology = peryx_core::TopologyConfig {
        mode: TopologyMode::Ha,
        group: Some("test".to_owned()),
        members,
        local_node: None,
    };
    let metrics = Arc::new(DcDurabilityMetrics::default());
    let durability = Arc::new(DistributedBlobDurability::new(
        topology.clone(),
        peryx_ha::DurabilityPolicy::Local,
        Vec::new(),
        Vec::new(),
        Duration::ZERO,
        metrics,
    ));
    state
        .install_distributed_availability(peryx_ha::AvailabilityStateInstall {
            role: peryx_core::NodeRole::Writer,
            topology,
            blobs: peryx_ha::BlobServices::new(None, durability),
            analytics: Arc::new(DistributedAnalyticsCompleteness),
            capabilities: peryx_ha::AvailabilityCapabilities::default(),
            authority_drainer: None,
            operations: None,
        })
        .unwrap();
    state.register_http_routes(Arc::new(crate::DistributedHttpRoutes));
}

#[cfg(test)]
mod tests {
    use super::*;
    use peryx_driver::state::IndexKind;
    use peryx_identity::IndexAcl;

    #[test]
    fn example_driver_rejects_unknown_credentials_and_classifies_routes() {
        let index = Index {
            name: "example".to_owned(),
            route: "example".to_owned(),
            ecosystem: Ecosystem::new("example"),
            kind: IndexKind::Hosted { volatile: false },
            policy: peryx_policy::Policy::default(),
            acl: IndexAcl {
                anonymous_read: false,
                tokens: Vec::new(),
            },
        };

        assert_eq!(
            EXAMPLE_CREDENTIALS.authorize(&index, Some("invalid"), Action::Read, 0),
            Err(Denial::Unauthenticated)
        );
        assert_eq!(
            EXAMPLE_DRIVER.classify_route("/artifact"),
            peryx_driver::rate_limit::RouteClass::Artifact
        );
    }
}
