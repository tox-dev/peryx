use futures_util::TryStreamExt as _;
use rstest::rstest;
use wiremock::matchers::{header, header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{mount_get, simple_client};
use crate::client::{Auth, UpstreamClient, UpstreamError, redact_url};

#[tokio::test]
async fn test_fetch_bytes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.bin"))
        .and(header("accept-encoding", "identity"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .mount(&server)
        .await;
    let client = simple_client(&server);

    let bytes = client
        .fetch_bytes(&format!("{}/files/pkg.bin", server.uri()))
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"artifactbytes");
}

#[tokio::test]
async fn test_send_validated_uses_modification_time_without_etag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(&server)
        .await;
    let client = simple_client(&server);

    let response = client
        .send_validated(
            client.base().clone(),
            "application/json",
            None,
            Some("Wed, 15 Jul 2026 12:00:00 GMT"),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::NOT_MODIFIED);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers.get("if-modified-since").unwrap().to_str().unwrap(),
        "Wed, 15 Jul 2026 12:00:00 GMT"
    );
}

#[tokio::test]
async fn test_send_validated_prefers_etag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .and(header("if-none-match", "catalog-etag"))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(&server)
        .await;
    let client = simple_client(&server);

    client
        .send_validated(
            client.base().clone(),
            "application/json",
            Some("catalog-etag"),
            Some("Wed, 15 Jul 2026 12:00:00 GMT"),
        )
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    assert!(requests[0].headers.get("if-modified-since").is_none());
}

#[tokio::test]
async fn test_fetch_bytes_limited_accepts_body_at_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.bin"))
        .and(header("accept-encoding", "identity"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .mount(&server)
        .await;
    let client = simple_client(&server);

    let bytes = client
        .fetch_bytes_limited(&format!("{}/files/pkg.bin", server.uri()), b"artifactbytes".len())
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"artifactbytes");
}

#[tokio::test]
async fn test_fetch_bytes_limited_rejects_content_length_over_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .mount(&server)
        .await;
    let client = simple_client(&server);

    let err = client
        .fetch_bytes_limited(&format!("{}/files/pkg.bin", server.uri()), 9)
        .await
        .unwrap_err();

    assert_eq!(err.user_message(), "upstream response exceeds the 9-byte limit");
}

#[tokio::test]
async fn test_stream_bytes_streams_file() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.bin"))
        .and(header("accept-encoding", "identity"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .mount(&server)
        .await;
    let client = simple_client(&server);

    let bytes = client
        .stream_bytes(&format!("{}/files/pkg.bin", server.uri()))
        .await
        .unwrap()
        .try_fold(Vec::new(), |mut bytes, chunk| async move {
            bytes.extend_from_slice(&chunk);
            Ok(bytes)
        })
        .await
        .unwrap();

    assert_eq!(bytes, b"artifactbytes");
}

#[tokio::test]
async fn test_fetch_range_requests_identity_bytes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.bin"))
        .and(header("accept-encoding", "identity"))
        .and(header("range", "bytes=1-3"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 1-3/5")
                .set_body_bytes(b"hee".to_vec()),
        )
        .mount(&server)
        .await;
    let client = simple_client(&server);

    let bytes = client
        .fetch_range(&format!("{}/files/pkg.bin", server.uri()), 1, 3)
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"hee");
    assert!(client.may_support_ranges());
}

#[tokio::test]
async fn test_fetch_range_accepts_unknown_total() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.bin"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 1-3/*")
                .set_body_bytes(b"hee".to_vec()),
        )
        .mount(&server)
        .await;
    let client = simple_client(&server);

    let bytes = client
        .fetch_range(&format!("{}/files/pkg.bin", server.uri()), 1, 3)
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"hee");
    assert!(client.may_support_ranges());
}

