//! Upstream PEP 740 retention, revalidation, and race tests.

use super::support::*;
use super::upstream_attestations::{
    FILENAME, PYPI_PROVENANCE, REVISED_PYPI_PROVENANCE, mount_provenance, provenance_requests,
    put_cached_attestation_page, upstream_harness, upstream_page, upstream_policy, upstream_provenance_uri,
};
use crate::store::UpstreamAttestation;
use peryx_policy::RemoteMetadataMode;
use rstest::rstest;

fn replace_attestation_on_response(
    meta: MetaStore,
    digest: String,
    record: UpstreamAttestation,
    response: ResponseTemplate,
) -> impl Fn(&wiremock::Request) -> ResponseTemplate + Send + Sync + 'static {
    move |_| {
        meta.put_upstream_attestation("pypi", &digest, FILENAME, &record)
            .unwrap();
        response.clone()
    }
}

#[tokio::test]
async fn test_upstream_attestation_cache_mode_uses_a_fresh_body() {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "3".repeat(64);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200)
            .insert_header("cache-control", "max-age=60")
            .insert_header("etag", "\"attestation-1\"")
            .set_body_raw(PYPI_PROVENANCE, "Application/JSON; charset=utf-8"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;

    for _ in 0..2 {
        let (status, headers, body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, PYPI_PROVENANCE);
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        assert_eq!(headers["x-peryx-provenance-availability"], "cached");
    }
    assert_eq!(provenance_requests(&harness).await, 1);
    let record = harness
        .state
        .meta
        .get_upstream_attestation("pypi", "peryxpkg", &digest, FILENAME)
        .unwrap()
        .unwrap();
    assert_eq!(record.body.as_deref(), Some(PYPI_PROVENANCE));
    assert_eq!(record.etag.as_deref(), Some("\"attestation-1\""));
    let restarted = restarted_state(&harness);
    let (status, _, body) = get(&restarted, &upstream_provenance_uri(&digest), None).await;
    assert_eq!(
        (
            status,
            body,
            restarted
                .meta
                .get_upstream_attestation("pypi", "peryxpkg", &digest, FILENAME)
                .unwrap(),
            provenance_requests(&harness).await,
        ),
        (StatusCode::OK, PYPI_PROVENANCE.to_owned(), Some(record), 1)
    );
}

#[tokio::test]
async fn test_upstream_attestation_no_cache_revalidates_every_request() {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "31".repeat(32);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200)
            .insert_header("cache-control", "no-cache")
            .insert_header("etag", "\"attestation-1\"")
            .set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;

    get(&harness.state, &upstream_provenance_uri(&digest), None).await;
    get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    let requests: Vec<_> = harness
        .server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path().starts_with("/integrity/"))
        .collect();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].headers.get("if-none-match").is_none());
    assert_eq!(requests[1].headers.get("if-none-match").unwrap(), "\"attestation-1\"");
}

#[tokio::test]
async fn test_upstream_attestation_no_cache_does_not_serve_stale_on_failure() {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "33".repeat(32);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200)
            .insert_header("cache-control", "no-cache")
            .set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;
    get(&harness.state, &upstream_provenance_uri(&digest), None).await;
    harness.server.reset().await;
    mount_provenance(&harness, ResponseTemplate::new(503)).await;

    let (status, ..) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_upstream_attestation_304_without_cache_control_preserves_no_cache() {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "34".repeat(32);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200)
            .insert_header("cache-control", "no-cache")
            .insert_header("etag", "\"attestation-1\"")
            .set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;
    get(&harness.state, &upstream_provenance_uri(&digest), None).await;
    harness.server.reset().await;
    mount_provenance(&harness, ResponseTemplate::new(304)).await;
    let (revalidated, ..) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;
    assert_eq!(revalidated, StatusCode::OK);
    harness.server.reset().await;
    mount_provenance(&harness, ResponseTemplate::new(503)).await;

    let (status, ..) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_upstream_attestation_no_store_is_served_without_retention() {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "32".repeat(32);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200)
            .insert_header("cache-control", "no-store")
            .insert_header("etag", "\"secret-validator\"")
            .set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;

    for expected_requests in 1..=2 {
        let (status, headers, body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, PYPI_PROVENANCE);
        assert_eq!(headers["x-peryx-provenance-availability"], "remote-only");
        assert_eq!(provenance_requests(&harness).await, expected_requests);
    }
    let record = harness
        .state
        .meta
        .get_upstream_attestation("pypi", "peryxpkg", &digest, FILENAME)
        .unwrap()
        .unwrap();
    assert!(record.body.is_none());
    assert!(record.etag.is_none());
}

