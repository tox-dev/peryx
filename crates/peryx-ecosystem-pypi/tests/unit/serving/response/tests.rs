use http_body_util::BodyExt as _;
use peryx_storage::blob::{BlobError, Digest};
use peryx_storage::meta::MetaError;

use super::*;

#[tokio::test]
async fn test_index_response_names_store_failures() {
    let response = index_response(Err(CacheError::Meta(meta_error())), Format::Json, "root/pypi");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "project list on index \"root/pypi\": metadata store error: expected ident at line 1 column 2"
    );
}

#[tokio::test]
async fn test_cache_error_response_preserves_upstream_retry_after() {
    let response = cache_error_response(
        &CacheError::UpstreamRateLimited {
            retry_after: Some("Wed, 21 Oct 2037 07:28:00 GMT".to_owned()),
        },
        CacheContext::project("root/pypi", "demo"),
    );

    assert_eq!(
        (
            response.status(),
            response.headers()[header::RETRY_AFTER].to_str().unwrap(),
        ),
        (StatusCode::TOO_MANY_REQUESTS, "Wed, 21 Oct 2037 07:28:00 GMT")
    );
}

#[test]
fn test_provenance_response_tags_the_integrity_media_type() {
    let served = provenance_response(ProvenanceBody {
        bytes: bytes::Bytes::from_static(br#"{"version":1}"#),
        media_type: crate::attestation::PROVENANCE_MEDIA_TYPE.to_owned(),
        source: "hosted".to_owned(),
        immutable: true,
        availability: AttestationAvailability::Cached,
    });
    assert_eq!(served.status(), StatusCode::OK);
    assert_eq!(
        served.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/vnd.pypi.integrity.v1+json"
    );
    assert_eq!(served.headers().get("x-peryx-provenance-source").unwrap(), "hosted");
    assert_eq!(
        served.headers().get("x-peryx-provenance-availability").unwrap(),
        "cached"
    );
}

#[test]
fn test_cache_error_status_maps_store_and_policy_errors() {
    let context = CacheContext::mutation("file removal");
    assert_eq!(
        cache_error_status(&CacheError::Meta(meta_error()), &context),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        cache_error_status(
            &CacheError::Blob(BlobError::not_found(&Digest::of(b"missing"))),
            &context
        ),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        cache_error_status(&CacheError::FileExists("pkg-1.0.whl".to_owned()), &context),
        StatusCode::CONFLICT
    );
    assert_eq!(
        cache_error_status(&CacheError::NotVolatile, &context),
        StatusCode::FORBIDDEN
    );
}

fn meta_error() -> MetaError {
    MetaError::Decode(serde_json::from_str::<serde_json::Value>("not json").unwrap_err())
}

/// A policy denial is reported in the vocabulary the client speaks. The neutral engine names a
/// resource, an artifact and a group; a PyPI reader knows those as a project, a filename and a
/// version, and knows its allow lists by project. A name left untranslated reaches the client as
/// engine jargon it has no way to act on.
#[test]
fn test_a_denial_is_reported_in_pypi_vocabulary() {
    let rules = ["resource-allow-list", "resource-block-list", "max-artifact-size", "unknown-rule"]
        .map(super::pypi_rule);
    let fields = ["resource", "artifact", "group", "unmapped"].map(super::pypi_field);

    assert_eq!(
        (rules, fields),
        (
            ["project-allow-list", "project-block-list", "max-file-size", "unknown-rule"],
            ["project", "filename", "version", "unmapped"]
        )
    );
}
