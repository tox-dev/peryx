use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get as route_get;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_core::{Ecosystem, Role as IndexRole};
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::serving::{AbsoluteProtocolDriver, EcosystemDriver, MetricsDriver, ProtocolDriver};
use peryx_driver::{AppState, HttpRoutes, Index, IndexKind, PrometheusSource, ServingState};
use peryx_events::metrics::{MetricFamily, MetricKind, Observation};
use peryx_identity::{GrantScope, IndexAcl, PasswordPolicy, Role};
use peryx_policy::Policy;
use tower::ServiceExt as _;

const USER_PASSWORD: &str = "local password";
const EXTENSION: MetricFamily = MetricFamily {
    key: "extension",
    prom_name: "peryx_example_extension_total",
    help: "Example extension events.",
    ui_label: "Extension events",
    roles: &[IndexRole::Hosted, IndexRole::Cached],
    json_name: Some("extension_events"),
    kind: MetricKind::Counter,
};
const ECOSYSTEM: MetricFamily = MetricFamily {
    key: "ecosystem",
    prom_name: "peryx_example_ecosystem_total",
    help: "Example ecosystem events.",
    ui_label: "Ecosystem events",
    roles: &[IndexRole::Hosted],
    json_name: None,
    kind: MetricKind::Gauge,
};

struct Driver {
    ecosystem: &'static str,
}

impl MetricsDriver for Driver {
    fn metric_families(&self) -> &'static [MetricFamily] {
        &[EXTENSION, ECOSYSTEM]
    }
}

impl EcosystemDriver for Driver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new(self.ecosystem)
    }
}

#[async_trait::async_trait]
impl AbsoluteProtocolDriver for Driver {
    fn prefixes(&self) -> &'static [&'static str] {
        &["/+usage-fixture"]
    }

    fn classify_route(&self, _path: &str) -> RouteClass {
        RouteClass::Artifact
    }

    async fn serve(&self, _state: Arc<ServingState>, _request: Request) -> Response {
        StatusCode::IM_A_TEAPOT.into_response()
    }
}

struct ProcessMetrics;

impl PrometheusSource for ProcessMetrics {
    fn write_metrics(&self, body: &mut String) {
        body.push_str("peryx_process_fixture 1\n");
    }
}

struct OwnerRoutes;

impl HttpRoutes for OwnerRoutes {
    fn routes(&self) -> axum::Router<Arc<AppState>> {
        axum::Router::new().route("/+owner", route_get(|| async { "owner" }))
    }
}

async fn app() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let users = peryx_driver::users::UserService::with_password_settings(
        meta.clone(),
        PasswordPolicy::new(8, 1, 1).unwrap(),
        2,
    );
    let user = users.create("Olivia").unwrap();
    users.set_password(&user.id, USER_PASSWORD).await.unwrap();
    peryx_driver::authz::AuthorizationService::new(meta.clone())
        .grant(&user.id, Role::Operator, GrantScope::Server)
        .unwrap();
    let mut state = AppState::new(
        meta,
        peryx_storage::blob::BlobStore::new(dir.path().join("blobs")),
        60,
        vec![
            index("hosted-a", "example", IndexKind::Hosted { volatile: false }),
            index("hosted-b", "example", IndexKind::Hosted { volatile: false }),
            index(
                "cached",
                "example",
                IndexKind::Cached {
                    client: peryx_upstream::UpstreamClient::new("https://upstream.example/artifacts/").unwrap(),
                    offline: false,
                },
            ),
            index(
                "virtual",
                "example",
                IndexKind::Virtual {
                    layers: vec![0],
                    write_target: None,
                },
            ),
            index("bare", "bare", IndexKind::Hosted { volatile: false }),
            index("missing", "missing", IndexKind::Hosted { volatile: false }),
        ],
    );
    Arc::get_mut(&mut state.serving).unwrap().users = users;
    let driver = Arc::new(Driver { ecosystem: "example" });
    let bare_driver = Arc::new(Driver { ecosystem: "bare" });
    state.register_capabilities(|registrar| {
        registrar.register_metrics(Ecosystem::new("example"), driver.clone());
        registrar.register_metrics(Ecosystem::new("bare"), bare_driver.clone());
    });
    state
        .register_protocol(ProtocolDriver::Absolute(driver), peryx_search::default_indexer())
        .unwrap();
    state.register_driver(bare_driver);
    state.register_prometheus(Arc::new(ProcessMetrics));
    state.register_http_routes(Arc::new(OwnerRoutes));
    state.serving.metrics.increment("hosted-a", &EXTENSION, 3);
    state.serving.metrics.increment("hosted-b", &EXTENSION, 4);
    state.serving.metrics.record(Observation::Write {
        repository: "hosted-a".to_owned(),
        resource: "resource".to_owned(),
    });
    state.serving.metrics.record(Observation::Page {
        repository: "hosted-a".to_owned(),
        resource: "resource".to_owned(),
    });
    for _ in 0..5 {
        state.serving.metrics.record(Observation::Ecosystem {
            repository: "hosted-a".to_owned(),
            resource: "resource".to_owned(),
            artifact: None,
            family: ECOSYSTEM.key,
        });
    }
    state.serving.metrics.flush().unwrap();
    (dir, Arc::new(state))
}

