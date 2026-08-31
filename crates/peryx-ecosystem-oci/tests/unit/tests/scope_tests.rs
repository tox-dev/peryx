use axum::http::{Method, StatusCode};

use super::{
    app_with_indexes, auth, body_has_code, image_manifest, oci_digest, seed_config, send, send_body, writable_index,
};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";

/// Shared content must retain repository-scoped authorization.
fn two_stores(dir: &tempfile::TempDir) -> axum::Router {
    let hosted = |name: &str| writable_index(name, name, true, TOKEN);
    let (_state, app) = app_with_indexes(dir, vec![hosted("store"), hosted("vault")]);
    app
}

/// An image index naming one child.
fn index_over(child_digest: &str, child_size: usize) -> Vec<u8> {
    format!(
        r#"{{"schemaVersion":2,"mediaType":"{INDEX_TYPE}","manifests":[{{"mediaType":"{MANIFEST_TYPE}","digest":"{child_digest}","size":{child_size}}}]}}"#
    )
    .into_bytes()
}

async fn push(app: &axum::Router, name: &str, reference: &str, media_type: &str, body: &[u8]) {
    let (status, _, response) = send_body(
        app,
        Method::PUT,
        &format!("/v2/{name}/manifests/{reference}"),
        &[("authorization", &auth(TOKEN)), ("content-type", media_type)],
        body.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{response:?}");
}

#[tokio::test]
async fn test_manifest_by_digest_is_scoped_to_the_pushing_repository() {
    let dir = tempfile::tempdir().unwrap();
    let app = two_stores(&dir);
    seed_config(&app, "store/app", &auth(TOKEN)).await;
    let body = image_manifest(MANIFEST_TYPE, "");
    let digest = oci_digest(&body);
    push(&app, "store/app", &digest, MANIFEST_TYPE, &body).await;

    let (status, headers, got) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], digest);
    assert_eq!(got, &body[..]);

    for name in ["vault/app", "store/elsewhere"] {
        let (get, _, denied) = send(&app, Method::GET, &format!("/v2/{name}/manifests/{digest}")).await;
        assert_eq!(get, StatusCode::NOT_FOUND, "{name}");
        assert!(body_has_code(&denied, "MANIFEST_UNKNOWN"), "{name}: {denied:?}");
        let (head, ..) = send(&app, Method::HEAD, &format!("/v2/{name}/manifests/{digest}")).await;
        assert_eq!(head, StatusCode::NOT_FOUND, "{name} HEAD");
    }

    let (status, headers, got) = send(&app, Method::HEAD, &format!("/v2/store/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], digest);
    assert!(got.is_empty());
}

#[tokio::test]
async fn test_image_index_naming_a_child_from_another_index_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let app = two_stores(&dir);
    seed_config(&app, "vault/app", &auth(TOKEN)).await;
    let child = image_manifest(MANIFEST_TYPE, "");
    let child_digest = oci_digest(&child);
    push(&app, "vault/app", &child_digest, MANIFEST_TYPE, &child).await;
    let index = index_over(&child_digest, child.len());

    let (status, _, rejected) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/latest",
        &[("authorization", &auth(TOKEN)), ("content-type", INDEX_TYPE)],
        index,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body_has_code(&rejected, "MANIFEST_BLOB_UNKNOWN"), "{rejected:?}");

    let (status, _, denied) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{child_digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&denied, "MANIFEST_UNKNOWN"), "{denied:?}");
}

#[tokio::test]
async fn test_image_index_child_is_retrievable_where_the_index_is_served() {
    let dir = tempfile::tempdir().unwrap();
    let app = two_stores(&dir);
    seed_config(&app, "store/app", &auth(TOKEN)).await;
    let child = image_manifest(MANIFEST_TYPE, "");
    let child_digest = oci_digest(&child);
    push(&app, "store/app", &child_digest, MANIFEST_TYPE, &child).await;
    let index = index_over(&child_digest, child.len());
    push(&app, "store/app", "latest", INDEX_TYPE, &index).await;

    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{child_digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &child[..]);

    for name in ["vault/app", "store/other"] {
        let (status, _, denied) = send(&app, Method::GET, &format!("/v2/{name}/manifests/{child_digest}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{name}");
        assert!(body_has_code(&denied, "MANIFEST_UNKNOWN"), "{name}: {denied:?}");
    }
}

#[tokio::test]
async fn test_referrer_is_retrievable_where_it_was_pushed() {
    let dir = tempfile::tempdir().unwrap();
    let app = two_stores(&dir);
    seed_config(&app, "store/app", &auth(TOKEN)).await;
    let subject = oci_digest(b"a-subject-manifest");
    let referrer = image_manifest(
        MANIFEST_TYPE,
        &format!(r#","subject":{{"mediaType":"{MANIFEST_TYPE}","digest":"{subject}","size":18}}"#),
    );
    let referrer_digest = oci_digest(&referrer);
    push(&app, "store/app", &referrer_digest, MANIFEST_TYPE, &referrer).await;

    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{referrer_digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &referrer[..]);
    let (status, _, denied) = send(&app, Method::GET, &format!("/v2/vault/app/manifests/{referrer_digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&denied, "MANIFEST_UNKNOWN"), "{denied:?}");
}