#[tokio::test]
async fn test_cached_upstream_attestation_without_a_media_type_uses_the_pep_740_type() {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "f".repeat(64);
    put_cached_attestation_page(&harness, &digest);
    let mut record = UpstreamAttestation::remote("https://example.test/pkg.provenance", "pypi", "peryxpkg", None);
    record.body = Some(PYPI_PROVENANCE.to_owned());
    record.fetched_at_unix = Some(1_000);
    record.fresh_secs = Some(60);
    record.availability = crate::store::AttestationAvailability::Cached;
    harness
        .state
        .meta
        .put_upstream_attestation("pypi", &digest, FILENAME, &record)
        .unwrap();

    let (status, headers, body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, PYPI_PROVENANCE);
    assert_eq!(headers[header::CONTENT_TYPE], crate::attestation::PROVENANCE_MEDIA_TYPE);
    assert_eq!(provenance_requests(&harness).await, 0);
}

#[rstest]
#[case::negative_ttl(-1, None)]
#[case::negative_upstream_lifetime(60, Some(-1))]
#[tokio::test]
async fn test_negative_attestation_freshness_revalidates(#[case] ttl_secs: i64, #[case] fresh_secs: Option<i64>) {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "0b".repeat(32);
    put_cached_attestation_page(&harness, &digest);
    let mut record = UpstreamAttestation::remote(
        &format!("{}/integrity/{FILENAME}.provenance", harness.server.uri()),
        "pypi",
        "peryxpkg",
        None,
    );
    record.body = Some(PYPI_PROVENANCE.to_owned());
    record.fetched_at_unix = Some(1_000);
    record.fresh_secs = fresh_secs;
    record.availability = crate::store::AttestationAvailability::Cached;
    harness
        .state
        .meta
        .put_upstream_attestation("pypi", &digest, FILENAME, &record)
        .unwrap();
    mount_provenance(
        &harness,
        ResponseTemplate::new(200).set_body_raw(REVISED_PYPI_PROVENANCE, "application/json"),
    )
    .await;

    let (status, _, body) = get(
        &restarted_state_with_ttl(&harness, ttl_secs),
        &upstream_provenance_uri(&digest),
        None,
    )
    .await;

    assert_eq!((status, body), (StatusCode::OK, REVISED_PYPI_PROVENANCE.to_owned()));
    assert_eq!(provenance_requests(&harness).await, 1);
}

#[tokio::test]
async fn test_offline_upstream_attestation_without_a_body_is_unavailable() {
    let harness = offline_harness(upstream_policy(RemoteMetadataMode::Cache)).await;
    let digest = "0a".repeat(32);
    put_cached_attestation_page(&harness, &digest);
    harness
        .state
        .meta
        .put_upstream_attestation(
            "pypi",
            &digest,
            FILENAME,
            &UpstreamAttestation::remote("https://example.test/pkg.provenance", "pypi", "peryxpkg", None),
        )
        .unwrap();

    let (status, _, body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("offline mode has no cached provenance"), "{body}");
}

#[tokio::test]
async fn test_offline_upstream_attestation_serves_a_stale_valid_body() {
    let harness = offline_harness(upstream_policy(RemoteMetadataMode::Cache)).await;
    let digest = "0b".repeat(32);
    put_cached_attestation_page(&harness, &digest);
    let mut record = UpstreamAttestation::remote("https://example.test/pkg.provenance", "pypi", "peryxpkg", None);
    record.body = Some(PYPI_PROVENANCE.to_owned());
    record.media_type = Some("application/json".to_owned());
    record.fetched_at_unix = Some(999);
    record.fresh_secs = Some(0);
    record.availability = crate::store::AttestationAvailability::Cached;
    harness
        .state
        .meta
        .put_upstream_attestation("pypi", &digest, FILENAME, &record)
        .unwrap();

    let (status, _, body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, PYPI_PROVENANCE);
}

