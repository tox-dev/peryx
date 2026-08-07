use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_driver::AppState;
use peryx_ecosystem_registry::pypi::store::PypiStore as _;
use peryx_ecosystem_registry::pypi::upload::{TrashInfo, Uploaded};
use peryx_ecosystem_registry::pypi::{CoreMetadata, File, Provenance, Yanked};
use peryx_identity::{GrantScope, Role};
use peryx_storage::blob::Digest;
use tower::ServiceExt as _;

use crate::config::{Config, IndexConfig, IndexKind, SecretSource};
use crate::server::{build_state, router_for};

const UPLOAD_TOKEN: &str = "s3cret";
const PASSWORD: &str = "local password";

fn config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![
            hosted("hosted", peryx_ecosystem_registry::PYPI),
            hosted("images", peryx_ecosystem_registry::OCI),
        ],
        ..Config::default()
    }
}

fn hosted(name: &str, ecosystem: peryx_core::Ecosystem) -> IndexConfig {
    IndexConfig {
        name: name.to_owned(),
        route: name.to_owned(),
        policy: peryx_policy::PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem,
        anonymous_read: None,
        tokens: vec![crate::tests::writer_token(SecretSource::Literal(
            UPLOAD_TOKEN.to_owned(),
        ))],
        kind: IndexKind::Hosted { volatile: true },
    }
}

async fn provision_users(state: &AppState) {
    for (name, role, scope) in [
        ("Alice", Role::Administrator, GrantScope::Server),
        (
            "Rita",
            Role::RepositoryReader,
            GrantScope::Repository {
                name: "hosted".to_owned(),
            },
        ),
    ] {
        let user = state.users.create(name).unwrap();
        state.users.set_password(&user.id, PASSWORD).await.unwrap();
        state.authorization.grant(&user.id, role, scope).unwrap();
    }
}

