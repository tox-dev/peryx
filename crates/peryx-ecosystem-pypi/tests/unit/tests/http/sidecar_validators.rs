use super::attestations::{FILENAME, attestations_field, upload_with_attestations};
use super::support::*;
use super::upstream_attestations::{
    PYPI_PROVENANCE, mount_provenance, upstream_harness, upstream_page, upstream_provenance_uri,
};
use crate::policy::RemoteMetadataMode;

/// What the revocation cap leaves on a sidecar an index may serve from cache.
const REVALIDATED: &str = "public, max-age=60, must-revalidate, no-transform";

/// Both directions of the conditional request, against the tag the `200` published.
///
/// A client holding the representation is answered `304` under the same policy the `200` stated, and
/// one holding anything else still gets the whole document back.
async fn assert_sidecar_validator(state: &Arc<AppState>, uri: &str, body: &[u8], policy: &str) {
    let etag = format!("\"{}\"", Digest::of(body).as_str());
    let (status, headers, served) = get_bytes(state, uri, None).await;
    assert_eq!(
        (
            status,
            headers[header::ETAG].to_str().unwrap(),
            headers[header::CACHE_CONTROL].to_str().unwrap(),
            headers[header::CONTENT_LENGTH].to_str().unwrap(),
            served.as_slice(),
        ),
        (
            StatusCode::OK,
            etag.as_str(),
            policy,
            body.len().to_string().as_str(),
            body,
        )
    );

    let (status, headers, revalidated) =
        get_bytes_with_headers(state, uri, &[(header::IF_NONE_MATCH.as_str(), &etag)]).await;
    // The `200` states the document's length; RFC 9112 s6.2 admits no other on the `304`, so it
    // states none rather than the zero its dropped body would measure.
    assert_eq!(
        (
            status,
            headers[header::ETAG].to_str().unwrap(),
            headers[header::CACHE_CONTROL].to_str().unwrap(),
            headers.contains_key(header::CONTENT_LENGTH),
            revalidated.as_slice(),
        ),
        (StatusCode::NOT_MODIFIED, etag.as_str(), policy, false, b"".as_slice())
    );

    let (status, headers, refused) =
        get_bytes_with_headers(state, uri, &[(header::IF_NONE_MATCH.as_str(), "\"an-older-document\"")]).await;
    assert_eq!(
        (status, headers[header::ETAG].to_str().unwrap(), refused.as_slice()),
        (StatusCode::OK, etag.as_str(), body)
    );
}

#[tokio::test]
async fn test_metadata_sidecar_validates_against_the_documents_own_digest() {
    let h = harness().await;
    let wheel_digest = Digest::of(b"wheel-bytes");
    let metadata = b"Metadata-Version: 2.1\nName: flask\nRequires-Dist: werkzeug\n";
    let wheel_url = format!("{}/files/flask.whl", h.server.uri());
    let json = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0.whl\",\"size\":11,\"url\":\"{wheel_url}\",\
         \"hashes\":{{\"sha256\":\"{wheel}\"}},\"core-metadata\":{{\"sha256\":\"{meta}\"}}}}]}}",
        wheel = wheel_digest.as_str(),
        meta = Digest::of(metadata).as_str(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(json.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl.metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(metadata.to_vec()))
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    let uri = format!("/pypi/files/{}/flask-1.0.whl.metadata", wheel_digest.as_str());

    assert_sidecar_validator(&h.state, &uri, metadata, REVALIDATED).await;
}

#[tokio::test]
async fn test_hosted_provenance_validates_against_the_stored_blob() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let sha256 = Digest::of(&wheel).as_str().to_owned();
    assert_eq!(
        upload_with_attestations(&h.state, &wheel, &attestations_field(FILENAME, &sha256)).await,
        StatusCode::OK
    );
    let (stored, _) = h
        .state
        .serving
        .meta
        .get_provenance("hosted", "peryxpkg", &sha256, FILENAME)
        .unwrap()
        .unwrap();
    let document = h
        .state
        .serving
        .blobs
        .read_bytes(&Digest::from_hex(&stored).unwrap(), 1 << 20)
        .await
        .unwrap();

    let uri = format!("/root/pypi/files/{sha256}/{FILENAME}.provenance");

    assert_sidecar_validator(&h.state, &uri, &document, REVALIDATED).await;
}

/// A proxied document is refreshed from its upstream on every read, so its `304` has to repeat the
/// `no-cache` the `200` carried rather than the immutable policy a hosted sidecar answers with.
#[tokio::test]
async fn test_proxied_provenance_repeats_its_no_cache_policy_on_a_304() {
    let h = upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "3".repeat(64);
    mount_provenance(
        &h,
        ResponseTemplate::new(200).set_body_raw(PYPI_PROVENANCE, "application/vnd.pypi.integrity.v1+json"),
    )
    .await;
    upstream_page(&h, &digest, "application/json").await;

    let uri = upstream_provenance_uri(&digest);

    assert_sidecar_validator(&h.state, &uri, PYPI_PROVENANCE.as_bytes(), "public, no-cache").await;
}
