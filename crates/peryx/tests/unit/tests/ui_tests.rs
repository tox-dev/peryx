use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_ha::{ArtifactPlacement, ArtifactPlacementStore, ArtifactSource};
use peryx_identity::{GrantScope, Role};
use tower::ServiceExt as _;

use crate::config::{AvailabilityConfig, Config, DcMember, DcMembership, DcRole, ReplicationConfig, SecretSource};
use crate::server::{build_state_with_plugins, router_for};
use crate::tests::support::plugins;

const ADMIN_PASSWORD: &str = "local password";

fn neutral_router() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let state = peryx_driver::AppState::new(
        peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        peryx_storage::blob::BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
    );
    let router = router_for(std::sync::Arc::new(state));
    (dir, router)
}

async fn seed_administrator(state: &peryx_driver::AppState) -> String {
    let user = state.serving.users.create("Alice").unwrap();
    state
        .serving
        .users
        .set_password(&user.id, ADMIN_PASSWORD)
        .await
        .unwrap();
    state
        .serving
        .authorization
        .grant(&user.id, Role::Administrator, GrantScope::Server)
        .unwrap();
    format!("Basic {}", STANDARD.encode(format!("Alice:{ADMIN_PASSWORD}")))
}

async fn get(router: &axum::Router, uri: &str) -> (StatusCode, String) {
    get_authorized(router, uri, "").await
}

async fn get_authorized(router: &axum::Router, uri: &str, authorization: &str) -> (StatusCode, String) {
    let mut request = Request::builder().uri(uri);
    if !authorization.is_empty() {
        request = request.header(header::AUTHORIZATION, authorization);
    }
    // Leptos SSR uses process-global arenas and can lose wakes during concurrent test renders.
    let _render = render_gate().lock().await;
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn render_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    GATE.get_or_init(tokio::sync::Mutex::default)
}

fn topology_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        availability: AvailabilityConfig::Dc(ReplicationConfig::Primary {
            source: "writer-a".to_owned(),
            token: SecretSource::Literal("test-token".to_owned()),
        }),
        dc_membership: Some(DcMembership {
            group: "east".to_owned(),
            members: vec![
                DcMember {
                    node: "writer-a".to_owned(),
                    dc: "east-1".to_owned(),
                    address: "writer-a.internal:8443".to_owned(),
                    role: DcRole::Writer,
                },
                DcMember {
                    node: "replica-b".to_owned(),
                    dc: "east-2".to_owned(),
                    address: "replica-b.internal:8443".to_owned(),
                    role: DcRole::Replica,
                },
            ],
        }),
        writer_identity: Some("writer-a".to_owned()),
        ..Config::with_plugins(&plugins())
    }
}

async fn topology_router() -> (tempfile::TempDir, axum::Router, String) {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state_with_plugins(&topology_config(&dir), &plugins()).unwrap();
    let authorization = seed_administrator(&state).await;
    (dir, router_for(state), authorization)
}

async fn placement_router() -> (tempfile::TempDir, axum::Router, String) {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state_with_plugins(&topology_config(&dir), &plugins()).unwrap();
    ArtifactPlacementStore::insert_artifact_placement(
        &state.serving.meta,
        "sha256:aaa",
        &ArtifactPlacement::record(ArtifactSource::Hosted, true),
    )
    .unwrap();
    ArtifactPlacementStore::insert_artifact_placement(
        &state.serving.meta,
        "sha256:bbb",
        &ArtifactPlacement::record(ArtifactSource::Proxy, false),
    )
    .unwrap();
    ArtifactPlacementStore::insert_artifact_placement(
        &state.serving.meta,
        "sha256:ccc",
        &ArtifactPlacement::record(ArtifactSource::Generated, false),
    )
    .unwrap();
    let authorization = seed_administrator(&state).await;
    (dir, router_for(state), authorization)
}