fn index(route: &str, ecosystem: &'static str, kind: IndexKind) -> Index {
    Index {
        name: route.to_owned(),
        route: route.to_owned(),
        ecosystem: Ecosystem::new(ecosystem),
        kind,
        policy: Policy::default(),
        acl: IndexAcl {
            anonymous_read: false,
            tokens: Vec::new(),
        },
    }
}

async fn get(state: &Arc<AppState>, uri: &str, authenticated: bool) -> (StatusCode, String) {
    let mut request = Request::builder().uri(uri);
    if authenticated {
        request = request.header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(format!("Olivia:{USER_PASSWORD}"))),
        );
    }
    let response = crate::router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn test_metrics_renders_neutral_extension_rate_limit_and_process_families() {
    let (_dir, state) = app().await;
    let (status, body) = get(&state, "/metrics", false).await;

    assert_eq!(status, StatusCode::OK);
    for sample in [
        "peryx_requests_total 0",
        "peryx_artifacts_uploaded_total{ecosystem=\"example\",role=\"hosted\"} 1",
        "peryx_example_extension_total{ecosystem=\"example\",role=\"hosted\"} 7",
        "peryx_example_extension_total{ecosystem=\"example\",role=\"cached\"} 0",
        "peryx_example_ecosystem_total{ecosystem=\"example\",role=\"hosted\"} 5",
        "peryx_upstream_rate_limit_denied_total 0",
        "peryx_process_fixture 1",
    ] {
        assert!(body.contains(sample), "missing {sample}: {body}");
    }
}

#[test]
fn test_metrics_driver_exposes_owned_families() {
    let driver = Driver { ecosystem: "example" };

    assert_eq!(driver.ecosystem(), Ecosystem::new("example"));
    assert_eq!(driver.classify_route("/+usage-fixture"), RouteClass::Artifact);
    assert_eq!(
        driver
            .metric_families()
            .iter()
            .map(|family| family.key)
            .collect::<Vec<_>>(),
        ["extension", "ecosystem"]
    );
}

#[tokio::test]
async fn test_registered_absolute_driver_serves_its_prefix() {
    let (_dir, state) = app().await;
    let response = crate::router(state)
        .oneshot(Request::builder().uri("/+usage-fixture").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
}

#[tokio::test]
async fn test_status_renders_ecosystem_summaries_and_metric_families() {
    let (_dir, state) = app().await;
    let (status, body) = get(&state, "/+status", true).await;
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["metric_families"], expected_metric_families());
    assert_eq!(body["by_ecosystem"][1]["families"]["ecosystem"], 5);
}

#[tokio::test]
async fn test_family_descriptors_are_stable_and_ordered_by_owner_and_key() {
    let (_dir, state) = app().await;

    assert_eq!(
        (
            serde_json::json!(crate::handlers::family_descriptors(&state)),
            serde_json::json!(crate::handlers::family_descriptors(&state)),
        ),
        (expected_metric_families(), expected_metric_families())
    );
}

fn expected_metric_families() -> serde_json::Value {
    serde_json::json!([
        {"ecosystem": "bare", "key": "ecosystem", "label": "Ecosystem events", "roles": ["hosted"]},
        {"ecosystem": "bare", "key": "extension", "label": "Extension events", "roles": ["hosted", "cached"]},
        {"ecosystem": "example", "key": "ecosystem", "label": "Ecosystem events", "roles": ["hosted"]},
        {"ecosystem": "example", "key": "extension", "label": "Extension events", "roles": ["hosted", "cached"]},
    ])
}

#[tokio::test]
async fn test_stats_renders_extension_fields_at_each_drill_depth() {
    let (_dir, state) = app().await;
    let mut values = BTreeSet::new();
    for uri in [
        "/+stats",
        "/+stats?repository=hosted-a",
        "/+stats?repository=hosted-a&resource=resource",
    ] {
        let (status, body) = get(&state, uri, true).await;
        assert_eq!(status, StatusCode::OK);
        values.insert(body);
    }
    assert_eq!(values.len(), 3);
    assert!(values.iter().all(|body| body.contains("extension_events")));

    let (status, body) = get(&state, "/+stats?repository=cached", true).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("extension_events"), "{body}");
}

#[tokio::test]
async fn test_registered_http_routes_are_merged() {
    let (_dir, state) = app().await;
    assert_eq!(
        get(&state, "/+owner", false).await,
        (StatusCode::OK, "owner".to_owned())
    );
}

#[tokio::test]
async fn test_readiness_reports_an_unhealthy_blob_store() {
    let dir = tempfile::tempdir().unwrap();
    let blob_path = dir.path().join("blobs");
    std::fs::write(&blob_path, b"not a directory").unwrap();
    let state = Arc::new(AppState::new(
        peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        peryx_storage::blob::BlobStore::new(blob_path),
        60,
        Vec::new(),
    ));

    assert_eq!(get(&state, "/+ready", false).await.0, StatusCode::SERVICE_UNAVAILABLE);
}
