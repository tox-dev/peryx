//! The neutral `GET /+ui/…` browse endpoints: each resolves an index route to its driver and returns
//! the driver's view model as JSON, answering `404` for an unknown route or absent item and `500` when
//! the driver fails.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt as _;

use peryx_core::{Ecosystem, UiManifest, UiMember, UiMemberChunk, UiProjectView};
use peryx_driver::state::{AppState, Index, IndexKind, ServingState};

/// A driver whose browse methods answer by their inputs, so one instance exercises every outcome the
/// handlers branch on: a value, an absent item, and a read error.
struct UiStub;

#[async_trait::async_trait]
impl peryx_driver::serving::EcosystemDriver for UiStub {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::Pypi
    }

    fn classify_route(&self, _path: &str) -> peryx_driver::rate_limit::RouteClass {
        peryx_driver::rate_limit::RouteClass::Listing
    }

    fn discover_index(
        &self,
        index: peryx_driver::state::IndexDescription,
        _base: Option<&peryx_driver::discovery::BaseUrl>,
    ) -> serde_json::Value {
        peryx_driver::discovery::minimal_entry(&index)
    }

    fn project_names(&self, _state: &ServingState, position: usize) -> Result<Vec<String>, String> {
        if position == 0 {
            Ok(vec!["flask".to_owned()])
        } else {
            Err("index unreadable".to_owned())
        }
    }

    async fn browse_project(
        &self,
        _state: Arc<ServingState>,
        _position: usize,
        project: String,
    ) -> Result<Option<UiProjectView>, String> {
        match project.as_str() {
            "boom" => Err("project unreadable".to_owned()),
            "missing" => Ok(None),
            _ => Ok(Some(UiProjectView::References {
                names: vec!["1.0".to_owned()],
            })),
        }
    }

    async fn manifest_view(
        &self,
        _state: Arc<ServingState>,
        _position: usize,
        _project: String,
        reference: String,
    ) -> Result<Option<UiManifest>, String> {
        match reference.as_str() {
            "boom" => Err("manifest unreadable".to_owned()),
            "absent" => Ok(None),
            _ => Ok(Some(UiManifest {
                media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
                ..UiManifest::default()
            })),
        }
    }

    async fn artifact_members(
        &self,
        _state: Arc<ServingState>,
        _position: usize,
        project: String,
        _digest: String,
    ) -> Result<Vec<UiMember>, String> {
        if project == "boom" {
            return Err("layer unreadable".to_owned());
        }
        Ok(vec![UiMember {
            path: "usr/bin/app".to_owned(),
            size: 42,
            kind: "file".to_owned(),
            previewable: true,
        }])
    }

    async fn artifact_member_chunk(
        &self,
        _state: Arc<ServingState>,
        _position: usize,
        project: String,
        _digest: String,
        _member: String,
        _offset: u64,
    ) -> Result<UiMemberChunk, String> {
        if project == "boom" {
            return Err("member unreadable".to_owned());
        }
        Ok(UiMemberChunk {
            text: "hello".to_owned(),
            ..UiMemberChunk::default()
        })
    }
}

fn index(route: &str, ecosystem: Ecosystem) -> Index {
    Index {
        name: route.to_owned(),
        route: route.to_owned(),
        ecosystem,
        kind: IndexKind::Hosted {
            upload_token: None,
            volatile: false,
        },
        policy: peryx_policy::Policy::default(),
    }
}

/// Indexes `good` and `bad` are served by the stub; `orphan` is configured for an ecosystem with no
/// driver, so it resolves to an index but not a driver.
fn ui_app() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let indexes = vec![
        index("good", Ecosystem::Pypi),
        index("bad", Ecosystem::Pypi),
        index("orphan", Ecosystem::Oci),
    ];
    let mut state = AppState::new(meta, blobs, 60, indexes);
    state.register_ecosystem(Arc::new(UiStub), Arc::new(peryx_search::EmptyIndexer));
    (dir, crate::router(Arc::new(state)))
}

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let document = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, document)
}