#[tokio::test]
async fn test_ui_header_marks_outbound_links_external() {
    let (_dir, router) = neutral_router();
    let (status, body) = get(&router, "/").await;
    assert_eq!(status, StatusCode::OK);
    for url in ["https://peryx.readthedocs.io/", "https://github.com/tox-dev/peryx"] {
        assert!(
            body.contains(&format!("href=\"{url}\" rel=\"external nofollow noopener noreferrer\"")),
            "{url} lacks the external relationship: {body}"
        );
    }
    assert!(body.contains("<a href=\"/admin/status\">"), "{body}");
    assert!(body.contains("<a href=\"/admin/policy-decisions\">"), "{body}");
}

#[tokio::test]
async fn test_ui_policy_decisions_renders_inert_credential_form() {
    let (_dir, router) = neutral_router();
    let (status, body) = get(&router, "/admin/policy-decisions").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Policy decisions"));
    assert!(body.contains("read-only"));
    assert!(body.contains("id=\"policy-password\" type=\"password\" autocomplete=\"off\""));
    assert!(body.contains("Enter credentials and search to load decisions."));
    assert!(!body.contains("id=\"policy-user\" value="));
    assert!(!body.contains("id=\"policy-password\" value="));
}

#[tokio::test]
async fn test_ui_admin_status_empty_state() {
    let (_dir, router) = neutral_router();
    let (status, body) = get(&router, "/admin/status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No indexes configured."));
    assert!(body.contains("No usage recorded yet."));
    assert!(body.contains("No writes recorded yet."));
}

#[tokio::test]
async fn test_ui_topology_renders_roster_for_administrator() {
    let (_dir, router, authorization) = topology_router().await;
    let (status, body) = get_authorized(&router, "/admin/topology", &authorization).await;
    assert_eq!(status, StatusCode::OK);
    for value in [
        "Availability topology",
        "read-only",
        "east",
        "writer-a",
        "replica-b",
        "east-1",
        "east-2",
        "this node",
        ">Live<",
        ">Unknown<",
        "writer-a.internal:8443",
        "replica-b.internal:8443",
        "id=\"topology-role\"",
    ] {
        assert!(body.contains(value), "{value}: {body}");
    }
}

#[tokio::test]
async fn test_ui_topology_withholds_private_fields_from_anonymous() {
    let (_dir, router, _authorization) = topology_router().await;
    let (status, body) = get(&router, "/admin/topology").await;
    assert_eq!(status, StatusCode::OK);
    for value in ["writer-a", "replica-b", "east-1", "Writer", "Restricted"] {
        assert!(body.contains(value), "{value}: {body}");
    }
    for value in [
        ">Live<",
        ">Unknown<",
        "writer-a.internal:8443",
        "replica-b.internal:8443",
    ] {
        assert!(!body.contains(value), "{value}: {body}");
    }
}

#[tokio::test]
async fn test_ui_topology_reports_standalone_node() {
    let (_dir, router) = neutral_router();
    let (status, body) = get(&router, "/admin/topology").await;
    assert_eq!(status, StatusCode::OK);
    for value in ["Availability topology", "Single node", "runs standalone"] {
        assert!(body.contains(value), "{value}: {body}");
    }
}

#[tokio::test]
async fn test_ui_placements_render_for_administrator() {
    let (_dir, router, authorization) = placement_router().await;
    let (status, body) = get_authorized(&router, "/admin/placements", &authorization).await;
    assert_eq!(status, StatusCode::OK);
    for value in [
        "Artifact placement health",
        "total artifacts",
        "sha256:aaa",
        "sha256:bbb",
        "sha256:ccc",
        "src-hosted",
        "src-proxy",
        "src-generated",
        "avail-local",
        "avail-remote-only",
        "avail-unavailable",
    ] {
        assert!(body.contains(value), "{value}: {body}");
    }
}

#[tokio::test]
async fn test_ui_placements_withhold_view_from_anonymous() {
    let (_dir, router, _authorization) = placement_router().await;
    let (status, body) = get(&router, "/admin/placements").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("do not have access"), "{body}");
    assert!(!body.contains("sha256:"), "{body}");
}

#[tokio::test]
async fn test_ui_unknown_route_falls_back() {
    let (_dir, router) = neutral_router();
    let (status, body) = get(&router, "/nosuchpage").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not found"));
}