#[tokio::test]
async fn test_concurrent_upstream_attestation_cache_misses_share_one_fetch() {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "b".repeat(64);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200)
            .set_delay(std::time::Duration::from_millis(50))
            .set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;
    let uri = upstream_provenance_uri(&digest);

    let (first, second) = tokio::join!(get(&harness.state, &uri, None), get(&harness.state, &uri, None),);

    let request_count = provenance_requests(&harness).await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.2);
    assert_eq!(second.0, StatusCode::OK, "{}", second.2);
    assert_eq!(first.2, PYPI_PROVENANCE);
    assert_eq!(second.2, PYPI_PROVENANCE);
    assert_eq!(request_count, 1);
}

#[tokio::test]
async fn test_upstream_attestation_cache_reloads_a_locator_changed_during_fetch() {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "d".repeat(64);
    upstream_page(&harness, &digest, "application/json").await;
    let replacement_url = format!("{}/integrity/replacement.provenance", harness.server.uri());
    let replacement_body = REVISED_PYPI_PROVENANCE;
    Mock::given(method("GET"))
        .and(path("/integrity/replacement.provenance"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(replacement_body, "application/json"))
        .mount(&harness.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/integrity/{FILENAME}.provenance")))
        .respond_with(replace_attestation_on_response(
            harness.state.meta.clone(),
            digest.clone(),
            UpstreamAttestation::remote(&replacement_url, "pypi", "peryxpkg", None),
            ResponseTemplate::new(200).set_body_raw(PYPI_PROVENANCE, "application/json"),
        ))
        .mount(&harness.server)
        .await;

    let (status, _, body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, replacement_body);
    assert_eq!(provenance_requests(&harness).await, 2);
    let record = harness
        .state
        .meta
        .get_upstream_attestation("pypi", "peryxpkg", &digest, FILENAME)
        .unwrap()
        .unwrap();
    assert_eq!(record.url, replacement_url);
    assert_eq!(record.body.as_deref(), Some(replacement_body));
}

#[rstest]
#[case::cache(None)]
#[case::no_store(Some("no-store"))]
#[tokio::test]
async fn test_upstream_attestation_cache_bounds_repeated_locator_changes(#[case] cache_control: Option<&str>) {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "e".repeat(64);
    upstream_page(&harness, &digest, "application/json").await;
    let second_url = format!("{}/integrity/second.provenance", harness.server.uri());
    let final_url = format!("{}/integrity/final.provenance", harness.server.uri());
    let response = || {
        cache_control.map_or_else(
            || ResponseTemplate::new(200).set_body_raw(PYPI_PROVENANCE, "application/json"),
            |value| {
                ResponseTemplate::new(200)
                    .insert_header("cache-control", value)
                    .set_body_raw(PYPI_PROVENANCE, "application/json")
            },
        )
    };
    Mock::given(method("GET"))
        .and(path("/integrity/second.provenance"))
        .respond_with(replace_attestation_on_response(
            harness.state.meta.clone(),
            digest.clone(),
            UpstreamAttestation::remote(&final_url, "pypi", "peryxpkg", None),
            response(),
        ))
        .mount(&harness.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/integrity/{FILENAME}.provenance")))
        .respond_with(replace_attestation_on_response(
            harness.state.meta.clone(),
            digest.clone(),
            UpstreamAttestation::remote(&second_url, "pypi", "peryxpkg", None),
            response(),
        ))
        .mount(&harness.server)
        .await;

    let (status, _, body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body.contains("upstream is unavailable and no cached page exists"),
        "{body}"
    );
    assert_eq!(provenance_requests(&harness).await, 2);
    let record = harness
        .state
        .meta
        .get_upstream_attestation("pypi", "peryxpkg", &digest, FILENAME)
        .unwrap()
        .unwrap();
    assert_eq!(record.url, final_url);
    assert!(record.body.is_none());
}

#[rstest]
#[case::etag("etag", "\"attestation-1\"", "if-none-match")]
#[case::last_modified("last-modified", "Wed, 21 Oct 2015 07:28:00 GMT", "if-modified-since")]
#[tokio::test]
async fn test_upstream_attestation_cache_revalidates_and_retains_on_304(
    #[case] response_header: &str,
    #[case] validator: &str,
    #[case] request_header: &str,
) {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "4".repeat(64);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200)
            .insert_header("cache-control", "max-age=60")
            .insert_header(response_header, validator)
            .set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;
    get(&harness.state, &upstream_provenance_uri(&digest), None).await;
    harness.clock.store(1_061, Ordering::Relaxed);
    Mock::given(method("GET"))
        .and(path(format!("/integrity/{FILENAME}.provenance")))
        .respond_with(ResponseTemplate::new(304).insert_header("cache-control", "max-age=30"))
        .with_priority(1)
        .mount(&harness.server)
        .await;

    let (status, _, body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, PYPI_PROVENANCE);
    assert_eq!(provenance_requests(&harness).await, 2);
    let request = harness
        .server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path().starts_with("/integrity/"))
        .last()
        .unwrap();
    assert_eq!(request.headers.get(request_header).unwrap(), validator);
    let record = harness
        .state
        .meta
        .get_upstream_attestation("pypi", "peryxpkg", &digest, FILENAME)
        .unwrap()
        .unwrap();
    assert_eq!(record.fetched_at_unix, Some(1_061));
    assert_eq!(record.fresh_secs, Some(30));
}

#[tokio::test]
async fn test_upstream_attestation_304_no_store_refetches_once_without_retention() {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "41".repeat(32);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200)
            .insert_header("cache-control", "max-age=60")
            .insert_header("etag", "\"attestation-1\"")
            .set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;
    get(&harness.state, &upstream_provenance_uri(&digest), None).await;
    harness.clock.store(1_061, Ordering::Relaxed);
    harness.server.reset().await;
    Mock::given(method("GET"))
        .and(path(format!("/integrity/{FILENAME}.provenance")))
        .and(match_header("if-none-match", "\"attestation-1\""))
        .respond_with(ResponseTemplate::new(304).insert_header("cache-control", "no-store"))
        .with_priority(1)
        .mount(&harness.server)
        .await;
    mount_provenance(
        &harness,
        ResponseTemplate::new(200)
            .insert_header("cache-control", "no-store")
            .set_body_raw(REVISED_PYPI_PROVENANCE, "application/json"),
    )
    .await;

    let (status, headers, body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, REVISED_PYPI_PROVENANCE);
    assert_eq!(headers["x-peryx-provenance-availability"], "remote-only");
    assert_eq!(provenance_requests(&harness).await, 2);
    let record = harness
        .state
        .meta
        .get_upstream_attestation("pypi", "peryxpkg", &digest, FILENAME)
        .unwrap()
        .unwrap();
    assert!(record.body.is_none());
    assert!(record.etag.is_none());
}