async fn get_status(app: &axum::Router, uri: &str) -> StatusCode {
    app.clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn test_status_reads_the_client_endpoint_from_a_registered_driver() {
    // `good`/`bad` have a driver, so their endpoint comes from `client_endpoint`; `orphan` has none,
    // so it falls back to the bare route. One request exercises both arms.
    let (_dir, app) = ui_app();
    let (status, document) = get_json(&app, "/+status").await;
    assert_eq!(status, StatusCode::OK);
    let endpoints: std::collections::HashMap<&str, &str> = document["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|index| (index["route"].as_str().unwrap(), index["endpoint"].as_str().unwrap()))
        .collect();
    assert_eq!(endpoints["good"], "/good/");
    assert_eq!(endpoints["orphan"], "/orphan/");
}

#[tokio::test]
async fn test_ui_projects_returns_the_driver_project_names() {
    let (_dir, app) = ui_app();
    let (status, document) = get_json(&app, "/+ui/projects?index=good").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document, serde_json::json!(["flask"]));
}

#[tokio::test]
async fn test_ui_projects_reports_a_driver_error_as_500() {
    let (_dir, app) = ui_app();
    assert_eq!(
        get_status(&app, "/+ui/projects?index=bad").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn test_ui_projects_is_404_for_an_unknown_route() {
    let (_dir, app) = ui_app();
    assert_eq!(
        get_status(&app, "/+ui/projects?index=nope").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_ui_projects_is_404_when_the_route_has_no_driver() {
    let (_dir, app) = ui_app();
    assert_eq!(
        get_status(&app, "/+ui/projects?index=orphan").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_ui_project_returns_the_browse_view() {
    let (_dir, app) = ui_app();
    let (status, document) = get_json(&app, "/+ui/project?index=good&project=flask").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["kind"], "references");
    assert_eq!(document["names"], serde_json::json!(["1.0"]));
}

#[tokio::test]
async fn test_ui_project_is_404_when_absent() {
    let (_dir, app) = ui_app();
    assert_eq!(
        get_status(&app, "/+ui/project?index=good&project=missing").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_ui_project_reports_a_driver_error_as_500() {
    let (_dir, app) = ui_app();
    assert_eq!(
        get_status(&app, "/+ui/project?index=good&project=boom").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn test_ui_project_is_404_for_an_unknown_route() {
    let (_dir, app) = ui_app();
    assert_eq!(
        get_status(&app, "/+ui/project?index=nope&project=flask").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_ui_manifest_returns_the_manifest_view() {
    let (_dir, app) = ui_app();
    let (status, document) = get_json(&app, "/+ui/manifest?index=good&project=img&ref=1.0").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["media_type"], "application/vnd.oci.image.manifest.v1+json");
}

#[tokio::test]
async fn test_ui_manifest_is_404_when_the_reference_is_absent() {
    let (_dir, app) = ui_app();
    assert_eq!(
        get_status(&app, "/+ui/manifest?index=good&project=img&ref=absent").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_ui_manifest_reports_a_driver_error_as_500() {
    let (_dir, app) = ui_app();
    assert_eq!(
        get_status(&app, "/+ui/manifest?index=good&project=img&ref=boom").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn test_ui_manifest_is_404_for_an_unknown_route() {
    let (_dir, app) = ui_app();
    assert_eq!(
        get_status(&app, "/+ui/manifest?index=nope&project=img&ref=1.0").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_ui_members_lists_the_layer_members() {
    let (_dir, app) = ui_app();
    let (status, document) = get_json(&app, "/+ui/members?index=good&project=img&digest=sha256:a").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document[0]["path"], "usr/bin/app");
}

#[tokio::test]
async fn test_ui_members_reports_a_driver_error_as_500() {
    let (_dir, app) = ui_app();
    assert_eq!(
        get_status(&app, "/+ui/members?index=good&project=boom&digest=sha256:a").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn test_ui_members_is_404_for_an_unknown_route() {
    let (_dir, app) = ui_app();
    assert_eq!(
        get_status(&app, "/+ui/members?index=nope&project=img&digest=sha256:a").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_ui_member_returns_a_text_chunk() {
    let (_dir, app) = ui_app();
    let (status, document) = get_json(&app, "/+ui/member?index=good&project=img&digest=sha256:a&member=f").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["text"], "hello");
}

#[tokio::test]
async fn test_ui_member_reports_a_driver_error_as_500() {
    let (_dir, app) = ui_app();
    assert_eq!(
        get_status(&app, "/+ui/member?index=good&project=boom&digest=sha256:a&member=f").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn test_ui_member_is_404_for_an_unknown_route() {
    let (_dir, app) = ui_app();
    assert_eq!(
        get_status(&app, "/+ui/member?index=nope&project=img&digest=sha256:a&member=f").await,
        StatusCode::NOT_FOUND
    );
}