#[rstest]
#[case::unsupported_status(200, None, b"whole-file".as_slice())]
#[case::missing_content_range(206, None, b"hee".as_slice())]
#[case::non_bytes_unit(206, Some("items 1-3/5"), b"hee".as_slice())]
#[case::missing_total(206, Some("bytes 1-3"), b"hee".as_slice())]
#[case::missing_span(206, Some("bytes 1/5"), b"hee".as_slice())]
#[case::offset_mismatch(206, Some("bytes 2-4/5"), b"hee".as_slice())]
#[case::end_mismatch(206, Some("bytes 1-4/6"), b"hee".as_slice())]
#[case::non_numeric_total(206, Some("bytes 1-3/not-a-number"), b"hee".as_slice())]
#[case::total_not_past_end(206, Some("bytes 1-3/3"), b"hee".as_slice())]
#[tokio::test]
async fn test_fetch_range_disables_on_bad_range_response(
    #[case] status: u16,
    #[case] content_range: Option<&str>,
    #[case] body: &[u8],
) {
    let server = MockServer::start().await;
    let mut response = ResponseTemplate::new(status).set_body_bytes(body.to_vec());
    if let Some(content_range) = content_range {
        response = response.insert_header("content-range", content_range);
    }
    mount_get(&server, "/files/pkg.bin", response).await;
    let client = simple_client(&server);

    let err = client
        .fetch_range(&format!("{}/files/pkg.bin", server.uri()), 1, 3)
        .await
        .unwrap_err();

    assert!(err.disables_ranges());
    assert!(!client.may_support_ranges());
}

#[tokio::test]
async fn test_fetch_range_rejects_short_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.bin"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 1-3/5")
                .set_body_bytes(b"he".to_vec()),
        )
        .mount(&server)
        .await;
    let client = simple_client(&server);

    let err = client
        .fetch_range(&format!("{}/files/pkg.bin", server.uri()), 1, 3)
        .await
        .unwrap_err();

    assert_eq!(
        err.to_string(),
        "upstream returned an invalid byte range response: expected 3 bytes, received 2"
    );
    assert!(!client.may_support_ranges());
}

#[tokio::test]
async fn test_head_file_for_range_requires_byte_ranges() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/files/pkg.bin"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-length", "10"))
        .mount(&server)
        .await;
    let client = simple_client(&server);

    let err = client
        .head_file_for_range(&format!("{}/files/pkg.bin", server.uri()))
        .await
        .unwrap_err();

    assert!(err.disables_ranges());
    assert!(!client.may_support_ranges());
}

#[tokio::test]
async fn test_head_file_for_range_requires_content_length() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/files/pkg.bin"))
        .respond_with(ResponseTemplate::new(200).insert_header("accept-ranges", "bytes"))
        .mount(&server)
        .await;
    let client = simple_client(&server);

    let err = client
        .head_file_for_range(&format!("{}/files/pkg.bin", server.uri()))
        .await
        .unwrap_err();

    assert!(err.disables_ranges());
    assert!(!client.may_support_ranges());
}

#[tokio::test]
async fn test_new_adds_trailing_slash() {
    let client = UpstreamClient::new("https://upstream.example/artifacts").unwrap();

    assert_eq!(client.base_url(), "https://upstream.example/artifacts/");
}

#[test]
fn test_new_rejects_invalid_url() {
    let err = UpstreamClient::new("not a url").unwrap_err();
    assert!(matches!(err, UpstreamError::Url(_)));
    assert_eq!(err.user_message(), "invalid upstream URL: relative URL without a base");
}

#[tokio::test]
async fn test_fetch_bytes_preserves_basic_auth_on_same_host_redirect() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/redirect/pkg.bin"))
        .and(header_regex("authorization", "^Basic "))
        .respond_with(ResponseTemplate::new(302).insert_header("location", format!("{}/files/pkg.bin", server.uri())))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.bin"))
        .and(header_regex("authorization", "^Basic "))
        .and(header("accept-encoding", "identity"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .mount(&server)
        .await;
    let client = UpstreamClient::with_auth(
        &format!("{}/simple/", server.uri()),
        Auth::Basic {
            username: "__token__".to_owned(),
            password: "secret".to_owned(),
        },
    )
    .unwrap();

    let bytes = client
        .fetch_bytes(&format!("{}/redirect/pkg.bin", server.uri()))
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"artifactbytes");
}