#[tokio::test]
async fn test_upstream_attestation_304_no_store_follows_a_replaced_locator() {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "43".repeat(32);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200)
            .insert_header("cache-control", "max-age=60")
            .insert_header("etag", "\"attestation-1\"")
            .set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;
    get(&harness.state, &upstream_provenance_uri(&digest), None).await;
    harness.clock.store(1_061, Ordering::Relaxed);
    harness.server.reset().await;
    let replacement_url = format!("{}/integrity/replacement.provenance", harness.server.uri());
    Mock::given(method("GET"))
        .and(path(format!("/integrity/{FILENAME}.provenance")))
        .respond_with(replace_attestation_on_response(
            harness.state.meta.clone(),
            digest.clone(),
            UpstreamAttestation::remote(&replacement_url, "pypi", "peryxpkg", None),
            ResponseTemplate::new(304).insert_header("cache-control", "no-store"),
        ))
        .mount(&harness.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/integrity/replacement.provenance"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("cache-control", "no-store")
                .set_body_raw(REVISED_PYPI_PROVENANCE, "application/json"),
        )
        .mount(&harness.server)
        .await;

    let (status, headers, body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, REVISED_PYPI_PROVENANCE);
    assert_eq!(headers["x-peryx-provenance-availability"], "remote-only");
    assert_eq!(provenance_requests(&harness).await, 2);
    let record = harness
        .state
        .meta
        .get_upstream_attestation("pypi", "peryxpkg", &digest, FILENAME)
        .unwrap()
        .unwrap();
    assert_eq!(record.url, replacement_url);
    assert!(record.body.is_none());
}

