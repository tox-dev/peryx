use axum::http::{Method, StatusCode};

use super::{app_with_indexes, auth, body_has_code, oci_digest, send, send_body, writable_index};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";

/// Shared content must retain repository-scoped authorization.
fn two_stores(dir: &tempfile::TempDir) -> axum::Router {
    let hosted = |name: &str| writable_index(name, name, true, TOKEN);
    let (_state, app) = app_with_indexes(dir, vec![hosted("store"), hosted("vault")]);
    app
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
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);
    push(&app, "store/app", &digest, MANIFEST_TYPE, body).await;

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
async fn test_image_index_child_is_retrievable_where_the_index_is_served() {
    let dir = tempfile::tempdir().unwrap();
    let app = two_stores(&dir);
    let child = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let child_digest = oci_digest(child);
    push(&app, "vault/app", &child_digest, MANIFEST_TYPE, child).await;
    let index = format!(r#"{{"schemaVersion":2,"manifests":[{{"digest":"{child_digest}"}}]}}"#).into_bytes();
    push(&app, "store/app", "latest", INDEX_TYPE, &index).await;

    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{child_digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &child[..]);

    let (status, _, denied) = send(&app, Method::GET, &format!("/v2/store/other/manifests/{child_digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&denied, "MANIFEST_UNKNOWN"), "{denied:?}");
}

#[tokio::test]
async fn test_referrer_is_retrievable_where_it_was_pushed() {
    let dir = tempfile::tempdir().unwrap();
    let app = two_stores(&dir);
    let subject = oci_digest(br#"{"schemaVersion":2}"#);
    let referrer = format!(r#"{{"schemaVersion":2,"mediaType":"{MANIFEST_TYPE}","subject":{{"digest":"{subject}"}}}}"#)
        .into_bytes();
    let referrer_digest = oci_digest(&referrer);
    push(&app, "store/app", &referrer_digest, MANIFEST_TYPE, &referrer).await;

    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{referrer_digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &referrer[..]);
    let (status, _, denied) = send(&app, Method::GET, &format!("/v2/vault/app/manifests/{referrer_digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&denied, "MANIFEST_UNKNOWN"), "{denied:?}");
}