#[tokio::test]
async fn test_fetch_bytes_strips_basic_auth_on_cross_origin_redirect() {
    let origin = MockServer::start().await;
    let target = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/redirect/pkg.bin"))
        .and(header_regex("authorization", "^Basic "))
        .respond_with(ResponseTemplate::new(302).insert_header("location", format!("{}/pkg.bin", target.uri())))
        .mount(&origin)
        .await;
    Mock::given(method("GET"))
        .and(path("/pkg.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .mount(&target)
        .await;
    let client = UpstreamClient::with_auth(
        &format!("{}/simple/", origin.uri()),
        Auth::Basic {
            username: "reader".to_owned(),
            password: "secret".to_owned(),
        },
    )
    .unwrap();

    let bytes = client
        .fetch_bytes(&format!("{}/redirect/pkg.bin", origin.uri()))
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"artifactbytes");
    assert_eq!(
        target.received_requests().await.unwrap()[0]
            .headers
            .get("authorization"),
        None
    );
}

#[tokio::test]
async fn test_fetch_bytes_does_not_authenticate_a_direct_cross_origin_url() {
    let origin = MockServer::start().await;
    let target = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/pkg.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .mount(&target)
        .await;
    let client = UpstreamClient::with_auth(
        &format!("{}/simple/", origin.uri()),
        Auth::Basic {
            username: "reader".to_owned(),
            password: "secret".to_owned(),
        },
    )
    .unwrap();

    let bytes = client.fetch_bytes(&format!("{}/pkg.bin", target.uri())).await.unwrap();

    assert_eq!(&bytes[..], b"artifactbytes");
    assert_eq!(
        target.received_requests().await.unwrap()[0]
            .headers
            .get("authorization"),
        None
    );
}

#[test]
fn test_auth_status_redacts_basic_credentials_and_url_secrets() {
    let client = UpstreamClient::with_auth(
        "https://user:pass@example.invalid/simple/?token=secret#frag",
        Auth::Basic {
            username: "__token__".to_owned(),
            password: "secret".to_owned(),
        },
    )
    .unwrap();

    assert_eq!(client.auth_status().as_str(), "basic");
    assert_eq!(client.redacted_base_url(), "https://example.invalid/simple/");
}

#[rstest]
#[case::none(Auth::None, "none")]
#[case::basic(Auth::Basic { username: "reader".to_owned(), password: "secret".to_owned() }, "basic")]
#[case::bearer(Auth::Bearer("secret".to_owned()), "bearer")]
fn test_auth_status_classifies_the_configured_credential(#[case] auth: Auth, #[case] expected: &str) {
    let client = UpstreamClient::with_auth("https://example.invalid/simple/", auth).unwrap();

    assert_eq!(client.auth_status().as_str(), expected);
}

#[test]
fn test_auth_returns_the_configured_credentials() {
    let auth = Auth::Basic {
        username: "alice".to_owned(),
        password: "s3cret".to_owned(),
    };
    let client = UpstreamClient::with_auth("https://example.invalid/simple/", auth.clone()).unwrap();
    assert_eq!(client.auth().current().unwrap().auth(), &auth);
    assert_eq!(client.current_credential().unwrap().auth(), &auth);
    assert_eq!(
        UpstreamClient::new("https://example.invalid/simple/")
            .unwrap()
            .current_credential()
            .unwrap()
            .auth(),
        &Auth::None
    );
}

#[tokio::test]
async fn test_send_validated_uses_the_cross_origin_client() {
    let origin = MockServer::start().await;
    let target = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&target)
        .await;
    let client = simple_client(&origin);

    assert_eq!(
        client
            .send_validated(
                url::Url::parse(&format!("{}/simple/", target.uri())).unwrap(),
                "application/json",
                None,
                None,
            )
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
}

#[test]
fn test_redact_url_removes_credential_bearing_parts() {
    assert_eq!(
        redact_url("https://user:pass@example.invalid/simple/?token=secret#frag"),
        "https://example.invalid/simple/"
    );
    assert_eq!(redact_url("not a url"), "<invalid upstream URL>");
}

#[tokio::test]
async fn test_warm_reaches_the_upstream_host() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let client = simple_client(&server);
    assert_eq!(client.reachability().as_str(), "unknown");
    client.warm().await;
    assert_eq!(client.reachability().as_str(), "reachable");
}

#[tokio::test]
async fn test_warm_records_an_unreachable_upstream_host() {
    let client = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    client.warm().await;
    assert_eq!(client.reachability().as_str(), "unreachable");
}
