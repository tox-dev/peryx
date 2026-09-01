use async_trait::async_trait;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::IntoResponse as _;
use peryx_core::Ecosystem;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use std::sync::Arc;

use peryx_driver::rate_limit::RouteClass;
use peryx_driver::serving::{AbsoluteProtocolDriver, EcosystemDriver, IndexedProtocolDriver, ProtocolDriver};
use peryx_driver::state::{AppState, ServingState};

struct Driver;

impl EcosystemDriver for Driver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }
}

fn state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, AppState::new(meta, blobs, 60, Vec::new()))
}

#[tokio::test]
async fn protocol_driver_exposes_only_indexed_operations() {
    let (_dir, state) = state();
    let serving: Arc<ServingState> = state.serving.clone();
    let protocol = ProtocolDriver::Indexed(Arc::new(Driver));
    assert!(protocol.absolute().is_none());
    assert_eq!(protocol.driver().ecosystem(), Ecosystem::new("example"));
    assert_eq!(protocol.driver_arc().ecosystem(), Ecosystem::new("example"));
    let driver = protocol.indexed().unwrap();
    assert_eq!(driver.ecosystem(), Ecosystem::new("example"));
    assert_eq!(driver.classify_route("/artifact"), RouteClass::Artifact);
    assert_eq!(
        driver
            .get(
                serving.clone(),
                0,
                String::new(),
                Uri::from_static("/"),
                HeaderMap::new(),
                Method::GET
            )
            .await
            .status(),
        204
    );
    assert_eq!(
        driver
            .post(
                serving,
                String::new(),
                Request::builder().body(Body::from("post body")).unwrap(),
            )
            .await
            .status(),
        204
    );
    assert_eq!(
        driver
            .put(
                state.serving.clone(),
                Request::builder()
                    .method(Method::PUT)
                    .uri("/artifact")
                    .body(Body::from("put body"))
                    .unwrap(),
            )
            .await
            .status(),
        204
    );
    assert_eq!(
        driver
            .delete(state.serving, axum::extract::Request::new(Body::empty()))
            .await
            .status(),
        204
    );
}

#[async_trait]
impl IndexedProtocolDriver for Driver {
    fn classify_route(&self, _path: &str) -> RouteClass {
        RouteClass::Artifact
    }

    async fn get(
        &self,
        _: Arc<ServingState>,
        _: usize,
        _: String,
        _: Uri,
        _: HeaderMap,
        _: Method,
    ) -> axum::response::Response {
        axum::http::StatusCode::NO_CONTENT.into_response()
    }
    async fn post(&self, _: Arc<ServingState>, _: String, _: Request) -> axum::response::Response {
        axum::http::StatusCode::NO_CONTENT.into_response()
    }
    async fn put(&self, _: Arc<ServingState>, _: Request) -> axum::response::Response {
        axum::http::StatusCode::NO_CONTENT.into_response()
    }
    async fn delete(&self, _: Arc<ServingState>, _: axum::extract::Request) -> axum::response::Response {
        axum::http::StatusCode::NO_CONTENT.into_response()
    }
}

struct AbsoluteDriver;

impl EcosystemDriver for AbsoluteDriver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("absolute")
    }
}

#[async_trait]
impl AbsoluteProtocolDriver for AbsoluteDriver {
    fn prefixes(&self) -> &'static [&'static str] {
        &["/wire/"]
    }
    fn classify_route(&self, _: &str) -> RouteClass {
        RouteClass::Artifact
    }
    async fn serve(&self, _: Arc<ServingState>, _: Request) -> axum::response::Response {
        axum::http::StatusCode::ACCEPTED.into_response()
    }
}

#[tokio::test]
async fn protocol_driver_exposes_only_absolute_operations() {
    let (_dir, state) = state();
    let protocol = ProtocolDriver::Absolute(Arc::new(AbsoluteDriver));
    assert!(protocol.indexed().is_none());
    let driver = protocol.absolute().unwrap();
    assert_eq!(
        (driver.prefixes(), protocol.ecosystem(), protocol.driver().ecosystem()),
        (&["/wire/"][..], Ecosystem::new("absolute"), Ecosystem::new("absolute"))
    );
    assert_eq!(driver.classify_route("/wire/artifact"), RouteClass::Artifact);
    assert_eq!(
        driver.serve(state.serving, Request::new(Body::empty())).await.status(),
        202
    );
}
