use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_driver::authz::AuthorizationService;
use peryx_driver::state::AppState;
use peryx_driver::users::UserService;
use peryx_identity::{ArtifactDigest, GrantScope, PasswordPolicy, Role};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{
    BackendId, BackendLocation, BlobPlacementFailure, BlobPlacementKey, BlobPlacementTransition, DataCenterId,
    MetaStore,
};
use tower::ServiceExt as _;

const USER_PASSWORD: &str = "local password";

fn digest() -> ArtifactDigest {
    ArtifactDigest::from_sha256("a".repeat(64)).unwrap()
}

/// Record a placement for `digest` in `data_center` and drive it through `transitions`, so a test seeds
/// a datacenter's copy in any of the four lifecycle states.
fn seed(meta: &MetaStore, data_center: &str, transitions: &[BlobPlacementTransition]) {
    let key = BlobPlacementKey {
        digest: digest(),
        backend: BackendId::new("filesystem").unwrap(),
        data_center: DataCenterId::new(data_center).unwrap(),
        location: BackendLocation::new(format!("blobs/{data_center}")).unwrap(),
    };
    for transition in transitions {
        meta.apply_blob_placement(&key, transition, 1, 1000).unwrap();
    }
}

async fn app() -> (tempfile::TempDir, Arc<AppState>) {
    build_app(false).await
}

/// Build an app with one datacenter seeded in each placement state. When `corrupt` is set, the blob
/// placement table is dropped and reopened with a mismatched value type, so every placement read fails
/// and the endpoint answers with a server error.
async fn build_app(corrupt: bool) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    let authorization = AuthorizationService::new(meta.clone());
    let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    for (name, role) in [("Alice", Role::Administrator), ("Olivia", Role::Operator)] {
        let user = users.create(name).unwrap();
        users.set_password(&user.id, USER_PASSWORD).await.unwrap();
        authorization.grant(&user.id, role, GrantScope::Server).unwrap();
    }
    drop(authorization);
    drop(users);
    // One datacenter in each lifecycle state, so a read covers every projection arm.
    let verify = BlobPlacementTransition::Verify {
        observed: digest(),
        size: 4096,
    };
    seed(&meta, "east-1", &[BlobPlacementTransition::Stage, verify.clone()]);
    seed(&meta, "west-2", &[BlobPlacementTransition::Stage]);
    seed(
        &meta,
        "south-3",
        &[
            BlobPlacementTransition::Stage,
            BlobPlacementTransition::Fail {
                class: BlobPlacementFailure::SourceUnavailable,
            },
        ],
    );
    seed(
        &meta,
        "north-4",
        &[BlobPlacementTransition::Stage, verify, BlobPlacementTransition::Revoke],
    );
    drop(meta);
    if corrupt {
        let database = redb::Database::open(&path).unwrap();
        let transaction = database.begin_write().unwrap();
        transaction
            .delete_table(redb::TableDefinition::<&str, &[u8]>::new("blob_placement"))
            .unwrap();
        transaction
            .open_table(redb::TableDefinition::<&str, u64>::new("blob_placement"))
            .unwrap();
        transaction.commit().unwrap();
    }
    let meta = MetaStore::open_existing(&path).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta.clone(), blobs, 60, Vec::new());
    state.users = UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    (dir, Arc::new(state))
}

async fn get(state: &Arc<AppState>, uri: &str, credential: Option<&str>) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().uri(uri);
    if let Some(user) = credential {
        request = request.header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(format!("{user}:{USER_PASSWORD}"))),
        );
    }
    let response = crate::router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, value)
}

fn uri(digest: &ArtifactDigest) -> String {
    format!("/+availability/placements/{}", digest.canonical())
}

#[tokio::test]
async fn test_blob_placement_administrator_reads_every_datacenter_state() {
    let (_dir, state) = app().await;

    let (status, body) = get(&state, &uri(&digest()), Some("Alice")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["digest"], digest().canonical());
    let dcs = body["datacenters"].as_array().unwrap();
    assert_eq!(dcs.len(), 4, "{body}");
    // Datacenter order, and one of each lifecycle state.
    assert_eq!(dcs[0]["data_center"], "east-1");
    assert_eq!(dcs[0]["status"], "verified");
    assert_eq!(dcs[0]["size"], 4096);
    assert_eq!(dcs[1]["data_center"], "north-4");
    assert_eq!(dcs[1]["status"], "revoked");
    assert_eq!(dcs[2]["data_center"], "south-3");
    assert_eq!(dcs[2]["status"], "failed");
    assert_eq!(dcs[3]["data_center"], "west-2");
    assert_eq!(dcs[3]["status"], "pending");
    assert!(dcs[3].get("size").is_none(), "a pending copy has no size: {body}");
}

#[tokio::test]
async fn test_blob_placement_never_leaks_the_backend_or_location() {
    let (_dir, state) = app().await;

    let (status, body) = get(&state, &uri(&digest()), Some("Alice")).await;

    assert_eq!(status, StatusCode::OK);
    let text = body.to_string();
    assert!(!text.contains("filesystem"), "the backend identity never leaks: {text}");
    assert!(!text.contains("blobs/"), "the on-disk location never leaks: {text}");
}

#[tokio::test]
async fn test_blob_placement_is_administrator_only() {
    let (_dir, state) = app().await;

    let (operator, _) = get(&state, &uri(&digest()), Some("Olivia")).await;
    let (anonymous, _) = get(&state, &uri(&digest()), None).await;

    assert_eq!(
        operator,
        StatusCode::FORBIDDEN,
        "the datacenter layout is administrator-only"
    );
    assert_eq!(anonymous, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_blob_placement_rejects_an_invalid_digest() {
    let (_dir, state) = app().await;

    let (status, _) = get(&state, "/+availability/placements/not-a-digest", Some("Alice")).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_blob_placement_store_fault_is_a_server_error() {
    let (_dir, state) = build_app(true).await;

    let (status, _) = get(&state, &uri(&digest()), Some("Alice")).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_blob_placement_of_an_unknown_digest_is_empty() {
    let (_dir, state) = app().await;
    let unknown = ArtifactDigest::from_sha256("b".repeat(64)).unwrap();

    let (status, body) = get(&state, &uri(&unknown), Some("Alice")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["datacenters"].as_array().unwrap().is_empty(), "{body}");
}
