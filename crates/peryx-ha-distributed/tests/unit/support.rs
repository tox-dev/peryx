use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use peryx_core::{Ecosystem, TopologyMember, TopologyMode};
use peryx_driver::serving::{AbsoluteProtocolDriver, IndexCredentialDriver};
use peryx_driver::state::AppState;
use peryx_identity::parse_basic;
use peryx_storage::meta::MetaStore;
use peryx_test_support::EcosystemDriverFixture;

use crate::{DcDurabilityMetrics, DistributedAnalyticsCompleteness, DistributedBlobDurability};

pub struct RequestBlocker {
    signals: Mutex<Option<(tokio::sync::oneshot::Sender<()>, tokio::sync::oneshot::Sender<()>)>>,
}

impl RequestBlocker {
    pub fn new() -> (
        Self,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let (started, entered) = tokio::sync::oneshot::channel();
        let (cancelled, dropped) = tokio::sync::oneshot::channel();
        (
            Self {
                signals: Mutex::new(Some((started, cancelled))),
            },
            entered,
            dropped,
        )
    }

    pub fn wait<T>(&self) -> impl Future<Output = T> + Send + use<T> {
        let (started, cancelled) = self
            .signals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap();
        let _ = started.send(());
        PendingRequest {
            cancelled: Some(cancelled),
            output: std::marker::PhantomData,
        }
    }
}

struct PendingRequest<T> {
    cancelled: Option<tokio::sync::oneshot::Sender<()>>,
    output: std::marker::PhantomData<fn() -> T>,
}

impl<T> Future for PendingRequest<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}

impl<T> Drop for PendingRequest<T> {
    fn drop(&mut self) {
        if let Some(signal) = self.cancelled.take() {
            let _ = signal.send(());
        }
    }
}

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
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
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
    install_distributed_services_with_capabilities(state, members, peryx_ha::AvailabilityCapabilities::default());
}

pub fn install_distributed_services_with_capabilities(
    state: &mut AppState,
    members: Vec<TopologyMember>,
    capabilities: peryx_ha::AvailabilityCapabilities,
) {
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
            capabilities,
            authority_drainer: None,
            operations: None,
        })
        .unwrap();
    state.register_http_routes(Arc::new(crate::DistributedHttpRoutes));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_driver_recognizes_its_own_credentials_and_classifies_routes() {
        let credential =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("{EXTERNAL_USER}:s"));

        assert!(EXAMPLE_CREDENTIALS.recognizes(&format!("Basic {credential}")));
        assert!(!EXAMPLE_CREDENTIALS.recognizes("invalid"));
        assert_eq!(
            EXAMPLE_DRIVER.classify_route("/artifact"),
            peryx_driver::rate_limit::RouteClass::Artifact
        );
    }
}
