//! A hosted push validates the manifest document against the schema its media type declares, so peryx
//! never stores bytes it would hand back as a manifest an OCI client cannot read.

use axum::http::{Method, StatusCode};
use bytes::Bytes;
use rstest::rstest;

use super::{
    app_with_journal, auth, body_has_code, hosted_writable, image_manifest, oci_digest, seed_config, send, send_body,
    writable_index,
};

const TOKEN: &str = "s3cret";
const IMAGE_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const CONFIG_TYPE: &str = "application/vnd.oci.image.config.v1+json";
const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

async fn push(app: &axum::Router, media_type: &str, body: &str) -> (StatusCode, Bytes) {
    let (status, _, response) = send_body(
        app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", media_type)],
        body.as_bytes().to_vec(),
    )
    .await;
    (status, response)
}

#[rstest]
#[case::truncated_json(IMAGE_TYPE, "{", "manifest body is not JSON")]
#[case::not_json(IMAGE_TYPE, "not json at all", "manifest body is not JSON")]
#[case::array_root(IMAGE_TYPE, "[]", "manifest body is not a JSON object")]
#[case::string_root(IMAGE_TYPE, r#""a manifest""#, "manifest body is not a JSON object")]
#[case::no_schema_version(IMAGE_TYPE, "{}", "manifest schemaVersion must be 2")]
#[case::schema_version_one(
    IMAGE_TYPE,
    r#"{"schemaVersion":1,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#,
    "manifest schemaVersion must be 2"
)]
#[case::schema_version_string(
    IMAGE_TYPE,
    r#"{"schemaVersion":"2","mediaType":"application/vnd.oci.image.manifest.v1+json"}"#,
    "manifest schemaVersion must be 2"
)]
#[case::index_body_under_the_image_type(
    IMAGE_TYPE,
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#,
    "manifest mediaType must be application/vnd.oci.image.manifest.v1+json"
)]
#[case::no_media_type(IMAGE_TYPE, r#"{"schemaVersion":2}"#, "manifest mediaType must be")]
#[case::no_config(
    IMAGE_TYPE,
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","layers":[]}"#,
    "manifest is missing the required config field"
)]
#[case::no_layers(
    IMAGE_TYPE,
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:aaaa","size":2}}"#,
    "manifest is missing the required layers field"
)]
#[case::layers_not_a_list(
    IMAGE_TYPE,
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:aaaa","size":2},"layers":{}}"#,
    "manifest layers must be an array of descriptors"
)]
#[case::config_without_a_digest(
    IMAGE_TYPE,
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","size":2},"layers":[]}"#,
    "the config descriptor requires a digest string"
)]
#[case::layer_with_a_negative_size(
    IMAGE_TYPE,
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:aaaa","size":2},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"sha256:bbbb","size":-1}]}"#,
    "the layers[0] descriptor requires a non-negative integer size"
)]
#[case::no_manifests(
    INDEX_TYPE,
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json"}"#,
    "manifest is missing the required manifests field"
)]
#[case::index_entry_without_a_size(
    INDEX_TYPE,
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:aaaa"}]}"#,
    "the manifests[0] descriptor requires a non-negative integer size"
)]
#[case::subject_that_is_not_a_descriptor(
    INDEX_TYPE,
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[],"subject":"sha256:aaaa"}"#,
    "the subject descriptor requires a JSON object"
)]
#[tokio::test]
async fn test_a_manifest_that_breaks_its_schema_is_rejected(
    #[case] media_type: &str,
    #[case] body: &str,
    #[case] detail: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);

    let (status, response) = push(&app, media_type, body).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{response:?}");
    assert!(body_has_code(&response, "MANIFEST_INVALID"), "{response:?}");
    assert!(
        std::str::from_utf8(&response).unwrap().contains(detail),
        "{response:?} does not explain {detail}"
    );
}

/// A non-distributable layer names bytes the registry never holds, so the schema accepts it and the
/// reference check skips it: rejecting one would make a foreign-layer image unpushable.
#[tokio::test]
async fn test_a_foreign_layer_is_accepted_without_its_blob() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    seed_config(&app, "store/app", &auth(TOKEN)).await;
    let manifest = image_manifest(
        IMAGE_TYPE,
        &format!(
            r#","layers":[{{"mediaType":"application/vnd.oci.image.layer.nondistributable.v1.tar+gzip","digest":"{DIGEST}","size":9,"urls":["https://example.invalid/layer"]}}]"#
        ),
    );

    let (status, _, response) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", IMAGE_TYPE)],
        manifest,
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "{response:?}");
}

/// Extension fields and an absent `subject` are both ordinary, so neither costs a push.
#[tokio::test]
async fn test_a_manifest_without_a_subject_keeps_its_extension_fields() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    seed_config(&app, "store/app", &auth(TOKEN)).await;
    let manifest = image_manifest(
        IMAGE_TYPE,
        r#","artifactType":"application/vnd.example","annotations":{"a":"b"}"#,
    );
    let digest = oci_digest(&manifest);

    let (status, headers, response) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", IMAGE_TYPE)],
        manifest.clone(),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "{response:?}");
    assert!(!headers.contains_key("oci-subject"));
    let (status, _, served) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{digest}")).await;
    assert_eq!((status, served), (StatusCode::OK, Bytes::from(manifest)));
}

/// A rejected body is refused before the repository is claimed, quota is reserved or anything is
/// written, so the repository is left exactly as it was and no replica sees a mutation.
#[tokio::test]
async fn test_a_rejected_manifest_leaves_no_state_or_events() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = app_with_journal(&dir, vec![writable_index("store", "store", true, TOKEN)], true);
    let body = format!(
        r#"{{"schemaVersion":2,"mediaType":"{IMAGE_TYPE}","config":{{"mediaType":"{CONFIG_TYPE}"}},"layers":[]}}"#
    );

    let (status, response) = push(&app, IMAGE_TYPE, &body).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{response:?}");
    assert_eq!(
        send(&app, Method::GET, "/v2/store/app/manifests/v1").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            Method::GET,
            &format!("/v2/store/app/manifests/{}", oci_digest(body.as_bytes()))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, _, tags) = send(&app, Method::GET, "/v2/store/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&tags).unwrap()["tags"],
        serde_json::json!([])
    );
    assert!(
        state.serving.meta.journal_after(0, 100).unwrap().is_empty(),
        "a rejected push journals no mutation"
    );
}
