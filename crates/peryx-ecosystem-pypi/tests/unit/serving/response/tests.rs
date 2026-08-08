use peryx_storage::blob::{BlobError, Digest};
use peryx_storage::meta::MetaError;

use super::*;

#[test]
fn test_provenance_response_tags_the_integrity_media_type_and_maps_errors() {
    let served = provenance_response(
        Ok(ProvenanceBody {
            bytes: bytes::Bytes::from_static(br#"{"version":1}"#),
            media_type: crate::attestation::PROVENANCE_MEDIA_TYPE.to_owned(),
            source: "hosted".to_owned(),
            immutable: true,
            availability: AttestationAvailability::Cached,
        }),
        CacheContext::provenance("root/pypi", "abc", "pkg-1.0-py3-none-any.whl.provenance"),
    );
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

    let missing = provenance_response(
        Err(CacheError::FileNotFound),
        CacheContext::provenance("root/pypi", "abc", "pkg-1.0-py3-none-any.whl.provenance"),
    );
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
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