#[tokio::test]
async fn test_upstream_attestation_304_no_store_bounds_repeated_locator_changes() {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "44".repeat(32);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200)
            .insert_header("cache-control", "max-age=60")
            .insert_header("etag", "\"attestation-1\"")
            .set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;
    get(&harness.state, &upstream_provenance_uri(&digest), None).await;
    harness.clock.store(1_061, Ordering::Relaxed);
    harness.server.reset().await;
    let second_url = format!("{}/integrity/second.provenance", harness.server.uri());
    let final_url = format!("{}/integrity/final.provenance", harness.server.uri());
    Mock::given(method("GET"))
        .and(path(format!("/integrity/{FILENAME}.provenance")))
        .respond_with(replace_attestation_on_response(
            harness.state.meta.clone(),
            digest.clone(),
            UpstreamAttestation::remote(&second_url, "pypi", "peryxpkg", None),
            ResponseTemplate::new(304).insert_header("cache-control", "no-store"),
        ))
        .mount(&harness.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/integrity/second.provenance"))
        .respond_with(replace_attestation_on_response(
            harness.state.meta.clone(),
            digest.clone(),
            UpstreamAttestation::remote(&final_url, "pypi", "peryxpkg", None),
            ResponseTemplate::new(304).insert_header("cache-control", "no-store"),
        ))
        .mount(&harness.server)
        .await;

    let (status, ..) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(provenance_requests(&harness).await, 2);
    assert_eq!(
        harness
            .state
            .meta
            .get_upstream_attestation("pypi", "peryxpkg", &digest, FILENAME)
            .unwrap()
            .unwrap()
            .url,
        final_url
    );
}

#[tokio::test]
async fn test_upstream_attestation_304_no_store_does_not_refetch_repeatedly() {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "42".repeat(32);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200)
            .insert_header("cache-control", "max-age=60")
            .insert_header("etag", "\"attestation-1\"")
            .set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;
    get(&harness.state, &upstream_provenance_uri(&digest), None).await;
    harness.clock.store(1_061, Ordering::Relaxed);
    harness.server.reset().await;
    mount_provenance(
        &harness,
        ResponseTemplate::new(304).insert_header("cache-control", "no-store"),
    )
    .await;

    let (status, ..) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(provenance_requests(&harness).await, 2);
}

#[rstest]
#[case::server_error(StatusCode::SERVICE_UNAVAILABLE)]
#[case::request_timeout(StatusCode::REQUEST_TIMEOUT)]
#[case::rate_limit(StatusCode::TOO_MANY_REQUESTS)]
#[tokio::test]
async fn test_upstream_attestation_cache_serves_stale_on_transient_failure(#[case] failure: StatusCode) {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "5".repeat(64);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200)
            .insert_header("cache-control", "max-age=60")
            .set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;
    get(&harness.state, &upstream_provenance_uri(&digest), None).await;
    harness.clock.store(1_061, Ordering::Relaxed);
    harness.server.reset().await;
    mount_provenance(&harness, ResponseTemplate::new(failure.as_u16())).await;

    let (status, _, body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, PYPI_PROVENANCE);
    let record = harness
        .state
        .meta
        .get_upstream_attestation("pypi", "peryxpkg", &digest, FILENAME)
        .unwrap()
        .unwrap();
    assert_eq!(record.body.as_deref(), Some(PYPI_PROVENANCE));
}

#[tokio::test]
async fn test_invalid_upstream_attestation_does_not_replace_a_cached_body() {
    let harness = upstream_harness(RemoteMetadataMode::Cache).await;
    let digest = "6".repeat(64);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200)
            .insert_header("cache-control", "max-age=60")
            .set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;
    get(&harness.state, &upstream_provenance_uri(&digest), None).await;
    let expected = harness
        .state
        .meta
        .get_upstream_attestation("pypi", "peryxpkg", &digest, FILENAME)
        .unwrap()
        .unwrap();
    harness.clock.store(1_061, Ordering::Relaxed);
    harness.server.reset().await;
    mount_provenance(
        &harness,
        ResponseTemplate::new(200).set_body_raw(r#"{"version":2}"#, "application/json"),
    )
    .await;

    let (status, ..) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let record = harness
        .state
        .meta
        .get_upstream_attestation("pypi", "peryxpkg", &digest, FILENAME)
        .unwrap()
        .unwrap();
    assert_eq!(record, expected);
    harness.server.reset().await;
    mount_provenance(&harness, ResponseTemplate::new(503)).await;

    let (stale_status, _, stale_body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!((stale_status, stale_body), (StatusCode::OK, PYPI_PROVENANCE.to_owned()));
}
