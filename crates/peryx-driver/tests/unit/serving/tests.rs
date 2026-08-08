use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{FromRequest, Multipart, Request};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use peryx_core::{DefaultIndex, Ecosystem};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use super::{CompiledEcosystemSettings, EcosystemCapability, EcosystemDriver, EcosystemPlugin, RouteMount};
use crate::rate_limit::RouteClass;
use crate::state::{AppState, IndexDescription, ServingState};

struct Driver;

#[async_trait]
impl EcosystemDriver for Driver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }

    fn classify_route(&self, _path: &str) -> RouteClass {
        RouteClass::Artifact
    }

    fn discover_index(&self, _index: IndexDescription, _base: Option<&crate::discovery::BaseUrl>) -> serde_json::Value {
        serde_json::Value::Null
    }
}

struct Plugin {
    installs: AtomicUsize,
}

impl EcosystemPlugin for Plugin {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }

    fn default_indexes(&self) -> &'static [DefaultIndex] {
        &[]
    }

    fn driver(&self) -> Arc<dyn EcosystemDriver> {
        Arc::new(Driver)
    }

    fn compile_index_settings(
        &self,
        _name: &str,
        _settings: &toml::Table,
    ) -> Result<Option<CompiledEcosystemSettings>, String> {
        Ok(None)
    }

    fn install(&self, _state: &mut AppState, _settings: &[(&str, &CompiledEcosystemSettings)]) -> Result<(), String> {
        self.installs.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn openapi_paths(&self, paths: utoipa::openapi::PathsBuilder) -> utoipa::openapi::PathsBuilder {
        paths
    }

    fn snippet_text(
        &self,
        _base: &crate::discovery::BaseUrl,
        _route: &str,
        _uploads: bool,
        _format: &str,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }
}

fn state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, AppState::new(meta, blobs, 60, Vec::new()))
}

fn description() -> IndexDescription {
    IndexDescription {
        name: "packages".to_owned(),
        route: "packages".to_owned(),
        ecosystem: "example",
        kind: "hosted",
        layers: Vec::new(),
        precedence: Vec::new(),
        uploads: false,
        volatile_deletes: false,
        upload_to: None,
        upstream: None,
        hosted: None,
    }
}

#[test]
fn test_driver_defaults_are_neutral() {
    let driver = Driver;

    assert_eq!(driver.ecosystem(), Ecosystem::new("example"));
    assert_eq!(driver.classify_route("/artifact"), RouteClass::Artifact);
    assert_eq!(driver.discover_index(description(), None), serde_json::Value::Null);
    assert_eq!(driver.mount(), RouteMount::Indexed);
    assert_eq!(driver.client_endpoint("team/packages"), "/team/packages/");
    assert!(driver.capabilities().jobs.is_none());
}

#[test]
fn test_plugin_defaults_delegate_install_and_reject_capabilities() {
    let (_dir, mut state) = state();
    let plugin = Plugin {
        installs: AtomicUsize::new(0),
    };

    assert_eq!(plugin.ecosystem(), Ecosystem::new("example"));
    assert!(plugin.default_indexes().is_empty());
    assert_eq!(plugin.driver().ecosystem(), Ecosystem::new("example"));
    assert!(
        plugin
            .compile_index_settings("packages", &toml::Table::new())
            .unwrap()
            .is_none()
    );
    assert!(
        plugin
            .openapi_paths(utoipa::openapi::PathsBuilder::new())
            .build()
            .paths
            .is_empty()
    );
    let base = crate::discovery::BaseUrl::parse("https://packages.example/").unwrap();
    assert!(plugin.snippet_text(&base, "packages", false, "text").unwrap().is_none());
    plugin.install_distributed(&mut state, &[]).unwrap();

    assert_eq!(plugin.installs.load(Ordering::Relaxed), 1);
    assert!(!plugin.supports(EcosystemCapability::CatalogSync));
    assert!(!plugin.supports(EcosystemCapability::TrustedPublishing));
}

#[test]
fn test_driver_default_principal_is_anonymous() {
    let (_dir, state) = state();

    assert_eq!(
        Driver.rate_limit_principal(&state.serving, None, &HeaderMap::new()),
        peryx_identity::Principal::Anonymous
    );
}

fn assert_wrong_mount(response: &axum::response::Response) {
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_driver_default_request_handlers_fail_closed() {
    let (_dir, state) = state();
    let serving: Arc<ServingState> = state.serving.clone();
    assert_wrong_mount(&Driver.serve(serving.clone(), Request::new(Body::empty())).await);
    assert_wrong_mount(
        &Driver
            .get(
                serving.clone(),
                0,
                String::new(),
                Uri::from_static("/"),
                HeaderMap::new(),
                Method::GET,
            )
            .await,
    );
    assert_wrong_mount(
        &Driver
            .put(serving.clone(), Uri::from_static("/"), HeaderMap::new())
            .await,
    );
    assert_wrong_mount(
        &Driver
            .delete(serving.clone(), Uri::from_static("/"), HeaderMap::new())
            .await,
    );

    let request = Request::builder()
        .header(header::CONTENT_TYPE, "multipart/form-data; boundary=X")
        .body(Body::from("--X--\r\n"))
        .unwrap();
    let multipart = Multipart::from_request(request, &()).await.unwrap();
    assert_wrong_mount(&Driver.post(serving, String::new(), HeaderMap::new(), multipart).await);
}
