//! By-digest manifest reads authorize against the requesting repository, not the global,
//! content-addressed manifest store every index shares for dedup — the read half of the scoping #103
//! gave DELETE. A digest cached under one repository is not readable under another it does not belong to.

use axum::http::{Method, StatusCode};
use peryx_index::IndexKind;

use super::{app_with_indexes, auth, body_has_code, oci_digest, oci_index, send, send_body};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";

/// Two writable hosted indexes, `store` and `vault`, over one pair of stores: the manifest bytes dedup
/// into a single content pool, but the repositories are distinct.
fn two_stores(dir: &tempfile::TempDir) -> axum::Router {
    let hosted = |name: &str| {
        oci_index(
            name,
            name,
            IndexKind::Hosted {
                upload_token: Some(TOKEN.to_owned()),
                volatile: true,
            },
        )
    };
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

    // The repository that holds it serves it by digest.
    let (status, headers, got) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], digest);
    assert_eq!(got, &body[..]);

    // Another index, and another repository on the same index, do not: the bytes are one dedup'd copy,
    // yet neither repository holds it, so the read is manifest-unknown rather than a cross-repo leak —
    // and HEAD leaks nothing a GET would not.
    for name in ["vault/app", "store/elsewhere"] {
        let (get, _, denied) = send(&app, Method::GET, &format!("/v2/{name}/manifests/{digest}")).await;
        assert_eq!(get, StatusCode::NOT_FOUND, "{name}");
        assert!(body_has_code(&denied, "MANIFEST_UNKNOWN"), "{name}: {denied:?}");
        let (head, ..) = send(&app, Method::HEAD, &format!("/v2/{name}/manifests/{digest}")).await;
        assert_eq!(head, StatusCode::NOT_FOUND, "{name} HEAD");
    }

    // HEAD parity for the repository that does hold it.
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
    // The child lands in `vault`; `store` never receives it on its own, only as a member of the index.
    push(&app, "vault/app", &child_digest, MANIFEST_TYPE, child).await;
    let index = format!(r#"{{"schemaVersion":2,"manifests":[{{"digest":"{child_digest}"}}]}}"#).into_bytes();
    push(&app, "store/app", "latest", INDEX_TYPE, &index).await;

    // `store` serves the index, so a by-digest read of the child it names is authorized there.
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{child_digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &child[..]);

    // A repository that serves neither the index nor the child cannot read it.
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

    // The repository that recorded the referrer serves it by digest; another does not.
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{referrer_digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &referrer[..]);
    let (status, _, denied) = send(&app, Method::GET, &format!("/v2/vault/app/manifests/{referrer_digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&denied, "MANIFEST_UNKNOWN"), "{denied:?}");
}