fn seed_pypi_trash(state: &AppState, filename: &str, deleted_at_unix: i64) {
    let uploaded = Uploaded {
        version: "1.0".to_owned(),
        file: File {
            filename: filename.to_owned(),
            url: format!("https://files/{filename}"),
            hashes: BTreeMap::from([("sha256".to_owned(), "deadbeef".to_owned())]),
            requires_python: None,
            size: Some(1_024),
            upload_time: Some("2020-01-01T00:00:00Z".to_owned()),
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
        trashed: Some(TrashInfo {
            deleted_at_unix,
            actor: Some("cleanup-bot".to_owned()),
            reason: Some("mistaken upload".to_owned()),
        }),
    };
    state
        .meta
        .put_upload("hosted", "flask", filename, &serde_json::to_vec(&uploaded).unwrap())
        .unwrap();
}

async fn upload_blob(router: &axum::Router, bytes: &[u8]) -> String {
    let digest = format!("sha256:{}", Digest::of(bytes).as_str());
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v2/images/app/blobs/uploads/?digest={digest}"))
                .header(
                    header::AUTHORIZATION,
                    format!("Basic {}", STANDARD.encode(format!("_:{UPLOAD_TOKEN}"))),
                )
                .body(Body::from(bytes.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    digest
}

/// Push an image tagged `1.0`, then soft-delete it by digest so the OCI trash carries one record.
async fn seed_oci_trash(router: &axum::Router) -> String {
    let config = upload_blob(router, b"{}").await;
    let manifest = format!(
        concat!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","#,
            r#""config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config}","size":2}},"#,
            r#""layers":[]}}"#,
        ),
        config = config,
    );
    let digest = format!("sha256:{}", Digest::of(manifest.as_bytes()).as_str());
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v2/images/app/manifests/1.0")
                .header(
                    header::AUTHORIZATION,
                    format!("Basic {}", STANDARD.encode(format!("_:{UPLOAD_TOKEN}"))),
                )
                .header(header::CONTENT_TYPE, "application/vnd.oci.image.manifest.v1+json")
                .body(Body::from(manifest))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/v2/images/app/manifests/{digest}"))
                .header(
                    header::AUTHORIZATION,
                    format!("Basic {}", STANDARD.encode(format!("_:{UPLOAD_TOKEN}"))),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    digest
}

async fn app() -> (tempfile::TempDir, axum::Router, String) {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&config(&dir)).unwrap();
    provision_users(&state).await;
    seed_pypi_trash(&state, "flask-1.0.whl", 1_000);
    let router = router_for(state);
    let oci_digest = seed_oci_trash(&router).await;
    (dir, router, oci_digest)
}

async fn get(
    router: &axum::Router,
    uri: &str,
    credential: (&str, &str),
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let (user, password) = credential;
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(
                    header::AUTHORIZATION,
                    format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

fn records(document: &serde_json::Value) -> &Vec<serde_json::Value> {
    document["trash"].as_array().unwrap()
}

#[tokio::test]
async fn test_administrator_lists_pypi_and_oci_trash_with_actor() {
    let (_dir, router, oci_digest) = app().await;

    let (status, headers, document) = get(&router, "/+trash", ("Alice", PASSWORD)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let seen: Vec<(&str, &str, Option<&str>)> = records(&document)
        .iter()
        .map(|record| {
            (
                record["ecosystem"].as_str().unwrap(),
                record["repository"].as_str().unwrap(),
                record["actor"].as_str(),
            )
        })
        .collect();
    assert!(seen.contains(&("pypi", "hosted", Some("cleanup-bot"))), "{seen:?}");
    // The OCI soft-delete recorded the deleting token's username, which the administrator may see.
    assert!(seen.contains(&("oci", "images", Some("_"))), "{seen:?}");
    let oci = records(&document)
        .iter()
        .find(|record| record["ecosystem"] == "oci")
        .unwrap();
    assert_eq!(oci["name"], "app");
    assert_eq!(oci["reference"], "1.0");
    assert_eq!(oci["digest"], oci_digest);
    assert_eq!(oci["state"], "restorable");
    assert_eq!(oci["restorable"], true);
}

#[tokio::test]
async fn test_repository_reader_sees_records_without_actor() {
    let (_dir, router, _) = app().await;

    let (status, _, document) = get(&router, "/+trash?repository=hosted", ("Rita", PASSWORD)).await;

    assert_eq!(status, StatusCode::OK);
    let record = &records(&document)[0];
    assert_eq!(record["ecosystem"], "pypi");
    assert_eq!(record["reason"], "mistaken upload");
    assert!(
        record.get("actor").is_none(),
        "the role filter redacts the actor: {record}"
    );
}

#[tokio::test]
async fn test_ecosystem_filter_narrows_to_one_ecosystem() {
    let (_dir, router, _) = app().await;

    let (_, _, document) = get(&router, "/+trash?ecosystem=oci", ("Alice", PASSWORD)).await;

    let ecosystems: Vec<&str> = records(&document)
        .iter()
        .map(|record| record["ecosystem"].as_str().unwrap())
        .collect();
    assert_eq!(ecosystems, vec!["oci"]);
}

#[tokio::test]
async fn test_pagination_is_stable_across_pages() {
    let (_dir, router, _) = app().await;

    let (_, _, first) = get(&router, "/+trash?limit=1", ("Alice", PASSWORD)).await;
    assert_eq!(records(&first).len(), 1);
    let cursor = first["next_cursor"].as_str().expect("a second record remains");

    let (_, _, second) = get(
        &router,
        &format!("/+trash?limit=1&cursor={}", urlencoding(cursor)),
        ("Alice", PASSWORD),
    )
    .await;
    assert_eq!(records(&second).len(), 1);
    assert_ne!(
        records(&first)[0]["ecosystem"],
        records(&second)[0]["ecosystem"],
        "the pages do not overlap"
    );
    assert_eq!(second["next_cursor"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_inspect_returns_one_record() {
    let (_dir, router, _) = app().await;

    let (status, _, document) = get(
        &router,
        "/+trash/record?ecosystem=pypi&repository=hosted&name=flask&reference=flask-1.0.whl&digest=sha256:deadbeef",
        ("Alice", PASSWORD),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["record"]["reference"], "flask-1.0.whl");
    assert_eq!(document["record"]["actor"], "cleanup-bot");
}

#[tokio::test]
async fn test_web_view_renders_the_trash_page() {
    let (_dir, router, _) = app().await;

    let response = router
        .clone()
        .oneshot(Request::builder().uri("/admin/trash").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(body.contains("Trash"), "{body}");
    assert!(body.contains("Restorable"), "{body}");
}

#[tokio::test]
async fn test_a_corrupt_record_surfaces_a_store_error() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&config(&dir)).unwrap();
    provision_users(&state).await;
    state
        .meta
        .put_upload("hosted", "flask", "flask-1.0.whl", b"not json")
        .unwrap();
    let router = router_for(state);

    let (list, _, _) = get(&router, "/+trash", ("Alice", PASSWORD)).await;
    assert_eq!(list, StatusCode::INTERNAL_SERVER_ERROR);

    let (inspect, _, document) = get(
        &router,
        "/+trash/record?ecosystem=pypi&repository=hosted&name=flask",
        ("Alice", PASSWORD),
    )
    .await;
    assert_eq!(inspect, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(document["error"], "trash query failed");
}

fn urlencoding(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
